//! 工具箱桌面入口：批量改名、码率修复、可执行转码（分集重组 / 智能微调）、本机环境。
//!
//! 与 `dossier.rs` / `erase.rs` 的分工一致：这一层只做参数适配、进程编排与
//! 中文错误转换；改名规则、码率分级、切点规划等**计算全部在
//! `dossier-core::tools`**（有单元测试覆盖）。
//!
//! 长任务（重编码）用 `spawn_blocking` 包裹，避免阻塞 Tauri 主线程卡死界面；
//! 界面侧沿用擦除的「按钮禁用 + 完成后一次性回报」模式，不做进度流。

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use dossier_core::{
    bitrate_report, load_settings, nudge_frames, output_dir_hits_sources, plan_recut_segments,
    plan_rename, probe_episode, resolve_product_dir, BitrateItem, EpisodeFile, FfprobeConfig,
    ProbedEpisode,
};
use dossier_media::shot::{detect_shots, ShotDetectConfig};
use dossier_runtime::probe::probe_tool;
use dossier_runtime::{CommandRunner, CommandSpec, SystemRunner, ToolReport};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::dossier::{load_drama, probe_all};
use crate::paths::normalize;

/// 码率安全线默认值（与竞品一致的 4 Mbps）。
pub const DEFAULT_MIN_BITRATE_KBPS: u64 = 4000;
/// 单条 ffmpeg 命令的超时：短剧分集都在几分钟量级，10 分钟足够。
const FFMPEG_TIMEOUT_SECS: u64 = 600;

// ---------------------------------------------------------------------------
// 运行环境解析与一键安装
// ---------------------------------------------------------------------------

/// 应用数据目录下的运行环境安装位置（一键安装的 ffmpeg/ffprobe 放这里）。
fn app_bin_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|dir| dir.join("bin"))
        .map_err(|error| format!("无法定位应用数据目录：{error}。请检查系统权限。"))
}

/// 一键安装落地的可执行文件名：Windows 必须带 `.exe`，其余平台不带。
fn exe_name(base: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

/// 读取当前设置（缺失/损坏回退默认，与 `load_app_settings` 同语义）。
pub(crate) fn read_settings(app: &AppHandle) -> Result<dossier_core::AppSettings, String> {
    let path = crate::settings_path(app)?;
    Ok(load_settings(&path).settings)
}

/// ffmpeg 解析顺序：设置里显式路径 → 一键安装目录 → `PATH`。
pub(crate) fn resolve_ffmpeg(app: &AppHandle) -> Result<PathBuf, String> {
    if let Some(path) = read_settings(app)?.ffmpeg_path {
        if path.is_file() {
            return Ok(path);
        }
    }
    if let Ok(bin) = app_bin_dir(app) {
        let candidate = bin.join(exe_name("ffmpeg"));
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Ok(PathBuf::from("ffmpeg"))
}

/// ffprobe 解析顺序：界面显式提示（填了就用，错误让它自然暴露）→ 设置 → 一键安装目录 → `PATH`。
pub(crate) fn resolve_ffprobe(app: &AppHandle, hint: Option<&str>) -> Result<PathBuf, String> {
    if let Some(hint) = hint.map(str::trim).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(hint));
    }
    if let Some(path) = read_settings(app)?.ffprobe_path {
        if path.is_file() {
            return Ok(path);
        }
    }
    if let Ok(bin) = app_bin_dir(app) {
        let candidate = bin.join(exe_name("ffprobe"));
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Ok(PathBuf::from("ffprobe"))
}

// ---------------------------------------------------------------------------
// 本机环境
// ---------------------------------------------------------------------------

/// 本机环境探测结果，供「全局横幅」与「设置-本机环境」共用。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStatus {
    pub ffmpeg: ToolReport,
    pub ffprobe: ToolReport,
    pub python: ToolReport,
    /// 运行环境自动安装只在有可用下载源的平台提供（macOS / Windows）；
    /// 其余平台前端据此隐藏入口，避免给出「点了必然报错」的按钮。
    pub install_supported: bool,
    /// 平台标识（macos / windows / linux），前端据此给出对应平台的手动安装指引。
    pub platform: String,
}

#[tauri::command]
pub fn runtime_status(
    app: AppHandle,
    ffprobe_path: Option<String>,
) -> Result<RuntimeStatus, String> {
    let ffmpeg = resolve_ffmpeg(&app)?;
    let ffprobe = resolve_ffprobe(&app, ffprobe_path.as_deref())?;
    let runner = SystemRunner::new();
    Ok(RuntimeStatus {
        // 只探解释器版本（秒级）；torch/onnxruntime 导入太慢，留给擦除页自查。
        ffmpeg: probe_tool(&runner, "ffmpeg", &ffmpeg, &["-hide_banner", "-version"]),
        ffprobe: probe_tool(&runner, "ffprobe", &ffprobe, &["-hide_banner", "-version"]),
        python: probe_tool(&runner, "python", Path::new("python3"), &["--version"]),
        install_supported: cfg!(any(target_os = "macos", target_os = "windows")),
        platform: std::env::consts::OS.to_string(),
    })
}

// ---------------------------------------------------------------------------
// 长任务单元进度
// ---------------------------------------------------------------------------

/// 工具箱长任务的单元进度（`task-progress` 事件）。
///
/// 与逐集探测的 `probe-progress` 同构，但服务于**由本应用直接驱动 ffmpeg** 的任务
/// （智能微调 / 分集重组 / 码率修复）：这些任务之前只有「不确定进度条 + 已用秒数」，
/// 一部剧重编码几分钟时界面看着像卡死。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskProgress {
    /// 任务标识（`transcode` / `bitrate`）：界面目前只用文案，标识留给排查与后续分措辞。
    pub task: String,
    pub done: u64,
    pub total: u64,
    pub label: String,
}

/// 发一条单元进度。UI 未监听或窗口已关闭都不该让长任务失败，因此忽略发送错误。
fn emit_task_progress(app: &AppHandle, task: &str, done: u64, total: u64, label: &str) {
    let payload = TaskProgress {
        task: task.to_string(),
        done,
        total,
        label: label.to_string(),
    };
    let _ = app.emit("task-progress", payload);
}

// ---------------------------------------------------------------------------
// 批量改名
// ---------------------------------------------------------------------------

/// 前端回传的改名项（只含文件名，防路径注入在 `apply_rename` 里校验）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameRequestItem {
    pub from: String,
    pub to: String,
}

#[tauri::command]
pub fn list_rename_plan(root: String, name: String) -> Result<dossier_core::RenamePlan, String> {
    let drama = load_drama(&root, &name)?;
    Ok(plan_rename(&drama))
}

/// 校验「纯文件名」：拒绝任何带目录成分的名字。
fn plain_file_name(value: &str) -> Result<&str, String> {
    let path = Path::new(value);
    if value.is_empty()
        || path.components().count() != 1
        || path
            .parent()
            .map(|p| !p.as_os_str().is_empty())
            .unwrap_or(false)
    {
        return Err(format!("非法文件名（不允许包含路径）：{value}"));
    }
    Ok(value)
}

#[tauri::command]
pub fn apply_rename(
    root: String,
    name: String,
    items: Vec<RenameRequestItem>,
) -> Result<usize, String> {
    let drama = load_drama(&root, &name)?;
    let dir = drama.dir;

    // 先整体预检，全部通过才动手（避免改到一半失败留下半成品状态）。
    let mut operations: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(items.len());
    let mut seen_targets: HashSet<String> = HashSet::new();
    for item in &items {
        let from = plain_file_name(&item.from)?;
        let to = plain_file_name(&item.to)?;
        if from == to {
            continue;
        }
        let source = dir.join(from);
        let target = dir.join(to);
        if !source.is_file() {
            return Err(format!("源文件不存在：{from}"));
        }
        if target.exists() {
            return Err(format!("目标名已存在，拒绝覆盖：{to}"));
        }
        if !seen_targets.insert(to.to_string()) {
            return Err(format!("多个文件改成了同一个名字：{to}"));
        }
        operations.push((source, target));
    }

    let mut renamed = 0;
    for (source, target) in operations {
        std::fs::rename(&source, &target)
            .map_err(|error| format!("重命名 {} 失败：{error}", source.display()))?;
        renamed += 1;
    }
    Ok(renamed)
}

// ---------------------------------------------------------------------------
// 码率修复
// ---------------------------------------------------------------------------

/// 码率探测结果（附阈值判断）。
#[tauri::command]
pub fn probe_bitrate(
    app: AppHandle,
    root: String,
    name: String,
    min_kbps: Option<u64>,
    ffprobe_path: Option<String>,
) -> Result<Vec<BitrateItem>, String> {
    let drama = load_drama(&root, &name)?;
    let ffprobe = resolve_ffprobe(&app, ffprobe_path.as_deref())?;
    let episodes = probe_all(&drama, Some(ffprobe.display().to_string()));
    if episodes.is_empty() {
        return Err("没有任何成功探测规格的分集；请先确认素材可读且 ffprobe 可用。".into());
    }
    let min_bps = min_kbps.unwrap_or(DEFAULT_MIN_BITRATE_KBPS) * 1000;
    Ok(bitrate_report(&episodes, min_bps))
}

/// 修复结果：修了什么、跳过了什么。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FixBitrateReport {
    pub fixed: Vec<FixedItem>,
    pub failed: Vec<FailedItem>,
    pub untouched: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FixedItem {
    pub file_name: String,
    pub from_kbps: u64,
    pub to_kbps: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailedItem {
    pub file_name: String,
    pub reason: String,
}

/// 跑一条 ffmpeg 命令；失败时把 stderr 首行带回给界面。
fn run_ffmpeg(program: &Path, args: &[&str]) -> Result<(), String> {
    let mut spec = CommandSpec::new(program.display().to_string());
    for arg in args {
        spec = spec.arg(*arg);
    }
    spec = spec.timeout(std::time::Duration::from_secs(FFMPEG_TIMEOUT_SECS));
    let runner = SystemRunner::new();
    let output = runner
        .run(&spec)
        .map_err(|error| format!("ffmpeg 启动失败（{}）：{error}", program.display()))?;
    if output.success() {
        return Ok(());
    }
    let detail = output.combined();
    let first = detail
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("（无输出）");
    Err(format!("ffmpeg 退出码 {}：{first}", output.code_label()))
}

/// 重编码一条视频到目标码率：先写 `*.fixing.mp4` 临时文件，成功后原子替换原文件。
fn reencode_to_bitrate(ffmpeg: &Path, input: &Path, min_bps: u64) -> Result<(), String> {
    let target_kbps = (min_bps / 1000).max(1);
    let maxrate_kbps = target_kbps + target_kbps / 4;
    let bufsize_kbps = target_kbps * 2;
    let tmp = input.with_extension("fixing.mp4");
    let tmp_str = tmp.display().to_string();
    let input_str = input.display().to_string();
    let result = run_ffmpeg(
        ffmpeg,
        &[
            "-y",
            "-i",
            &input_str,
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-b:v",
            &format!("{target_kbps}k"),
            "-maxrate",
            &format!("{maxrate_kbps}k"),
            "-bufsize",
            &format!("{bufsize_kbps}k"),
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-movflags",
            "+faststart",
            &tmp_str,
        ],
    );
    match result {
        Ok(()) => std::fs::rename(&tmp, input).map_err(|error| {
            let _ = std::fs::remove_file(&tmp);
            format!("替换原文件失败：{error}")
        }),
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            Err(error)
        }
    }
}

fn fix_bitrate_blocking(
    app: &AppHandle,
    root: String,
    name: String,
    min_kbps: Option<u64>,
    ffprobe_path: Option<String>,
) -> Result<FixBitrateReport, String> {
    let ffmpeg = resolve_ffmpeg(app)?;
    let ffprobe = resolve_ffprobe(app, ffprobe_path.as_deref())?;
    let drama = load_drama(&root, &name)?;
    let episodes = probe_all(&drama, Some(ffprobe.display().to_string()));
    if episodes.is_empty() {
        return Err("没有任何成功探测规格的分集；请先确认素材可读且 ffprobe 可用。".into());
    }
    let min_bps = min_kbps.unwrap_or(DEFAULT_MIN_BITRATE_KBPS) * 1000;
    let report = bitrate_report(&episodes, min_bps);

    let mut fixed = Vec::new();
    let mut failed = Vec::new();
    let mut untouched = 0_usize;
    let fix_total = report.iter().filter(|item| item.needs_fix).count() as u64;
    let mut fix_index = 0_u64;
    for item in &report {
        if !item.needs_fix {
            untouched += 1;
            continue;
        }
        fix_index += 1;
        emit_task_progress(
            app,
            "bitrate",
            fix_index - 1,
            fix_total,
            &format!(
                "正在重编码第 {}/{} 个分集：{}",
                fix_index, fix_total, item.file_name
            ),
        );
        let from_kbps = item.bitrate_bps.unwrap_or(0) / 1000;
        let path = PathBuf::from(&item.path);
        match reencode_to_bitrate(&ffmpeg, &path, min_bps) {
            Ok(()) => {
                // 修完回探一次，把实际码率报给界面。
                let re_probed = probe_episode(
                    &EpisodeFile {
                        path: path.clone(),
                        file_name: item.file_name.clone(),
                        size_bytes: 0,
                        episode_number: None,
                    },
                    &FfprobeConfig {
                        bin: Some(ffprobe.clone()),
                    },
                );
                fixed.push(FixedItem {
                    file_name: item.file_name.clone(),
                    from_kbps,
                    to_kbps: re_probed
                        .ok()
                        .and_then(|probed| probed.spec.bitrate_bps)
                        .map(|bps| bps / 1000),
                });
            }
            Err(reason) => failed.push(FailedItem {
                file_name: item.file_name.clone(),
                reason,
            }),
        }
    }
    Ok(FixBitrateReport {
        fixed,
        failed,
        untouched,
    })
}

#[tauri::command]
pub async fn fix_bitrate(
    app: AppHandle,
    root: String,
    name: String,
    min_kbps: Option<u64>,
    ffprobe_path: Option<String>,
) -> Result<FixBitrateReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        fix_bitrate_blocking(&app, root, name, min_kbps, ffprobe_path)
    })
    .await
    .map_err(|error| format!("码率修复任务失败：{error}"))?
}

// ---------------------------------------------------------------------------
// 可执行转码：智能微调 / 分集重组
// ---------------------------------------------------------------------------

/// 转码模式与参数。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscodeRequest {
    /// `nudge`（智能微调）或 `recut`（分集重组）。
    pub mode: String,
    /// recut 的目标集数；缺省 = 保持原集数。
    pub target_count: Option<usize>,
    /// 输出目录；缺省 = 设置里的「默认输出目录」（产物根目录）下的 `<剧名>/<微调输出|重组输出>`，
    /// 设置也为空时退回剧目目录内的同名子目录。
    pub output_dir: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscodeItemReport {
    pub source: String,
    pub output: String,
    pub detail: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscodeRunReport {
    pub mode: String,
    pub output_dir: String,
    pub items: Vec<TranscodeItemReport>,
    pub note: String,
}

/// 统一的重编码参数：画质优先兼容，音频统一 AAC。
/// 委托给 `crate::encode`（M4b-1）：三个转码功能和处理版本共用同一份参数生成，
/// 旧功能没有输出规格，按「不限码率、软件编码」的口径走（`u64::MAX` = 不封顶）。
fn encode_output_args(output: &Path) -> Vec<String> {
    let request = crate::encode::EncodeRequest {
        output,
        max_video_kbps: None,
        audio_kbps: 128,
        metadata_tags: false,
    };
    let mut args = crate::encode::encode_output_args("libx264", false, &request);
    args.push(output.display().to_string());
    args
}

fn seed_for(file_name: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    file_name.hash(&mut hasher);
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos() as u64)
        .unwrap_or(0)
        .hash(&mut hasher);
    hasher.finish()
}

/// 智能微调：每集从开头裁掉 1~3 帧（帧率未知时按 30fps 折算），输出同名新文件。
fn transcode_nudge_blocking(
    ffmpeg: &Path,
    drama_name: &str,
    episodes: &[ProbedEpisode],
    output_dir: &Path,
    on_step: &mut dyn FnMut(u64, u64, &str),
) -> Result<TranscodeRunReport, String> {
    std::fs::create_dir_all(output_dir)
        .map_err(|error| format!("创建输出目录失败（{}）：{error}", output_dir.display()))?;
    let mut items = Vec::new();
    let total = episodes.len() as u64;
    for (index, episode) in episodes.iter().enumerate() {
        // 全剧重编码动辄几分钟，逐集上报让进度条能真的动起来（done = 已完成集数）。
        on_step(
            index as u64,
            total,
            &format!(
                "正在微调第 {}/{} 集：{}",
                index + 1,
                total,
                episode.file.file_name
            ),
        );
        let frames = nudge_frames(seed_for(&episode.file.file_name));
        let fps = episode.spec.fps.unwrap_or(30.0).max(1.0);
        let offset = frames as f64 / fps;
        let output = output_dir.join(&episode.file.file_name);
        let input_str = episode.file.path.display().to_string();
        let mut args: Vec<String> = vec![
            "-y".into(),
            "-ss".into(),
            format!("{offset:.6}"),
            "-i".into(),
            input_str.clone(),
        ];
        args.extend(encode_output_args(&output));
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run_ffmpeg(ffmpeg, &arg_refs).map_err(|error| {
            let _ = std::fs::remove_file(&output);
            format!("{}：{error}", episode.file.file_name)
        })?;
        items.push(TranscodeItemReport {
            source: input_str,
            output: output.display().to_string(),
            detail: format!("裁掉开头 {frames} 帧（{offset:.4} 秒，{fps:.2} fps）"),
        });
    }
    Ok(TranscodeRunReport {
        mode: "nudge".into(),
        output_dir: output_dir.display().to_string(),
        items,
        note: format!("《{drama_name}》每集开头已微调 1~3 帧，输出到新目录，原素材未改动。"),
    })
}

/// 一个跨源文件的连续片段：`(源文件, 本地起点秒, 时长秒)`。
struct SourceSpan {
    file: PathBuf,
    file_name: String,
    local_start: f64,
    duration: f64,
}

/// 把全局区间映射回各源文件的本地区间。
fn spans_for_segment(
    episodes: &[ProbedEpisode],
    file_starts: &[f64],
    segment: (f64, f64),
) -> Vec<SourceSpan> {
    let (seg_start, seg_end) = segment;
    let mut spans = Vec::new();
    for (index, episode) in episodes.iter().enumerate() {
        let file_start = file_starts[index];
        let file_end = file_start + episode.spec.duration_sec;
        let local_start = (seg_start - file_start).max(0.0);
        let local_end = (seg_end - file_start).min(file_end - file_start);
        if local_end - local_start > 0.05 {
            spans.push(SourceSpan {
                file: episode.file.path.clone(),
                file_name: episode.file.file_name.clone(),
                local_start,
                duration: local_end - local_start,
            });
        }
    }
    spans
}

/// 分集重组：全剧拼成一条时间线，在场景切点附近重新分集；切不到精确关键帧，
/// 因此每个片段统一重编码（保证切点帧级精确）。输出名保持「第NN集」以便再次扫描。
pub(crate) fn transcode_recut_blocking(
    ffmpeg: &Path,
    episodes: &[ProbedEpisode],
    target_count: usize,
    output_dir: &Path,
    on_step: &mut dyn FnMut(u64, u64, &str),
) -> Result<TranscodeRunReport, String> {
    std::fs::create_dir_all(output_dir)
        .map_err(|error| format!("创建输出目录失败（{}）：{error}", output_dir.display()))?;

    // 逐集检测切点，映射到全剧时间线。
    let mut boundaries: Vec<f64> = Vec::new();
    let mut file_starts: Vec<f64> = Vec::with_capacity(episodes.len());
    let mut offset = 0.0;
    let ffmpeg_str = ffmpeg.display().to_string();
    let detect_total = episodes.len() as u64;
    for (index, episode) in episodes.iter().enumerate() {
        on_step(
            index as u64,
            detect_total,
            &format!(
                "正在检测场景切点 第 {}/{} 集：{}",
                index + 1,
                detect_total,
                episode.file.file_name
            ),
        );
        file_starts.push(offset);
        let report = detect_shots(
            &ffmpeg_str,
            &episode.file.path,
            &ShotDetectConfig::default(),
        )
        .map_err(|error| format!("{} 场景检测失败：{error}", episode.file.file_name))?;
        boundaries.extend(report.boundaries.iter().map(|b| offset + b.time_sec));
        offset += episode.spec.duration_sec;
    }
    let total = offset;
    // 用 partial_cmp 保持 NaN 拒绝语义（`<=` 写法会放行 NaN，让重组段变成垃圾值）。
    if total.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
        return Err("全剧总时长为 0，无法重组。".into());
    }

    let segments = plan_recut_segments(&boundaries, total, target_count, 1.0);
    let mut items = Vec::new();
    let segment_total = segments.len() as u64;
    for (index, segment) in segments.iter().enumerate() {
        on_step(
            index as u64,
            segment_total,
            &format!("正在重组第 {}/{} 集", index + 1, segment_total),
        );
        let output = output_dir.join(format!("第{:02}集.mp4", index + 1));
        let spans = spans_for_segment(episodes, &file_starts, *segment);
        if spans.is_empty() {
            continue;
        }

        if spans.len() == 1 {
            // 单源片段：一条命令直达。
            let span = &spans[0];
            let mut args: Vec<String> = vec![
                "-y".into(),
                "-ss".into(),
                format!("{:.6}", span.local_start),
                "-i".into(),
                span.file.display().to_string(),
                "-t".into(),
                format!("{:.6}", span.duration),
            ];
            args.extend(encode_output_args(&output));
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            run_ffmpeg(ffmpeg, &arg_refs).map_err(|error| {
                let _ = std::fs::remove_file(&output);
                format!("第 {} 集：{error}", index + 1)
            })?;
        } else {
            // 跨源片段：先逐源切片，再用 concat demuxer 无损拼接（编码参数一致）。
            let mut part_paths: Vec<PathBuf> = Vec::new();
            for (part_index, span) in spans.iter().enumerate() {
                let part = output_dir.join(format!(".recut-{index:02}-part{part_index:02}.mp4"));
                let mut args: Vec<String> = vec![
                    "-y".into(),
                    "-ss".into(),
                    format!("{:.6}", span.local_start),
                    "-i".into(),
                    span.file.display().to_string(),
                    "-t".into(),
                    format!("{:.6}", span.duration),
                ];
                args.extend(encode_output_args(&part));
                let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
                if let Err(error) = run_ffmpeg(ffmpeg, &arg_refs) {
                    for part in &part_paths {
                        let _ = std::fs::remove_file(part);
                    }
                    let _ = std::fs::remove_file(&output);
                    return Err(format!(
                        "第 {} 集（{}）：{error}",
                        index + 1,
                        span.file_name
                    ));
                }
                part_paths.push(part);
            }

            let list_path = output_dir.join(format!(".recut-{index:02}-list.txt"));
            let list_body = part_paths
                .iter()
                .map(|part| format!("file '{}'", part.display()))
                .collect::<Vec<_>>()
                .join("\n");
            if let Err(error) = std::fs::write(&list_path, &list_body) {
                for part in &part_paths {
                    let _ = std::fs::remove_file(part);
                }
                return Err(format!("写 concat 清单失败：{error}"));
            }
            let list_str = list_path.display().to_string();
            let output_str = output.display().to_string();
            let concat_result = run_ffmpeg(
                ffmpeg,
                &[
                    "-y",
                    "-f",
                    "concat",
                    "-safe",
                    "0",
                    "-i",
                    &list_str,
                    "-c",
                    "copy",
                    &output_str,
                ],
            );
            for part in &part_paths {
                let _ = std::fs::remove_file(part);
            }
            let _ = std::fs::remove_file(&list_path);
            concat_result.map_err(|error| {
                let _ = std::fs::remove_file(&output);
                format!("第 {} 集：拼接失败：{error}", index + 1)
            })?;
        }

        items.push(TranscodeItemReport {
            source: spans
                .iter()
                .map(|span| span.file_name.as_str())
                .collect::<Vec<_>>()
                .join(" + "),
            output: output.display().to_string(),
            detail: format!(
                "{:.1}s – {:.1}s（{} 个源片段）",
                segment.0,
                segment.1,
                spans.len()
            ),
        });
    }

    let produced = items.len();
    Ok(TranscodeRunReport {
        mode: "recut".into(),
        output_dir: output_dir.display().to_string(),
        items,
        note: format!(
            "已按场景切点重新分成 {produced} 集（统一重编码保证切点精确），输出到新目录，原素材未改动。"
        ),
    })
}

fn transcode_blocking(
    app: &AppHandle,
    root: String,
    name: String,
    request: TranscodeRequest,
    ffprobe_path: Option<String>,
    output_root: Option<String>,
) -> Result<TranscodeRunReport, String> {
    let ffmpeg = resolve_ffmpeg(app)?;
    let ffprobe = resolve_ffprobe(app, ffprobe_path.as_deref())?;
    let drama = load_drama(&root, &name)?;
    let episodes = probe_all(&drama, Some(ffprobe.display().to_string()));
    if episodes.is_empty() {
        return Err("没有任何成功探测规格的分集；请先确认素材可读且 ffprobe 可用。".into());
    }

    let mode = request.mode.as_str();
    if !matches!(mode, "nudge" | "recut") {
        return Err(format!("未知转码模式：{mode}（支持 nudge / recut）"));
    }
    let subdir = match mode {
        "recut" => "重组输出",
        _ => "微调输出",
    };
    // 与报审包同一套落点规则（`dossier_core::resolve_product_dir`）：
    // 本次覆盖 → 设置里的产物根目录下的 `<剧名>/<子目录>` → 剧目目录内的同名子目录。
    let output_dir = resolve_product_dir(
        request.output_dir.as_deref().map(Path::new),
        output_root.as_deref().map(Path::new),
        &drama.dir,
        &drama.name,
        subdir,
    );
    reject_source_dir(&output_dir, &episodes)?;

    let mut on_step = |done: u64, total: u64, label: &str| {
        emit_task_progress(app, "transcode", done, total, label);
    };
    match mode {
        "recut" => {
            let target = request
                .target_count
                .unwrap_or(episodes.len())
                .clamp(1, episodes.len().max(1) * 10);
            transcode_recut_blocking(&ffmpeg, &episodes, target, &output_dir, &mut on_step)
        }
        _ => transcode_nudge_blocking(&ffmpeg, &drama.name, &episodes, &output_dir, &mut on_step),
    }
}

/// 拒绝把输出写回素材目录。
///
/// `智能微调` 输出的是同名文件、`分集重组` 固定输出 `第NN集.mp4`，两者都带 `-y`；
/// 一旦输出目录就是素材目录，ffmpeg 会直接覆盖原素材。这里在**动手之前**拦住，
/// 并把「哪个文件会被覆盖」如实告诉使用者。
fn reject_source_dir(output_dir: &Path, episodes: &[ProbedEpisode]) -> Result<(), String> {
    let normalized_dir = normalize(output_dir);
    let sources: Vec<PathBuf> = episodes
        .iter()
        .map(|episode| normalize(&episode.file.path))
        .collect();
    match output_dir_hits_sources(&normalized_dir, &sources) {
        Some(hit) => Err(format!(
            "输出目录就是素材目录，继续会覆盖原素材（例如 {}）。请换一个输出目录，留空则默认写到剧目目录下的子目录。",
            hit.display()
        )),
        None => Ok(()),
    }
}

#[tauri::command]
pub async fn transcode_execute(
    app: AppHandle,
    root: String,
    name: String,
    request: TranscodeRequest,
    ffprobe_path: Option<String>,
    output_root: Option<String>,
) -> Result<TranscodeRunReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        transcode_blocking(&app, root, name, request, ffprobe_path, output_root)
    })
    .await
    .map_err(|error| format!("转码任务失败：{error}"))?
}

// ---------------------------------------------------------------------------
// 一键安装运行环境
// ---------------------------------------------------------------------------

/// 一键安装结果（前端只用 `note`，其余字段供排查与单测）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallRuntimeReport {
    pub ffmpeg_path: String,
    pub ffprobe_path: String,
    pub skipped_existing: Vec<String>,
    /// 实际用到的下载来源（来源名 + 地址），每个下载过的工具一条；全部跳过时为空。
    ///
    /// 新增字段：老前端忽略未知字段；`serde(default)` 让反序列化侧（测试/未来回读）也能缺省。
    #[serde(default)]
    pub sources_used: Vec<String>,
    pub note: String,
}

// 下载源都是**第三方站点**（evermeet.cx / gyan.dev / BtbN），包不是 FFmpeg 官方发布物。
// 钉死版本 + 钉死 SHA-256 只能保证「下到的就是你核对过的那一版」，**不能证明来源可信**；
// 多来源互相印证能降低风险，但不等于可信。
//
// 「选来源 → 下载 → 校验 → 换下一个」刻意做成与平台无关的纯逻辑（下面的 `installer`），
// 平台差异只剩两处：用哪条命令下载/解压、可执行文件叫什么名字。这样在开发机（Linux）上
// 就能用假的 `Downloader` 把「连不上」「哈希不符」「全部失败」这些分支跑进单测，
// 不必真的连 Windows / macOS 网络。
#[cfg(any(target_os = "macos", target_os = "windows", test))]
mod installer {
    use std::path::Path;
    // 只有 Windows 分支（以及 Linux 单测）用到 `PathBuf`（找解压出来的 exe）；
    // macOS 分支不用，不门控会成为未使用导入，被 CI 的 -D warnings 拦下。
    #[cfg(any(target_os = "windows", test))]
    use std::path::PathBuf;

    // 平台相关部分才需要的导入：Linux 非测试构建整块都不编译，Linux 测试构建则会编译
    // 本模块，所以这些导入必须自己按平台门控，否则要么缺名字、要么成未使用项。
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    use super::{app_bin_dir, exe_name, InstallRuntimeReport};

    // -----------------------------------------------------------------------
    // 维护者维护区：固定版本 + 固定校验值
    // -----------------------------------------------------------------------

    /// macOS 静态 ffmpeg 发行源：evermeet.cx（第三方构建）。版本钉死 9.0.2 保证可复现；
    /// 该站单连接限流（HTTP 429 / 带宽限速），因此顺序下载并交给 curl 重试。
    /// 注意：该站只提供 Intel（x86_64）构建（官方明确不提供 ARM 原生版），
    /// 在 Apple Silicon 上经 Rosetta 2 运行；Rosetta 缺失时自检会拦截并提示安装。
    #[cfg(target_os = "macos")]
    const EVERMEET_FFMPEG_URL: &str = "https://evermeet.cx/ffmpeg/ffmpeg-9.0.2.zip";
    #[cfg(target_os = "macos")]
    const EVERMEET_FFPROBE_URL: &str = "https://evermeet.cx/ffmpeg/ffprobe-9.0.2.zip";

    /// `ffmpeg-9.0.2.zip` 的 SHA-256（小写十六进制）。
    ///
    /// **占位值，必须由维护者实际下载一次后填入。** 在 macOS 上取法：
    /// `curl -fL -o f.zip https://evermeet.cx/ffmpeg/ffmpeg-9.0.2.zip && shasum -a 256 f.zip`，
    /// 把输出的 64 位十六进制替换掉下面的 `unverified`。**不要猜、不要从别处抄**：
    /// 写错会让所有用户装不上，只能由真正下载过的人核对。
    /// 未填时一键安装会明确报「哈希未配置」并给出手动步骤，**绝不会跳过校验直接装**。
    #[cfg(target_os = "macos")]
    const EVERMEET_FFMPEG_SHA256: &str =
        "4acc0be580f9b2788029eb7bd4d645ff87968911b0a62aeeb3940d42d54558d5";

    /// `ffprobe-9.0.2.zip` 的 SHA-256。占位值，填法见上一条注释。
    #[cfg(target_os = "macos")]
    const EVERMEET_FFPROBE_SHA256: &str =
        "24a9c968cd4da72d99c7245e914b921815835eb6dff01d99868031aebaf1d439";

    /// Windows 来源 1：gyan.dev 的**按版本归档**地址。浮动地址 `ffmpeg-release-*`
    /// 会随新版本滚动，钉不了哈希，所以不用它。
    /// 校验值来源：同目录下的 `ffmpeg-9.0.2-essentials_build.zip.sha256`，已实际核对。
    #[cfg(any(target_os = "windows", test))]
    pub const GYAN_FFMPEG_URL: &str =
        "https://www.gyan.dev/ffmpeg/builds/packages/ffmpeg-9.0.2-essentials_build.zip";
    #[cfg(any(target_os = "windows", test))]
    pub const GYAN_FFMPEG_SHA256: &str =
        "60f467265b1e312373dbcd92200c2618a74850f98d3d078e94296bb3fa2047ba";

    /// Windows 来源 2：BtbN 的 GitHub Releases（autobuild）。
    ///
    /// **占位，必须由维护者固定。** BtbN 的 `latest` 是滚动标签，指向的包与其哈希每天都变，
    /// 钉 `latest` 等于没钉。做法：挑一个具体的 autobuild 标签（形如
    /// `autobuild-2026-01-01-12-00`），下载其中的
    /// `ffmpeg-n9.0-*-win64-gpl-9.0.zip`（`-gpl-` 还是 `-gpl-shared-` 二选一并写清），
    /// 算出 SHA-256，再把地址与哈希分别填进下面两个常量。
    /// 开发期没有 Windows 也无法下载，所以这里只能是占位值；未填时该来源报「哈希未配置」。
    #[cfg(any(target_os = "windows", test))]
    pub const BTBN_FFMPEG_URL: &str =
        "unfilled: 先固定到某个具体 autobuild 标签与文件名，再把下载地址填在这里";
    #[cfg(any(target_os = "windows", test))]
    pub const BTBN_FFMPEG_SHA256: &str = "unverified";

    /// Windows 的下载来源列表：按顺序尝试，每项都是「固定版本 + 固定哈希」。
    #[cfg(any(target_os = "windows", test))]
    pub const WINDOWS_SOURCES: &[InstallSource] = &[
        InstallSource {
            label: "gyan.dev（按版本归档）",
            url: GYAN_FFMPEG_URL,
            sha256: GYAN_FFMPEG_SHA256,
        },
        InstallSource {
            label: "BtbN GitHub Releases（autobuild）",
            url: BTBN_FFMPEG_URL,
            sha256: BTBN_FFMPEG_SHA256,
        },
    ];

    // -----------------------------------------------------------------------
    // 与平台无关：来源、下载器、切换规则
    // -----------------------------------------------------------------------

    /// 一个下载来源：固定版本 + 固定哈希，不满足就换下一项。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct InstallSource {
        /// 给人看的来源名（写进日志、错误信息与安装报告）。
        pub label: &'static str,
        /// 钉死版本的压缩包地址。
        pub url: &'static str,
        /// 压缩包的 SHA-256（小写十六进制）；不是 64 位十六进制即视为「未配置」。
        pub sha256: &'static str,
    }

    /// 「把一个地址下载到本地文件」的能力。
    ///
    /// 生产实现是系统自带的 curl；测试实现用来模拟「连不上 / 内容不对 / 成功」三种情况，
    /// 因此切换逻辑在没有网络、也没有 Windows/macOS 的机器上也能被完整验证。
    pub trait Downloader {
        fn download(&self, url: &str, target: &Path) -> Result<(), String>;
    }

    /// 某个来源失败的原因分类。**哈希不符比网络问题严重**（文件被换过或不完整），
    /// 日志与错误信息里必须能区分开。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum SourceFailureKind {
        /// 连不上 / 超时 / HTTP 非 2xx。
        Network,
        /// 下载成功但 SHA-256 对不上（下载的文件已删除）。
        Checksum,
        /// 代码里的哈希还是占位值：配置错误，既不联网，也不该被「换下一个」掩盖。
        HashUnconfigured,
        /// 下载后本地读写失败（磁盘满、被占用等）。
        LocalIo,
    }

    impl SourceFailureKind {
        pub fn label(self) -> &'static str {
            match self {
                Self::Network => "网络或 HTTP 失败",
                Self::Checksum => "哈希不符",
                Self::HashUnconfigured => "哈希未配置",
                Self::LocalIo => "本地读写失败",
            }
        }
    }

    /// 一个来源的失败记录。
    #[derive(Debug, Clone)]
    pub struct SourceFailure {
        pub label: &'static str,
        pub url: &'static str,
        pub kind: SourceFailureKind,
        pub reason: String,
    }

    /// 一次成功的来源选择。
    #[derive(Debug, Clone)]
    pub struct InstalledSource {
        pub source: InstallSource,
        /// 成功之前失败过的来源（按尝试顺序）——「换过哪几家、为什么换」不能丢。
        pub failures: Vec<SourceFailure>,
    }

    impl InstalledSource {
        /// 给安装报告用的一句话（界面会直接显示，所以把「换过来源」也写进去）。
        pub fn summary(&self) -> String {
            if self.failures.is_empty() {
                return format!("{}（{}）", self.source.label, self.source.url);
            }
            let earlier = self
                .failures
                .iter()
                .map(|failure| format!("{}{}", failure.label, failure.kind.label()))
                .collect::<Vec<_>>()
                .join("、");
            format!(
                "{}（{}；此前失败：{earlier}）",
                self.source.label, self.source.url
            )
        }
    }

    /// 按顺序尝试每个来源：下载 → 算 SHA-256 → 与钉住值比对，失败就换下一个。
    ///
    /// 规则（全部写进日志与错误信息，不静默）：
    /// - 连不上 / 超时 / HTTP 非 2xx → 记下原因，用下一个来源；
    /// - 下载成功但**哈希不符** → 删掉下载的文件，记下「哈希不符」，用下一个来源；
    /// - 哈希是占位值 → **立即停**并报「哈希未配置」：这是配置错误，既不能跳过校验
    ///   直接装，也不该被后面某个来源的成功悄悄掩盖；
    /// - 全部失败 → 错误信息逐条列出每个来源的原因，并附 `manual_steps`（手动安装步骤）。
    pub fn try_sources(
        downloader: &dyn Downloader,
        sources: &[InstallSource],
        target: &Path,
        manual_steps: &str,
    ) -> Result<InstalledSource, String> {
        let mut failures: Vec<SourceFailure> = Vec::new();
        for source in sources {
            let expected = source.sha256.trim();
            if !dossier_runtime::is_sha256_hex(expected) {
                failures.push(SourceFailure {
                    label: source.label,
                    url: source.url,
                    kind: SourceFailureKind::HashUnconfigured,
                    reason: format!(
                        "校验值仍是占位值「{expected}」，必须由维护者实际下载一次后填入"
                    ),
                });
                // 不下载、不安装：占位哈希意味着没人核对过这个包。
                return Err(config_error(&failures, manual_steps));
            }
            if let Err(reason) = downloader.download(source.url, target) {
                // 半截文件不能留给下一个来源，也不能进后续解压流程。
                let _ = std::fs::remove_file(target);
                eprintln!("运行环境下载来源「{}」不可用：{reason}", source.label);
                failures.push(SourceFailure {
                    label: source.label,
                    url: source.url,
                    kind: SourceFailureKind::Network,
                    reason,
                });
                continue;
            }
            let actual = match dossier_runtime::sha256_file(target) {
                Ok(actual) => actual,
                Err(error) => {
                    let _ = std::fs::remove_file(target);
                    failures.push(SourceFailure {
                        label: source.label,
                        url: source.url,
                        kind: SourceFailureKind::LocalIo,
                        reason: format!("下载后读取文件失败：{error}"),
                    });
                    continue;
                }
            };
            if !actual.eq_ignore_ascii_case(expected) {
                let _ = std::fs::remove_file(target);
                eprintln!(
                    "运行环境下载来源「{}」校验失败：SHA-256 不符（期望 {expected}，实际 {actual}），已删除下载的文件，改用下一个来源",
                    source.label
                );
                failures.push(SourceFailure {
                    label: source.label,
                    url: source.url,
                    kind: SourceFailureKind::Checksum,
                    reason: format!("SHA-256 不符（期望 {expected}，实际 {actual}）"),
                });
                continue;
            }
            return Ok(InstalledSource {
                source: *source,
                failures,
            });
        }
        Err(all_failed_error(sources.len(), &failures, manual_steps))
    }

    /// 逐条渲染失败原因（每个来源一行，带序号）。
    fn describe_failures(failures: &[SourceFailure]) -> String {
        failures
            .iter()
            .enumerate()
            .map(|(index, failure)| {
                format!(
                    "\n  {}. {}（{}）—— {}：{}",
                    index + 1,
                    failure.label,
                    failure.url,
                    failure.kind.label(),
                    failure.reason
                )
            })
            .collect()
    }

    /// 全部来源都失败：必须能看出「试了哪几家、各自为什么失败」。
    fn all_failed_error(count: usize, failures: &[SourceFailure], manual_steps: &str) -> String {
        let labels = failures
            .iter()
            .map(|failure| failure.label)
            .collect::<Vec<_>>()
            .join("、");
        format!(
            "已尝试 {count} 个下载来源都失败（依次为：{labels}）：{}{manual_steps}",
            describe_failures(failures)
        )
    }

    /// 哈希未配置：配置错误，安装必须停下（不下载、不安装、不写设置）。
    fn config_error(failures: &[SourceFailure], manual_steps: &str) -> String {
        format!(
            "哈希未配置：一键安装已停止，没有下载也没有安装（不会跳过校验直接装）。{} 个来源的情况：{}{manual_steps}",
            failures.len(),
            describe_failures(failures)
        )
    }

    /// 在解压目录里找可执行文件：优先 `bin/` 下的那个
    /// （gyan / BtbN 的包都是 `<根>/<版本目录>/bin/ffmpeg.exe`）。
    ///
    /// 只用文件系统、与平台无关，所以能在 Linux 上单测；真正执行解压的命令才是平台相关部分。
    #[cfg(any(target_os = "windows", test))]
    pub fn find_in_extracted(root: &Path, file_name: &str) -> Result<PathBuf, String> {
        let mut stack = vec![root.to_path_buf()];
        let mut fallback: Option<PathBuf> = None;
        while let Some(dir) = stack.pop() {
            let entries = std::fs::read_dir(&dir)
                .map_err(|error| format!("读取解压目录 {} 失败：{error}", dir.display()))?;
            for entry in entries {
                let entry = entry.map_err(|error| format!("读取解压目录项失败：{error}"))?;
                let path = entry.path();
                if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                    stack.push(path);
                    continue;
                }
                if path.file_name().and_then(|name| name.to_str()) != Some(file_name) {
                    continue;
                }
                let in_bin = path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
                    == Some("bin");
                if in_bin {
                    return Ok(path);
                }
                fallback.get_or_insert(path);
            }
        }
        match fallback {
            Some(path) => Ok(path),
            None => Err(format!("压缩包里没有找到可执行文件 {file_name}")),
        }
    }

    // -----------------------------------------------------------------------
    // 平台相关：下载器与解压命令
    // -----------------------------------------------------------------------

    /// 系统自带工具缺失时说的话。Windows 10 1803 之前不带 `curl.exe` / `tar.exe`，
    /// 这种系统上不能装作在装，必须明确让用户手动装。
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn missing_tool_error(tool: &str) -> String {
        if cfg!(target_os = "windows") {
            format!(
                "系统版本太旧：找不到系统自带的 {tool}（Windows 10 1803 之前的系统不带它）。请手动安装 ffmpeg（含 ffprobe），步骤见本提示末尾。"
            )
        } else {
            format!(
                "找不到系统自带的 {tool}：请确认系统完整（macOS 自带 /usr/bin/curl 与 /usr/bin/unzip），或改用手动安装。"
            )
        }
    }

    /// 用系统自带的 curl 下载：macOS 是 `/usr/bin/curl`，Windows 是 `curl.exe`。
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    struct SystemCurl {
        program: &'static str,
        retries: u32,
        retry_delay_secs: u32,
        /// `--retry-all-errors` 需要 curl 7.71+。Windows 10 1803/1903/1909 自带的
        /// curl 太老，加了这个参数会直接报「option not supported」，因此只在 macOS 开启。
        retry_all_errors: bool,
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    impl Downloader for SystemCurl {
        fn download(&self, url: &str, target: &Path) -> Result<(), String> {
            let mut spec = dossier_runtime::CommandSpec::new(self.program)
                .arg("-fL")
                .arg("-sS")
                .arg("--connect-timeout")
                .arg("30")
                .arg("--retry")
                .arg(self.retries.to_string())
                .arg("--retry-delay")
                .arg(self.retry_delay_secs.to_string())
                .arg("--max-time")
                .arg("1800");
            if self.retry_all_errors {
                spec = spec.arg("--retry-all-errors");
            }
            let spec = spec.arg("-o").arg(target.display().to_string()).arg(url);
            let runner = dossier_runtime::SystemRunner::new();
            let output = runner.run(&spec).map_err(|error| match error {
                dossier_runtime::CommandError::NotFound { .. } => missing_tool_error(self.program),
                other => format!("{} 启动失败：{other}", self.program),
            })?;
            if output.success() {
                return Ok(());
            }
            Err(format!(
                "下载失败（{}）：{}",
                output.code_label(),
                first_line(&output.combined())
            ))
        }
    }

    /// 取输出的第一行非空内容：错误信息只要一句，别把整段进度糊给用户。
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn first_line(detail: &str) -> &str {
        detail
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("（无输出）")
    }

    /// 用 `tar.exe` 解压 zip（Windows 10 1803+ 的 bsdtar 认 zip）。
    #[cfg(target_os = "windows")]
    fn extract_zip(zip: &Path, dest: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dest).map_err(|error| format!("创建解压目录失败：{error}"))?;
        let spec = dossier_runtime::CommandSpec::new("tar.exe")
            .arg("-xf")
            .arg(zip.display().to_string())
            .arg("-C")
            .arg(dest.display().to_string());
        let runner = dossier_runtime::SystemRunner::new();
        let output = runner.run(&spec).map_err(|error| match error {
            dossier_runtime::CommandError::NotFound { .. } => missing_tool_error("tar.exe"),
            other => format!("tar.exe 启动失败：{other}"),
        })?;
        if output.success() {
            return Ok(());
        }
        Err(format!(
            "解压失败（{}）：{}",
            output.code_label(),
            first_line(&output.combined())
        ))
    }

    /// 用 macOS 自带的 `unzip` 解压。
    #[cfg(target_os = "macos")]
    fn unzip_file(zip: &Path, dest: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dest).map_err(|error| format!("创建解压目录失败：{error}"))?;
        let spec = dossier_runtime::CommandSpec::new("/usr/bin/unzip")
            .arg("-o")
            .arg("-q")
            .arg(zip.display().to_string())
            .arg("-d")
            .arg(dest.display().to_string());
        let runner = dossier_runtime::SystemRunner::new();
        let output = runner
            .run(&spec)
            .map_err(|error| format!("unzip 启动失败：{error}"))?;
        if output.success() {
            return Ok(());
        }
        Err(format!(
            "解压失败（{}）：{}",
            output.code_label(),
            first_line(&output.combined())
        ))
    }

    /// 手动安装步骤：一键安装全失败、哈希未配置或系统太旧时跟着错误一起给出。
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn manual_steps(bin_dir: &Path) -> String {
        #[cfg(target_os = "windows")]
        let source_hint = "① 打开 https://www.gyan.dev/ffmpeg/builds/ 下载 essentials_build 的 zip（或执行「winget install Gyan.FFmpeg」）；";
        #[cfg(target_os = "macos")]
        let source_hint =
            "① 在终端执行「brew install ffmpeg」（或从 evermeet.cx 下载 zip 后解压）；";
        format!(
            " 手动安装步骤：{source_hint}② 把压缩包里 bin 目录下的 ffmpeg{} 与 ffprobe{} 复制到 {}；③ 回到「设置 → 本机环境」点「重新检测」，也可以直接在那里指定 ffprobe 路径。",
            if cfg!(target_os = "windows") { ".exe" } else { "" },
            if cfg!(target_os = "windows") { ".exe" } else { "" },
            bin_dir.display()
        )
    }

    // -----------------------------------------------------------------------
    // 平台相关：安装流程
    // -----------------------------------------------------------------------

    /// 安装后收尾：自检（真跑 `-version`）+ 写回设置 + 生成报告。
    ///
    /// 自检放在写设置之前：跑不起来的二进制不能被写进设置，否则界面会显示「就绪」。
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn finalize_install(
        app: &tauri::AppHandle,
        bin_dir: &Path,
        skipped_existing: Vec<String>,
        sources_used: Vec<String>,
    ) -> Result<InstallRuntimeReport, String> {
        let ffmpeg_path = bin_dir.join(exe_name("ffmpeg"));
        let ffprobe_path = bin_dir.join(exe_name("ffprobe"));

        let runner = dossier_runtime::SystemRunner::new();
        for (name, path) in [("ffmpeg", &ffmpeg_path), ("ffprobe", &ffprobe_path)] {
            let report = dossier_runtime::probe::probe_tool(
                &runner,
                name,
                path,
                &["-hide_banner", "-version"],
            );
            if report.ok {
                continue;
            }
            // 只可能在 macOS 上出现（evermeet.cx 只有 Intel 构建）。
            #[cfg(target_os = "macos")]
            if report.detail.contains("Bad CPU type") {
                return Err("安装完成但自检失败：CPU 架构不匹配（下载的是 Intel 版，需要 Rosetta 2）。请先在终端执行「softwareupdate --install-rosetta --agree-to-license」安装 Rosetta，再重新点击一键配置。".into());
            }
            return Err(format!("安装完成但 {name} 自检失败：{}", report.detail));
        }

        // 写回设置：只在用户没有显式配置（或配置失效）时写入，避免覆盖手工配置。
        let settings_path = crate::settings_path(app)?;
        let mut settings = dossier_core::load_settings(&settings_path).settings;
        let needs_ffmpeg = settings
            .ffmpeg_path
            .as_ref()
            .map(|path| !path.is_file())
            .unwrap_or(true);
        let needs_ffprobe = settings
            .ffprobe_path
            .as_ref()
            .map(|path| !path.is_file())
            .unwrap_or(true);
        if needs_ffmpeg {
            settings.ffmpeg_path = Some(ffmpeg_path.clone());
        }
        if needs_ffprobe {
            settings.ffprobe_path = Some(ffprobe_path.clone());
        }
        if needs_ffmpeg || needs_ffprobe {
            // 全限定调用而不是顶部导入：这一行只在 macOS/Windows 分支编译，
            // 导入在 Linux 上会成为未使用项，被 CI 的 -D warnings 拦下。
            dossier_core::save_settings(&settings_path, &settings)
                .map_err(|error| format!("写入设置失败：{error}"))?;
        }

        let note = if sources_used.is_empty() {
            format!("运行环境已就绪（安装目录：{}）", bin_dir.display())
        } else {
            format!(
                "运行环境已就绪（安装目录：{}；下载来源：{}）",
                bin_dir.display(),
                sources_used.join("；")
            )
        };

        Ok(InstallRuntimeReport {
            ffmpeg_path: ffmpeg_path.display().to_string(),
            ffprobe_path: ffprobe_path.display().to_string(),
            skipped_existing,
            sources_used,
            note,
        })
    }

    /// 把一个解压出来的文件移到安装目录。
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn move_into_place(extracted: &Path, target: &Path, name: &str) -> Result<(), String> {
        // 目标可能在另一个盘上（或刚被杀软占用），rename 失败就退回复制。
        std::fs::rename(extracted, target).or_else(|error| {
            std::fs::copy(extracted, target)
                .map(|_| ())
                .map_err(|copy_error| {
                    format!("安装 {name} 失败：{error}；复制也失败：{copy_error}")
                })
        })
    }

    /// Windows：多来源下载 → 校验 → `tar.exe` 解压 → 取 `bin\*.exe` → 移到安装目录 → 自检。
    #[cfg(target_os = "windows")]
    pub fn install_runtime_blocking(
        app: &tauri::AppHandle,
    ) -> Result<InstallRuntimeReport, String> {
        let bin_dir = app_bin_dir(app)?;
        std::fs::create_dir_all(&bin_dir)
            .map_err(|error| format!("创建运行环境目录失败：{error}"))?;

        let mut skipped_existing = Vec::new();
        let mut missing = Vec::new();
        for base in ["ffmpeg", "ffprobe"] {
            if bin_dir.join(exe_name(base)).is_file() {
                skipped_existing.push(base.to_string());
            } else {
                missing.push(base);
            }
        }

        let mut sources_used = Vec::new();
        if !missing.is_empty() {
            // gyan.dev / BtbN 的 essentials 包里 ffmpeg 与 ffprobe 都有，一个包够两个工具用。
            let zip_path = bin_dir.join("ffmpeg.download.zip");
            let staging = bin_dir.join("ffmpeg.staging");
            let _ = std::fs::remove_dir_all(&staging);
            let _ = std::fs::remove_file(&zip_path);

            let downloader = SystemCurl {
                program: "curl.exe",
                // 多来源之间要换得快，单个来源少试几轮。
                retries: 1,
                retry_delay_secs: 3,
                retry_all_errors: false,
            };
            let installed = try_sources(
                &downloader,
                WINDOWS_SOURCES,
                &zip_path,
                &manual_steps(&bin_dir),
            )?;
            sources_used.push(installed.summary());

            let result = extract_zip(&zip_path, &staging).and_then(|()| {
                for base in &missing {
                    let file_name = exe_name(base);
                    let extracted = find_in_extracted(&staging, &file_name)?;
                    move_into_place(&extracted, &bin_dir.join(&file_name), &file_name)?;
                }
                Ok(())
            });
            let _ = std::fs::remove_dir_all(&staging);
            let _ = std::fs::remove_file(&zip_path);
            result?;
        }

        finalize_install(app, &bin_dir, skipped_existing, sources_used)
    }

    /// macOS：evermeet.cx 单一来源 + 哈希校验（哈希未配置则明确报错）。
    /// 返回 `None` 表示目标已存在、本次跳过。
    #[cfg(target_os = "macos")]
    fn install_one(
        bin_dir: &Path,
        name: &str,
        sources: &[InstallSource],
    ) -> Result<Option<InstalledSource>, String> {
        let target = bin_dir.join(name);
        if target.is_file() {
            return Ok(None);
        }
        let zip_path = bin_dir.join(format!("{name}.download.zip"));
        let staging = bin_dir.join(format!("{name}.staging"));
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_file(&zip_path);

        let downloader = SystemCurl {
            program: "/usr/bin/curl",
            // evermeet.cx 单连接限流（429 / 限速），单一来源需要多试几轮。
            retries: 4,
            retry_delay_secs: 10,
            retry_all_errors: true,
        };
        let result = try_sources(&downloader, sources, &zip_path, &manual_steps(bin_dir)).and_then(
            |installed| {
                unzip_file(&zip_path, &staging)?;
                let extracted = staging.join(name);
                if !extracted.is_file() {
                    return Err(format!(
                        "压缩包里没有找到 {name} 可执行文件（来源：{}）",
                        installed.source.label
                    ));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&extracted, std::fs::Permissions::from_mode(0o755))
                        .map_err(|error| format!("设置可执行权限失败：{error}"))?;
                }
                move_into_place(&extracted, &target, name)?;
                Ok(installed)
            },
        );
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_file(&zip_path);
        result.map(Some)
    }

    /// macOS 入口：ffmpeg 与 ffprobe 各下一个包，两个都先过哈希校验。
    #[cfg(target_os = "macos")]
    pub fn install_runtime_blocking(
        app: &tauri::AppHandle,
    ) -> Result<InstallRuntimeReport, String> {
        let bin_dir = app_bin_dir(app)?;
        std::fs::create_dir_all(&bin_dir)
            .map_err(|error| format!("创建运行环境目录失败：{error}"))?;

        let mut skipped_existing = Vec::new();
        let mut sources_used = Vec::new();
        for (name, url, sha256) in [
            ("ffmpeg", EVERMEET_FFMPEG_URL, EVERMEET_FFMPEG_SHA256),
            ("ffprobe", EVERMEET_FFPROBE_URL, EVERMEET_FFPROBE_SHA256),
        ] {
            let sources = [InstallSource {
                label: "evermeet.cx",
                url,
                sha256,
            }];
            match install_one(&bin_dir, name, &sources)? {
                Some(installed) => sources_used.push(installed.summary()),
                None => skipped_existing.push(name.to_string()),
            }
        }

        finalize_install(app, &bin_dir, skipped_existing, sources_used)
    }
}

/// Linux 上没有可靠的一键安装源（发行版包管理器才是正路），明确让用户手动装。
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn install_runtime_blocking(_app: &AppHandle) -> Result<InstallRuntimeReport, String> {
    Err("运行环境自动安装只支持 macOS 与 Windows。Linux 请用系统包管理器安装：Debian/Ubuntu 执行「sudo apt install ffmpeg」，Fedora/RHEL 执行「sudo dnf install ffmpeg」，装好后回到「设置 → 本机环境」点「重新检测」。".to_string())
}

#[tauri::command]
pub async fn install_runtime(app: AppHandle) -> Result<InstallRuntimeReport, String> {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        tauri::async_runtime::spawn_blocking(move || installer::install_runtime_blocking(&app))
            .await
            .map_err(|error| format!("运行环境安装任务失败：{error}"))?
    }
    // Linux 直接返回「用包管理器装」，不必开线程。
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        install_runtime_blocking(&app)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_file_name_rejects_path_components() {
        assert!(plain_file_name("第01集.mp4").is_ok());
        assert!(plain_file_name("剧名 - 第02集.mp4").is_ok());
        assert!(plain_file_name("").is_err());
        assert!(plain_file_name("a/b.mp4").is_err());
        assert!(plain_file_name("../越权.mp4").is_err());
        assert!(plain_file_name("/绝对路径.mp4").is_err());
    }

    fn probe_at(path: &str, duration: f64) -> ProbedEpisode {
        ProbedEpisode {
            file: EpisodeFile {
                path: path.into(),
                file_name: path.rsplit('/').next().unwrap_or(path).to_string(),
                size_bytes: 1,
                episode_number: Some(1),
            },
            spec: dossier_core::EpisodeSpec {
                duration_sec: duration,
                width: 1080,
                height: 1920,
                video_codec: Some("h264".into()),
                audio_codec: Some("aac".into()),
                fps: Some(30.0),
                bitrate_bps: Some(4_000_000),
            },
        }
    }

    #[test]
    fn spans_map_global_segment_back_to_local_files() {
        // 两集各 60 秒，全局 [50, 130] 跨两集。
        let episodes = vec![
            probe_at("/a/第01集.mp4", 60.0),
            probe_at("/a/第02集.mp4", 60.0),
        ];
        let spans = spans_for_segment(&episodes, &[0.0, 60.0], (50.0, 130.0));
        assert_eq!(spans.len(), 2);
        assert!((spans[0].local_start - 50.0).abs() < 1e-9);
        assert!((spans[0].duration - 10.0).abs() < 1e-9);
        assert!((spans[1].local_start - 0.0).abs() < 1e-9);
        assert!((spans[1].duration - 60.0).abs() < 1e-9);

        // 完全落在第二集内部：只有一个 span。
        let spans = spans_for_segment(&episodes, &[0.0, 60.0], (70.0, 80.0));
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].file_name, "第02集.mp4");
        assert!((spans[0].local_start - 10.0).abs() < 1e-9);
    }

    #[test]
    fn seed_is_stable_per_file_name() {
        // seed_for 混入了当前时间，只验证输出落在 1~3 帧的下游约束由 nudge_frames 保证；
        // 这里验证同一进程内两次调用不会 panic 且互不相同（时间项不同）。
        let _ = seed_for("第01集.mp4");
        let _ = seed_for("第01集.mp4");
    }

    #[test]
    fn ffmpeg_failure_surfaces_first_stderr_line() {
        // 用不存在的程序名触发启动失败路径，确认错误信息里带“ffmpeg 启动失败”。
        let mut spec = CommandSpec::new("ffmpeg-不存在的程序");
        spec = spec.arg("-version");
        let runner = SystemRunner::new();
        assert!(runner.run(&spec).is_err());
    }

    // -----------------------------------------------------------------------
    // 一键安装：多来源切换与哈希校验（与平台无关，Linux 上即可跑）
    // -----------------------------------------------------------------------

    use super::installer::{
        find_in_extracted, try_sources, Downloader, InstallSource, SourceFailureKind,
        WINDOWS_SOURCES,
    };

    /// 假下载内容与它的 SHA-256（用真实哈希，校验逻辑才走同一条路）。
    const PAYLOAD_GOOD: &[u8] = b"fake-ffmpeg-package-A";
    const HASH_GOOD: &str = "f19bab7e62f7857a9a5d63466d36088a38e504d885b47d8c483b6f4c85d9f22c";
    /// 另一个包的内容与哈希：用它冒充「下到了别的文件」。
    const PAYLOAD_OTHER: &[u8] = b"fake-ffmpeg-package-B";
    const HASH_OTHER: &str = "8914e5ca5ffff8ab0f8f3f58c1787a7cd48e4f70aa254f5c160abb8f7093f52e";

    /// 假下载器：按 URL 决定「连不上」还是「写入什么字节」，并记录被调用过哪些地址。
    struct FakeDownloader {
        plan: Vec<(&'static str, Result<&'static [u8], &'static str>)>,
        calls: std::cell::RefCell<Vec<String>>,
    }

    impl FakeDownloader {
        fn new(plan: Vec<(&'static str, Result<&'static [u8], &'static str>)>) -> Self {
            Self {
                plan,
                calls: std::cell::RefCell::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl Downloader for FakeDownloader {
        fn download(&self, url: &str, target: &Path) -> Result<(), String> {
            self.calls.borrow_mut().push(url.to_string());
            // 连不上时不写文件：真实的 curl 也一样（-f 失败不落盘或只留半截）。
            let payload = self
                .plan
                .iter()
                .find(|(planned, _)| *planned == url)
                .map(|(_, outcome)| *outcome)
                .unwrap_or(Err("测试没有为这个地址准备结果"));
            match payload {
                Ok(bytes) => std::fs::write(target, bytes)
                    .map_err(|error| format!("写入测试文件失败：{error}")),
                Err(reason) => Err(reason.to_string()),
            }
        }
    }

    /// 一次性临时目录（测完删掉）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or_default();
            let dir = std::env::temp_dir().join(format!("dossier-install-{tag}-{nanos:x}"));
            std::fs::create_dir_all(&dir).expect("建临时目录");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn source(label: &'static str, url: &'static str, sha256: &'static str) -> InstallSource {
        InstallSource { label, url, sha256 }
    }

    const MANUAL: &str = " 手动安装步骤：解压后把 bin 下的 ffmpeg 放进安装目录。";

    #[test]
    fn install_falls_back_to_second_source_when_first_is_unreachable() {
        let dir = TempDir::new("fallback-network");
        let target = dir.path().join("ffmpeg.zip");
        let downloader = FakeDownloader::new(vec![
            ("https://one.example/ffmpeg.zip", Err("连接超时")),
            ("https://two.example/ffmpeg.zip", Ok(PAYLOAD_GOOD)),
        ]);
        let sources = [
            source("来源一", "https://one.example/ffmpeg.zip", HASH_GOOD),
            source("来源二", "https://two.example/ffmpeg.zip", HASH_GOOD),
        ];

        let installed =
            try_sources(&downloader, &sources, &target, MANUAL).expect("第二个来源应当成功");
        assert_eq!(installed.source.label, "来源二");
        assert_eq!(installed.failures.len(), 1);
        assert_eq!(installed.failures[0].label, "来源一");
        assert_eq!(installed.failures[0].kind, SourceFailureKind::Network);
        assert!(installed.failures[0].reason.contains("连接超时"));
        // 先试一后试二，顺序不能乱。
        assert_eq!(
            downloader.calls(),
            vec![
                "https://one.example/ffmpeg.zip".to_string(),
                "https://two.example/ffmpeg.zip".to_string()
            ]
        );
        assert!(target.is_file(), "成功来源的文件要留下");
    }

    #[test]
    fn install_switches_source_when_checksum_mismatches_and_deletes_file() {
        let dir = TempDir::new("fallback-checksum");
        let target = dir.path().join("ffmpeg.zip");
        // 来源一「下载成功」，但内容是另一个包：声明 HASH_GOOD、实际是 HASH_OTHER 的内容。
        let downloader = FakeDownloader::new(vec![
            ("https://one.example/ffmpeg.zip", Ok(PAYLOAD_OTHER)),
            ("https://two.example/ffmpeg.zip", Ok(PAYLOAD_GOOD)),
        ]);
        let sources = [
            source("来源一", "https://one.example/ffmpeg.zip", HASH_GOOD),
            source("来源二", "https://two.example/ffmpeg.zip", HASH_GOOD),
        ];

        let installed =
            try_sources(&downloader, &sources, &target, MANUAL).expect("第二个来源应当成功");
        assert_eq!(installed.source.label, "来源二");
        assert_eq!(installed.failures.len(), 1);
        // 哈希不符要和网络失败区分开：类别与措辞都必须写明。
        assert_eq!(installed.failures[0].kind, SourceFailureKind::Checksum);
        assert_eq!(installed.failures[0].kind.label(), "哈希不符");
        assert!(installed.failures[0].reason.contains(HASH_GOOD));
        assert!(installed.failures[0].reason.contains(HASH_OTHER));
        let summary = installed.summary();
        assert!(
            summary.contains("来源二"),
            "报告要写明实际用了哪个来源：{summary}"
        );
        assert!(
            summary.contains("哈希不符"),
            "报告要保留换来源的原因：{summary}"
        );
    }

    #[test]
    fn install_error_lists_every_source_failure() {
        let dir = TempDir::new("all-failed");
        let target = dir.path().join("ffmpeg.zip");
        let downloader = FakeDownloader::new(vec![
            ("https://one.example/ffmpeg.zip", Err("HTTP 503")),
            ("https://two.example/ffmpeg.zip", Err("连接被重置")),
        ]);
        let sources = [
            source("来源一", "https://one.example/ffmpeg.zip", HASH_GOOD),
            source("来源二", "https://two.example/ffmpeg.zip", HASH_GOOD),
        ];

        let error =
            try_sources(&downloader, &sources, &target, MANUAL).expect_err("全部失败必须报错");
        assert!(error.contains("已尝试 2 个下载来源"), "{error}");
        for expected in [
            "来源一",
            "来源二",
            "HTTP 503",
            "连接被重置",
            "网络或 HTTP 失败",
            "手动安装步骤",
        ] {
            assert!(
                error.contains(expected),
                "错误信息缺「{expected}」：{error}"
            );
        }
        assert!(!target.is_file(), "全部失败后不该留下下载文件");
    }

    #[test]
    fn install_refuses_to_run_when_hash_is_placeholder() {
        let dir = TempDir::new("hash-unconfigured");
        let target = dir.path().join("ffmpeg.zip");
        // 计划里给了「下载会成功」，但哈希是占位值：必须一步都不走。
        let downloader =
            FakeDownloader::new(vec![("https://one.example/ffmpeg.zip", Ok(PAYLOAD_GOOD))]);
        let sources = [
            source("来源一", "https://one.example/ffmpeg.zip", "unverified"),
            source("来源二", "https://two.example/ffmpeg.zip", HASH_GOOD),
        ];

        let error =
            try_sources(&downloader, &sources, &target, MANUAL).expect_err("占位哈希必须报错");
        assert!(error.contains("哈希未配置"), "{error}");
        assert!(!target.is_file(), "哈希未配置时不能下载/落盘");
        assert!(downloader.calls().is_empty(), "哈希未配置时不该联网");
        // 同一份占位哈希也要被当成非法：不是 64 位十六进制一律算未配置。
        let empty = try_sources(
            &downloader,
            &[source("来源一", "https://one.example/ffmpeg.zip", "")],
            &target,
            MANUAL,
        )
        .expect_err("空哈希同样算未配置");
        assert!(empty.contains("哈希未配置"), "{empty}");
    }

    #[test]
    fn install_reports_which_source_succeeded() {
        let dir = TempDir::new("first-source-ok");
        let target = dir.path().join("ffmpeg.zip");
        let downloader =
            FakeDownloader::new(vec![("https://one.example/ffmpeg.zip", Ok(PAYLOAD_GOOD))]);
        let sources = [source(
            "gyan.dev",
            "https://one.example/ffmpeg.zip",
            HASH_GOOD,
        )];

        let installed = try_sources(&downloader, &sources, &target, MANUAL).expect("应当成功");
        assert_eq!(installed.source.label, "gyan.dev");
        assert!(installed.failures.is_empty());
        // 一次成功不该去试第二个来源。
        assert_eq!(downloader.calls().len(), 1);
        let summary = installed.summary();
        assert!(summary.contains("gyan.dev"));
        assert!(summary.contains("https://one.example/ffmpeg.zip"));
    }

    #[test]
    fn windows_source_list_keeps_gyan_first_and_pins_every_hash() {
        // 来源顺序是承诺：gyan.dev 在前（已知可用），BtbN 在后。
        assert_eq!(WINDOWS_SOURCES.len(), 2);
        assert!(WINDOWS_SOURCES[0].label.contains("gyan.dev"));
        assert!(WINDOWS_SOURCES[0]
            .url
            .starts_with("https://www.gyan.dev/ffmpeg/builds/packages/ffmpeg-9.0.2-"));
        // 已核对的哈希必须保持原值，不许被顺手改掉。
        assert_eq!(
            WINDOWS_SOURCES[0].sha256,
            "60f467265b1e312373dbcd92200c2618a74850f98d3d078e94296bb3fa2047ba"
        );
        // 每个来源要么带合法哈希，要么是明确的占位值：不能出现「看着像哈希但其实不合法」的值，
        // 那会被当成未配置而让所有人装不上，或者更糟——被当成通过。
        for source in WINDOWS_SOURCES {
            let pinned = dossier_runtime::is_sha256_hex(source.sha256);
            let placeholder =
                source.sha256 == "unverified" || source.sha256.starts_with("unfilled:");
            assert!(
                pinned || placeholder,
                "来源「{}」的哈希既不是 64 位十六进制也不是明确占位值：{}",
                source.label,
                source.sha256
            );
            assert!(!source.url.is_empty(), "来源「{}」缺地址", source.label);
            // 地址还是占位、哈希却填了：说明只填了一半，装上会静默跳过这一家。
            if source.url.starts_with("unfilled:") {
                assert!(
                    !pinned,
                    "来源「{}」地址还是占位，哈希却已是真实值，请两处一起补齐",
                    source.label
                );
            }
        }
        // BtbN 当前是占位（维护者需固定到具体 autobuild 标签并填入哈希）：
        // 填好地址与哈希后这里自然变为 false，断言不会成为填坑的阻碍。
        assert!(
            WINDOWS_SOURCES[1].url.starts_with("unfilled:")
                || dossier_runtime::is_sha256_hex(WINDOWS_SOURCES[1].sha256),
            "BtbN 这一项要么两处都是占位，要么地址与哈希都填好"
        );
    }

    #[test]
    fn extracted_binary_is_found_under_bin_directory() {
        // gyan / BtbN 的包结构都是 <根>/<版本目录>/bin/ffmpeg.exe。
        let dir = TempDir::new("find-in-extracted");
        let root = dir.path();
        let nested_bin = root.join("ffmpeg-9.0.2-essentials_build").join("bin");
        std::fs::create_dir_all(&nested_bin).expect("建解压目录");
        std::fs::write(nested_bin.join("ffmpeg.exe"), b"exe").expect("写假可执行文件");
        std::fs::write(nested_bin.join("ffprobe.exe"), b"exe").expect("写假可执行文件");
        // 根目录放一个同名文件：不能选它，`bin/` 下的才是要的那个。
        std::fs::write(root.join("ffmpeg.exe"), b"wrong").expect("写干扰文件");

        let found = find_in_extracted(root, "ffmpeg.exe").expect("应当找到 bin/ffmpeg.exe");
        assert_eq!(
            found.file_name().and_then(|name| name.to_str()),
            Some("ffmpeg.exe")
        );
        assert_eq!(
            found
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str()),
            Some("bin")
        );
        let missing = find_in_extracted(root, "nonexistent.exe").expect_err("没有就该报错");
        assert!(missing.contains("nonexistent.exe"), "{missing}");
    }
}
