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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dossier_core::{
    bitrate_report, load_settings, nudge_frames, output_dir_hits_sources, plan_recut_by_duration,
    plan_recut_segments, plan_rename, probe_episode, resolve_product_dir, BitrateItem, EpisodeFile,
    FfprobeConfig, ProbedEpisode,
};
use dossier_media::shot::{detect_shots, ShotDetectConfig};
use dossier_runtime::caps::{
    plan_encoder, verify_encoder_plan, EncoderVerificationConfig, HardwareSignal,
};
use dossier_runtime::probe::{probe_ffmpeg_capabilities, probe_gpu, probe_tool};
use dossier_runtime::{CommandRunner, CommandSpec, SystemRunner, ToolReport};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::dossier::{load_drama, probe_all};
use crate::paths::normalize;

/// 码率安全线默认值（与竞品一致的 4 Mbps）。
pub const DEFAULT_MIN_BITRATE_KBPS: u64 = 4000;
/// 单条 ffmpeg 命令的超时：短剧分集都在几分钟量级，10 分钟足够。
const FFMPEG_TIMEOUT_SECS: u64 = 600;

/// 转码停止开关：`transcode_stop` 置位后，正在跑的 ffmpeg 会被立刻杀掉，
/// 主流程在每一步之间也会检查，发现置位就收尾清理输出目录。
/// 每次转码开始时复位；没有转码在跑时置位无副作用。
static TRANSCODE_CANCEL: AtomicBool = AtomicBool::new(false);

pub(crate) fn transcode_cancelled() -> bool {
    TRANSCODE_CANCEL.load(Ordering::SeqCst)
}

/// 用户请求停止转码。真正杀进程、清理输出在转码主流程里完成，
/// 这里的返回只代表「停止指令已受理」。
#[tauri::command]
pub fn transcode_stop() -> Result<bool, String> {
    TRANSCODE_CANCEL.store(true, Ordering::SeqCst);
    crate::logging::log_event("转码", "收到停止指令，正在终止并清理输出");
    Ok(true)
}

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

/// 读取当前设置（缺失/损坏回退默认，与 `load_app_settings` 同语义）。
pub(crate) fn read_settings(app: &AppHandle) -> Result<dossier_core::AppSettings, String> {
    let path = crate::settings_path(app)?;
    Ok(load_settings(&path).settings)
}

/// ffmpeg 解析顺序：设置里显式路径 → 一键安装目录 → `PATH`。
/// Windows 上文件名带 `.exe` 后缀。
fn resolve_ffmpeg(app: &AppHandle) -> Result<PathBuf, String> {
    if let Some(path) = read_settings(app)?.ffmpeg_path {
        if path.is_file() {
            return Ok(path);
        }
    }
    let exe_name = dossier_inpaint::erase::binary_name("ffmpeg");
    if let Ok(bin) = app_bin_dir(app) {
        let candidate = bin.join(&exe_name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Ok(PathBuf::from(exe_name))
}

/// ffprobe 解析顺序：界面显式提示（填了就用，错误让它自然暴露）→ 设置 → 一键安装目录 → `PATH`。
/// GUI 进程 PATH 很干净，按名兜底只是最后的退路。Windows 上文件名带 `.exe` 后缀。
pub(crate) fn resolve_ffprobe(app: &AppHandle, hint: Option<&str>) -> Result<PathBuf, String> {
    if let Some(hint) = hint.map(str::trim).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(hint));
    }
    if let Some(path) = read_settings(app)?.ffprobe_path {
        if path.is_file() {
            return Ok(path);
        }
    }
    let exe_name = dossier_inpaint::erase::binary_name("ffprobe");
    if let Ok(bin) = app_bin_dir(app) {
        let candidate = bin.join(&exe_name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Ok(PathBuf::from(exe_name))
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
    /// 运行环境自动安装在 macOS / Windows 都有可用下载源；其余平台前端据此隐藏入口，
    /// 避免给出「点了必然报错」的按钮。
    pub install_supported: bool,
    /// 平台标识（macos / windows / linux），前端据此给出对应平台的手动安装指引。
    pub platform: String,
    /// 内容模型运行时（ASR/OCR），供设置页预检材料线程。
    pub models: crate::content::ModelRuntimeStatus,
}

#[tauri::command]
pub fn runtime_status(
    app: AppHandle,
    ffprobe_path: Option<String>,
) -> Result<RuntimeStatus, String> {
    let result = runtime_status_inner(&app, ffprobe_path.as_deref());
    // 测试期关键日志：环境徽章 / 横幅 / 转码阻断都依赖这份检测结果。
    crate::logging::log_event(
        "环境",
        crate::logging::result_text(&result, |status| {
            let yes = |tool: &ToolReport| {
                if tool.ok {
                    format!("正常（{}）", tool.version.as_deref().unwrap_or(""))
                } else {
                    format!("缺失（{}）", tool.detail)
                }
            };
            format!(
                "ffmpeg {}，ffprobe {}，python {}，平台 {}",
                yes(&status.ffmpeg),
                yes(&status.ffprobe),
                yes(&status.python),
                status.platform
            )
        }),
    );
    result
}

fn runtime_status_inner(
    app: &AppHandle,
    ffprobe_path: Option<&str>,
) -> Result<RuntimeStatus, String> {
    let ffmpeg = resolve_ffmpeg(app)?;
    let ffprobe = resolve_ffprobe(app, ffprobe_path)?;
    let runner = SystemRunner::new();
    // 只探解释器版本（秒级）；torch/onnxruntime 导入太慢，留给擦除页自查。
    let python_report = probe_python(app, &runner);
    Ok(RuntimeStatus {
        ffmpeg: probe_tool(&runner, "ffmpeg", &ffmpeg, &["-hide_banner", "-version"]),
        ffprobe: probe_tool(&runner, "ffprobe", &ffprobe, &["-hide_banner", "-version"]),
        python: python_report,
        install_supported: cfg!(any(target_os = "macos", target_os = "windows")),
        platform: std::env::consts::OS.to_string(),
        models: crate::content::model_runtime(&read_settings(app)?, &runner),
    })
}

/// 探测 Python 解释器：设置里的绝对路径优先，随后 PATH 上的候选名。
/// Windows 候选是 `python` → `py -3`（商店占位符会以 9009 被判未找到）；
/// macOS/Linux 是 `python3`。
fn probe_python(app: &AppHandle, runner: &SystemRunner) -> ToolReport {
    let name = "python";
    if let Some(path) = configured_python(app) {
        let report = probe_tool(runner, name, &path, &["--version"]);
        if report.ok {
            return report;
        }
    }
    let primary = if cfg!(windows) {
        probe_tool(runner, name, Path::new("python"), &["--version"])
    } else {
        probe_tool(runner, name, Path::new("python3"), &["--version"])
    };
    if primary.ok || !cfg!(windows) {
        primary
    } else {
        probe_tool(runner, name, Path::new("py"), &["-3", "--version"])
    }
}

/// 设置里配置的 Python 路径（存在才返回）。
fn configured_python(app: &AppHandle) -> Option<PathBuf> {
    read_settings(app)
        .ok()?
        .python_path
        .filter(|path| path.is_file())
}

/// 为擦除 worker 解析一个可用的 Python 解释器；失败时给可行动的指引。
pub(crate) fn resolve_python(app: &AppHandle) -> Result<PathBuf, String> {
    if let Some(path) = configured_python(app) {
        return Ok(path);
    }
    let runner = SystemRunner::new();
    let report = probe_python(app, &runner);
    if report.ok {
        return Ok(PathBuf::from(report.program));
    }
    Err(format!(
        "未找到可用的 Python 解释器（{}）。请安装 Python 3 并加入 PATH，\
         或在 Windows 上使用「一键配置运行环境」自动安装。",
        report.detail
    ))
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

/// 一键安装的进度事件：弹窗据此渲染「需要下载 / 已下载 / 组件」和进度条。
#[derive(Debug, Clone, Serialize)]
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[serde(rename_all = "camelCase")]
pub struct InstallProgress {
    /// 正在处理的组件：ffmpeg / ffprobe。
    pub component: String,
    /// 机器可读阶段：connecting / downloading / extracting / verifying / finished。
    pub stage: String,
    /// 直接可展示的中文阶段名。
    pub stage_label: String,
    /// 下载总大小（HEAD 探测不到时为空，进度条退化为不定进度）。
    pub total_bytes: Option<u64>,
    pub downloaded_bytes: u64,
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn emit_install_progress(
    app: &AppHandle,
    component: &str,
    stage: &str,
    stage_label: &str,
    total_bytes: Option<u64>,
    downloaded_bytes: u64,
) {
    let _ = app.emit(
        "install-progress",
        InstallProgress {
            component: component.to_string(),
            stage: stage.to_string(),
            stage_label: stage_label.to_string(),
            total_bytes,
            downloaded_bytes,
        },
    );
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
pub(crate) fn run_ffmpeg(program: &Path, args: &[&str]) -> Result<(), String> {
    let mut spec = CommandSpec::new(program.display().to_string());
    for arg in args {
        spec = spec.arg(*arg);
    }
    spec = spec.timeout(std::time::Duration::from_secs(FFMPEG_TIMEOUT_SECS));
    let runner = SystemRunner::new();
    // 传入转码停止开关：点「停止」时当前这条 ffmpeg 会被立刻杀掉（其他任务未置位时行为不变）。
    let output = match runner.run_cancellable(&spec, &|| transcode_cancelled()) {
        Ok(output) => output,
        Err(dossier_runtime::CommandError::Cancelled { .. }) => {
            return Err("ffmpeg 已被停止".into());
        }
        Err(error) => return Err(format!("ffmpeg 启动失败（{}）：{error}", program.display())),
    };
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
    // 测试期关键日志：码率修复同样是长任务，入口与结果留痕。
    crate::logging::log_event("码率", format!("开始：{name}（阈值 {:?} kbps）", min_kbps));
    let result = tauri::async_runtime::spawn_blocking(move || {
        fix_bitrate_blocking(&app, root, name, min_kbps, ffprobe_path)
    })
    .await
    .map_err(|error| format!("码率修复任务失败：{error}"))?;
    crate::logging::log_event(
        "码率",
        crate::logging::result_text(&result, |report| {
            format!(
                "完成：修复 {} 项，失败 {} 项，无需处理 {} 项",
                report.fixed.len(),
                report.failed.len(),
                report.untouched
            )
        }),
    );
    result
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
    /// 深度处理并行材料流水线结果；普通转码为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub materials: Option<crate::content::MaterialPipelineReport>,
}

/// 视频编码参数：软件 libx264 用 CRF 23（质量优先，沿用历史行为）；
/// 硬件编码器（NVENC / VideoToolbox 等）用 6 Mbps 目标码率——
/// 码率参数在各家硬件编码器间通用，且高于 4 Mbps 报审安全线。
fn video_encode_args(encoder: &str) -> Vec<String> {
    if encoder == "libx264" {
        vec![
            "-preset".into(),
            "veryfast".into(),
            "-crf".into(),
            "23".into(),
        ]
    } else {
        vec!["-b:v".into(), "6M".into()]
    }
}

fn encode_output_args(output: &Path, encoder: &str) -> Vec<String> {
    let mut args: Vec<String> = vec!["-c:v".into(), encoder.to_string()];
    args.extend(video_encode_args(encoder));
    args.extend(["-c:a".into(), "aac".into(), "-b:a".into(), "128k".into()]);
    args.push("-movflags".into());
    args.push("+faststart".into());
    args.push(output.display().to_string());
    args
}

// ---------------------------------------------------------------------------
// 编码器选型与多路并行执行
// ---------------------------------------------------------------------------

/// 并行编码档位：编码器名 + 路数 + 给界面/日志的决策理由。
#[derive(Debug, Clone)]
struct EncodeLanes {
    encoder: String,
    lanes: usize,
    reason: String,
}

impl EncodeLanes {
    /// 界面/证明里的可读描述。
    fn describe(&self) -> String {
        format!("{}（{} 路并行）", self.encoder, self.lanes)
    }
}

/// 探测本机可用的 h264 编码器并决定并行路数：
/// NVENC / VideoToolbox 等硬件编码器 3 路（Grove-Max 同款）；
/// 回退 libx264 时 CPU 编码吃满核，降到 2 路避免卡死机器。
/// 不强制 GPU：任何机器都能跑，只是路数与编码器不同。
fn detect_encode_lanes(ffmpeg: &Path) -> EncodeLanes {
    let software_fallback = |reason: String| EncodeLanes {
        encoder: "libx264".to_string(),
        lanes: 2,
        reason,
    };
    let runner = SystemRunner::new();
    let caps = probe_ffmpeg_capabilities(&runner, Some(ffmpeg));
    if !caps.usable() {
        return software_fallback("ffmpeg 能力探测失败，回退 libx264 2 路".to_string());
    }
    // CUDA 设备信号在 Windows/Linux 上没有统一设备节点，用 nvidia-smi 探测结果补强。
    let gpu = probe_gpu(&runner);
    let signal = HardwareSignal::detect().with_cuda(gpu.available);
    let verified = plan_encoder(&caps.encoders, "h264", true, &signal).and_then(|plan| {
        verify_encoder_plan(
            &runner,
            &EncoderVerificationConfig::new(ffmpeg.display().to_string()),
            &plan,
        )
    });
    match verified {
        Ok(plan) if plan.hardware => EncodeLanes {
            encoder: plan.selected,
            lanes: 3,
            reason: plan.reason,
        },
        Ok(plan) => EncodeLanes {
            encoder: plan.selected,
            lanes: 2,
            reason: plan.reason,
        },
        Err(error) => software_fallback(format!("编码器探测失败（{error}），回退 libx264 2 路")),
    }
}

/// 互不依赖的编码单元并行执行：`lanes` 路工作线程从共享计数器领任务。
///
/// 单元形如 `(序号, 载荷)`；任一单元失败或用户停止后不再领新任务，
/// 等在跑的单元收尾，返回首个错误（或「已停止」）。成功单元的产出按序号
/// 归位到结果槽，调用方拿到的向量长度与 `units` 一致、顺序与输入一致。
/// `on_done` 在工作线程上回调（已完成数, 总数, 标签），实现须可跨线程共享。
fn run_encode_units<T, S, F>(
    units: &[(usize, T)],
    lanes: usize,
    on_done: &(dyn Fn(u64, u64, &str) + Sync),
    run_unit: F,
) -> Result<Vec<Option<S>>, String>
where
    T: Sync,
    S: Send,
    F: Fn(&T) -> Result<(S, String), String> + Sync,
{
    let total = units.len();
    if total == 0 {
        return Ok(Vec::new());
    }
    let next = AtomicUsize::new(0);
    let completed = AtomicUsize::new(0);
    let first_error: Mutex<Option<String>> = Mutex::new(None);
    let results: Mutex<Vec<Option<S>>> = Mutex::new((0..total).map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..lanes.max(1).min(total) {
            scope.spawn(|| loop {
                // 失败 / 用户停止后不再领新任务；在跑的单元由其内部收尾。
                if transcode_cancelled() {
                    return;
                }
                if first_error
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .is_some()
                {
                    return;
                }
                let slot = next.fetch_add(1, Ordering::SeqCst);
                if slot >= total {
                    return;
                }
                let (index, payload) = &units[slot];
                match run_unit(payload) {
                    Ok((value, label)) => {
                        let done = completed.fetch_add(1, Ordering::SeqCst) + 1;
                        on_done(done as u64, total as u64, &label);
                        results.lock().unwrap_or_else(|error| error.into_inner())[*index] =
                            Some(value);
                    }
                    Err(error) => {
                        let mut guard = first_error
                            .lock()
                            .unwrap_or_else(|error| error.into_inner());
                        if guard.is_none() {
                            *guard = Some(error);
                        }
                    }
                }
            });
        }
    });
    if transcode_cancelled() {
        return Err("已停止".into());
    }
    match first_error
        .into_inner()
        .unwrap_or_else(|error| error.into_inner())
    {
        Some(error) => Err(error),
        None => Ok(results
            .into_inner()
            .unwrap_or_else(|error| error.into_inner())),
    }
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
/// 各集互不依赖，`lanes` 路并行执行。
fn transcode_nudge_blocking(
    ffmpeg: &Path,
    lanes: &EncodeLanes,
    drama_name: &str,
    episodes: &[ProbedEpisode],
    output_dir: &Path,
    emit: &(dyn Fn(u64, u64, &str) + Sync),
    produced: &mut Vec<PathBuf>,
) -> Result<TranscodeRunReport, String> {
    std::fs::create_dir_all(output_dir)
        .map_err(|error| format!("创建输出目录失败（{}）：{error}", output_dir.display()))?;
    let units: Vec<(usize, &ProbedEpisode)> = episodes.iter().enumerate().collect();
    let results = run_encode_units(&units, lanes.lanes, emit, |episode| {
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
        args.extend(encode_output_args(&output, &lanes.encoder));
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run_ffmpeg(ffmpeg, &arg_refs).map_err(|error| {
            let _ = std::fs::remove_file(&output);
            format!("{}：{error}", episode.file.file_name)
        })?;
        let detail = format!("裁掉开头 {frames} 帧（{offset:.4} 秒，{fps:.2} fps）");
        Ok((
            (output.clone(), detail),
            format!("{}：完成", episode.file.file_name),
        ))
    })?;

    let mut items = Vec::new();
    for (episode, (output, detail)) in episodes.iter().zip(results.into_iter().flatten()) {
        produced.push(output.clone());
        items.push(TranscodeItemReport {
            source: episode.file.path.display().to_string(),
            output: output.display().to_string(),
            detail,
        });
    }
    Ok(TranscodeRunReport {
        mode: "nudge".into(),
        output_dir: output_dir.display().to_string(),
        items,
        materials: None,
        note: format!(
            "《{drama_name}》每集开头已微调 1~3 帧（{}），输出到新目录，原素材未改动。",
            lanes.describe()
        ),
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

/// 重组切段策略：独立重组按「目标集数」等分，深度处理按「单集时长区间」反推集数。
enum RecutStrategy {
    TargetCount(usize),
    DurationRange { min_sec: f64, max_sec: f64 },
}

/// 一个重组编码任务：目标输出 + 对应的源片段（任务间互不依赖，可并行）。
struct RecutJob {
    index: usize,
    output: PathBuf,
    spans: Vec<SourceSpan>,
}

/// 分集重组：全剧拼成一条时间线，在场景切点附近重新分集；切不到精确关键帧，
/// 因此每个片段统一重编码（保证切点帧级精确）。输出名保持「第NN集」以便再次扫描。
/// 场景检测与片段编码都按 `lanes` 路并行。
fn transcode_recut_blocking(
    ffmpeg: &Path,
    lanes: &EncodeLanes,
    episodes: &[ProbedEpisode],
    strategy: RecutStrategy,
    output_dir: &Path,
    emit: &(dyn Fn(u64, u64, &str) + Sync),
    produced: &mut Vec<PathBuf>,
) -> Result<TranscodeRunReport, String> {
    std::fs::create_dir_all(output_dir)
        .map_err(|error| format!("创建输出目录失败（{}）：{error}", output_dir.display()))?;

    // 阶段一：逐集检测切点（并行，结果按集序合并），映射到全剧时间线。
    let ffmpeg_str = ffmpeg.display().to_string();
    let detect_units: Vec<(usize, &ProbedEpisode)> = episodes.iter().enumerate().collect();
    emit(0, episodes.len() as u64, "正在检测场景切点");
    let detect_results = run_encode_units(&detect_units, lanes.lanes, emit, |episode| {
        let report = detect_shots(
            &ffmpeg_str,
            &episode.file.path,
            &ShotDetectConfig::default(),
        )
        .map_err(|error| format!("{} 场景检测失败：{error}", episode.file.file_name))?;
        let label = format!(
            "{}：切点 {} 个",
            episode.file.file_name,
            report.boundaries.len()
        );
        Ok((
            report
                .boundaries
                .iter()
                .map(|b| b.time_sec)
                .collect::<Vec<f64>>(),
            label,
        ))
    })?;
    let mut boundaries: Vec<f64> = Vec::new();
    let mut file_starts: Vec<f64> = Vec::with_capacity(episodes.len());
    let mut offset = 0.0;
    for (episode, per_file) in episodes.iter().zip(detect_results) {
        file_starts.push(offset);
        boundaries.extend(
            per_file
                .unwrap_or_default()
                .iter()
                .map(|time| offset + time),
        );
        offset += episode.spec.duration_sec;
    }
    let total = offset;
    // 用 partial_cmp 保持 NaN 拒绝语义（`<=` 写法会放行 NaN，让重组段变成垃圾值）。
    if total.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
        return Err("全剧总时长为 0，无法重组。".into());
    }

    let segments = match strategy {
        RecutStrategy::TargetCount(target) => plan_recut_segments(&boundaries, total, target, 1.0),
        RecutStrategy::DurationRange { min_sec, max_sec } => {
            plan_recut_by_duration(&boundaries, total, min_sec, max_sec)
        }
    };
    // 预构造编码任务：跨源片段的中间件名带集号，任务间互不冲突。
    let jobs: Vec<RecutJob> = segments
        .iter()
        .enumerate()
        .map(|(index, segment)| RecutJob {
            index,
            output: output_dir.join(format!("第{:02}集.mp4", index + 1)),
            spans: spans_for_segment(episodes, &file_starts, *segment),
        })
        .filter(|job| !job.spans.is_empty())
        .collect();
    let job_units: Vec<(usize, &RecutJob)> = jobs.iter().enumerate().collect();
    let encode_results = run_encode_units(&job_units, lanes.lanes, emit, |job| {
        encode_recut_job(ffmpeg, lanes, job)?;
        let label = format!(
            "第 {} 集：完成（{} 个源片段）",
            job.index + 1,
            job.spans.len()
        );
        Ok((job.output.clone(), label))
    })?;

    let mut items = Vec::new();
    for (job, output) in jobs.iter().zip(encode_results.into_iter().flatten()) {
        produced.push(output);
        items.push(TranscodeItemReport {
            source: job
                .spans
                .iter()
                .map(|span| span.file_name.as_str())
                .collect::<Vec<_>>()
                .join(" + "),
            output: job.output.display().to_string(),
            detail: format!(
                "{:.1}s – {:.1}s（{} 个源片段）",
                job.spans.first().map_or(0.0, |s| s.local_start),
                job.spans.last().map_or(0.0, |s| s.local_start + s.duration),
                job.spans.len()
            ),
        });
    }

    let produced_count = items.len();
    Ok(TranscodeRunReport {
        mode: "recut".into(),
        output_dir: output_dir.display().to_string(),
        items,
        materials: None,
        note: format!(
            "已按场景切点重新分成 {produced_count} 集（统一重编码保证切点精确，{}），输出到新目录，原素材未改动。",
            lanes.describe()
        ),
    })
}

/// 执行单个重组编码任务：单源片段一条命令直达；跨源片段先逐源切片，
/// 再用 concat demuxer 无损拼接（编码参数一致）。失败时清理本次中间文件。
fn encode_recut_job(ffmpeg: &Path, lanes: &EncodeLanes, job: &RecutJob) -> Result<(), String> {
    let output = &job.output;
    let spans = &job.spans;
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
        args.extend(encode_output_args(output, &lanes.encoder));
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run_ffmpeg(ffmpeg, &arg_refs).map_err(|error| {
            let _ = std::fs::remove_file(output);
            format!("第 {} 集：{error}", job.index + 1)
        })?;
    } else {
        // 跨源片段：先逐源切片，再用 concat demuxer 无损拼接（编码参数一致）。
        let mut part_paths: Vec<PathBuf> = Vec::new();
        for (part_index, span) in spans.iter().enumerate() {
            let part = output
                .parent()
                .unwrap_or(Path::new("."))
                .join(format!(".recut-{:02}-part{:02}.mp4", job.index, part_index));
            let mut args: Vec<String> = vec![
                "-y".into(),
                "-ss".into(),
                format!("{:.6}", span.local_start),
                "-i".into(),
                span.file.display().to_string(),
                "-t".into(),
                format!("{:.6}", span.duration),
            ];
            args.extend(encode_output_args(&part, &lanes.encoder));
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            if let Err(error) = run_ffmpeg(ffmpeg, &arg_refs) {
                for part in &part_paths {
                    let _ = std::fs::remove_file(part);
                }
                let _ = std::fs::remove_file(output);
                return Err(format!(
                    "第 {} 集（{}）：{error}",
                    job.index + 1,
                    span.file_name
                ));
            }
            part_paths.push(part);
        }

        let list_path = output
            .parent()
            .unwrap_or(Path::new("."))
            .join(format!(".recut-{:02}-list.txt", job.index));
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
            let _ = std::fs::remove_file(output);
            format!("第 {} 集：拼接失败：{error}", job.index + 1)
        })?;
    }
    Ok(())
}

/// 材料先行阶段的产出与来源记录（写 AI 制作证明用）。
#[derive(Debug, Default)]
struct MaterialSources {
    title: Option<String>,
    title_source: String,
    synopsis: Option<String>,
    synopsis_source: String,
    poster: Option<PathBuf>,
    poster_source: String,
    storyboard_source: String,
}

/// trim 后非空才返回（空字符串视同未填写）。
fn non_empty(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// LLM 来源标签（配置了模型名就带上，便于在证明里溯源）。
fn ai_source_label(model: &Option<String>) -> String {
    match non_empty(model) {
        Some(name) => format!("AI 生成（模型 {name}）"),
        None => "AI 生成".to_string(),
    }
}

/// 从原始素材里均匀抽 `count` 个代表集（不足时循环复用），供过程图抽帧。
/// 抽帧源是**原始素材**而非成片：材料先行意味着过程图在编码前就要产出。
fn representative_episodes(episodes: &[ProbedEpisode], count: usize) -> Vec<(PathBuf, f64)> {
    let total = episodes.len();
    if total == 0 {
        return Vec::new();
    }
    (0..count)
        .map(|slot| {
            let index = if total >= count {
                slot * (total - 1) / (count - 1).max(1)
            } else {
                slot % total
            };
            let episode = &episodes[index];
            (episode.file.path.clone(), episode.spec.duration_sec)
        })
        .collect()
}

/// 当天日期（本地时区按东八区口径，YYYY-MM-DD）。std 没有日期格式化，用民用历算法换算。
fn local_date_string() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
        + 8 * 3600;
    let days = secs.div_euclid(86_400);
    // Howard Hinnant 的 civil_from_days：days 自 1970-01-01 → (年, 月, 日)。
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

/// 在素材目录里找封面图：文件名含「封面 / cover / poster」的图片（与清单规则同口径）。
/// 报审素材目录按规范都会带封面，深度处理直接复用，无需用户手动上传。
fn find_local_cover(dir: &Path) -> Option<PathBuf> {
    let image_exts = ["jpg", "jpeg", "png", "webp"];
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext.to_ascii_lowercase())
                    .is_some_and(|ext| image_exts.contains(&ext.as_str()))
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        let lower = name.to_ascii_lowercase();
                        ["封面", "cover", "poster"]
                            .iter()
                            .any(|keyword| lower.contains(keyword))
                    })
        })
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

/// 把素材目录里的封面复制为输出目录的「剧目海报.<ext>」；找不到或复制失败返回 None。
pub(crate) fn copy_local_cover(dir: &Path, output_dir: &Path) -> Option<PathBuf> {
    let source = find_local_cover(dir)?;
    let ext = source
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())?;
    std::fs::create_dir_all(output_dir).ok()?;
    let target = output_dir.join(format!("剧目海报.{ext}"));
    std::fs::copy(&source, &target).ok()?;
    Some(target)
}

/// 读素材目录里的简介文本：文件名含「简介 / intro / synopsis」的 txt/md（与清单规则同口径）。
/// 返回 trim 后的非空内容。
fn read_local_synopsis(dir: &Path) -> Option<String> {
    let text_exts = ["txt", "md"];
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| ext.to_ascii_lowercase())
                    .is_some_and(|ext| text_exts.contains(&ext.as_str()))
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        let lower = name.to_ascii_lowercase();
                        ["简介", "intro", "synopsis"]
                            .iter()
                            .any(|keyword| lower.contains(keyword))
                    })
        })
        .collect();
    candidates.sort();
    let path = candidates.into_iter().next()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// 写入 AI 制作证明（Grove-Max 同款）：无条件生成一份 txt 落输出目录，
/// 记录剧名/简介/海报/过程图的来源（AI 生成（模型名）/ 本地提供）、
/// 制作工具与版本、日期和处理方式，末尾附 AI 声明。
fn write_ai_proof(
    app: &AppHandle,
    drama_name: &str,
    materials: &MaterialSources,
    lanes: &EncodeLanes,
    episode_count: usize,
    output_dir: &Path,
) -> Result<PathBuf, String> {
    let version = app.package_info().version.to_string();
    let date = local_date_string();
    let line = |label: &str, value: Option<&str>, source: &str| {
        let value = value.unwrap_or("未填写");
        format!("{label}：{value}\n来源：{source}\n")
    };
    let mut body = String::new();
    body.push_str("AI 制作证明\n\n");
    body.push_str(&format!("生成日期：{date}（本地时间）\n"));
    body.push_str(&format!("制作工具：DramaDossier v{version}\n"));
    body.push_str(&format!(
        "原剧目：{drama_name}（原始素材 {episode_count} 集）\n"
    ));
    body.push_str(&format!(
        "处理方式：按场景切点重组分集并逐集微帧，成片输出到「视频成片/」子目录，编码 {}。\n\n",
        lanes.describe()
    ));
    body.push_str(&line(
        "剧名",
        materials.title.as_deref(),
        &materials.title_source,
    ));
    body.push_str(&line(
        "简介",
        materials.synopsis.as_deref(),
        &materials.synopsis_source,
    ));
    body.push_str(&line(
        "海报",
        materials
            .poster
            .as_ref()
            .map(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or(None)
            .as_deref(),
        &materials.poster_source,
    ));
    body.push_str(&format!(
        "过程图：过程图_01 ~ 过程图_04（画面抽自原始素材代表集）\n来源：{}\n\n",
        materials.storyboard_source
    ));
    body.push_str(
        "AI 声明：本目录中的剧名、简介、海报与过程图由人工智能辅助生成或整理，\
         分集重组与微帧由 DramaDossier 自动化完成，原始视频素材未作改动。\
         报审前请人工复核上述内容。\n",
    );
    let path = output_dir.join("AI制作证明.txt");
    std::fs::write(&path, body)
        .map_err(|error| format!("写入 AI 制作证明失败（{}）：{error}", path.display()))?;
    Ok(path)
}

/// 深度处理（材料先行）：先产出报审材料（剧名/简介/海报/过程图/AI 制作证明），
/// 再分集重组到临时中转目录、逐集微帧输出到最终目录（Grove-Max 同款时序）。
///
/// 材料流水线由调用方在编码前并行启动，结束编码后汇合，材料失败不回滚成片。
/// 进度映射：重组 0~50%，微帧 50~100%。
#[allow(clippy::too_many_arguments)]
fn transcode_deep_blocking(
    app: &AppHandle,
    ffmpeg: &Path,
    ffprobe: &Path,
    drama_name: &str,
    drama_dir: &Path,
    episodes: &[ProbedEpisode],
    min_sec: f64,
    max_sec: f64,
    lanes: &EncodeLanes,
    output_dir: &Path,
    local_materials: bool,
    emit: &(dyn Fn(u64, u64, &str) + Sync),
    produced: &mut Vec<PathBuf>,
) -> Result<TranscodeRunReport, String> {
    std::fs::create_dir_all(output_dir)
        .map_err(|error| format!("创建输出目录失败（{}）：{error}", output_dir.display()))?;
    // 材料阶段（仅本地兜底模式）：流水线未启用/未就绪时在编码前串行产出材料
    //（本地复用 → AI 生成逐级兜底）；流水线模式下材料与编码并行，此处跳过。
    if local_materials {
        let mut settings = read_settings(app)?;
        let mut materials = MaterialSources::default();
        let llm_label = ai_source_label(&settings.llm_model);
        // 本地简介：设置里已存的优先，其次素材目录里的 简介*.txt（报审素材按规范都带，清单也要求）。
        let settings_synopsis = non_empty(&settings.drama_synopsis);
        let file_synopsis = if settings_synopsis.is_some() {
            None
        } else {
            read_local_synopsis(drama_dir)
        };
        let local_synopsis = settings_synopsis.clone().or(file_synopsis.clone());

        // 剧名：AI 开且当前为空 → 生成（素材用简介）；已有值沿用；AI 关且为空 → 跳过不阻塞（可后补）。
        let title_ai_on = settings.ai_title_enabled != Some(false);
        materials.title = non_empty(&settings.drama_title);
        materials.title_source = if materials.title.is_some() {
            if title_ai_on {
                llm_label.clone()
            } else {
                "本地提供".to_string()
            }
        } else if title_ai_on {
            emit(1, 100, "材料 · AI 生成剧名");
            let context = local_synopsis
                .clone()
                .unwrap_or_else(|| format!("剧目目录名：{drama_name}"));
            match crate::ai::generate_text_blocking(&settings, crate::ai::TextKind::Title, &context)
            {
                Ok(text) => {
                    settings.drama_title = Some(text.clone());
                    materials.title = Some(text);
                    llm_label.clone()
                }
                Err(error) => {
                    crate::logging::log_event(
                        "材料",
                        format!("剧名生成失败（不阻塞，可后补）：{error}"),
                    );
                    "AI 生成失败，未填写".to_string()
                }
            }
        } else {
            "未填写（本地模式）".to_string()
        };

        // 简介：设置已存 → 素材目录 简介*.txt → AI 生成 → 跳过，逐级兜底且都不阻塞。
        let synopsis_ai_on = settings.ai_synopsis_enabled != Some(false);
        materials.synopsis = local_synopsis.clone();
        materials.synopsis_source = if settings_synopsis.is_some() {
            if synopsis_ai_on {
                llm_label.clone()
            } else {
                "本地提供".to_string()
            }
        } else if file_synopsis.is_some() {
            "本地提供（素材目录）".to_string()
        } else if synopsis_ai_on {
            emit(2, 100, "材料 · AI 生成简介");
            let context = materials
                .title
                .clone()
                .unwrap_or_else(|| format!("剧目目录名：{drama_name}"));
            match crate::ai::generate_text_blocking(
                &settings,
                crate::ai::TextKind::Synopsis,
                &context,
            ) {
                Ok(text) => {
                    settings.drama_synopsis = Some(text.clone());
                    materials.synopsis = Some(text);
                    llm_label.clone()
                }
                Err(error) => {
                    crate::logging::log_event(
                        "材料",
                        format!("简介生成失败（不阻塞，可后补）：{error}"),
                    );
                    "AI 生成失败，未填写".to_string()
                }
            }
        } else {
            "未填写（本地模式）".to_string()
        };
        // 简介落到设置：海报提示词与后续复用都读这里（任务末尾随材料一并写回）。
        if settings.drama_synopsis.is_none() {
            settings.drama_synopsis = materials.synopsis.clone();
        }

        // 海报：复用已有（设置里的路径或输出目录内上次的产物）→ AI 生成 → 复制素材目录封面 → 跳过。
        let poster_ai_on = settings.ai_poster_enabled != Some(false);
        let poster_in_output = ["png", "jpg", "jpeg", "webp"]
            .iter()
            .map(|ext| output_dir.join(format!("剧目海报.{ext}")))
            .find(|path| path.is_file());
        let existing_poster = settings
            .poster_path
            .as_ref()
            .filter(|path| path.is_file())
            .cloned()
            .or(poster_in_output);
        if let Some(path) = existing_poster {
            settings.poster_path = Some(path.clone());
            materials.poster = Some(path.clone());
            materials.poster_source = "已生成（本次复用）".to_string();
        } else if poster_ai_on {
            emit(3, 100, "材料 · AI 生成海报");
            match crate::ai::generate_poster_blocking(&settings, output_dir) {
                Ok(path) => {
                    settings.poster_path = Some(path.clone());
                    materials.poster = Some(path.clone());
                    materials.poster_source = ai_source_label(&settings.img_model);
                }
                Err(error) => {
                    crate::logging::log_event(
                        "材料",
                        format!("海报生成失败（不阻塞，可后补）：{error}"),
                    );
                    materials.poster_source = "AI 生成失败，未生成".to_string();
                }
            }
        }
        // 兜底：还没海报就复制素材目录的封面（报审素材按规范都带封面）。
        if materials.poster.is_none() {
            if let Some(path) = copy_local_cover(drama_dir, output_dir) {
                settings.poster_path = Some(path.clone());
                materials.poster = Some(path);
                materials.poster_source = "本地提供（素材封面）".to_string();
            }
        }

        // 过程图：抽帧源是原始素材的 4 个代表集（编码前就要产出，不能依赖成片）。
        emit(4, 100, "材料 · 生成过程图");
        materials.storyboard_source = if crate::ai::storyboard_llm_ready(&settings) {
            llm_label.clone()
        } else {
            "模板生成（未配置 LLM）".to_string()
        };
        let picks = representative_episodes(episodes, PROCESS_IMAGE_COUNT);
        if let Err(error) = generate_process_images_blocking(
            app,
            ffmpeg,
            drama_name,
            &picks,
            episodes.len(),
            output_dir,
        ) {
            crate::logging::log_event("材料", format!("过程图生成失败（不影响成片）：{error}"));
        } else {
            crate::logging::log_event("材料", format!("过程图已生成到 {}", output_dir.display()));
        }

        // AI 制作证明：无条件生成一份（Grove-Max 同款「已写入 AI 制作证明」）。
        emit(5, 100, "材料 · 写出简介与 AI 制作证明");
        // 简介落盘：输出目录带一份 简介.txt（AI 生成或素材目录原有，随成片一起交付）。
        if let Some(text) = materials
            .synopsis
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            let synopsis_path = output_dir.join("简介.txt");
            if let Err(error) = std::fs::write(&synopsis_path, text) {
                crate::logging::log_event(
                    "材料",
                    format!("简介.txt 写入失败（不影响成片）：{error}"),
                );
            }
        }
        if let Err(error) = write_ai_proof(
            app,
            drama_name,
            &materials,
            lanes,
            episodes.len(),
            output_dir,
        ) {
            crate::logging::log_event("材料", format!("AI 制作证明写入失败：{error}"));
        }

        // 材料回写设置：界面在任务完成后重新加载设置即可看到（进度条文案走 emit）。
        emit(6, 100, "材料 · 已生成并校验");
        let settings_path = crate::settings_path(app)?;
        if let Err(error) = dossier_core::save_settings(&settings_path, &settings) {
            crate::logging::log_event("材料", format!("材料写回设置失败：{error}"));
        }
    }

    // ── 编码阶段：本地材料模式 重组（10~55%）→ 微帧（55~100%）；
    //    流水线模式 重组（0~50%）→ 微帧（50~100%）──────────
    let staging = output_dir.join(".deep-staging");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .map_err(|error| format!("创建中转目录失败（{}）：{error}", staging.display()))?;

    // 阶段一：分集重组（产物先落中转目录；本地材料模式映射 10~55，流水线模式映射 0~50）。
    {
        let recut_base = if local_materials { 10 } else { 0 };
        let step_recut = |done: u64, total: u64, label: &str| {
            let scaled = recut_base + done.min(total) * (50 - recut_base) / total.max(1);
            emit(scaled, 100, &format!("重组 · {label}"));
        };
        let mut staging_produced: Vec<PathBuf> = Vec::new();
        let recut_result = transcode_recut_blocking(
            ffmpeg,
            lanes,
            episodes,
            RecutStrategy::DurationRange { min_sec, max_sec },
            &staging,
            &step_recut,
            &mut staging_produced,
        );
        if let Err(error) = recut_result {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
    }

    // 收集中转目录里的重组产物（固定名 第NN集.mp4，按文件名排序保证集序）。
    let mut recut_files: Vec<ProbedEpisode> = Vec::new();
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&staging)
        .map_err(|error| format!("读取中转目录失败：{error}"))?
        .filter_map(|entry| entry.ok().map(|value| value.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("mp4"))
        .collect();
    entries.sort();
    let ffprobe_cfg = dossier_core::FfprobeConfig {
        bin: Some(ffprobe.to_path_buf()),
    };
    for path in &entries {
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let episode_file = EpisodeFile {
            path: path.clone(),
            file_name: file_name.clone(),
            size_bytes: std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0),
            episode_number: None,
        };
        let probed = probe_episode(&episode_file, &ffprobe_cfg)
            .map_err(|error| format!("探测重组产物 {file_name} 失败：{error}"))?;
        recut_files.push(probed);
    }
    if recut_files.is_empty() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err("分集重组没有产出任何片段，无法继续微帧。".into());
    }

    // 阶段二：对每个新集做智能微帧（本地材料模式映射 55~100，流水线模式映射 50~100）。
    // 成片单独放「视频成片/」子目录（Grove-Max 同款结构），材料与视频分层清晰。
    let final_video_dir = output_dir.join("视频成片");
    let nudge_result = {
        let nudge_base = if local_materials { 55 } else { 50 };
        let step_nudge = |done: u64, total: u64, label: &str| {
            let scaled = nudge_base + done.min(total) * (100 - nudge_base) / total.max(1);
            emit(scaled, 100, &format!("微帧 · {label}"));
        };
        transcode_nudge_blocking(
            ffmpeg,
            lanes,
            drama_name,
            &recut_files,
            &final_video_dir,
            &step_nudge,
            produced,
        )
    };
    let _ = std::fs::remove_dir_all(&staging);
    let nudge_report = nudge_result?;
    let episode_count = nudge_report.items.len();

    Ok(TranscodeRunReport {
        mode: "deep".into(),
        output_dir: output_dir.display().to_string(),
        items: nudge_report.items,
        materials: None,
        note: format!(
            "深度处理完成：{}，全剧按场景切点重组后逐集微帧（{}），共 {episode_count} 集，成片在「视频成片/」子目录，原素材未改动。",
            if local_materials {
                "材料先行（剧名/简介/海报/过程图/AI 制作证明）"
            } else {
                "材料与编码并行处理"
            },
            lanes.describe()
        ),
    })
}

const PROCESS_IMAGE_COUNT: usize = 4;
// 每图帧数固定为 3（抽帧步长 10%/40%/70% 已按 3 帧写死），暂未引用，注释保留备查。
// const FRAMES_PER_PROCESS_IMAGE: usize = 3;

/// 生成报审过程图：从给定代表集抽帧 + 分镜概要文字，交给 Pillow 脚本排版出 4 张图。
///
/// 材料先行后抽帧源是**原始素材代表集**（`video_specs` 由调用方用
/// [`representative_episodes`] 产出），不再是成片。概要文字优先用 LLM
/// （一次调用产 4 段），未配置或失败时退回模板（Python 侧同样兜底）。
/// 整个函数是「软失败」语义：调用方（deep 流程）只把错误记日志，不影响成片产物。
fn generate_process_images_blocking(
    app: &AppHandle,
    ffmpeg: &Path,
    drama_name: &str,
    video_specs: &[(PathBuf, f64)],
    episode_count: usize,
    output_dir: &Path,
) -> Result<(), String> {
    if video_specs.is_empty() {
        return Err("没有可用的抽帧素材，跳过过程图".into());
    }
    let settings = read_settings(app)?;
    let drama_title = settings.drama_title.clone().unwrap_or_default();

    // 1) 每个代表集在 10% / 40% / 70% 处各抓一帧（video_specs 已是代表集清单）。
    let frames_dir = output_dir.join(".process-frames");
    let _ = std::fs::remove_dir_all(&frames_dir);
    std::fs::create_dir_all(&frames_dir).map_err(|error| format!("创建抽帧目录失败：{error}"))?;
    let mut frame_paths: Vec<String> = Vec::new();
    let sample_points = [0.10f64, 0.40, 0.70];
    for slot in 0..PROCESS_IMAGE_COUNT {
        let (video, duration) = &video_specs[slot % video_specs.len()];
        for (step, pct) in sample_points.iter().enumerate() {
            let frame_path = frames_dir.join(format!("frame-{slot:02}-{step}.jpg"));
            let offset = duration * pct;
            let args = [
                "-y".to_string(),
                "-ss".to_string(),
                format!("{offset:.3}"),
                "-i".to_string(),
                video.display().to_string(),
                "-frames:v".to_string(),
                "1".to_string(),
                "-q:v".to_string(),
                "3".to_string(),
                frame_path.display().to_string(),
            ];
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            if let Err(error) = run_ffmpeg(ffmpeg, &arg_refs) {
                let _ = std::fs::remove_dir_all(&frames_dir);
                return Err(format!("抽帧失败（{}）：{error}", video.display()));
            }
            frame_paths.push(frame_path.display().to_string());
        }
    }

    // 2) 分镜概要：LLM 优先，失败退模板（模板 JSON 交由 Python 侧兜底也行，这里统一在 Rust 侧生成）。
    let summaries_path = frames_dir.join("summaries.json");
    let summaries: Vec<String> =
        match crate::ai::storyboard_summaries_blocking(&settings, &drama_title, drama_name) {
            Ok(list) => list,
            Err(_) => (0..PROCESS_IMAGE_COUNT)
                .map(|index| {
                    format!(
                        "《{drama_name}》分镜 {}：剧情推进，冲突升级，关键情节在画面中心展开。",
                        index + 1
                    )
                })
                .collect(),
        };
    let summaries_payload = serde_json::json!({ "summaries": summaries }).to_string();
    std::fs::write(&summaries_path, summaries_payload)
        .map_err(|error| format!("写概要 JSON 失败：{error}"))?;

    // 3) 调 Pillow 脚本排版（脚本与 cloud/.venv 同仓库根，安装版须从项目目录启动才有 cloud 环境）。
    let python = locate_cloud_python()?;
    let script = python
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(|repo| {
            repo.join("tools")
                .join("process_images")
                .join("make_process_images.py")
        })
        .ok_or_else(|| "无法从 Python 路径推断脚本位置".to_string())?;
    if !script.is_file() {
        let _ = std::fs::remove_dir_all(&frames_dir);
        return Err(format!("过程图脚本缺失：{}", script.display()));
    }
    let spec = CommandSpec::new(python.display().to_string())
        .args([
            script.display().to_string(),
            "--output-dir".into(),
            output_dir.display().to_string(),
            "--title".into(),
            drama_title.clone(),
            "--episode-label".into(),
            format!("全剧 {episode_count} 集"),
            "--frames".into(),
            frame_paths.join(","),
            "--poster".into(),
            settings
                .poster_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            "--summaries".into(),
            summaries_path.display().to_string(),
        ])
        // 排版是纯 CPU 图像处理，2 分钟封顶防挂死。
        .timeout(Duration::from_secs(120));
    let output = SystemRunner::new()
        .run(&spec)
        .map_err(|error| format!("过程图进程启动失败：{error}"))?;
    let frames_cleaned = std::fs::remove_dir_all(&frames_dir);
    if !output.success() {
        let _ = frames_cleaned;
        let stderr_tail: Vec<&str> = output.stderr.lines().rev().take(4).collect();
        let stderr_tail = stderr_tail.into_iter().rev().collect::<Vec<_>>().join("\n");
        return Err(format!(
            "过程图脚本退出码 {}：{stderr_tail}",
            output.code_label()
        ));
    }
    Ok(())
}

fn transcode_blocking(
    app: &AppHandle,
    root: String,
    name: String,
    request: TranscodeRequest,
    ffprobe_path: Option<String>,
    output_root: Option<String>,
) -> Result<TranscodeRunReport, String> {
    // 每次转码都是全新一次运行，先复位停止开关（上一轮的停止指令不残留）。
    TRANSCODE_CANCEL.store(false, Ordering::SeqCst);
    let ffmpeg = resolve_ffmpeg(app)?;
    let ffprobe = resolve_ffprobe(app, ffprobe_path.as_deref())?;
    // 编码器选型一次（NVENC / VideoToolbox → 3 路；libx264 → 2 路），全程共用。
    let lanes = detect_encode_lanes(&ffmpeg);
    crate::logging::log_event(
        "转码",
        format!("编码器：{}，依据：{}", lanes.describe(), lanes.reason),
    );
    let drama = load_drama(&root, &name)?;
    let episodes = probe_all(&drama, Some(ffprobe.display().to_string()));
    if episodes.is_empty() {
        return Err("没有任何成功探测规格的分集；请先确认素材可读且 ffprobe 可用。".into());
    }

    let mode = request.mode.as_str();
    if !matches!(mode, "nudge" | "recut" | "deep") {
        return Err(format!("未知转码模式：{mode}（支持 nudge / recut / deep）"));
    }
    // 深度处理的输出目录与目标集数一律以设置为准（页内不再单独收集）：
    // 设置里的「深度处理输出目录」等价于本次覆盖值；没配则走产物根目录 → 剧目目录的通用规则。
    let settings = read_settings(app)?;
    let effective_override = if mode == "deep" {
        settings
            .deep_output_dir
            .clone()
            .or_else(|| request.output_dir.clone().map(PathBuf::from))
    } else {
        request.output_dir.clone().map(PathBuf::from)
    };
    let subdir = match mode {
        "recut" => "重组输出",
        "deep" => "深度处理",
        _ => "微调输出",
    };
    // 与报审包同一套落点规则（`dossier_core::resolve_product_dir`）：
    // 本次覆盖 → 设置里的产物根目录下的 `<剧名>/<子目录>` → 剧目目录内的同名子目录。
    let output_dir = resolve_product_dir(
        effective_override.as_deref(),
        output_root.as_deref().map(Path::new),
        &drama.dir,
        &drama.name,
        subdir,
    );
    reject_source_dir(&output_dir, &episodes)?;
    // 停止时的清理策略依赖「目录是不是本次新建的」：新建的整目录删除，
    // 已存在的只删本次写出的文件，避免误删用户目录里的其他内容。
    let dir_existed = output_dir.exists();
    let mut produced: Vec<PathBuf> = Vec::new();

    // 并行执行器在工作线程上回调进度；emit_task_progress 只依赖 &AppHandle，可安全共享。
    let emit = |done: u64, total: u64, label: &str| {
        emit_task_progress(app, "transcode", done, total, label);
    };
    // 深度处理先把材料线程拉起来，与编码并行；材料失败不回滚成片。
    let mut material_job = if mode == "deep" && settings.effective_material_pipeline_enabled() {
        let runtime = crate::content::model_runtime(&settings, &SystemRunner::new());
        if !runtime.ready {
            crate::logging::log_event("材料", format!("运行时未就绪：{}", runtime.guidance));
        }
        crate::logging::log_event(
            "材料",
            format!(
                "启动并行流水线：ASR={}，OCR={}",
                runtime.asr.program, runtime.ocr.program
            ),
        );
        Some(crate::content::start_material_pipeline(
            app,
            &settings,
            &runtime,
            &ffmpeg,
            &episodes,
            &output_dir,
            &drama.dir,
            crate::content::MaterialProofContext {
                drama_name: drama.name.clone(),
                original_episodes: episodes.len(),
                encoder: lanes.describe(),
            },
        ))
    } else {
        None
    };
    let result = match mode {
        "recut" => {
            let target = request
                .target_count
                .unwrap_or(episodes.len())
                .clamp(1, episodes.len().max(1) * 10);
            transcode_recut_blocking(
                &ffmpeg,
                &lanes,
                &episodes,
                RecutStrategy::TargetCount(target),
                &output_dir,
                &emit,
                &mut produced,
            )
        }
        "deep" => {
            // 单集时长区间一律读设置（页内不再传）；集数由素材总时长反推。
            let (min_sec, max_sec) = settings.effective_recut_range();
            transcode_deep_blocking(
                app,
                &ffmpeg,
                &ffprobe,
                &drama.name,
                &drama.dir,
                &episodes,
                f64::from(min_sec),
                f64::from(max_sec),
                &lanes,
                &output_dir,
                // 流水线已启动（ASR/OCR 就绪）→ 材料并行；否则走本地材料先行兜底。
                material_job.is_none(),
                &emit,
                &mut produced,
            )
        }
        _ => transcode_nudge_blocking(
            &ffmpeg,
            &lanes,
            &drama.name,
            &episodes,
            &output_dir,
            &emit,
            &mut produced,
        ),
    };
    let materials = material_job
        .take()
        .and_then(|job| job.join())
        .map(|result| {
            result.unwrap_or_else(|error| {
                crate::logging::log_event("材料", format!("失败：{error}"));
                crate::content::MaterialPipelineReport::failed(error)
            })
        });
    if let Some(report) = &materials {
        crate::logging::log_event("材料", format!("已汇合：{}", report.status));
    }
    match result {
        Ok(mut report) => {
            report.materials = materials;
            Ok(report)
        }
        Err(error) => {
            if transcode_cancelled() {
                cleanup_transcode_output(&output_dir, dir_existed, &produced);
                crate::logging::log_event(
                    "转码",
                    format!(
                        "已停止：删除输出 {}（{} 个本次产物）",
                        output_dir.display(),
                        produced.len()
                    ),
                );
                return Err("已按要求停止转码，正在输出的目录已删除。".into());
            }
            Err(error)
        }
    }
}

/// 停止后清理本次转码产物：目录是本次新建的 → 整目录删除；
/// 目录早已存在 → 只删本次写出的文件，再尝试删掉空目录（删不掉说明还有别的内容，留着不动）。
/// 深度处理的成片在「视频成片/」子目录，一并尝试清掉空壳。
fn cleanup_transcode_output(output_dir: &Path, dir_existed: bool, produced: &[PathBuf]) {
    if !dir_existed {
        let _ = std::fs::remove_dir_all(output_dir);
        return;
    }
    for path in produced {
        let _ = std::fs::remove_file(path);
    }
    let _ = std::fs::remove_dir(output_dir.join("视频成片"));
    let _ = std::fs::remove_dir(output_dir);
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
    // 测试期关键日志：转码是最耗时的长任务，入口参数与落点必须留痕。
    crate::logging::log_event(
        "转码",
        format!(
            "开始：{name} 模式 {}（目标集数 {:?}，输出 {:?}，产物根目录 {:?}）",
            request.mode,
            request.target_count,
            request.output_dir.as_deref().unwrap_or("缺省"),
            output_root.as_deref().unwrap_or("未设"),
        ),
    );
    let result = tauri::async_runtime::spawn_blocking(move || {
        transcode_blocking(&app, root, name, request, ffprobe_path, output_root)
    })
    .await
    .map_err(|error| format!("转码任务失败：{error}"))?;
    crate::logging::log_event(
        "转码",
        crate::logging::result_text(&result, |report| {
            format!("完成：{} 项（{}）", report.items.len(), report.output_dir)
        }),
    );
    result
}

// ---------------------------------------------------------------------------
// V5 去重端到端验证（技术验证，依赖仓库内 cloud 虚拟环境）
// ---------------------------------------------------------------------------

/// V5 去重验证结果。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DedupDemoReport {
    pub output_dir: String,
    pub final_video: String,
    /// 子进程 stdout 逐行（已带时间戳的 per-episode 日志流）。
    pub log_lines: Vec<String>,
    /// final-manifest.json 原文（全链 sha256 汇总）。
    pub manifest: serde_json::Value,
}

/// 定位开发仓库里的 cloud 虚拟环境 Python（技术验证口径）。
///
/// 从当前工作目录与可执行文件位置逐级向上找 `cloud/.venv/bin/python`
/// （Windows 为 `cloud/.venv/Scripts/python.exe`）。桌面端打包分发不含仓库
/// 环境，找不到时给出中文指引——这是有意为之：该功能只用于技术验证。
pub(crate) fn locate_cloud_python() -> Result<PathBuf, String> {
    const REL: &str = if cfg!(target_os = "windows") {
        "cloud/.venv/Scripts/python.exe"
    } else {
        "cloud/.venv/bin/python"
    };
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            roots.push(dir.to_path_buf());
        }
    }
    for root in roots {
        let mut dir = Some(root);
        while let Some(current) = dir {
            let candidate = current.join(REL);
            if candidate.is_file() {
                return Ok(candidate);
            }
            dir = current.parent().map(Path::to_path_buf);
        }
    }
    Err(
        "未找到 cloud/.venv 虚拟环境 Python。V5 去重验证是技术验证功能，依赖开发仓库目录布局：\
         请先在仓库根目录创建 cloud 虚拟环境。"
            .into(),
    )
}

/// V5 去重验证主流程：拼 CLI 参数 → 跑 demo_dedup.py → 收日志 + 读 manifest。
#[allow(clippy::too_many_arguments)]
fn dedup_demo_blocking(
    app: &AppHandle,
    input: &str,
    output_dir: &str,
    seed: i64,
    series: &str,
    badge: Option<String>,
    watermark: Option<String>,
    time_stretch: &str,
) -> Result<DedupDemoReport, String> {
    let python = locate_cloud_python()?;
    // cloud 源码目录：python 位于 <仓库>/cloud/.venv/bin/python，上溯三级即 cloud。
    let cloud_dir = python
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .ok_or_else(|| format!("无法从 Python 路径推断 cloud 目录：{}", python.display()))?
        .to_path_buf();
    // ffmpeg/ffprobe 与其他工具箱功能同一套解析（设置 → 一键安装目录 → PATH），
    // 并显式作为参数传给子进程——macOS GUI 进程的 PATH 不可靠。
    let ffmpeg = resolve_ffmpeg(app)?;
    let ffprobe = resolve_ffprobe(app, None)?;

    let mut args: Vec<String> = vec![
        "-m".into(),
        "drama_cloud.demo_dedup".into(),
        "--input".into(),
        input.to_string(),
        "--output-dir".into(),
        output_dir.to_string(),
        "--seed".into(),
        seed.to_string(),
        "--series".into(),
        series.to_string(),
        "--time-stretch".into(),
        time_stretch.to_string(),
        "--ffmpeg".into(),
        ffmpeg.display().to_string(),
        "--ffprobe".into(),
        ffprobe.display().to_string(),
    ];
    if let Some(text) = badge
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        args.push("--badge".into());
        args.push(text.to_string());
    }
    if let Some(text) = watermark
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        args.push("--watermark".into());
        args.push(text.to_string());
    }

    let spec = CommandSpec::new(python.display().to_string())
        .args(args)
        .env("PYTHONPATH", cloud_dir.join("src").display().to_string())
        // 两轮编码耗时与片长成正比；30 分钟封顶防挂死。
        .timeout(Duration::from_secs(1800));
    let output = SystemRunner::new()
        .run(&spec)
        .map_err(|error| format!("V5 去重验证进程启动失败：{error}"))?;
    let log_lines: Vec<String> = output
        .stdout
        .lines()
        .map(str::to_string)
        .filter(|line| !line.trim().is_empty())
        .collect();
    if !output.success() {
        let stderr_tail: Vec<&str> = output.stderr.lines().rev().take(6).collect();
        let stderr_tail = stderr_tail.into_iter().rev().collect::<Vec<_>>().join("\n");
        return Err(format!(
            "V5 去重验证失败（退出码 {}）：\n{}",
            output.code_label(),
            stderr_tail
        ));
    }
    let final_video = Path::new(output_dir).join("final-encode.mp4");
    if !final_video.is_file() {
        return Err(format!(
            "进程已成功但未找到成片产物：{}",
            final_video.display()
        ));
    }
    let manifest_path = Path::new(output_dir).join("final-manifest.json");
    let manifest: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&manifest_path)
            .map_err(|error| format!("读取 final-manifest.json 失败：{error}"))?,
    )
    .map_err(|error| format!("解析 final-manifest.json 失败：{error}"))?;
    Ok(DedupDemoReport {
        output_dir: output_dir.to_string(),
        final_video: final_video.display().to_string(),
        log_lines,
        manifest,
    })
}

/// 跑仓库内 cloud 虚拟环境的 V5 两轮去重端到端验证（对照 Grove 逆向文档 §6/§12）。
#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn dedup_demo_execute(
    app: AppHandle,
    input: String,
    output_dir: String,
    seed: i64,
    series: Option<String>,
    badge: Option<String>,
    watermark: Option<String>,
    time_stretch: Option<String>,
) -> Result<DedupDemoReport, String> {
    let input = input.trim().to_string();
    if input.is_empty() {
        return Err("请先填写输入视频路径。".into());
    }
    if !Path::new(&input).is_file() {
        return Err(format!("输入视频不存在：{input}"));
    }
    let output_dir = output_dir.trim().to_string();
    if output_dir.is_empty() {
        return Err("请先填写输出目录。".into());
    }
    let series = series
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "示例剧".into());
    // 非法取值一律回落 auto（前端下拉只发这三个值，这里是兜底）。
    let time_stretch = match time_stretch.as_deref() {
        Some("atempo") | Some("rubberband") | Some("auto") => time_stretch.unwrap().to_string(),
        _ => "auto".to_string(),
    };
    crate::logging::log_event(
        "V5去重",
        format!("开始：{input} seed={seed} → {output_dir}（变调 {time_stretch}）"),
    );
    let result = tauri::async_runtime::spawn_blocking(move || {
        dedup_demo_blocking(
            &app,
            &input,
            &output_dir,
            seed,
            &series,
            badge,
            watermark,
            &time_stretch,
        )
    })
    .await
    .map_err(|error| format!("V5 去重验证任务失败：{error}"))?;
    crate::logging::log_event(
        "V5去重",
        crate::logging::result_text(&result, |report| format!("完成：{}", report.final_video)),
    );
    result
}

// ---------------------------------------------------------------------------
// 一键安装运行环境
// ---------------------------------------------------------------------------

/// 平台 curl：macOS 的 GUI 进程 PATH 干净，必须绝对路径；Windows 10+ 系统自带
/// `curl.exe`（System32），按名即可解析。
#[cfg(target_os = "macos")]
const CURL_PROGRAM: &str = "/usr/bin/curl";
#[cfg(target_os = "windows")]
const CURL_PROGRAM: &str = "curl";

/// macOS 静态 ffmpeg 发行源：evermeet.cx。版本钉死保证可复现；
/// 该站单连接限流（HTTP 429 / 带宽限速），因此**顺序**下载并交给 curl 重试。
/// 注意：该站只提供 Intel（x86_64）构建（官方明确不提供 ARM 原生版），
/// 在 Apple Silicon 上经 Rosetta 2 运行；Rosetta 缺失时自检会拦截并提示安装。
#[cfg(target_os = "macos")]
const EVERMEET_FFMPEG_URL: &str = "https://evermeet.cx/ffmpeg/ffmpeg-9.0.2.zip";
#[cfg(target_os = "macos")]
const EVERMEET_FFPROBE_URL: &str = "https://evermeet.cx/ffmpeg/ffprobe-9.0.2.zip";

/// Windows 发行源：gyan.dev 的 essentials 构建（官方推荐分发包），一个 zip 同时
/// 含 ffmpeg.exe 与 ffprobe.exe，因此 Windows 只需下载一次。这是滚动更新的
/// 「release-essentials」包（钉版 URL 会因官网清理旧包而 404），仅作兜底——
/// 正式安装包由 CI 预置 ffmpeg（见 bundled_runtime_dir），运行时不再联网。
#[cfg(target_os = "windows")]
const GYAN_FFMPEG_URL: &str = "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip";

/// Windows 一键配置的第 3 个组件：官方 CPython 3.11.9（amd64）安装器，约 25MB。
/// 静默装进应用数据目录，不装启动器、不改系统 PATH；升级 Python 大版本时换这里的钉版号。
#[cfg(target_os = "windows")]
const PYTHON_INSTALLER_URL: &str =
    "https://www.python.org/ftp/python/3.11.9/amd64/python-3.11.9-amd64.exe";
/// 静默安装实测 1~2 分钟，给足超时防网络/杀软拖慢时误杀。
#[cfg(target_os = "windows")]
const PYTHON_INSTALL_TIMEOUT_SECS: u64 = 600;

/// 一键安装结果。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallRuntimeReport {
    pub ffmpeg_path: String,
    pub ffprobe_path: String,
    pub skipped_existing: Vec<String>,
    pub note: String,
}

/// 用系统 curl 下载（macOS / Windows 10+ 均自带，支持重试与断点续传）。
/// 期间每 0.4 秒轮询一次落盘大小并上报 `install-progress`；
/// 总大小先用 HEAD 请求探测 content-length，探测不到就只报已下载量。
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn download_file(app: &AppHandle, component: &str, url: &str, target: &Path) -> Result<(), String> {
    let total = probe_content_length(url);
    emit_install_progress(app, component, "connecting", "连接下载源", total, 0);
    let mut command = std::process::Command::new(CURL_PROGRAM);
    // Windows 的 GUI 进程拉起 curl 会弹黑色 cmd 窗口，CREATE_NO_WINDOW 静默运行。
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    command
        .args([
            "-fL",
            "-sS",
            "--retry",
            "4",
            "--retry-delay",
            "10",
            "--retry-all-errors",
            "--max-time",
            "1800",
            "-o",
        ])
        .arg(target.display().to_string())
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("curl 启动失败：{error}"))?;
    loop {
        std::thread::sleep(std::time::Duration::from_millis(400));
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return Ok(());
                }
                // curl 的报错都很短（-sS 只打一行），等它退出后一次性读走不会堵管道。
                let mut detail = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    use std::io::Read;
                    let _ = stderr.read_to_string(&mut detail);
                }
                let first = detail
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .unwrap_or("（无输出）");
                return Err(format!(
                    "下载失败（退出码 {}）：{first}",
                    status.code().unwrap_or(-1)
                ));
            }
            Ok(None) => {
                let downloaded = std::fs::metadata(target)
                    .map(|meta| meta.len())
                    .unwrap_or(0);
                emit_install_progress(app, component, "downloading", "下载中", total, downloaded);
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("curl 运行失败：{error}"));
            }
        }
    }
}

/// HEAD 请求探测下载总大小；失败不影响安装，只是进度条没有百分比。
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn probe_content_length(url: &str) -> Option<u64> {
    let spec = CommandSpec::new(CURL_PROGRAM)
        .arg("-sIL")
        .arg("--max-time")
        .arg("20")
        .arg(url);
    let output = SystemRunner::new().run(&spec).ok()?;
    if !output.success() {
        return None;
    }
    output.combined().lines().rev().find_map(|line| {
        let value = line.strip_prefix("content-length:")?;
        value.trim().parse::<u64>().ok()
    })
}

/// 把 zip 解压到 staging：macOS 用系统 unzip；Windows 用系统自带 bsdtar
/// （`tar.exe` 能直接识别 zip 格式，Windows 10 1803+ 都有）。
#[cfg(target_os = "macos")]
fn extract_zip(zip_path: &Path, staging: &Path) -> Result<(), String> {
    let spec = CommandSpec::new("/usr/bin/unzip")
        .arg("-o")
        .arg("-q")
        .arg(zip_path.display().to_string())
        .arg("-d")
        .arg(staging.display().to_string());
    run_extraction(&spec)
}

#[cfg(target_os = "windows")]
fn extract_zip(zip_path: &Path, staging: &Path) -> Result<(), String> {
    let spec = CommandSpec::new("tar")
        .arg("-xf")
        .arg(zip_path.display().to_string())
        .arg("-C")
        .arg(staging.display().to_string());
    run_extraction(&spec)
}

/// 执行解压命令并把失败原因的第一行带回给界面。
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn run_extraction(spec: &CommandSpec) -> Result<(), String> {
    let runner = SystemRunner::new();
    let output = runner
        .run(spec)
        .map_err(|error| format!("解压器启动失败：{error}"))?;
    if output.success() {
        return Ok(());
    }
    let detail = output.combined();
    let first = detail
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("（无输出）");
    Err(format!("解压失败（{}）：{first}", output.code_label()))
}

/// 在解压目录里找目标可执行文件：macOS 的 zip 是平铺的；gyan 的 zip 多套一层
/// 版本目录（`ffmpeg-x.y.z-essentials_build/bin/ffmpeg.exe`），所以要向下找几层。
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn find_extracted(dir: &Path, file_name: &str, depth: u8) -> Option<PathBuf> {
    let direct = dir.join(file_name);
    if direct.is_file() {
        return Some(direct);
    }
    if depth == 0 {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_extracted(&path, file_name, depth - 1) {
                return Some(found);
            }
        }
    }
    None
}

/// 安装单个工具：下载 zip → 解压到临时目录 → 授权 → 移到目标位置。
/// 返回 `Ok(true)` 表示目标已存在、本次跳过。
/// 仅 macOS 主安装流使用；Windows 走内置复制 + 合包 zip 兜底（见 install_runtime_blocking）。
#[cfg(target_os = "macos")]
fn install_one(app: &AppHandle, bin_dir: &Path, name: &str, url: &str) -> Result<bool, String> {
    let exe_name = dossier_inpaint::erase::binary_name(name);
    let target = bin_dir.join(&exe_name);
    if target.is_file() {
        emit_install_progress(app, name, "finished", "已存在，跳过", Some(0), 0);
        return Ok(true);
    }
    let zip_path = bin_dir.join(format!("{name}.download.zip"));
    let staging = bin_dir.join(format!("{name}.staging"));
    let _ = std::fs::remove_dir_all(&staging);
    let result = (|| -> Result<(), String> {
        download_file(app, name, url, &zip_path)?;
        emit_install_progress(app, name, "extracting", "解压中", None, 0);
        std::fs::create_dir_all(&staging).map_err(|error| format!("创建解压目录失败：{error}"))?;
        extract_zip(&zip_path, &staging)?;
        let extracted = find_extracted(&staging, &exe_name, 3)
            .ok_or_else(|| format!("压缩包里没有找到 {exe_name} 可执行文件"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&extracted, std::fs::Permissions::from_mode(0o755))
                .map_err(|error| format!("设置可执行权限失败：{error}"))?;
        }
        std::fs::rename(&extracted, &target)
            .map_err(|error| format!("安装 {name} 失败：{error}"))?;
        emit_install_progress(app, name, "finished", "完成", Some(0), 0);
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::fs::remove_file(&zip_path);
    result.map(|_| false)
}

/// 安装后自检：把「下载成功但跑不起来」拦在报告给用户之前。
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn self_check_installed(app: &AppHandle, ffmpeg_path: &Path) -> Result<(), String> {
    emit_install_progress(app, "自检", "verifying", "检查安装结果", None, 0);
    let runner = SystemRunner::new();
    let report = probe_tool(
        &runner,
        "ffmpeg",
        ffmpeg_path,
        &["-hide_banner", "-version"],
    );
    if report.ok {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    if report.detail.contains("Bad CPU type") {
        return Err("安装完成但自检失败：CPU 架构不匹配（下载的是 Intel 版，需要 Rosetta 2）。请先在终端执行「softwareupdate --install-rosetta --agree-to-license」安装 Rosetta，再重新点击一键配置。".into());
    }
    Err(format!("安装完成但自检失败：{}", report.detail))
}

/// 把工具路径写回设置：只在用户没有显式配置（或配置失效）时写入，避免覆盖手工配置。
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn persist_tool_paths(
    app: &AppHandle,
    ffmpeg_path: &Path,
    ffprobe_path: &Path,
) -> Result<(), String> {
    let settings_path = crate::settings_path(app)?;
    let mut settings = load_settings(&settings_path).settings;
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
        settings.ffmpeg_path = Some(ffmpeg_path.to_path_buf());
    }
    if needs_ffprobe {
        settings.ffprobe_path = Some(ffprobe_path.to_path_buf());
    }
    if needs_ffmpeg || needs_ffprobe {
        dossier_core::save_settings(&settings_path, &settings)
            .map_err(|error| format!("写入设置失败：{error}"))?;
    }
    Ok(())
}

/// 构造官方安装器的静默安装参数：装进应用私有目录、不动系统 PATH、不装启动器，
/// 对用户零打扰。升级 Python 大版本时这里的开关要一起核对。
#[cfg(target_os = "windows")]
fn python_install_args(target_dir: &Path) -> Vec<String> {
    vec![
        "/quiet".into(),
        "/norestart".into(),
        "InstallAllUsers=0".into(),
        format!("TargetDir={}", target_dir.display()),
        "Include_launcher=0".into(),
        "PrependPath=0".into(),
        "AssociateFiles=0".into(),
        "Shortcuts=0".into(),
        "Include_doc=0".into(),
        "Include_test=0".into(),
    ]
}

/// 把装好的 Python 路径写回设置。走到这里时 `configured_python` 已确认没有可用配置，
/// 直接写不会覆盖用户手工配置。
#[cfg(target_os = "windows")]
fn persist_python_path(app: &AppHandle, python_exe: &Path) -> Result<(), String> {
    let settings_path = crate::settings_path(app)?;
    let mut settings = load_settings(&settings_path).settings;
    settings.python_path = Some(python_exe.to_path_buf());
    dossier_core::save_settings(&settings_path, &settings)
        .map_err(|error| format!("写入设置失败：{error}"))
}

/// Windows：一键配置的第 3 步——确保有可用的 Python 解释器（AI 擦除的 worker 用）。
/// 返回 `Ok(true)` 表示本次新装了 Python；`Ok(false)` 表示已有可用解释器、本轮跳过。
/// 失败时返回 Err，由调用方决定是否软处理（Python 缺失不影响转码）。
#[cfg(target_os = "windows")]
fn install_python_windows(app: &AppHandle, bin_dir: &Path) -> Result<bool, String> {
    // 1) 设置里已配置且可用（一键配置重入、或上轮装过）→ 跳过。
    if configured_python(app).is_some() {
        emit_install_progress(app, "python", "finished", "已存在，跳过", Some(0), 0);
        return Ok(false);
    }
    // 2) PATH 上有真实解释器（探测会拦掉微软商店占位符的 9009）→ 不必重复安装。
    let runner = SystemRunner::new();
    if probe_python(app, &runner).ok {
        emit_install_progress(
            app,
            "python",
            "finished",
            "系统已有 Python，跳过安装",
            Some(0),
            0,
        );
        return Ok(false);
    }
    // 3) 下载官方安装器（~25MB）→ 静默安装到应用数据目录 → 自检 → 写回设置。
    let target_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("无法定位应用数据目录：{error}。请检查系统权限。"))?
        .join("python");
    let python_exe = target_dir.join("python.exe");
    let installer_path = bin_dir.join("python-installer.exe");
    let result = (|| -> Result<(), String> {
        download_file(app, "python", PYTHON_INSTALLER_URL, &installer_path)?;
        emit_install_progress(
            app,
            "python",
            "installing",
            "正在静默安装（1~2 分钟）",
            None,
            0,
        );
        let spec = CommandSpec::new(installer_path.display().to_string())
            .args(python_install_args(&target_dir))
            .timeout(std::time::Duration::from_secs(PYTHON_INSTALL_TIMEOUT_SECS));
        let output = runner
            .run(&spec)
            .map_err(|error| format!("安装器启动失败：{error}"))?;
        if !output.success() {
            let combined = output.combined();
            let detail = combined
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("（无输出）");
            return Err(format!("静默安装失败（{}）：{detail}", output.code_label()));
        }
        // 安装器返回 0 不完全等于「解释器能用」，以直接自检为准。
        let report = probe_tool(&runner, "python", &python_exe, &["--version"]);
        if !report.ok {
            return Err(format!("安装后自检失败：{}", report.detail));
        }
        persist_python_path(app, &python_exe)?;
        Ok(())
    })();
    // 安装器是一次性文件，无论成败都清掉，不在 bin 目录留 ~25MB 垃圾。
    let _ = std::fs::remove_file(&installer_path);
    match result {
        Ok(()) => {
            emit_install_progress(app, "python", "finished", "完成", Some(0), 0);
            crate::logging::log_event(
                "环境",
                format!("Python 自动安装成功：{}", python_exe.display()),
            );
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

/// 安装包内置的运行环境目录：Windows 安装包由 CI 预置 ffmpeg/ffprobe
/// （见 .github/workflows/build-windows.yml），装好后**运行时零下载**。
/// Tauri v2 的 resources 按「相对 tauri.conf.json 的路径」原样落盘：
/// 声明 `resources/ffmpeg/*` 实际装在 `<resource_dir>/resources/ffmpeg/`；
/// 兼容再查一层 `<resource_dir>/ffmpeg`，防打包器行为差异。
/// 源码直跑或未内置（mac DMG）时返回 None，调用方退回下载流程。
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn bundled_runtime_dir(app: &AppHandle) -> Option<PathBuf> {
    use tauri::Manager;
    let root = app.path().resource_dir().ok()?;
    let exe_name = dossier_inpaint::erase::binary_name("ffmpeg");
    let dir = [root.join("resources").join("ffmpeg"), root.join("ffmpeg")]
        .into_iter()
        .find(|dir| dir.join(&exe_name).is_file());
    if dir.is_none() {
        crate::logging::log_event(
            "环境",
            format!("安装包内置运行环境未找到（已尝试 {}\\resources\\ffmpeg 与 {}\\ffmpeg），退回在线下载", root.display(), root.display()),
        );
    }
    dir
}

/// 从安装包内置资源把工具复制到 bin 目录；返回实际安装的组件名。
/// 复制虽是本地 IO，仍逐组件播报「正在安装 → 完成」进度事件，
/// 让安装弹窗有可见的推进（内置包约 200MB，复制肉眼可感知）。
/// 目标已存在且大小一致时跳过，避免每次点击都整块复制。
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn copy_bundled_tools(
    app: &AppHandle,
    bin_dir: &Path,
    bundled: &Path,
    names: &[&str],
) -> Vec<String> {
    let mut installed = Vec::new();
    for name in names {
        let exe_name = dossier_inpaint::erase::binary_name(name);
        let Some(source) = std::fs::metadata(bundled.join(&exe_name))
            .ok()
            .filter(|meta| meta.is_file())
            .map(|meta| (bundled.join(&exe_name), meta.len()))
        else {
            continue;
        };
        let target = bin_dir.join(&exe_name);
        let stale = std::fs::metadata(&target)
            .map(|meta| meta.len() != source.1)
            .unwrap_or(true);
        if stale {
            emit_install_progress(app, name, "installing", "正在安装", None, 0);
            if let Err(_error) = std::fs::copy(&source.0, &target) {
                // 单个复制失败不致命：可能是目标正被占用，交给下载流程或自检去暴露。
                continue;
            }
            emit_install_progress(app, name, "finished", "完成", Some(0), 0);
            installed.push(name.to_string());
        }
    }
    installed
}

#[cfg(target_os = "macos")]
fn install_runtime_blocking(app: &AppHandle) -> Result<InstallRuntimeReport, String> {
    let bin_dir = app_bin_dir(app)?;
    std::fs::create_dir_all(&bin_dir).map_err(|error| format!("创建运行环境目录失败：{error}"))?;

    // 优先用安装包内置的 ffmpeg（未来 mac 包也可能预置），没有再走下载。
    let mut bundled_note = String::new();
    let mut skipped_existing = Vec::new();
    if let Some(bundled) = bundled_runtime_dir(app) {
        let copied = copy_bundled_tools(app, &bin_dir, &bundled, &["ffmpeg", "ffprobe"]);
        if !copied.is_empty() {
            bundled_note = format!("已从安装包内置资源安装：{}", copied.join("、"));
        }
    }
    if install_one(app, &bin_dir, "ffmpeg", EVERMEET_FFMPEG_URL)? {
        skipped_existing.push("ffmpeg".into());
    }
    if install_one(app, &bin_dir, "ffprobe", EVERMEET_FFPROBE_URL)? {
        skipped_existing.push("ffprobe".into());
    }

    let ffmpeg_path = bin_dir.join("ffmpeg");
    let ffprobe_path = bin_dir.join("ffprobe");

    self_check_installed(app, &ffmpeg_path)?;
    persist_tool_paths(app, &ffmpeg_path, &ffprobe_path)?;

    Ok(InstallRuntimeReport {
        ffmpeg_path: ffmpeg_path.display().to_string(),
        ffprobe_path: ffprobe_path.display().to_string(),
        skipped_existing,
        note: format!(
            "运行环境已就绪（安装目录：{}）{bundled_note}",
            bin_dir.display()
        ),
    })
}

/// Windows：优先复制安装包内置的 ffmpeg/ffprobe（CI 预置，零下载）；
/// 内置缺失（如旧版安装包）才退回 gyan.dev 合包 zip 下载。目标都齐了直接自检收尾。
#[cfg(target_os = "windows")]
fn install_runtime_blocking(app: &AppHandle) -> Result<InstallRuntimeReport, String> {
    let bin_dir = app_bin_dir(app)?;
    std::fs::create_dir_all(&bin_dir).map_err(|error| format!("创建运行环境目录失败：{error}"))?;

    let ffmpeg_path = bin_dir.join("ffmpeg.exe");
    let ffprobe_path = bin_dir.join("ffprobe.exe");
    let mut skipped_existing = Vec::new();
    let mut bundled_note = String::new();

    if !(ffmpeg_path.is_file() && ffprobe_path.is_file()) {
        if let Some(bundled) = bundled_runtime_dir(app) {
            let copied = copy_bundled_tools(app, &bin_dir, &bundled, &["ffmpeg", "ffprobe"]);
            if !copied.is_empty() {
                bundled_note = format!("（已从安装包内置资源安装：{}）", copied.join("、"));
            }
        }
    }
    if ffmpeg_path.is_file() && ffprobe_path.is_file() {
        if bundled_note.is_empty() {
            // 两个工具本来就在（本轮既没下载也没复制）：报告里如实标注为跳过。
            skipped_existing.push("ffmpeg".into());
            skipped_existing.push("ffprobe".into());
        }
        // 都齐了（可能刚从内置资源复制）就不动网络，直接自检收尾。
    } else {
        let zip_path = bin_dir.join("ffmpeg.download.zip");
        let staging = bin_dir.join("ffmpeg.staging");
        let _ = std::fs::remove_dir_all(&staging);
        let result = (|| -> Result<(), String> {
            download_file(app, "ffmpeg", GYAN_FFMPEG_URL, &zip_path)?;
            emit_install_progress(app, "ffmpeg", "extracting", "解压中", None, 0);
            std::fs::create_dir_all(&staging)
                .map_err(|error| format!("创建解压目录失败：{error}"))?;
            extract_zip(&zip_path, &staging)?;
            for (file_name, target) in
                [("ffmpeg.exe", &ffmpeg_path), ("ffprobe.exe", &ffprobe_path)]
            {
                let extracted = find_extracted(&staging, file_name, 3)
                    .ok_or_else(|| format!("压缩包里没有找到 {file_name}"))?;
                std::fs::rename(&extracted, target)
                    .map_err(|error| format!("安装 {file_name} 失败：{error}"))?;
            }
            emit_install_progress(app, "ffmpeg", "finished", "完成", Some(0), 0);
            emit_install_progress(app, "ffprobe", "finished", "完成", Some(0), 0);
            Ok(())
        })();
        let _ = std::fs::remove_dir_all(&staging);
        let _ = std::fs::remove_file(&zip_path);
        result?;
    }

    self_check_installed(app, &ffmpeg_path)?;
    persist_tool_paths(app, &ffmpeg_path, &ffprobe_path)?;

    // Python 是 AI 擦除的依赖，不是转码的：装失败只追加提示，不拖垮 ffmpeg 的成果。
    let mut python_note = String::new();
    match install_python_windows(app, &bin_dir) {
        Ok(true) => {
            python_note = "（已自动安装 Python 3.11 到应用数据目录，供 AI 擦除使用）".into()
        }
        Ok(false) => skipped_existing.push("python".into()),
        Err(error) => {
            python_note = format!("（Python 自动安装未成功：{error}）");
            crate::logging::log_event("环境", format!("Python 自动安装失败：{error}"));
        }
    }

    Ok(InstallRuntimeReport {
        ffmpeg_path: ffmpeg_path.display().to_string(),
        ffprobe_path: ffprobe_path.display().to_string(),
        skipped_existing,
        note: format!(
            "运行环境已就绪（安装目录：{}）{bundled_note}{python_note}",
            bin_dir.display()
        ),
    })
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn install_runtime_blocking(_app: &AppHandle) -> Result<InstallRuntimeReport, String> {
    Err("运行环境自动安装目前仅支持 macOS / Windows；请手动安装 ffmpeg（含 ffprobe）。".to_string())
}

#[tauri::command]
pub async fn install_runtime(app: AppHandle) -> Result<InstallRuntimeReport, String> {
    // 测试期关键日志：一键配置运行环境（下载 ~25MB）是最关键的修复入口。
    crate::logging::log_event("环境", "开始一键安装运行环境");
    let result = tauri::async_runtime::spawn_blocking(move || install_runtime_blocking(&app))
        .await
        .map_err(|error| format!("运行环境安装任务失败：{error}"))?;
    crate::logging::log_event(
        "环境",
        crate::logging::result_text(&result, |report| {
            format!(
                "安装完成：{} / {}（已存在跳过：{:?}）",
                report.ffmpeg_path, report.ffprobe_path, report.skipped_existing
            )
        }),
    );
    result
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
    fn representative_episodes_samples_uniformly_and_reuses() {
        // 10 集抽 4 个：应命中首、约 1/3、约 2/3、末位，且不重复。
        let episodes: Vec<ProbedEpisode> = (0..10)
            .map(|index| probe_at(&format!("/a/第{index:02}集.mp4"), 60.0 + index as f64))
            .collect();
        let picks = representative_episodes(&episodes, 4);
        assert_eq!(picks.len(), 4);
        let indexes: Vec<usize> = picks
            .iter()
            .map(|(path, _)| {
                episodes
                    .iter()
                    .position(|e| e.file.path == *path)
                    .expect("抽中的路径必须来自原始素材")
            })
            .collect();
        assert_eq!(indexes, vec![0, 3, 6, 9]);
        // 时长跟集走，供抽帧命令换算中点。
        assert!((picks[3].1 - 69.0).abs() < 1e-9);

        // 2 集抽 4 个：不足时循环复用，数量仍然凑满。
        let two = vec![
            probe_at("/a/第01集.mp4", 60.0),
            probe_at("/a/第02集.mp4", 61.0),
        ];
        let picks = representative_episodes(&two, 4);
        assert_eq!(picks.len(), 4);
        assert_eq!(picks[0].0, two[0].file.path);
        assert_eq!(picks[1].0, two[1].file.path);
        assert_eq!(picks[2].0, two[0].file.path);

        // 空素材直接返回空，调用方据此跳过过程图。
        assert!(representative_episodes(&[], 4).is_empty());
    }

    #[test]
    fn local_date_string_is_valid_iso_date() {
        let date = local_date_string();
        // 格式：YYYY-MM-DD，各段都是数字。
        assert_eq!(date.len(), 10, "格式必须是 YYYY-MM-DD：{date}");
        let bytes = date.as_bytes();
        for (index, byte) in bytes.iter().enumerate() {
            if index == 4 || index == 7 {
                assert_eq!(byte, &b'-', "分隔符位置错：{date}");
            } else {
                assert!(byte.is_ascii_digit(), "非数字字符：{date}");
            }
        }
        // 东八区口径下年份必然落在 2020~2100，防 civil_from_days 写错量级。
        let year: i64 = date[..4].parse().expect("年份可解析");
        assert!((2020..=2100).contains(&year), "年份异常：{date}");
        let month: u32 = date[5..7].parse().expect("月份可解析");
        let day: u32 = date[8..10].parse().expect("日可解析");
        assert!((1..=12).contains(&month), "月份越界：{date}");
        assert!((1..=31).contains(&day), "日越界：{date}");
    }

    #[test]
    fn local_cover_and_synopsis_discovery_follows_checklist_names() {
        let dir =
            std::env::temp_dir().join(format!("dossier-test-materials-{}", uuid_like_suffix()));
        std::fs::create_dir_all(&dir).expect("创建临时素材目录");
        // 非候选文件不应被选中：视频、无关名字的图片。
        std::fs::write(dir.join("第01集.mp4"), b"x").unwrap();
        std::fs::write(dir.join("剧照.jpg"), b"x").unwrap();
        // 大小写不敏感的 cover 命中；按文件名排序取第一个（ASCII 字节序在中文之前）。
        std::fs::write(dir.join("Cover.PNG"), b"png").unwrap();
        std::fs::write(dir.join("封面.jpg"), b"jpg").unwrap();
        // 简介命中；「说明.txt」不含关键词，不应干扰。
        std::fs::write(dir.join("简介.txt"), "\n  一段简介正文。\n").unwrap();
        std::fs::write(dir.join("说明.txt"), "不是简介").unwrap();

        let cover = find_local_cover(&dir).expect("应找到封面");
        assert_eq!(cover.file_name().unwrap().to_str(), Some("Cover.PNG"));
        let synopsis = read_local_synopsis(&dir).expect("应读到简介");
        assert_eq!(synopsis, "一段简介正文。");

        // 复制封面到输出目录：扩展名统一小写「剧目海报.png」。
        let out_dir = dir.join("输出");
        let copied = copy_local_cover(&dir, &out_dir).expect("应复制封面");
        assert!(copied.is_file());
        assert_eq!(copied.file_name().unwrap().to_str(), Some("剧目海报.png"));

        // 空目录：全都返回 None。
        let empty = dir.join("空");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(find_local_cover(&empty).is_none());
        assert!(read_local_synopsis(&empty).is_none());
        assert!(copy_local_cover(&empty, &out_dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 测试专用的唯一后缀（避免引入 uuid 依赖，时间戳 + 计数足够）。
    fn uuid_like_suffix() -> u128 {
        use std::sync::atomic::AtomicU64;
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        (std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos())
        .wrapping_add(count as u128)
    }

    #[test]
    fn ffmpeg_failure_surfaces_first_stderr_line() {
        // 用不存在的程序名触发启动失败路径，确认错误信息里带“ffmpeg 启动失败”。
        let mut spec = CommandSpec::new("ffmpeg-不存在的程序");
        spec = spec.arg("-version");
        let runner = SystemRunner::new();
        assert!(runner.run(&spec).is_err());
    }

    /// Windows 一键配置的 Python 静默参数：必须安静、必须落在指定目录、
    /// 必须不碰系统 PATH / 启动器（对用户零打扰的关键约束）。
    #[cfg(target_os = "windows")]
    #[test]
    fn python_install_args_point_at_target_dir_and_stay_quiet() {
        let args = python_install_args(Path::new(r"C:\Apps\Dossier\python"));
        assert!(
            args.contains(&"/quiet".to_string()),
            "缺少 /quiet：{args:?}"
        );
        assert!(args.contains(&"/norestart".to_string()));
        assert!(
            args.contains(&"TargetDir=C:\\Apps\\Dossier\\python".to_string()),
            "TargetDir 拼错：{args:?}"
        );
        assert!(args.contains(&"InstallAllUsers=0".to_string()));
        assert!(
            args.contains(&"PrependPath=0".to_string()),
            "绝不能改系统 PATH"
        );
        assert!(args.contains(&"Include_launcher=0".to_string()));
    }
}
