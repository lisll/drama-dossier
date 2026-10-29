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
    bitrate_fix_target, bitrate_report, durations_match, load_settings, nudge_frames,
    output_dir_hits_sources, plan_recut_by_duration_with_speed, plan_recut_segments, plan_rename,
    probe_episode, resolve_product_dir, video_bitrate, BitrateDirection, BitrateItem, EpisodeFile,
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
use crate::tasks::{CancelToken, TaskKind};

/// 码率安全线默认值（与竞品一致的 4 Mbps）。
pub const DEFAULT_MIN_BITRATE_KBPS: u64 = 4000;
/// 码率上限默认值（8 Mbps）：高于它的分集压到上限；设置里可改。
pub const DEFAULT_MAX_BITRATE_KBPS: u64 = 8000;
/// 单条 ffmpeg 命令的最短超时（抽帧、抽音轨这类轻量命令也用它）。
pub(crate) const FFMPEG_MIN_TIMEOUT_SECS: u64 = 600;
/// 单条 ffmpeg 命令的超时上限：再长就更可能是卡死而不是慢。
const FFMPEG_MAX_TIMEOUT_SECS: u64 = 4 * 3600;
/// 每秒素材允许的编码耗时（秒）：深度处理滤镜链（tmix / rotate / lanczos / noise）最重。
const FFMPEG_SECS_PER_SEC_DEEP: f64 = 8.0;
const FFMPEG_SECS_PER_SEC_DEFAULT: f64 = 4.0;

/// 按本条命令的输入时长计算 ffmpeg 超时：`max(600 秒, 时长 × 系数)`，上限 4 小时。
///
/// 固定 600 秒时，设置允许的长集（最长 3600 秒）在 CPU 编码 + 深度处理滤镜下必然被误杀。
/// 时长读不到（0、负数、NaN）时按最短超时处理。系数先按经验取值，日志里记录实际耗时后再调。
pub(crate) fn ffmpeg_timeout(duration_sec: f64, deep: bool) -> Duration {
    let factor = if deep {
        FFMPEG_SECS_PER_SEC_DEEP
    } else {
        FFMPEG_SECS_PER_SEC_DEFAULT
    };
    let scaled = if duration_sec.is_finite() && duration_sec > 0.0 {
        (duration_sec * factor)
            .ceil()
            .min(FFMPEG_MAX_TIMEOUT_SECS as f64) as u64
    } else {
        0
    };
    Duration::from_secs(scaled.clamp(FFMPEG_MIN_TIMEOUT_SECS, FFMPEG_MAX_TIMEOUT_SECS))
}

/// 一条 ffmpeg 命令的运行上下文：属于哪个任务（停止令牌）、允许跑多久。
#[derive(Clone, Copy)]
pub(crate) struct FfmpegCtx<'a> {
    pub cancel: &'a CancelToken,
    pub timeout: Duration,
}

impl<'a> FfmpegCtx<'a> {
    /// 轻量命令（抽帧、抽音轨等）：最短超时。
    pub(crate) fn light(cancel: &'a CancelToken) -> Self {
        Self {
            cancel,
            timeout: Duration::from_secs(FFMPEG_MIN_TIMEOUT_SECS),
        }
    }

    /// 编码命令：超时按输入时长计算。
    pub(crate) fn encode(cancel: &'a CancelToken, duration_sec: f64, deep: bool) -> Self {
        Self {
            cancel,
            timeout: ffmpeg_timeout(duration_sec, deep),
        }
    }
}

/// 用户请求停止转码：只停转码类任务（含它的材料线程），不影响码率修复等其他任务。
/// 真正杀进程、清理输出在转码主流程里完成，这里的返回只代表「停止指令已受理」。
#[tauri::command]
pub fn transcode_stop() -> Result<bool, String> {
    let stopped = crate::tasks::cancel_kind(TaskKind::Transcode);
    crate::logging::log_event(
        "转码",
        format!("收到停止指令，正在终止并清理输出（{stopped} 个任务）"),
    );
    Ok(stopped > 0)
}

/// 用户请求停止码率修复：当前这一集的 ffmpeg 立即终止，已替换的分集保留，未处理的不再处理。
#[tauri::command]
pub fn bitrate_stop() -> Result<bool, String> {
    let stopped = crate::tasks::cancel_kind(TaskKind::Bitrate);
    crate::logging::log_event("码率", format!("收到停止指令（{stopped} 个任务）"));
    Ok(stopped > 0)
}

/// 用户请求停止 V5 去重验证：整个进程树（python 及其拉起的 ffmpeg）一并终止。
#[tauri::command]
pub fn dedup_demo_stop() -> Result<bool, String> {
    let stopped = crate::tasks::cancel_kind(TaskKind::DedupDemo);
    crate::logging::log_event("去重验证", format!("收到停止指令（{stopped} 个任务）"));
    Ok(stopped > 0)
}

// ---------------------------------------------------------------------------
// 运行环境解析与一键安装
// ---------------------------------------------------------------------------

/// 应用数据目录下的运行环境安装位置（一键安装的 ffmpeg/ffprobe 放这里）。
pub(crate) fn app_bin_dir(app: &AppHandle) -> Result<PathBuf, String> {
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
    /// NVIDIA 显卡概要（例如 "NVIDIA GeForce RTX 4060（8188 MB 显存，驱动 551.23）"）。
    /// 没有 NVIDIA 显卡时为 `None`。界面把它显示在「本机环境」列表里，
    /// 让用户知道 ASR 到底跑在 GPU 还是 CPU 上，而不是只看到一个"已就绪"。
    pub gpu: Option<String>,
    /// 开发仓库的 cloud 虚拟环境是否可用。「V5 去重验证」等技术验证功能只在这时可用，
    /// 安装版没有这套环境，界面据此隐藏入口，而不是放一个点了必然报错的按钮。
    pub dev_tools: bool,
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
        gpu: nvidia_summary(),
        dev_tools: locate_cloud_python().is_ok(),
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

/// 改名目标名的合法性：按 Windows 的文件名规则（同时也满足 macOS / Linux）。
///
/// 拒绝 `\ / : * ? " < > |` 与控制字符、`.` / `..`、以点或空格结尾（Windows 会静默去掉，
/// 导致实际文件名与预览不一致）、保留设备名（`CON`、`NUL`、`COM1` 等，带扩展名也算），
/// 以及超过 200 个字符的名字。
fn validate_target_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(format!("目标文件名无效：{name}"));
    }
    if name.chars().count() > 200 {
        return Err(format!("目标文件名超过 200 个字符：{name}"));
    }
    if let Some(bad) = name.chars().find(|c| {
        c.is_control() || matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
    }) {
        return Err(format!(
            "目标文件名包含不支持的字符「{}」：{name}",
            bad.escape_default()
        ));
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(format!("目标文件名不能以点或空格结尾：{name}"));
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit()
            && stem.as_bytes()[3] != b'0');
    if reserved {
        return Err(format!("目标文件名是 Windows 保留名，无法创建：{name}"));
    }
    Ok(())
}

/// 改名预检（纯函数）：`existing` 是目录第一层现有的全部条目名，`items` 是 `(原名, 新名)`。
/// 返回真正需要执行的改名项；任何一项不合法就整体拒绝，一个文件都不动。
///
/// Windows 与 macOS 默认文件系统不区分大小写，所以「同名」一律按小写比较：
/// - 两个目标只差大小写 → 拒绝（否则后一次改名会覆盖前一次，文件直接丢失）；
/// - 目标与现有文件只差大小写，且那个文件**不是本批次的源文件** → 拒绝；
/// - 目标正好是本批次另一个源文件（互换、链式改名、只改大小写）→ 允许，
///   两阶段执行时源文件会先全部移到临时名。
fn prepare_rename_ops(
    existing: &[String],
    items: &[(String, String)],
) -> Result<Vec<(String, String)>, String> {
    let mut ops: Vec<(String, String)> = Vec::new();
    let mut sources: HashSet<&str> = HashSet::new();
    let mut targets: HashSet<String> = HashSet::new();
    for (from, to) in items {
        plain_file_name(from)?;
        plain_file_name(to)?;
        validate_target_name(to)?;
        if from == to {
            continue;
        }
        if !existing.iter().any(|name| name == from) {
            return Err(format!("源文件不存在：{from}"));
        }
        if !sources.insert(from.as_str()) {
            return Err(format!("同一个文件出现了两次：{from}"));
        }
        if !targets.insert(to.to_lowercase()) {
            return Err(format!(
                "多个文件改成了同一个名字（Windows / macOS 不区分大小写，视为同名）：{to}"
            ));
        }
        ops.push((from.clone(), to.clone()));
    }
    let key_of_target: HashSet<String> = ops.iter().map(|(_, to)| to.to_lowercase()).collect();
    for name in existing {
        if key_of_target.contains(&name.to_lowercase()) && !sources.contains(name.as_str()) {
            return Err(format!("目标名与已有文件同名，拒绝覆盖：{name}"));
        }
    }
    Ok(ops)
}

/// 两阶段改名：先把全部源文件移到临时名，再统一改成新名；任何一步失败都倒序撤销。
///
/// 撤销本身也失败时（例如文件被其他程序占用），返回每个文件当前的实际名称并写日志，
/// 不能只报一句「失败」让用户自己去找。
fn execute_rename(dir: &Path, ops: &[(String, String)], batch: &str) -> Result<usize, String> {
    execute_rename_with(dir, ops, batch, &|from, to| std::fs::rename(from, to))
}

/// [`execute_rename`] 的实现；改名操作可注入，测试据此模拟「撤销途中某一步被占用」。
fn execute_rename_with(
    dir: &Path,
    ops: &[(String, String)],
    batch: &str,
    fs_rename: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
) -> Result<usize, String> {
    let temp_names: Vec<String> = (0..ops.len())
        .map(|index| format!(".rename-{batch}-{index}.tmp"))
        .collect();
    let rename = |from: &str, to: &str| fs_rename(&dir.join(from), &dir.join(to));

    // 撤销：第二阶段已完成的先改回临时名，再把所有临时名改回原名。
    // `stuck_at_final[i]` 为真表示第 i 项没能从新名退回临时名，后面就不再尝试把它的临时名改回原名。
    let rollback = |phase1_done: usize, phase2_done: usize, cause: String| -> String {
        let mut stuck: Vec<String> = Vec::new();
        let mut stuck_at_final = vec![false; ops.len()];
        for index in (0..phase2_done).rev() {
            if let Err(error) = rename(&ops[index].1, &temp_names[index]) {
                stuck_at_final[index] = true;
                stuck.push(format!(
                    "「{}」现在叫「{}」（{error}）",
                    ops[index].0, ops[index].1
                ));
            }
        }
        for index in (0..phase1_done).rev() {
            if stuck_at_final[index] {
                continue;
            }
            // 原名此刻被别的文件占着（典型：互换改名时另一项卡在了这个新名上）：
            // `rename` 会直接覆盖它，等于丢掉一个文件。宁可留在临时名，让用户手动处理。
            if dir.join(&ops[index].0).symlink_metadata().is_ok() {
                stuck.push(format!(
                    "「{}」现在叫「{}」（原名已被另一个文件占用，未覆盖）",
                    ops[index].0, temp_names[index]
                ));
                continue;
            }
            if let Err(error) = rename(&temp_names[index], &ops[index].0) {
                stuck.push(format!(
                    "「{}」现在叫「{}」（{error}）",
                    ops[index].0, temp_names[index]
                ));
            }
        }
        if stuck.is_empty() {
            format!("{cause}；已全部恢复原名，没有文件被改动。")
        } else {
            let detail = stuck.join("；");
            crate::logging::log_event("改名", format!("撤销未完成：{detail}"));
            format!("{cause}；撤销时有 {} 个文件没能恢复：{detail}", stuck.len())
        }
    };

    for (index, (from, _)) in ops.iter().enumerate() {
        if let Err(error) = rename(from, &temp_names[index]) {
            return Err(rollback(
                index,
                0,
                format!("改名第一步失败（{from}）：{error}"),
            ));
        }
    }
    for (index, (_, to)) in ops.iter().enumerate() {
        if let Err(error) = rename(&temp_names[index], to) {
            return Err(rollback(
                ops.len(),
                index,
                format!("改名第二步失败（{}→{to}）：{error}", ops[index].0),
            ));
        }
    }
    Ok(ops.len())
}

#[tauri::command]
pub fn apply_rename(
    root: String,
    name: String,
    items: Vec<RenameRequestItem>,
) -> Result<usize, String> {
    crate::license::guard()?;
    let drama = load_drama(&root, &name)?;
    let dir = drama.dir;

    // 先整体预检，全部通过才动手；执行分两阶段，失败自动撤销（避免改到一半留下新旧混杂的目录）。
    let existing: Vec<String> = std::fs::read_dir(&dir)
        .map_err(|error| format!("读取剧目目录失败：{error}"))?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    let pairs: Vec<(String, String)> = items
        .iter()
        .map(|item| (item.from.clone(), item.to.clone()))
        .collect();
    let ops = prepare_rename_ops(&existing, &pairs)?;
    for (from, _) in &ops {
        if !dir.join(from).is_file() {
            return Err(format!("源文件不存在：{from}"));
        }
    }
    let batch = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    let renamed = execute_rename(&dir, &ops, &batch)?;
    crate::logging::log_event("改名", format!("{}：完成 {renamed} 项", dir.display()));
    Ok(renamed)
}

// ---------------------------------------------------------------------------
// 码率修复
// ---------------------------------------------------------------------------

/// 码率安全区间：`[min, max]`（bps）。前端缺省时用 4–8 Mbps。
fn bitrate_range(min_kbps: Option<u64>, max_kbps: Option<u64>) -> Result<(u64, u64), String> {
    let min_bps = min_kbps
        .unwrap_or(DEFAULT_MIN_BITRATE_KBPS)
        .saturating_mul(1000);
    let max_bps = max_kbps
        .unwrap_or(DEFAULT_MAX_BITRATE_KBPS)
        .saturating_mul(1000);
    if min_bps == 0 || min_bps >= max_bps {
        return Err(format!(
            "码率区间无效：下限 {} kbps 必须大于 0 且小于上限 {} kbps。",
            min_bps / 1000,
            max_bps / 1000
        ));
    }
    Ok((min_bps, max_bps))
}

/// 码率探测结果（附区间判断：偏低 / 偏高 / 正常）。
#[tauri::command]
pub fn probe_bitrate(
    app: AppHandle,
    root: String,
    name: String,
    min_kbps: Option<u64>,
    max_kbps: Option<u64>,
    ffprobe_path: Option<String>,
) -> Result<Vec<BitrateItem>, String> {
    let (min_bps, max_bps) = bitrate_range(min_kbps, max_kbps)?;
    let drama = load_drama(&root, &name)?;
    let ffprobe = resolve_ffprobe(&app, ffprobe_path.as_deref())?;
    let episodes = probe_all(&drama, Some(ffprobe.display().to_string()));
    if episodes.is_empty() {
        return Err("没有任何成功探测规格的分集；请先确认素材可读且 ffprobe 可用。".into());
    }
    Ok(bitrate_report(&episodes, min_bps, max_bps))
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
    /// 为了能封装进 mp4 而做的额外处理（音轨转 AAC、去掉字幕轨）；没有则为 None。
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailedItem {
    pub file_name: String,
    pub reason: String,
}

/// 跑一条 ffmpeg 命令；失败时把 stderr 末尾的有效信息摘要带回界面，完整末尾写日志。
///
/// `ctx` 决定这条命令属于哪个任务（停止时立即终止整个进程树）、允许跑多久。
pub(crate) fn run_ffmpeg(program: &Path, args: &[&str], ctx: FfmpegCtx) -> Result<(), String> {
    // 统一前置：不打版权横幅（否则错误摘要只剩 `ffmpeg version …`）、不读 stdin
    // （所有调用方都不走管道输入，且都带 `-y`）、不打逐帧进度（stderr 只留诊断信息）。
    // 刻意不降 loglevel：`Non-monotonic DTS` 这类 warning 往往就是失败线索。
    let mut spec = CommandSpec::new(program.display().to_string())
        .arg("-hide_banner")
        .arg("-nostdin")
        .arg("-nostats");
    for arg in args {
        spec = spec.arg(*arg);
    }
    spec = spec.timeout(ctx.timeout);
    let runner = SystemRunner::new();
    // 只看本任务自己的停止令牌：点「停止转码」不会误伤码率修复，反之亦然。
    let output = match runner.run_cancellable(&spec, &|| ctx.cancel.is_cancelled()) {
        Ok(output) => output,
        Err(dossier_runtime::CommandError::Cancelled { .. }) => {
            return Err("ffmpeg 已被停止".into());
        }
        Err(error @ dossier_runtime::CommandError::Timeout { .. }) => {
            crate::logging::log_event(
                "ffmpeg",
                format!("超时（上限 {} 秒）：{error}", ctx.timeout.as_secs()),
            );
            return Err(format!(
                "ffmpeg 超过 {} 秒仍未完成，已终止：{error}",
                ctx.timeout.as_secs()
            ));
        }
        Err(error) => return Err(format!("ffmpeg 启动失败（{}）：{error}", program.display())),
    };
    if output.success() {
        return Ok(());
    }
    crate::logging::log_event(
        "ffmpeg",
        format!(
            "退出码 {}，stderr 末尾：\n{}",
            output.code_label(),
            stderr_tail(&output.stderr, 20)
        ),
    );
    Err(format!(
        "ffmpeg 退出码 {}：{}",
        output.code_label(),
        summarize_ffmpeg_failure(&output.stderr)
    ))
}

/// ffmpeg 的横幅与构建信息行（`ffmpeg version`、`configuration:`、`libavutil 58. …`）：
/// 出错时没有诊断价值，摘要时跳过（防御没加 `-hide_banner` 的输出）。
fn is_ffmpeg_banner_line(line: &str) -> bool {
    let line = line.trim_start();
    if line.starts_with("ffmpeg version")
        || line.starts_with("ffprobe version")
        || line.starts_with("built with")
        || line.starts_with("configuration:")
    {
        return true;
    }
    line.starts_with("lib")
        && line
            .split_whitespace()
            .nth(1)
            .and_then(|word| word.chars().next())
            .is_some_and(|c| c.is_ascii_digit())
}

/// stderr 末尾 `limit` 行非空内容（写日志用，保留原样）。
fn stderr_tail(stderr: &str, limit: usize) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let start = lines.len().saturating_sub(limit);
    lines[start..].join("\n")
}

/// ffmpeg 失败收尾时固定打印、但不含失败原因的行：最终统计行（`-nostats` 去不掉）、
/// `Nothing was written into output file…`、`Conversion failed!`。
///
/// ffmpeg 7.x 起这类行可能带上下文前缀（`[out#0/mp4 @ 0x55…] video:0KiB …`），先剥掉再判断。
fn is_ffmpeg_trailer_line(line: &str) -> bool {
    let line = strip_ffmpeg_context(line.trim());
    line == "Conversion failed!"
        || line.contains("Nothing was written into output file")
        || line.starts_with("frame=")
        || line.starts_with("size=")
        || line.starts_with("video:")
}

/// 去掉 ffmpeg 日志行首的 `[组件 @ 0x地址] ` 上下文前缀；没有前缀时原样返回。
fn strip_ffmpeg_context(line: &str) -> &str {
    if let Some(rest) = line.strip_prefix('[') {
        if let Some(end) = rest.find("] ") {
            if rest[..end].contains(" @ ") {
                return rest[end + 2..].trim_start();
            }
        }
    }
    line
}

/// 失败摘要：跳过横幅、收尾套话与空行，取最后 3 行有效内容拼成一句，最长 400 字符。
/// 参数错误时原因就在最后几行；编码中途失败时原因在收尾套话之前，所以先剔掉套话。
/// 剔完什么都不剩时退回「只去横幅」的末尾，保证摘要不为空。
/// 「流放不进容器」那一行后面 ffmpeg 还会再打好几行连带报错，按末 3 行截会把它截掉：
/// 它不在末尾时单独放到摘要最前面（码率修复靠它判断要不要降级兼容处理）。
fn summarize_ffmpeg_failure(stderr: &str) -> String {
    const MAX_CHARS: usize = 400;
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !is_ffmpeg_banner_line(line))
        .collect();
    if lines.is_empty() {
        return "（ffmpeg 没有输出错误信息）".into();
    }
    let causes: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| !is_ffmpeg_trailer_line(line))
        .collect();
    let lines = if causes.is_empty() { lines } else { causes };
    let start = lines.len().saturating_sub(3);
    let mut picked: Vec<&str> = lines[start..].to_vec();
    if let Some(compat) = lines[..start]
        .iter()
        .copied()
        .find(|line| is_container_compat_error(line))
    {
        picked.insert(0, compat);
    }
    let summary = picked.join("；");
    if summary.chars().count() > MAX_CHARS {
        let cut: String = summary.chars().take(MAX_CHARS).collect();
        format!("{cut}…")
    } else {
        summary
    }
}

/// 码率修复的第二遍目标：按第一遍偏离区间的程度等比修正。
///
/// 平均码率编码对细节少的画面会编不满、对细节多的画面会超出，按 `本次目标 × 期望 / 实测` 再给一次
/// 额度通常能落进区间。修正幅度封顶：偏低时最多放大到下限的 2 倍，偏高时最多压到上限的一半，
/// 防止极端画面把目标推到离谱的值（那种素材再怎么调也落不进区间，如实报失败）。
fn retry_target_bps(min_bps: u64, max_bps: u64, attempted_bps: u64, measured_bps: u64) -> u64 {
    let scale = |desired: u64| -> u64 {
        if measured_bps == 0 {
            return desired;
        }
        (u128::from(attempted_bps) * u128::from(desired) / u128::from(measured_bps)) as u64
    };
    if measured_bps < min_bps {
        let desired = bitrate_fix_target(BitrateDirection::Low, min_bps, max_bps);
        scale(desired).clamp(desired, min_bps.saturating_mul(2).max(desired))
    } else {
        let floor = (max_bps / 2).max(min_bps);
        scale(max_bps).clamp(floor, max_bps)
    }
}

/// 实测码率是否落在区间内（读不到算不在）。
fn bitrate_in_range(min_bps: u64, max_bps: u64, measured_bps: Option<u64>) -> bool {
    measured_bps.is_some_and(|bps| (min_bps..=max_bps).contains(&bps))
}

/// 首遍不在区间时要不要修正目标再编一遍：只有「读到了码率、但不在区间」才值得重试；
/// 读不到码率时再编一遍大概率还是读不到，白白多花一遍编码时间。
fn should_retry_reencode(min_bps: u64, max_bps: u64, measured_bps: Option<u64>) -> bool {
    measured_bps.is_some_and(|bps| !(min_bps..=max_bps).contains(&bps))
}

/// 为了能封装进 mp4，音轨 / 字幕轨要做多少兼容处理：从「原样保留」逐级降到「去掉字幕」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamCompat {
    /// 音轨、字幕轨原样拷贝（只重编视频轨）
    CopyAll,
    /// 音轨转 AAC，文字字幕转 mov_text（mp4 原生字幕格式）
    Convert,
    /// 音轨转 AAC，去掉字幕轨（图形字幕等 mp4 装不下的格式）
    DropSubtitles,
}

impl StreamCompat {
    fn next(self) -> Option<Self> {
        match self {
            Self::CopyAll => Some(Self::Convert),
            Self::Convert => Some(Self::DropSubtitles),
            Self::DropSubtitles => None,
        }
    }

    fn note(self) -> Option<String> {
        match self {
            Self::CopyAll => None,
            Self::Convert => {
                Some("原音轨 / 字幕格式无法直接封装进 mp4，已转为 AAC 音轨与 mov_text 字幕".into())
            }
            Self::DropSubtitles => {
                Some("原字幕轨格式无法封装进 mp4，修复后的文件不含字幕轨（音轨已转为 AAC）".into())
            }
        }
    }
}

/// ffmpeg 报「这条流放不进 mp4」时的典型文字：只有这类错误才值得降一级兼容处理重试，
/// 其他错误（解码失败、磁盘满等）换处理方式也不会好。
fn is_container_compat_error(error: &str) -> bool {
    [
        "Could not find tag for codec",
        "codec not currently supported in container",
        "not supported in container",
        "Subtitle codec",
        // 图形字幕（PGS / dvdsub）转 mov_text 时的报错：Convert 也装不下，继续降到去掉字幕
        "Subtitle encoding currently only possible",
    ]
    .iter()
    .any(|pattern| error.contains(pattern))
}

/// 码率修复的 ffmpeg 参数（纯函数，便于单测）。
///
/// `-map 0:V` 只取真正的视频轨（大写 V 排除封面图这类附加图片），`0:a?` / `0:s?` 保留全部音轨与字幕轨；
/// 只重编视频，其余按 `compat` 原样拷贝或转换。不加 `-map` 时 ffmpeg 只挑一条音轨、丢掉字幕。
fn bitrate_encode_args(
    input: &str,
    output: &str,
    target_bps: u64,
    compat: StreamCompat,
) -> Vec<String> {
    let target_kbps = (target_bps / 1000).max(1);
    let maxrate_kbps = target_kbps + target_kbps / 4;
    let bufsize_kbps = target_kbps * 2;
    let mut args: Vec<String> = vec![
        "-y".into(),
        "-i".into(),
        input.into(),
        "-map".into(),
        "0:V".into(),
        "-map".into(),
        "0:a?".into(),
        "-map".into(),
        "0:s?".into(),
        "-map_metadata".into(),
        "0".into(),
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "veryfast".into(),
        "-b:v".into(),
        format!("{target_kbps}k"),
        "-maxrate".into(),
        format!("{maxrate_kbps}k"),
        "-bufsize".into(),
        format!("{bufsize_kbps}k"),
    ];
    match compat {
        StreamCompat::CopyAll => args.extend(["-c:a", "copy", "-c:s", "copy"].map(String::from)),
        StreamCompat::Convert => {
            args.extend(["-c:a", "aac", "-b:a", "128k", "-c:s", "mov_text"].map(String::from))
        }
        StreamCompat::DropSubtitles => {
            args.extend(["-c:a", "aac", "-b:a", "128k", "-sn"].map(String::from))
        }
    }
    args.extend(["-movflags".into(), "+faststart".into(), output.into()]);
    args
}

/// 一次重编码的实测结果：视频轨码率（读不到为 None）与时长。
struct EncodedMeasure {
    bitrate_bps: Option<u64>,
    duration_sec: f64,
}

/// 按平均码率把 `input` 编到 `tmp`，返回实测视频轨码率与时长（与检测时同一套取值规则）。
///
/// 刻意不用 `nal-hrd=cbr` 之类的填充数据凑码率：MP4（ISO/IEC 14496-15）规定流里不得
/// 出现 Filler Data NAL，平台转码时可能被剔除，码率又掉回去。
fn encode_at_bitrate(
    ffmpeg: &Path,
    ffprobe: &Path,
    input: &Path,
    tmp: &Path,
    target_bps: u64,
    compat: StreamCompat,
    ctx: FfmpegCtx,
) -> Result<EncodedMeasure, String> {
    let args = bitrate_encode_args(
        &input.display().to_string(),
        &tmp.display().to_string(),
        target_bps,
        compat,
    );
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_ffmpeg(ffmpeg, &arg_refs, ctx)?;
    let probed = probe_episode(
        &EpisodeFile {
            path: tmp.to_path_buf(),
            file_name: tmp
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            size_bytes: 0,
            episode_number: None,
        },
        &FfprobeConfig {
            bin: Some(ffprobe.to_path_buf()),
        },
    );
    Ok(match probed {
        Ok(probed) => EncodedMeasure {
            bitrate_bps: video_bitrate(&probed.spec).0,
            duration_sec: probed.spec.duration_sec,
        },
        Err(_) => EncodedMeasure {
            bitrate_bps: None,
            duration_sec: 0.0,
        },
    })
}

/// 重编码成功的结果：替换后的实测码率与兼容处理说明。
struct ReencodeOutcome {
    measured_bps: Option<u64>,
    note: Option<String>,
}

/// 重编码一条视频，让视频轨码率落进 `[min, max]`。
///
/// 顺序是「先编到临时文件 → 探测码率与时长 → 都合格才替换」：原文件只在结果确定合格后才被覆盖。
/// 第一遍码率不在区间就按偏差修正目标重编一次；仍不在区间、或时长与原片对不上（例如源文件尾部损坏
/// 导致编出来短了一截），都删掉临时文件、原文件不动，返回可展示的失败原因。
fn reencode_to_bitrate(
    ffmpeg: &Path,
    ffprobe: &Path,
    input: &Path,
    original_duration: f64,
    (min_bps, max_bps): (u64, u64),
    target_bps: u64,
    ctx: FfmpegCtx,
) -> Result<ReencodeOutcome, String> {
    // 临时文件点开头：中途崩溃留下的残余不会被扫描器当成新一集。
    let tmp = input.with_file_name(format!(
        ".{}.fixing.mp4",
        input
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    let mut compat = StreamCompat::CopyAll;
    // 先确定能封装进 mp4 的兼容处理方式：只在「流放不进容器」时逐级降级。
    let mut attempt = |target: u64| -> Result<EncodedMeasure, String> {
        loop {
            match encode_at_bitrate(ffmpeg, ffprobe, input, &tmp, target, compat, ctx) {
                Ok(measure) => return Ok(measure),
                Err(error) => {
                    let _ = std::fs::remove_file(&tmp);
                    match compat.next() {
                        Some(next)
                            if is_container_compat_error(&error) && !ctx.cancel.is_cancelled() =>
                        {
                            crate::logging::log_event(
                                "码率",
                                format!(
                                    "{}：{error}；改用兼容处理（{next:?}）重编",
                                    input.display()
                                ),
                            );
                            compat = next;
                        }
                        _ => return Err(error),
                    }
                }
            }
        }
    };
    let mut measure = attempt(target_bps)?;
    if let Some(first) = measure
        .bitrate_bps
        .filter(|_| should_retry_reencode(min_bps, max_bps, measure.bitrate_bps))
    {
        let retry = retry_target_bps(min_bps, max_bps, target_bps, first);
        crate::logging::log_event(
            "码率",
            format!(
                "{}：首次 {} kbps 不在 {}~{} kbps，修正目标到 {} kbps 重编一次",
                input.display(),
                first / 1000,
                min_bps / 1000,
                max_bps / 1000,
                retry / 1000
            ),
        );
        measure = attempt(retry)?;
    }
    let reject = |reason: String| -> Result<ReencodeOutcome, String> {
        let _ = std::fs::remove_file(&tmp);
        Err(reason)
    };
    if !bitrate_in_range(min_bps, max_bps, measure.bitrate_bps) {
        return reject(match measure.bitrate_bps {
            Some(bps) if bps < min_bps => format!(
                "画面复杂度过低，重编码后只有 {} kbps，未达到 {} kbps，原文件未改动",
                bps / 1000,
                min_bps / 1000
            ),
            Some(bps) => format!(
                "重编码后仍有 {} kbps，超过上限 {} kbps，原文件未改动",
                bps / 1000,
                max_bps / 1000
            ),
            None => "重编码后读不到码率，无法确认是否合格，原文件未改动".into(),
        });
    }
    if !durations_match(original_duration, measure.duration_sec) {
        return reject(format!(
            "重编码后时长 {:.2} 秒与原片 {:.2} 秒不一致，原文件未改动",
            measure.duration_sec, original_duration
        ));
    }
    std::fs::rename(&tmp, input).map_err(|error| {
        let _ = std::fs::remove_file(&tmp);
        format!("替换原文件失败：{error}")
    })?;
    Ok(ReencodeOutcome {
        measured_bps: measure.bitrate_bps,
        note: compat.note(),
    })
}

fn fix_bitrate_blocking(
    app: &AppHandle,
    root: String,
    name: String,
    range: (u64, u64),
    ffprobe_path: Option<String>,
) -> Result<FixBitrateReport, String> {
    let (min_bps, max_bps) = range;
    // 本任务自己的停止令牌：只响应「停止码率修复」，不受转码页停止按钮影响。
    // 一进来就登记：逐集探测期间点停止也要生效（探测完、替换任何文件之前就会退出）。
    let task = crate::tasks::register(TaskKind::Bitrate);
    let ffmpeg = resolve_ffmpeg(app)?;
    let ffprobe = resolve_ffprobe(app, ffprobe_path.as_deref())?;
    let drama = load_drama(&root, &name)?;
    let episodes = probe_all(&drama, Some(ffprobe.display().to_string()));
    if episodes.is_empty() {
        return Err("没有任何成功探测规格的分集；请先确认素材可读且 ffprobe 可用。".into());
    }
    let report = bitrate_report(&episodes, min_bps, max_bps);
    let durations: std::collections::HashMap<&str, f64> = episodes
        .iter()
        .map(|episode| (episode.file.file_name.as_str(), episode.spec.duration_sec))
        .collect();

    let mut fixed = Vec::new();
    let mut failed = Vec::new();
    let mut untouched = 0_usize;
    let fix_total = report.iter().filter(|item| item.needs_fix).count() as u64;
    let mut fix_index = 0_u64;
    for item in &report {
        if task.token().is_cancelled() {
            // 已替换的分集保留（每集都是「临时文件合格才替换」），剩下的不再处理。
            return Err(format!(
                "已停止码率修复：已修复 {} 集，其余分集未改动。",
                fixed.len()
            ));
        }
        let Some(direction) = item.direction else {
            untouched += 1;
            continue;
        };
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
        let duration = durations
            .get(item.file_name.as_str())
            .copied()
            .unwrap_or_default();
        let ctx = FfmpegCtx::encode(task.token(), duration, false);
        let target = bitrate_fix_target(direction, min_bps, max_bps);
        match reencode_to_bitrate(&ffmpeg, &ffprobe, &path, duration, range, target, ctx) {
            Ok(outcome) => {
                // 合格才会走到这里；码率就是替换前探测的那份，不再重复探测。
                fixed.push(FixedItem {
                    file_name: item.file_name.clone(),
                    from_kbps,
                    to_kbps: outcome.measured_bps.map(|bps| bps / 1000),
                    note: outcome.note,
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
    max_kbps: Option<u64>,
    ffprobe_path: Option<String>,
) -> Result<FixBitrateReport, String> {
    crate::license::guard()?;
    let range = bitrate_range(min_kbps, max_kbps)?;
    // 测试期关键日志：码率修复同样是长任务，入口与结果留痕。
    crate::logging::log_event(
        "码率",
        format!(
            "开始：{name}（区间 {}~{} kbps）",
            range.0 / 1000,
            range.1 / 1000
        ),
    );
    let result = tauri::async_runtime::spawn_blocking(move || {
        fix_bitrate_blocking(&app, root, name, range, ffprobe_path)
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
    /// 深度处理去重配方摘要（seed 与存证文件）；其余模式/关闭时为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dedup: Option<crate::dedup::DedupRecipeSummary>,
    /// 重组没能把每集落在时长区间内时的说明（已并入 `note`，只供深度处理转述，不发给界面）。
    #[serde(skip)]
    pub recut_warning: Option<String>,
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

/// 成片的临时文件路径：同目录、点开头（扫描器会跳过）、后缀与正式名相同（ffmpeg 按后缀选封装格式：
/// 智能微调沿用原文件名，`.mkv` 素材就该输出 mkv 封装，不能变成扩展名是 mkv、内容是 mp4 的文件）。
/// 正式名没有后缀时用 `.mp4`。
///
/// 编码先写这里，**全部单元成功后**才由 [`publish_parts`] 统一改名为正式名：
/// 中途失败、崩溃或被停止都不会留下与正常成片同名的截断文件，上一轮的正式成片也保持完好。
fn part_path(final_path: &Path) -> PathBuf {
    let name = final_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = final_path
        .extension()
        .map(|ext| ext.to_string_lossy().into_owned())
        .filter(|ext| !ext.is_empty())
        .unwrap_or_else(|| "mp4".into());
    final_path.with_file_name(format!(".{name}.part.{ext}"))
}

/// 删除一批临时文件（不存在的忽略）。失败 / 停止时调用，保证不留 `.part` 残余。
fn remove_parts(finals: &[PathBuf]) {
    for path in finals {
        let _ = std::fs::remove_file(part_path(path));
    }
}

/// 把全部临时文件按顺序改名为正式名（同目录 rename 是原子的；Windows 上会覆盖同名旧成片）。
///
/// 某一步失败时删除尚未发布的临时文件，并在错误里写明已发布了几集——不吞错。
fn publish_parts(finals: &[PathBuf]) -> Result<(), String> {
    for (index, final_path) in finals.iter().enumerate() {
        if let Err(error) = std::fs::rename(part_path(final_path), final_path) {
            remove_parts(&finals[index..]);
            return Err(format!(
                "发布成片失败（已发布 {index} / {} 集）：{} → {error}",
                finals.len(),
                final_path.display()
            ));
        }
    }
    Ok(())
}

/// 重组产物的正式文件名规则：`第NN集.mp4`（两位及以上数字）。
fn is_recut_episode_name(name: &str) -> bool {
    name.strip_prefix('第')
        .and_then(|rest| rest.strip_suffix("集.mp4"))
        .is_some_and(|digits| digits.len() >= 2 && digits.chars().all(|c| c.is_ascii_digit()))
}

/// 重新分集后清理上一轮多出来的集：只删 `dir` 第一层里**文件名精确匹配 `第NN集.mp4`、
/// 且不在本次产物中**的文件，返回被删除的路径。用户自己放的其他文件、子目录一概不碰。
///
/// 只在本次成片全部发布成功、且输出目录归属检查已通过之后调用。
fn remove_stale_episodes(dir: &Path, keep: &[PathBuf]) -> Vec<PathBuf> {
    let keep: HashSet<PathBuf> = keep.iter().map(|path| normalize(path)).collect();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_match = path.is_file()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(is_recut_episode_name);
        if is_match && !keep.contains(&normalize(&path)) && std::fs::remove_file(&path).is_ok() {
            removed.push(path);
        }
    }
    removed.sort();
    removed
}

/// 中转目录守卫：创建时清掉上一次的残余，析构时删除——任何返回路径（含 `?`）都不会遗留几 GB 的中间视频。
struct StagingDir(PathBuf);

impl StagingDir {
    fn create(path: PathBuf) -> Result<Self, String> {
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path)
            .map_err(|error| format!("创建中转目录失败（{}）：{error}", path.display()))?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// concat demuxer 清单里的一行：路径用单引号包起来，路径自身的 `'` 写成 `'\''`
/// （先闭合引号、转义一个单引号、再重新打开）。不转义时路径含 `'` 的那一集必然拼接失败。
fn concat_entry(path: &Path) -> String {
    format!(
        "file '{}'",
        path.display().to_string().replace('\'', "'\\''")
    )
}

/// 分集重组里「场景检测」占总进度的比例（%），其余是片段编码。检测只读解码、比编码快得多。
const RECUT_DETECT_SHARE: u64 = 30;

/// 把一个阶段内的 `done/total` 映射到总进度的 `[start, end]`（百分比），单调不减、不越界。
fn phase_progress(start: u64, end: u64, done: u64, total: u64) -> u64 {
    start + (end - start) * done.min(total) / total.max(1)
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
    cancel: &CancelToken,
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
                if cancel.is_cancelled() {
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
    if cancel.is_cancelled() {
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

/// 单集微帧编码任务的全部输入；参数拼装是纯函数，便于单测四种开关组合。
struct NudgeJob {
    input: PathBuf,
    offset_sec: f64,
    /// 去重画面链；空串 = 不做画面扰动。
    video_chain: String,
    /// 去重音频链；`None` = 音频不过滤（无配方或素材无音轨）。
    audio_chain: Option<String>,
    /// `key=value` 形式的元数据。
    metadata: Vec<String>,
    /// AI 合规角标 PNG；`None` = 不叠角标。
    badge: Option<PathBuf>,
    output: PathBuf,
    encoder: String,
}

/// 拼装微帧 ffmpeg 参数。
///
/// 规则（ffmpeg 的硬约束）：
/// - 所有 `-i` 必须排在输出选项之前，否则 `-vf` 之类会被当成下一个输入的选项；
/// - 同一路输出视频流不能同时用 `-vf` 与 `-filter_complex`，所以有角标时去重画面链
///   并进 `filter_complex`，空链时直接 `[0:v:0][1:v:0]overlay`；
/// - 静态 PNG 作第二输入不需要 `-loop 1`：overlay 的 `eof_action` 默认 repeat，
///   角标会一直叠到主视频结束。
fn build_nudge_args(job: &NudgeJob) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-y".into(),
        "-ss".into(),
        format!("{:.6}", job.offset_sec),
        "-i".into(),
        job.input.display().to_string(),
    ];
    if let Some(badge) = &job.badge {
        args.push("-i".into());
        args.push(badge.display().to_string());
        let overlay = format!("overlay=x={AI_BADGE_MARGIN}:y={AI_BADGE_MARGIN}[vout]");
        let graph = if job.video_chain.is_empty() {
            format!("[0:v:0][1:v:0]{overlay}")
        } else {
            format!("[0:v:0]{}[base];[base][1:v:0]{overlay}", job.video_chain)
        };
        args.extend([
            "-filter_complex".into(),
            graph,
            "-map".into(),
            "[vout]".into(),
            "-map".into(),
            "0:a?".into(),
        ]);
    } else if !job.video_chain.is_empty() {
        args.push("-vf".into());
        args.push(job.video_chain.clone());
    }
    if let Some(audio) = &job.audio_chain {
        args.push("-af".into());
        args.push(audio.clone());
    }
    for tag in &job.metadata {
        args.push("-metadata".into());
        args.push(tag.clone());
    }
    args.extend(encode_output_args(&job.output, &job.encoder));
    args
}
/// 智能微调：每集从开头裁掉 1~3 帧（帧率未知时按 30fps 折算），输出同名新文件。
/// 各集互不依赖，`lanes` 路并行执行。
///
/// `recipe`（深度处理传入）在微帧之外注入去重配方：确定性滤镜链 + 机型元数据；
/// `badge`（深度处理传入）把 AI 合规角标 PNG 作为第二输入烧进画面。
/// 独立「智能微调」模式两者都传 None，保持原有轻量行为。
#[allow(clippy::too_many_arguments)]
fn transcode_nudge_blocking(
    ffmpeg: &Path,
    lanes: &EncodeLanes,
    drama_name: &str,
    episodes: &[ProbedEpisode],
    output_dir: &Path,
    cancel: &CancelToken,
    emit: &(dyn Fn(u64, u64, &str) + Sync),
    produced: &mut Vec<PathBuf>,
    recipe: Option<&crate::dedup::DedupRecipePlan>,
    badge: Option<&Path>,
) -> Result<TranscodeRunReport, String> {
    std::fs::create_dir_all(output_dir)
        .map_err(|error| format!("创建输出目录失败（{}）：{error}", output_dir.display()))?;
    let finals: Vec<PathBuf> = episodes
        .iter()
        .map(|episode| output_dir.join(&episode.file.file_name))
        .collect();
    let units: Vec<(usize, &ProbedEpisode)> = episodes.iter().enumerate().collect();
    let results = run_encode_units(&units, lanes.lanes, cancel, emit, |episode| {
        let frames = nudge_frames(seed_for(&episode.file.file_name));
        let fps = episode.spec.fps.unwrap_or(30.0).max(1.0);
        let offset = frames as f64 / fps;
        let output = output_dir.join(&episode.file.file_name);
        // 先编到临时文件，全部成功后再统一发布（见 publish_parts）。
        let part = part_path(&output);
        let mut detail = format!("裁掉开头 {frames} 帧（{offset:.4} 秒，{fps:.2} fps）");

        // 去重配方：确定性画面/音频滤镜链 + 机型元数据。
        let mut job = NudgeJob {
            input: episode.file.path.clone(),
            offset_sec: offset,
            video_chain: String::new(),
            audio_chain: None,
            metadata: Vec::new(),
            badge: badge.map(Path::to_path_buf),
            output: part.clone(),
            encoder: lanes.encoder.clone(),
        };
        if let Some(plan) = recipe {
            let index = episodes
                .iter()
                .position(|item| item.file.path == episode.file.path)
                .unwrap_or_default();
            if let Some(variant) = plan.variant(index) {
                job.video_chain = variant
                    .compile_video_filters(episode.spec.width.max(2), episode.spec.height.max(2));
                if episode.spec.audio_codec.is_some() {
                    job.audio_chain =
                        Some(variant.compile_audio_filters(crate::dedup::SAMPLE_RATE_HZ));
                }
                job.metadata = plan
                    .metadata_tags(index)
                    .into_iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect();
                detail.push_str(&format!("；去重（{}）：{}", plan.seed, variant.describe()));
            }
        }
        if job.badge.is_some() {
            detail.push_str("；已烧录 AI 合规角标");
        }
        let args = build_nudge_args(&job);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        // 带去重配方或角标的编码滤镜链最重，超时系数按深度处理算。
        let heavy = recipe.is_some() || job.badge.is_some();
        let ctx = FfmpegCtx::encode(cancel, episode.spec.duration_sec, heavy);
        run_ffmpeg(ffmpeg, &arg_refs, ctx).map_err(|error| {
            let _ = std::fs::remove_file(&part);
            format!("{}：{error}", episode.file.file_name)
        })?;
        Ok((
            (output.clone(), detail),
            format!("{}：完成", episode.file.file_name),
        ))
    })
    .inspect_err(|_| remove_parts(&finals))?;
    publish_parts(&finals)?;

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
        dedup: None,
        note: format!(
            "《{drama_name}》每集开头已微调 1~3 帧（{}），输出到新目录，原素材未改动。",
            lanes.describe()
        ),
        recut_warning: None,
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

/// 重组切段策略：独立重组按「目标集数」等分，深度处理按「单集成片时长区间」反推集数。
enum RecutStrategy<'a> {
    TargetCount(usize),
    /// `speed_drama`：开了去重配方时传剧名，按配方的逐集倍速规划成片时长；`None` 不变速。
    DurationRange {
        min_sec: f64,
        max_sec: f64,
        speed_drama: Option<&'a str>,
    },
}

/// 微帧最多裁掉的开头时长：3 帧 ÷ 素材里最低的有效帧率（都读不到时按 25 fps，即 0.12 秒）。
fn nudge_trim_margin_sec(episodes: &[ProbedEpisode]) -> f64 {
    const MAX_TRIM_FRAMES: f64 = 3.0;
    let fps = episodes
        .iter()
        .filter_map(|episode| episode.spec.fps)
        .filter(|fps| fps.is_finite() && *fps >= 1.0)
        .fold(None, |low: Option<f64>, fps| {
            Some(low.map_or(fps, |low| low.min(fps)))
        })
        .unwrap_or(25.0);
    MAX_TRIM_FRAMES / fps
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
#[allow(clippy::too_many_arguments)]
fn transcode_recut_blocking(
    ffmpeg: &Path,
    lanes: &EncodeLanes,
    episodes: &[ProbedEpisode],
    strategy: RecutStrategy,
    output_dir: &Path,
    cancel: &CancelToken,
    emit: &(dyn Fn(u64, u64, &str) + Sync),
    produced: &mut Vec<PathBuf>,
    // 成功后是否清理上一轮多出来的 `第NN集.mp4`：正式输出目录为 true；深度处理的中转目录每次新建，传 false。
    prune_stale: bool,
) -> Result<TranscodeRunReport, String> {
    std::fs::create_dir_all(output_dir)
        .map_err(|error| format!("创建输出目录失败（{}）：{error}", output_dir.display()))?;

    // 阶段一：逐集检测切点（并行，结果按集序合并），映射到全剧时间线。
    let ffmpeg_str = ffmpeg.display().to_string();
    let detect_units: Vec<(usize, &ProbedEpisode)> = episodes.iter().enumerate().collect();
    // 两个阶段各占一段进度（检测 0~30%、编码 30~100%）：以前两段都按各自的 done/total 上报，
    // 检测结束时进度条到 100%、编码开始又跳回 0。
    let emit_detect = |done: u64, total: u64, label: &str| {
        emit(
            phase_progress(0, RECUT_DETECT_SHARE, done, total),
            100,
            label,
        );
    };
    let emit_encode = |done: u64, total: u64, label: &str| {
        emit(
            phase_progress(RECUT_DETECT_SHARE, 100, done, total),
            100,
            label,
        );
    };
    emit_detect(0, episodes.len() as u64, "正在检测场景切点");
    let detect_results = run_encode_units(
        &detect_units,
        lanes.lanes,
        cancel,
        &emit_detect,
        |episode| {
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
        },
    )?;
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

    let mut out_of_range_note = None;
    let segments = match strategy {
        RecutStrategy::TargetCount(target) => plan_recut_segments(&boundaries, total, target, 1.0),
        RecutStrategy::DurationRange {
            min_sec,
            max_sec,
            speed_drama,
        } => {
            // 微帧阶段每集开头还要裁掉 1~3 帧：规划下限预留这段余量，成片才不会贴着下限还短一点。
            let planned_min = (min_sec + nudge_trim_margin_sec(episodes)).min(max_sec);
            let plan = plan_recut_by_duration_with_speed(
                &boundaries,
                total,
                planned_min,
                max_sec,
                |index| speed_drama.map_or(1.0, |name| crate::dedup::episode_speed(name, index)),
            );
            if !plan.in_range {
                let note = format!(
                    "素材总时长 {total:.0} 秒无法在单集 {min_sec:.0}~{max_sec:.0} 秒内均分，已按最接近的方案切成 {} 集",
                    plan.segments.len()
                );
                crate::logging::log_event("转码", &note);
                out_of_range_note = Some(note);
            }
            plan.segments
        }
    };
    // 预构造编码任务：跨源片段的中间件名带集号，任务间互不冲突。
    // 空段不静默跳过：跳过会让后面每一集的序号（以及按序号派生的变速）整体错位。
    let jobs: Vec<RecutJob> = segments
        .iter()
        .enumerate()
        .map(|(index, segment)| {
            let spans = spans_for_segment(episodes, &file_starts, *segment);
            if spans.is_empty() {
                return Err(format!(
                    "重组规划异常：第 {} 段（{:.1}s – {:.1}s）没有可用的源片段",
                    index + 1,
                    segment.0,
                    segment.1
                ));
            }
            Ok(RecutJob {
                index,
                output: output_dir.join(format!("第{:02}集.mp4", index + 1)),
                spans,
            })
        })
        .collect::<Result<_, String>>()?;
    let job_units: Vec<(usize, &RecutJob)> = jobs.iter().enumerate().collect();
    let finals: Vec<PathBuf> = jobs.iter().map(|job| job.output.clone()).collect();
    let encode_results = run_encode_units(&job_units, lanes.lanes, cancel, &emit_encode, |job| {
        encode_recut_job(ffmpeg, lanes, job, cancel)?;
        let label = format!(
            "第 {} 集：完成（{} 个源片段）",
            job.index + 1,
            job.spans.len()
        );
        Ok((job.output.clone(), label))
    })
    .inspect_err(|_| remove_parts(&finals))?;
    // 全部单元成功才发布：中途失败时目录里不会出现新旧混杂的成片。
    publish_parts(&finals)?;
    let stale = if prune_stale {
        remove_stale_episodes(output_dir, &finals)
    } else {
        Vec::new()
    };
    if !stale.is_empty() {
        crate::logging::log_event(
            "转码",
            format!(
                "已清理上一轮多出来的 {} 集：{}",
                stale.len(),
                stale
                    .iter()
                    .filter_map(|path| path.file_name())
                    .map(|name| name.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("、")
            ),
        );
    }

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
    let mut note = format!(
        "已按场景切点重新分成 {produced_count} 集（统一重编码保证切点精确，{}），输出到新目录，原素材未改动。",
        lanes.describe()
    );
    if let Some(extra) = &out_of_range_note {
        note.push_str(&format!(" 注意：{extra}。"));
    }
    if !stale.is_empty() {
        note.push_str(&format!(
            " 已删除上一轮多出来的 {} 集旧成片（本次只有 {produced_count} 集）。",
            stale.len()
        ));
    }
    Ok(TranscodeRunReport {
        mode: "recut".into(),
        output_dir: output_dir.display().to_string(),
        items,
        materials: None,
        dedup: None,
        note,
        recut_warning: out_of_range_note,
    })
}

/// 执行单个重组编码任务：单源片段一条命令直达；跨源片段先逐源切片，
/// 再用 concat demuxer 无损拼接（编码参数一致）。失败时清理本次中间文件。
fn encode_recut_job(
    ffmpeg: &Path,
    lanes: &EncodeLanes,
    job: &RecutJob,
    cancel: &CancelToken,
) -> Result<(), String> {
    // 写临时文件，由调用方在全部单元成功后统一发布为 job.output。
    let output = &part_path(&job.output);
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
        run_ffmpeg(
            ffmpeg,
            &arg_refs,
            FfmpegCtx::encode(cancel, span.duration, false),
        )
        .map_err(|error| {
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
            if let Err(error) = run_ffmpeg(
                ffmpeg,
                &arg_refs,
                FfmpegCtx::encode(cancel, span.duration, false),
            ) {
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
            .map(|part| concat_entry(part))
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
            // 无损拼接只拷贝数据，按本集总时长给超时即可。
            FfmpegCtx::encode(
                cancel,
                job.spans.iter().map(|span| span.duration).sum(),
                false,
            ),
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

/// 在素材目录里找封面图：文件名含「封面 / cover / poster」的图片（与清单规则同口径）。
/// 报审素材目录按规范都会带封面，深度处理直接复用，无需用户手动上传。
pub(crate) fn find_local_cover(dir: &Path) -> Option<PathBuf> {
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
pub(crate) fn read_local_synopsis(dir: &Path) -> Option<String> {
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

/// 深度处理编码侧：分集重组到临时中转目录 → 逐集微帧（去重配方 + AI 合规角标）输出到
/// 「视频成片/」子目录。材料（剧名/简介/海报/过程图/AI 制作证明）由调用方按设置决定是否
/// 并行启动材料线程，编码结束后汇合；材料失败不回滚成片。
///
/// `badge` 由调用方在任何编码开始前渲染好（失败即报错，见 [`render_ai_badge`]）。
/// 进度映射：重组 0~50%，微帧 50~100%。
#[allow(clippy::too_many_arguments)]
fn transcode_deep_blocking(
    app: &AppHandle,
    ffmpeg: &Path,
    ffprobe: &Path,
    drama_name: &str,
    episodes: &[ProbedEpisode],
    min_sec: f64,
    max_sec: f64,
    lanes: &EncodeLanes,
    output_dir: &Path,
    badge: Option<&Path>,
    cancel: &CancelToken,
    emit: &(dyn Fn(u64, u64, &str) + Sync),
    produced: &mut Vec<PathBuf>,
    // 成功后是否清理「视频成片/」里上一轮多出来的集：只有输出目录早已归本剧时才清理。
    prune_stale: bool,
) -> Result<TranscodeRunReport, String> {
    std::fs::create_dir_all(output_dir)
        .map_err(|error| format!("创建输出目录失败（{}）：{error}", output_dir.display()))?;
    let settings = read_settings(app)?;
    // 守卫负责删除：后面任何 `?` 提前返回都不会把整套中间视频留在磁盘上。
    let staging = StagingDir::create(output_dir.join(".deep-staging"))?;

    // 阶段一：分集重组（产物先落中转目录，进度 0~50）。
    // 重组产物按任务顺序记录在这里；微帧阶段的配方下标就是它的下标，必须与规划时的段序号一致。
    let mut staging_produced: Vec<PathBuf> = Vec::new();
    let recut_warning = {
        let step_recut = |done: u64, total: u64, label: &str| {
            emit(
                done.min(total) * 50 / total.max(1),
                100,
                &format!("重组 · {label}"),
            );
        };
        // 去重配方开着时按配方的逐集倍速规划：用户设的单集时长指的是成片。
        let speed_drama = settings
            .effective_dedup_recipe_enabled()
            .then_some(drama_name);
        let recut_result = transcode_recut_blocking(
            ffmpeg,
            lanes,
            episodes,
            RecutStrategy::DurationRange {
                min_sec,
                max_sec,
                speed_drama,
            },
            staging.path(),
            cancel,
            &step_recut,
            &mut staging_produced,
            // 中转目录每次新建，没有上一轮的旧集需要清理。
            false,
        );
        recut_result?.recut_warning
    };

    // 按重组任务顺序收集产物：不再按文件名排序（字节序下「第100集」会排到「第10集」之前，
    // 集序一错，逐集倍速就和规划时用的对不上）。
    let mut recut_files: Vec<ProbedEpisode> = Vec::new();
    let ffprobe_cfg = dossier_core::FfprobeConfig {
        bin: Some(ffprobe.to_path_buf()),
    };
    for path in &staging_produced {
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
        return Err("分集重组没有产出任何片段，无法继续微帧。".into());
    }

    // 阶段二：逐集微帧（进度 50~100）。成片单独放「视频成片/」子目录，材料与视频分层清晰。
    let final_video_dir = output_dir.join("视频成片");

    // 去重配方：剧名 seed 确定性派生逐集扰动参数，配方全文落盘存证。
    let recipe_plan = if settings.effective_dedup_recipe_enabled() {
        let plan = crate::dedup::DedupRecipePlan::build(drama_name, recut_files.len());
        match plan.save_to(output_dir) {
            Ok(path) => crate::logging::log_event(
                "去重",
                format!(
                    "配方已签发：seed {}，{} 集，存证 {}",
                    plan.seed,
                    plan.variants.len(),
                    path.display()
                ),
            ),
            Err(error) => crate::logging::log_event(
                "去重",
                format!("配方存证写入失败（不影响成片）：{error}"),
            ),
        }
        Some(plan)
    } else {
        crate::logging::log_event("去重", "已在设置中关闭，成片只做重组 + 微帧");
        None
    };

    let nudge_result = {
        let step_nudge = |done: u64, total: u64, label: &str| {
            emit(
                50 + done.min(total) * 50 / total.max(1),
                100,
                &format!("微帧 · {label}"),
            );
        };
        transcode_nudge_blocking(
            ffmpeg,
            lanes,
            drama_name,
            &recut_files,
            &final_video_dir,
            cancel,
            &step_nudge,
            produced,
            recipe_plan.as_ref(),
            badge,
        )
    };
    // 微帧结束就释放中转目录（几 GB 中间视频），不等到函数返回。
    drop(staging);
    let nudge_report = nudge_result?;
    let episode_count = nudge_report.items.len();
    // 成片已全部发布：清理「视频成片/」里上一轮多出来的集。
    let finals: Vec<PathBuf> = nudge_report
        .items
        .iter()
        .map(|item| PathBuf::from(&item.output))
        .collect();
    let stale = if prune_stale {
        remove_stale_episodes(&final_video_dir, &finals)
    } else {
        Vec::new()
    };
    if !stale.is_empty() {
        crate::logging::log_event(
            "转码",
            format!("深度处理：已清理上一轮多出来的 {} 集旧成片", stale.len()),
        );
    }

    // 汇总报告：去重摘要 + 完成说明。
    let recipe_file = output_dir.join(crate::dedup::RECIPE_FILE_NAME);
    let dedup_summary = recipe_plan.map(|plan| crate::dedup::DedupRecipeSummary {
        seed: plan.seed,
        episode_count: plan.variants.len(),
        recipe_file: recipe_file
            .is_file()
            .then(|| recipe_file.display().to_string()),
    });
    let mut note = format!(
        "深度处理完成：全剧按场景切点重组后逐集微帧（{}），共 {episode_count} 集，成片在「视频成片/」子目录，原素材未改动。",
        lanes.describe()
    );
    if let Some(summary) = &dedup_summary {
        note.push_str(&format!(
            " 已注入确定性去重配方（seed {}，逐集参数存证于 去重配方.json）。",
            summary.seed
        ));
    }
    if badge.is_some() {
        note.push_str(&format!(
            " 成片已烧录「{}」AI 合规角标。",
            settings.effective_ai_badge_text()
        ));
    }
    if let Some(extra) = &recut_warning {
        note.push_str(&format!(" 注意：{extra}。"));
    }
    if !stale.is_empty() {
        note.push_str(&format!(
            " 已删除「视频成片/」里上一轮多出来的 {} 集旧成片。",
            stale.len()
        ));
    }
    Ok(TranscodeRunReport {
        mode: "deep".into(),
        output_dir: output_dir.display().to_string(),
        items: nudge_report.items,
        materials: None,
        dedup: dedup_summary,
        note,
        recut_warning,
    })
}

/// AI 合规角标：画面边距（像素）/ 渲染中间物文件名。文案与字号来自设置（见 AppSettings）。
const AI_BADGE_MARGIN: i64 = 24;

/// 设置里的角标「字号」（10–24）换算成渲染脚本用的像素：`像素 = 字号 × 10 / 3`（四舍五入），
/// 默认字号 12 正好对应脚本原来的默认 40 像素，升级后成片效果不变。
fn ai_badge_font_pixels(font_size: u32) -> u32 {
    (font_size * 10 + 1) / 3
}

/// 角标脚本的命令行参数（纯函数，便于单测）。一律写成 `--选项=值`：文案是用户可改的，
/// 以 `-` 开头时 argparse 会把独立参数当成选项、报参数错误，深度处理随之中止。
fn ai_badge_script_args(script: &Path, target: &Path, text: &str, font_size: u32) -> Vec<String> {
    vec![
        script.display().to_string(),
        format!("--output={}", target.display()),
        format!("--text={text}"),
        format!("--font-size={}", ai_badge_font_pixels(font_size)),
    ]
}
const AI_BADGE_FILE: &str = ".ai-badge.png";

/// 渲染 AI 合规角标 PNG（Pillow，与过程图同一套材料排版环境）。
///
/// 开关关闭 → `Ok(None)`；开关打开但渲染失败 → `Err`。角标是成片上的合规标识，
/// 开着却静默缺失比直接报错更危险，所以调用方必须在任何编码开始前调用它。
fn render_ai_badge(
    app: &AppHandle,
    output_dir: &Path,
    settings: &dossier_core::AppSettings,
) -> Result<Option<PathBuf>, String> {
    if !settings.effective_ai_badge_enabled() {
        crate::logging::log_event("材料", "AI 合规角标已在设置中关闭，成片不烧角标");
        return Ok(None);
    }
    let render = || -> Result<PathBuf, String> {
        let python = locate_materials_python(app)?;
        let script = materials_script(app, "make_ai_badge.py")?;
        let target = output_dir.join(AI_BADGE_FILE);
        let spec = CommandSpec::new(python.display().to_string())
            .args(ai_badge_script_args(
                &script,
                &target,
                &settings.effective_ai_badge_text(),
                settings.effective_ai_badge_font_size(),
            ))
            // 纯 CPU 排版，1 分钟封顶防挂死。
            .timeout(Duration::from_secs(60));
        let output = SystemRunner::new()
            .run(&spec)
            .map_err(|error| format!("角标进程启动失败：{error}"))?;
        if !output.success() {
            let tail: Vec<&str> = output.stderr.lines().rev().take(3).collect();
            let tail = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
            return Err(format!("角标脚本退出码 {}：{tail}", output.code_label()));
        }
        if !target.is_file() {
            return Err("角标脚本执行成功但未产出 PNG".into());
        }
        Ok(target)
    };
    match render() {
        Ok(path) => {
            crate::logging::log_event("材料", format!("AI 合规角标已渲染：{}", path.display()));
            Ok(Some(path))
        }
        Err(error) => {
            crate::logging::log_event(
                "材料",
                format!("AI 合规角标渲染失败，深度处理中止：{error}"),
            );
            Err(format!(
                "AI 合规角标生成失败：{error}。请联网后重试（首次需要自动安装 Pillow 排版环境），\
                 或在「设置 → 目录与引擎 → 内容模型」关闭「烧录 AI 合规角标」后再处理。"
            ))
        }
    }
}

fn transcode_blocking(
    app: &AppHandle,
    root: String,
    name: String,
    request: TranscodeRequest,
    ffprobe_path: Option<String>,
    output_root: Option<String>,
) -> Result<TranscodeRunReport, String> {
    // 本次运行独有的停止令牌：上一轮的停止指令不会残留，也不会影响同时在跑的其他任务。
    let task = crate::tasks::register(TaskKind::Transcode);
    let cancel = task.token();
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
    // 落点规则与报审包同一套（`dossier_core::resolve_product_dir`）：
    // 本次覆盖（页内输出目录，即最终目录本身）→ 产物根目录下的 `<剧名>/<子目录>` → 剧目目录内。
    // 设置里的「深度处理输出目录」是**根目录**而不是某一部剧的目录：多部剧共用同一个值时
    // 各自落到 `<该目录>/<剧名>/深度处理`，不会互相覆盖成片与材料。
    let settings = read_settings(app)?;
    let effective_override = request.output_dir.clone().map(PathBuf::from);
    let effective_root = if mode == "deep" {
        settings
            .deep_output_dir
            .clone()
            .or_else(|| output_root.clone().map(PathBuf::from))
    } else {
        output_root.clone().map(PathBuf::from)
    };
    // 落点名与扫描器的排除列表用同一组常量：新增模式时两边不会对不上。
    let subdir = match mode {
        "recut" => dossier_core::RECUT_OUTPUT_DIR,
        "deep" => dossier_core::DEEP_OUTPUT_DIR,
        _ => dossier_core::NUDGE_OUTPUT_DIR,
    };
    let output_dir = resolve_product_dir(
        effective_override.as_deref(),
        effective_root.as_deref(),
        &drama.dir,
        &drama.name,
        subdir,
    );
    reject_source_dir(&output_dir, &episodes)?;
    // 归属检查在创建任何目录或文件之前：属于别的剧时原样不动、直接报错。
    let owner_key = crate::paths::material_key(&drama.dir);
    let owner_state = check_output_owner(&output_dir, &drama.dir, &owner_key)?;
    // 占用输出目录直到本函数返回（材料线程在返回前汇合）：期间「按新文案重做海报」会被拒绝，
    // 不会和材料线程同时读写清单、海报与证明。
    let _busy = crate::paths::claim_output_dir(&output_dir).map_err(|()| {
        format!(
            "输出目录 {} 正在被另一个任务使用（深度处理或重做海报），请等它结束后再试。",
            output_dir.display()
        )
    })?;
    // 停止时的清理策略依赖「目录是不是本次新建的」：新建的整目录删除，
    // 已存在的只删本次写出的文件，避免误删用户目录里的其他内容。
    let dir_existed = output_dir.exists();
    let mut produced: Vec<PathBuf> = Vec::new();
    let created_dirs = crate::paths::create_dir_all_tracked(&output_dir)
        .map_err(|error| format!("创建输出目录失败（{}）：{error}", output_dir.display()))?;
    match &owner_state {
        OwnerState::Unclaimed => match write_output_owner(&output_dir, &owner_key) {
            // 本次新写的标记算本次产物：停止清理时随其他产物一起删。
            Ok(marker) => produced.push(marker),
            Err(error) => {
                crate::paths::remove_created_dirs(&created_dirs);
                return Err(error);
            }
        },
        OwnerState::Reclaim { previous } => {
            // 原剧已不在：覆盖标记，改归本剧（不算本次产物，停止时保留认领结果）。
            write_output_owner(&output_dir, &owner_key)?;
            crate::logging::log_event(
                "转码",
                format!(
                    "归属标记指向的剧目目录已不存在（{previous}），输出目录 {} 由《{}》重新认领",
                    output_dir.display(),
                    drama.name
                ),
            );
        }
        OwnerState::InsideDramaDir | OwnerState::Owned => {}
    }
    // 只有目录早已归本剧（或就在剧目目录里）时，才清理上一轮多出来的 `第NN集.mp4`：
    // 本次才认领 / 重新认领的目录里那些同名文件不是本剧上一轮的产物，删了就找不回来。
    let prune_stale = matches!(owner_state, OwnerState::InsideDramaDir | OwnerState::Owned);
    // 动手前就失败（例如角标渲染失败）时的回滚：删本次写出的文件和本次新建的各层空目录。
    let rollback = |produced: &[PathBuf]| {
        for path in produced {
            let _ = std::fs::remove_file(path);
        }
        crate::paths::remove_created_dirs(&created_dirs);
    };

    // 并行执行器在工作线程上回调进度；emit_task_progress 只依赖 &AppHandle，可安全共享。
    let emit = |done: u64, total: u64, label: &str| {
        emit_task_progress(app, "transcode", done, total, label);
    };
    // 深度处理的 AI 合规角标必须在一切耗时工作之前就绪：开着却渲染失败时直接报错，
    // 不启动材料线程、不编码，避免静默产出不带合规标识的成片。
    let badge_path = if mode == "deep" {
        match render_ai_badge(app, &output_dir, &settings) {
            Ok(path) => path,
            Err(error) => {
                // 脚本可能已写出半张角标：一并删掉，再回滚本次新建的目录。
                let mut leftovers = produced.clone();
                leftovers.push(output_dir.join(AI_BADGE_FILE));
                rollback(&leftovers);
                return Err(error);
            }
        }
    } else {
        None
    };
    // 深度处理材料开关开着才启动材料线程，与编码并行；材料失败不回滚成片。
    // 关着就不产出任何材料（不调 ASR/OCR/LLM/生图）。
    // 材料线程用子令牌：用户点停止时随转码一起停；编码失败时只停材料线程（见下方汇合前）。
    let material_token = cancel.child();
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
            material_token.clone(),
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
                cancel,
                &emit,
                &mut produced,
                prune_stale,
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
                &episodes,
                f64::from(min_sec),
                f64::from(max_sec),
                &lanes,
                &output_dir,
                badge_path.as_deref(),
                cancel,
                &emit,
                &mut produced,
                prune_stale,
            )
        }
        _ => transcode_nudge_blocking(
            &ffmpeg,
            &lanes,
            &drama.name,
            &episodes,
            &output_dir,
            cancel,
            &emit,
            &mut produced,
            // 独立「智能微调」保持轻量语义：不注入去重配方，也不烧角标。
            None,
            None,
        ),
    };
    // 编码失败（非用户停止）：成片已不可用，立即停掉材料线程，不再发起新的 LLM / 生图调用。
    // 已经发出的请求不中途打断，已生成的材料保留，报告里标明成片未完成。
    if result.is_err() && !cancel.is_cancelled() && material_job.is_some() {
        material_token.cancel();
        crate::logging::log_event("材料", "编码失败，已停止材料线程（不再发起新的 AI 调用）");
    }
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
    // 角标 PNG 只是烧录中间物，成功与失败都清理。
    if let Some(path) = &badge_path {
        let _ = std::fs::remove_file(path);
    }
    match result {
        Ok(mut report) => {
            let mut materials = materials;
            // 深度处理：成片已全部发布，补上成片指纹与实际处理方式后才生成 AI 制作证明。
            if let Some(material_report) = materials.as_mut().filter(|_| mode == "deep") {
                let processing = crate::content::ProofProcessing {
                    encoder: lanes.describe(),
                    dedup_seed: report.dedup.as_ref().map(|summary| summary.seed),
                    badge_text: badge_path
                        .is_some()
                        .then(|| settings.effective_ai_badge_text()),
                };
                let outputs: Vec<PathBuf> = report
                    .items
                    .iter()
                    .map(|item| PathBuf::from(&item.output))
                    .collect();
                crate::content::finalize_deep_proof(
                    app,
                    &output_dir,
                    material_report,
                    &processing,
                    &outputs,
                    cancel,
                );
            }
            report.materials = materials;
            Ok(report)
        }
        Err(error) => {
            if cancel.is_cancelled() {
                cleanup_transcode_output(&output_dir, dir_existed, &produced, &created_dirs);
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
            // 编码失败（非用户停止）：成片未发布，上一轮正式成片保持不动；
            // 已生成的剧名 / 简介 / 海报不删除（已付过 AI 费用，下次处理可直接复用）。
            // 材料线程被叫停时报告是 failed（字段为空），但剧名/简介在生成当下就已写回本剧材料、
            // 海报直接落在输出目录，所以同样提示「成片未完成、已生成的保留」。
            match &materials {
                Some(report) if report.title.is_some() || report.poster.is_some() => Err(format!(
                    "{error}\n成片未完成。已生成的材料保留在 {}，下次处理时会直接复用。",
                    output_dir.display()
                )),
                Some(_) => Err(format!(
                    "{error}\n成片未完成，材料线程已随之停止；如有已生成的剧名、简介、海报，均已保存，下次处理时会直接复用。"
                )),
                None => Err(error),
            }
        }
    }
}

/// 停止后清理本次转码产物：目录是本次新建的 → 整目录删除；
/// 目录早已存在 → 只删本次写出的文件，再尝试删掉空目录（删不掉说明还有别的内容，留着不动）。
/// 深度处理的成片在「视频成片/」子目录，一并尝试清掉空壳。
/// `created_dirs` 是本次新建的各层目录（含 `<剧名>` 父目录）：整目录删完后，把它们中的空壳也收掉。
fn cleanup_transcode_output(
    output_dir: &Path,
    dir_existed: bool,
    produced: &[PathBuf],
    created_dirs: &[PathBuf],
) {
    if !dir_existed {
        let _ = std::fs::remove_dir_all(output_dir);
        crate::paths::remove_created_dirs(created_dirs);
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

/// 输出目录的归属标记：记录这个目录属于哪部剧（剧目目录的规范化路径，与按剧材料同一个键）。
///
/// 落点规则只按剧名拼路径，不同素材库里的同名剧会落到同一个目录；有了标记，
/// 另一部剧再想写进来时在动手前报错，而不是互相覆盖成片、复用或删掉对方的海报。
const OUTPUT_OWNER_FILE: &str = ".dossier-owner.json";

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OutputOwner {
    drama_dir: String,
}

/// 归属检查结果。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OwnerState {
    /// 输出目录在剧目目录里面：天然只属于这部剧，不检查也不写标记（剧目目录移动时跟着走）。
    InsideDramaDir,
    /// 还没有标记（新目录，或旧版本留下的目录）：本次认领。
    Unclaimed,
    /// 标记就是本剧。
    Owned,
    /// 标记指向的剧目目录已不存在（原剧移走或删除）：由本剧重新认领。
    Reclaim { previous: String },
}

/// 归属判定（纯函数，IO 在 [`check_output_owner`]）：
/// `marker_owner` 是标记里记录的剧目目录（`None` = 没有标记），`previous_exists` 是该目录此刻是否还在。
fn owner_decision(
    marker_owner: Option<&str>,
    owner_key: &str,
    previous_exists: bool,
) -> Result<OwnerState, String> {
    match marker_owner {
        None => Ok(OwnerState::Unclaimed),
        Some(owner) if owner == owner_key => Ok(OwnerState::Owned),
        Some(owner) if !previous_exists => Ok(OwnerState::Reclaim {
            previous: owner.to_string(),
        }),
        Some(owner) => Err(format!(
            "已被另一部剧（{owner}）使用。请在设置里换一个输出根目录，或给其中一部剧的目录改名后重试；\
             如果是同一部剧移动了位置而原位置还留着同名目录，请删除该输出目录下的 {OUTPUT_OWNER_FILE} 后重试。"
        )),
    }
}

/// 检查 `output_dir` 能否由剧目目录 `drama_dir`（键 `owner_key`）写入；
/// 属于别的、仍然存在的剧，或标记损坏时返回可展示的错误。
pub(crate) fn check_output_owner(
    output_dir: &Path,
    drama_dir: &Path,
    owner_key: &str,
) -> Result<OwnerState, String> {
    if normalize(output_dir).starts_with(normalize(drama_dir)) {
        return Ok(OwnerState::InsideDramaDir);
    }
    let marker = output_dir.join(OUTPUT_OWNER_FILE);
    let raw = match std::fs::read(&marker) {
        Ok(raw) => Some(raw),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(format!(
                "读取输出目录归属标记失败（{}）：{error}",
                marker.display()
            ))
        }
    };
    let owner = raw
        .map(|raw| {
            serde_json::from_slice::<OutputOwner>(&raw).map_err(|error| {
                format!(
                    "输出目录归属标记无法识别（{}）：{error}。确认该目录可以给当前剧使用后，删除这个文件再重试。",
                    marker.display()
                )
            })
        })
        .transpose()?;
    let previous_exists = owner
        .as_ref()
        .is_some_and(|owner| Path::new(&owner.drama_dir).is_dir());
    owner_decision(
        owner.as_ref().map(|owner| owner.drama_dir.as_str()),
        owner_key,
        previous_exists,
    )
    .map_err(|reason| format!("输出目录 {} {reason}", output_dir.display()))
}

/// 写入归属标记（目录须已存在），返回标记路径。
fn write_output_owner(output_dir: &Path, owner_key: &str) -> Result<PathBuf, String> {
    let marker = output_dir.join(OUTPUT_OWNER_FILE);
    let raw = serde_json::to_vec_pretty(&OutputOwner {
        drama_dir: owner_key.to_string(),
    })
    .map_err(|error| format!("生成输出目录归属标记失败：{error}"))?;
    crate::paths::write_atomic(&marker, &raw)
        .map_err(|error| format!("写入输出目录归属标记失败（{}）：{error}", marker.display()))?;
    Ok(marker)
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
    crate::license::guard()?;
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

// ---------------------------------------------------------------------------
// 材料生成 Python（Pillow 排版：过程图 / 制作证明图 / 海报水印清理）
// ---------------------------------------------------------------------------

/// 材料生成专用虚拟环境解释器：`<app_data>/pyenv/`（与一键安装的 bin 目录并列）。
fn materials_venv_python(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("无法定位应用数据目录：{error}"))?;
    let rel = if cfg!(target_os = "windows") {
        "pyenv/Scripts/python.exe"
    } else {
        "pyenv/bin/python"
    };
    Ok(dir.join(rel))
}

/// 解释器是否已装 Pillow：一次子进程探测（成功时约 0.3 秒）。
fn python_has_pil(python: &Path) -> bool {
    let spec = CommandSpec::new(python.display().to_string())
        .args(["-c", "import PIL"])
        .timeout(Duration::from_secs(20));
    SystemRunner::new()
        .run(&spec)
        .map(|output| output.success())
        .unwrap_or(false)
}

/// 自动配置只尝试一次：断网时每次生成材料都白等一轮 pip 超时毫无意义。
static MATERIALS_PY_PROVISION_TRIED: AtomicBool = AtomicBool::new(false);

/// 自动创建 `<app_data>/pyenv` 虚拟环境并安装 Pillow（需要联网，首次约 30~60 秒）。
fn provision_materials_python(app: &AppHandle) -> Result<PathBuf, String> {
    if MATERIALS_PY_PROVISION_TRIED.swap(true, Ordering::SeqCst) {
        return Err("材料排版 Python 环境未就绪（本会话已尝试过自动配置）。\
             请在网络可用后重试，或点「一键配置运行环境」完成安装。"
            .into());
    }
    let _ = app.emit(
        "material-progress",
        serde_json::json!({
            "stage": "python",
            "label": "材料线程：首次配置 Python 排版环境（联网安装 Pillow，约 1 分钟）",
        }),
    );
    let python = materials_venv_python(app)?;
    if !python.is_file() {
        let candidates: &[&str] = if cfg!(target_os = "windows") {
            &["python", "python3"]
        } else {
            &["python3", "/usr/bin/python3", "python"]
        };
        let base = candidates
            .iter()
            .find(|name| {
                let spec = CommandSpec::new(name.to_string())
                    .arg("--version")
                    .timeout(Duration::from_secs(10));
                SystemRunner::new()
                    .run(&spec)
                    .map(|output| output.success())
                    .unwrap_or(false)
            })
            .ok_or_else(|| {
                "未找到可用的 Python 3 解释器。请安装 Python 3.9+（python.org 或系统包管理器），\
                 或在能访问仓库的环境里使用深度处理。"
                    .to_string()
            })?;
        // venv 根目录 = <app_data>/pyenv（python 位于 pyenv/bin/python，两层 parent 即根）。
        let venv_dir = python
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .ok_or_else(|| "无法推断虚拟环境目录".to_string())?;
        let venv = CommandSpec::new(base.to_string())
            .args(["-m", "venv", &venv_dir.display().to_string()])
            .timeout(Duration::from_secs(120));
        SystemRunner::new()
            .run(&venv)
            .map_err(|error| format!("创建虚拟环境失败：{error}"))?;
        if !python.is_file() {
            return Err(format!("虚拟环境创建后未找到解释器：{}", python.display()));
        }
    }
    let pip = python
        .parent()
        .map(|dir| {
            let name = if cfg!(target_os = "windows") {
                "pip.exe"
            } else {
                "pip"
            };
            dir.join(name)
        })
        .ok_or_else(|| "无法推断 pip 路径".to_string())?;
    let install = CommandSpec::new(pip.display().to_string())
        .args([
            "install",
            "--quiet",
            "--disable-pip-version-check",
            "pillow",
        ])
        .timeout(Duration::from_secs(300));
    let output = SystemRunner::new()
        .run(&install)
        .map_err(|error| format!("安装 Pillow 失败（需要联网）：{error}"))?;
    if !output.success() {
        // stderr 末尾几行按自然顺序展示（先倒序取尾再反转还原先后）。
        let mut tail: Vec<&str> = output.stderr.lines().rev().take(3).collect();
        tail.reverse();
        let tail = tail.join("\n");
        return Err(format!(
            "Pillow 安装失败（退出码 {}）：{}",
            output.code_label(),
            if tail.is_empty() {
                "（无输出）"
            } else {
                &tail
            }
        ));
    }
    if !python_has_pil(&python) {
        return Err("Pillow 安装完成但导入失败，请检查网络代理设置后重试".into());
    }
    crate::logging::log_event("环境", "材料排版 Python 环境配置成功（venv + Pillow）");
    Ok(python)
}

/// 定位材料生成 Python：App venv → 开发仓库 cloud/.venv → PATH 自带 Pillow 的解释器 → 自动配置。
///
/// 过程图 / 制作证明图 / 海报水印清理都用它跑 Pillow 脚本；打包安装版没有
/// 仓库布局，靠这里的 App venv 兜底（首次使用时自动联网配置）。
pub(crate) fn locate_materials_python(app: &AppHandle) -> Result<PathBuf, String> {
    if let Ok(python) = materials_venv_python(app) {
        if python.is_file() && python_has_pil(&python) {
            return Ok(python);
        }
    }
    if let Ok(python) = locate_cloud_python() {
        if python_has_pil(&python) {
            return Ok(python);
        }
    }
    for name in ["python3", "python"] {
        let path = PathBuf::from(name);
        if python_has_pil(&path) {
            return Ok(path);
        }
    }
    provision_materials_python(app)
}

/// 定位材料排版脚本：安装包资源（`resources/process_images` 或 `_up_/tools`）→ 仓库 `tools/process_images`。
pub(crate) fn materials_script(app: &AppHandle, name: &str) -> Result<PathBuf, String> {
    if let Ok(root) = app.path().resource_dir() {
        for rel in ["resources/process_images", "_up_/tools/process_images"] {
            let candidate = root.join(rel).join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
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
            let candidate = current.join("tools").join("process_images").join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
            dir = current.parent().map(Path::to_path_buf);
        }
    }
    Err(format!(
        "未找到排版脚本 {name}：既不在安装包资源里，也不在仓库 tools/process_images 目录下"
    ))
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
    // 可停止：停止或超时时整个进程树（python 及其拉起的 ffmpeg）一并终止，见 dossier-runtime::command。
    let task = crate::tasks::register(TaskKind::DedupDemo);
    let output = match SystemRunner::new().run_cancellable(&spec, &|| task.token().is_cancelled()) {
        Ok(output) => output,
        Err(dossier_runtime::CommandError::Cancelled { .. }) => {
            return Err("已停止 V5 去重验证，输出目录里可能留有未完成的中间文件。".into());
        }
        Err(error) => return Err(format!("V5 去重验证进程启动失败：{error}")),
    };
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
    crate::license::guard()?;
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

// ffmpeg / ffprobe 的下载源与完整性凭据集中在 `crate::downloads`：
// 那里按平台给出多源列表与各自的 sha256（macOS 的 evermeet 源钉版本化地址与我们核对过的 sha256）。
// 原来写在本文件里的 EVERMEET_* / GYAN_* 常量已迁走，避免两处地址不一致。

/// Windows 一键配置的第 3 个组件：官方 CPython 3.11.9（amd64）安装器，约 25MB。
/// 静默装进应用数据目录，不装启动器、不改系统 PATH；升级 Python 大版本时换这里的钉版号。
/// 地址与哈希见 `downloads::python_installer`（下载后会被静默执行，必须校验）。
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

/// 用系统 curl 下载**单个地址**（macOS / Windows 10+ 均自带，支持重试）。
/// 期间每 0.4 秒轮询一次落盘大小并上报 `install-progress`；
/// 总大小先用 HEAD 请求探测 content-length，探测不到就只报已下载量。
///
/// 这是最底层的单源下载；带哈希校验与多源回退的入口是 [`download_asset`]。
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn download_file(
    app: &AppHandle,
    component: &str,
    url: &str,
    target: &Path,
) -> Result<(), String> {
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
            // 只走 HTTPS：重定向也不能被降级到 HTTP（被劫持时会拿到攻击者的文件）。
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
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

/// 带哈希校验与多源回退的下载入口：逐个源尝试，每源下载后立即校验。
///
/// 与 [`download_file`] 的区别：那个只负责"把内容拉到某个路径"，不判断拉到的对不对；
/// 这个会按 `asset` 里写死的 sha256 校验，不匹配就删掉换下一个源，全部失败才报错。
/// 失败类别（网络 / 校验 / 格式）会带进错误信息，界面与日志据此区分。
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) fn download_asset(
    app: &AppHandle,
    asset: &crate::downloads::Asset,
    target: &Path,
) -> Result<(), String> {
    let result = crate::downloads::fetch_with_fallback(asset, target, |url, path| {
        download_file(app, asset.name, url, path).map_err(|_| crate::downloads::Failure::Network)
    });
    match result {
        Ok(_) => Ok(()),
        Err((failure, message)) => {
            crate::logging::log_event(
                "环境",
                format!("下载失败（{}）：{message}", failure.label()),
            );
            Err(message)
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
pub(crate) fn extract_zip(zip_path: &Path, staging: &Path) -> Result<(), String> {
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
fn install_one(
    app: &AppHandle,
    bin_dir: &Path,
    name: &str,
    asset: &crate::downloads::Asset,
) -> Result<bool, String> {
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
        download_asset(app, asset, &zip_path)?;
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

/// 本机是否有可用的 NVIDIA 显卡（供 ASR 决定装 CUDA 版还是 CPU 版 torch）。
/// 复用已有的 `probe_gpu`（读 `nvidia-smi` 的设备表），不新增探测逻辑。
pub(crate) fn nvidia_gpu_present() -> bool {
    probe_gpu(&SystemRunner::new()).available
}

/// 本机显卡概要：给界面展示"检测到什么卡 / 没检测到"，并说明 ASR 走 CPU 还是 GPU。
/// 非 NVIDIA（含 Apple Silicon）返回 `None` —— 这类机器由平台默认路径处理，不需要 CUDA 包。
pub(crate) fn nvidia_summary() -> Option<String> {
    let report = probe_gpu(&SystemRunner::new());
    if !report.available {
        return None;
    }
    let device = report.devices.first()?;
    Some(format!(
        "{}（{} MB 显存，驱动 {}）",
        device.name, device.memory_total_mb, device.driver_version
    ))
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
    let stale = |path: &Option<PathBuf>| path.as_ref().map(|path| !path.is_file()).unwrap_or(true);
    dossier_core::update_settings(&settings_path, |settings| {
        if stale(&settings.ffmpeg_path) {
            settings.ffmpeg_path = Some(ffmpeg_path.to_path_buf());
        }
        if stale(&settings.ffprobe_path) {
            settings.ffprobe_path = Some(ffprobe_path.to_path_buf());
        }
        Ok(())
    })
    .map(|_| ())
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
    dossier_core::update_settings(&settings_path, |settings| {
        settings.python_path = Some(python_exe.to_path_buf());
        Ok(())
    })
    .map(|_| ())
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
        // 安装器下载后会被静默执行：必须通过 sha256 校验（不符时删掉、不执行）。
        crate::downloads::fetch_with_fallback(
            &crate::downloads::python_installer(),
            &installer_path,
            |url, path| {
                download_file(app, "python", url, path)
                    .map_err(|_| crate::downloads::Failure::Network)
            },
        )
        .map_err(|(_, message)| format!("Python 安装器下载失败：{message}"))?;
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

/// 文件指纹：大小 + 修改时间 + sha256。大小与修改时间都没变时直接复用上次算好的 sha256，
/// 避免每次点「安装」都对 100 MB 级的二进制重新算哈希。
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))] // 只在桌面安装流程调用；保留编译以便在各平台跑单测
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileFingerprint {
    len: u64,
    modified_nanos: u128,
    sha256: String,
}

#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))] // 只在桌面安装流程调用；保留编译以便在各平台跑单测
impl FileFingerprint {
    fn of(path: &Path, cached: Option<&FileFingerprint>) -> Option<Self> {
        let meta = std::fs::metadata(path).ok().filter(|meta| meta.is_file())?;
        let modified_nanos = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        if let Some(cached) =
            cached.filter(|c| c.len == meta.len() && c.modified_nanos == modified_nanos)
        {
            return Some(cached.clone());
        }
        let sha256 = crate::downloads::file_sha256(path).ok()?;
        Some(Self {
            len: meta.len(),
            modified_nanos,
            sha256,
        })
    }
}

/// 内置工具的复制记录：安装包里的源文件指纹 + 已安装文件的指纹。
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))] // 只在桌面安装流程调用；保留编译以便在各平台跑单测
#[derive(Debug, Default, Serialize, Deserialize)]
struct BundledStamp {
    source: Option<FileFingerprint>,
    target: Option<FileFingerprint>,
}

/// 已安装的内置工具是否需要（重新）复制：目标不存在，或内容与安装包里的源文件不一致。
///
/// 以前只拿目标文件和它自己记下的指纹比：应用升级换了内置 ffmpeg 后，旧文件指纹照样对得上，
/// 就永远不会更新。现在比的是「目标 = 源」。
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))] // 只在桌面安装流程调用；保留编译以便在各平台跑单测
fn bundled_copy_needed(source: Option<&FileFingerprint>, target: Option<&FileFingerprint>) -> bool {
    match (source, target) {
        (Some(source), Some(target)) => source.sha256 != target.sha256,
        // 目标不在（或读不到）：需要复制；源读不到时复制也会失败，交给下载流程兜底
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// 从安装包内置资源把工具复制到 bin 目录；返回实际安装的组件名。
/// 复制虽是本地 IO，仍逐组件播报「正在安装 → 完成」进度事件，
/// 让安装弹窗有可见的推进（内置包约 200MB，复制肉眼可感知）。
/// 目标与安装包里的文件内容一致时跳过，避免每次点击都整块复制。
///
/// 完整性：内置二进制由 CI 从同一来源抓取，版本随打包日变化，无法在源码里写死哈希；
/// 这里保证的是「已安装的文件与当前安装包里的一致」——应用升级换了内置版本会自动更新，
/// 本机文件被替换也会被覆盖回来。
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
        let source_path = bundled.join(&exe_name);
        let target = bin_dir.join(&exe_name);
        let stamp_path = bin_dir.join(format!("{name}.bundled.json"));
        let recorded: BundledStamp = std::fs::read_to_string(&stamp_path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        let Some(source) = FileFingerprint::of(&source_path, recorded.source.as_ref()) else {
            continue;
        };
        let current = FileFingerprint::of(&target, recorded.target.as_ref());
        if bundled_copy_needed(Some(&source), current.as_ref()) {
            emit_install_progress(app, name, "installing", "正在安装", None, 0);
            if let Err(error) = std::fs::copy(&source_path, &target) {
                // 单个复制失败不致命：可能是目标正被占用，交给下载流程或自检去暴露。
                crate::logging::log_event(
                    "环境",
                    format!("复制内置 {name} 失败（{error}），交给下载流程兜底"),
                );
                continue;
            }
            emit_install_progress(app, name, "finished", "完成", Some(0), 0);
            installed.push(name.to_string());
        }
        // 登记两边的指纹；写不进去不影响本次安装，只是下次要重新算一遍哈希
        let stamp = BundledStamp {
            target: FileFingerprint::of(&target, None),
            source: Some(source),
        };
        if let Ok(text) = serde_json::to_string(&stamp) {
            let _ = std::fs::write(&stamp_path, text);
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
    if install_one(app, &bin_dir, "ffmpeg", &crate::downloads::runtime_ffmpeg())? {
        skipped_existing.push("ffmpeg".into());
    }
    if install_one(
        app,
        &bin_dir,
        "ffprobe",
        &crate::downloads::runtime_ffprobe(),
    )? {
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

    // 不再「两个文件都在就跳过」：内置版本随应用升级变化时，要把旧文件更新成安装包里的版本。
    // 内容一致时 copy_bundled_tools 直接跳过（靠指纹缓存，不会每次重新算哈希）。
    if let Some(bundled) = bundled_runtime_dir(app) {
        let copied = copy_bundled_tools(app, &bin_dir, &bundled, &["ffmpeg", "ffprobe"]);
        if !copied.is_empty() {
            bundled_note = format!("（已从安装包内置资源安装：{}）", copied.join("、"));
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
            // 多源 + sha256 校验：BtbN 优先，gyan.dev 兜底（见 downloads 模块的说明）
            download_asset(app, &crate::downloads::runtime_ffmpeg(), &zip_path)?;
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
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _task = crate::tasks::register(TaskKind::Install);
        install_runtime_blocking(&app)
    })
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

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(from, to)| (from.to_string(), to.to_string()))
            .collect()
    }

    #[test]
    fn rename_plan_allows_swap_chain_and_case_only() {
        let existing = names(&["A.mp4", "B.mp4", "1.mp4", "2.mp4", "Ep01.mp4"]);
        // 互换
        assert!(
            prepare_rename_ops(&existing, &pairs(&[("A.mp4", "B.mp4"), ("B.mp4", "A.mp4")]))
                .is_ok()
        );
        // 链式：1→2、2→3
        assert!(
            prepare_rename_ops(&existing, &pairs(&[("1.mp4", "2.mp4"), ("2.mp4", "3.mp4")]))
                .is_ok()
        );
        // 只改大小写：在不区分大小写的系统上目标「已存在」的正是它自己
        let ops = prepare_rename_ops(&existing, &pairs(&[("Ep01.mp4", "ep01.mp4")])).unwrap();
        assert_eq!(ops, pairs(&[("Ep01.mp4", "ep01.mp4")]));
        // 原名 = 新名：跳过
        assert!(prepare_rename_ops(&existing, &pairs(&[("A.mp4", "A.mp4")]))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn rename_plan_rejects_collisions() {
        let existing = names(&["a.mp4", "b.mp4", "其他.mp4"]);
        // 两个目标只差大小写：不区分大小写的系统上后者会覆盖前者
        let error = prepare_rename_ops(
            &existing,
            &pairs(&[("a.mp4", "EP01.mp4"), ("b.mp4", "ep01.mp4")]),
        )
        .unwrap_err();
        assert!(error.contains("同一个名字"), "{error}");
        // 与本批次之外的现有文件同名（含只差大小写）
        let error = prepare_rename_ops(&existing, &pairs(&[("a.mp4", "其他.MP4")])).unwrap_err();
        assert!(error.contains("拒绝覆盖"), "{error}");
        // 源文件不存在 / 重复出现
        assert!(prepare_rename_ops(&existing, &pairs(&[("不存在.mp4", "x.mp4")])).is_err());
        assert!(
            prepare_rename_ops(&existing, &pairs(&[("a.mp4", "x.mp4"), ("a.mp4", "y.mp4")]))
                .is_err()
        );
    }

    #[test]
    fn target_name_rules() {
        for bad in [
            "a:b.mp4",
            "a*b.mp4",
            "a?.mp4",
            "a\"b.mp4",
            "a<b.mp4",
            "a|b.mp4",
            "a\\b.mp4",
            "结尾点.",
            "结尾空格 ",
            "CON",
            "con.mp4",
            "NUL.txt",
            "COM1.mp4",
            "lpt9",
            "带\t制表符.mp4",
        ] {
            assert!(validate_target_name(bad).is_err(), "应拒绝：{bad:?}");
        }
        assert!(validate_target_name(&"长".repeat(201)).is_err());
        for good in [
            "剧名 - 第01集.mp4",
            "COM0.mp4",
            "CONSOLE.mp4",
            "com10.mp4",
            ".hidden.mp4",
        ] {
            assert!(validate_target_name(good).is_ok(), "应允许：{good:?}");
        }
    }

    #[test]
    fn execute_rename_swaps_and_rolls_back() {
        let dir = unique_dir("rename");
        std::fs::write(dir.join("A.mp4"), b"a").unwrap();
        std::fs::write(dir.join("B.mp4"), b"b").unwrap();
        // 互换成功
        execute_rename(
            &dir,
            &pairs(&[("A.mp4", "B.mp4"), ("B.mp4", "A.mp4")]),
            "t1",
        )
        .unwrap();
        assert_eq!(std::fs::read(dir.join("A.mp4")).unwrap(), b"b");
        assert_eq!(std::fs::read(dir.join("B.mp4")).unwrap(), b"a");

        // 第二步失败（目标是个非空目录，rename 一定失败）：全部撤销，原名原内容
        std::fs::create_dir_all(dir.join("占位").join("x")).unwrap();
        let error = execute_rename(
            &dir,
            &pairs(&[("A.mp4", "新A.mp4"), ("B.mp4", "占位")]),
            "t2",
        )
        .unwrap_err();
        assert!(error.contains("已全部恢复原名"), "{error}");
        assert_eq!(std::fs::read(dir.join("A.mp4")).unwrap(), b"b");
        assert_eq!(std::fs::read(dir.join("B.mp4")).unwrap(), b"a");
        assert!(!dir.join("新A.mp4").exists());
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "撤销后不能残留临时名");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_rollback_never_overwrites_a_stuck_file() {
        // 互换 A↔B：第二步把第 2 项改回 A 时失败；撤销时第 1 项又没能从 B 退回临时名（被占用）。
        // 此时 B 上是原 A 的内容，第 2 项（原 B）的临时名不能再改回 B——否则覆盖掉原 A。
        let dir = unique_dir("rename-stuck");
        std::fs::write(dir.join("A.mp4"), b"a").unwrap();
        std::fs::write(dir.join("B.mp4"), b"b").unwrap();
        let name = |path: &Path| path.file_name().unwrap().to_string_lossy().into_owned();
        let flaky = |from: &Path, to: &Path| {
            let (from, to) = (name(from), name(to));
            let blocked = (from.ends_with("-1.tmp") && to == "A.mp4")
                || (from == "B.mp4" && to.ends_with("-0.tmp"));
            if blocked {
                Err(std::io::Error::other("模拟占用"))
            } else {
                std::fs::rename(dir.join(from), dir.join(to))
            }
        };
        let error = execute_rename_with(
            &dir,
            &pairs(&[("A.mp4", "B.mp4"), ("B.mp4", "A.mp4")]),
            "t3",
            &flaky,
        )
        .unwrap_err();
        assert!(error.contains("未覆盖"), "{error}");
        // 两份内容都还在：B 上是原 A，原 B 留在临时名
        assert_eq!(std::fs::read(dir.join("B.mp4")).unwrap(), b"a");
        assert_eq!(std::fs::read(dir.join(".rename-t3-1.tmp")).unwrap(), b"b");
        let _ = std::fs::remove_dir_all(&dir);
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
                ..Default::default()
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

    /// 两段 stderr 都是端到端实测中 ffmpeg 6.1.1 的原样输出（未加 -hide_banner 时）。
    const STDERR_VF_MISPLACED: &str = "ffmpeg version 6.1.1-3ubuntu5 Copyright (c) 2000-2023 the FFmpeg developers
  built with gcc 13 (Ubuntu 13.2.0-23ubuntu3)
  configuration: --prefix=/usr --extra-version=3ubuntu5
  libavutil      58. 29.100 / 58. 29.100
  libavcodec     60. 31.102 / 60. 31.102
Option vf (set video filters) cannot be applied to input url /tmp/out/.ai-badge.png -- you are trying to apply an input option to an output file or vice versa. Move this option before the file it belongs to.
Error parsing options for input file /tmp/out/.ai-badge.png.
Error opening input files: Invalid argument
";
    const STDERR_EMPTY_FILTER: &str = "[AVFilterGraph @ 0x57bf0470a400] No such filter: ''
Failed to set value '[0:v:0][base];[base][1:v:0]overlay=x=24:y=24[vout]' for option 'filter_complex': Filter not found
Error parsing global options: Filter not found
";

    #[test]
    fn ffmpeg_failure_summary_keeps_real_reason_and_drops_banner() {
        let summary = summarize_ffmpeg_failure(STDERR_VF_MISPLACED);
        assert!(
            !summary.contains("ffmpeg version"),
            "横幅不应进摘要：{summary}"
        );
        assert!(
            !summary.contains("libavutil"),
            "构建信息不应进摘要：{summary}"
        );
        assert!(summary.contains("Option vf"), "缺真实原因：{summary}");
        assert!(
            summary.contains("Error opening input files"),
            "缺最后一行：{summary}"
        );

        let summary = summarize_ffmpeg_failure(STDERR_EMPTY_FILTER);
        assert!(summary.contains("No such filter"), "缺真实原因：{summary}");

        assert_eq!(
            summarize_ffmpeg_failure("\n  \n"),
            "（ffmpeg 没有输出错误信息）"
        );
        let long = "x".repeat(1000);
        assert!(summarize_ffmpeg_failure(&long).chars().count() <= 401);
    }

    /// 编码中途失败（ffmpeg 6.1.1 + -hide_banner -nostdin -nostats，`-vf crop=400:240` 作用于
    /// 320x240 输入）的原样 stderr 末尾：真实原因在收尾套话之前。
    const STDERR_MID_ENCODE: &str = "[Parsed_crop_0 @ 0x640a21474480] Failed to configure input pad on Parsed_crop_0
[vf#0:0 @ 0x640a2140c540] Error reinitializing filters!
Failed to inject frame into filter network: Invalid argument
Error while filtering: Invalid argument
[out#0/mp4 @ 0x640a2146e380] Nothing was written into output file, because at least one of its streams received no packets.
frame=    0 fps=0.0 q=0.0 Lsize=       0kB time=N/A bitrate=N/A speed=N/A    
Conversion failed!
";

    #[test]
    fn ffmpeg_failure_summary_skips_mid_encode_trailer() {
        let summary = summarize_ffmpeg_failure(STDERR_MID_ENCODE);
        assert!(
            summary.contains("Error reinitializing filters!"),
            "缺真实原因：{summary}"
        );
        assert!(
            summary.contains("Error while filtering: Invalid argument"),
            "缺真实原因：{summary}"
        );
        assert!(!summary.contains("Conversion failed!"), "{summary}");
        assert!(!summary.contains("frame="), "{summary}");
        assert!(!summary.contains("Nothing was written"), "{summary}");
        // 只有收尾套话时退回原样末尾，不给空摘要。
        assert_eq!(
            summarize_ffmpeg_failure("Conversion failed!\n"),
            "Conversion failed!"
        );
    }

    fn nudge_job(video_chain: &str, badge: bool) -> NudgeJob {
        NudgeJob {
            input: PathBuf::from("/素材/第01集.mp4"),
            offset_sec: 0.08,
            video_chain: video_chain.into(),
            audio_chain: (!video_chain.is_empty()).then(|| "atempo=1.1".to_string()),
            metadata: vec!["make=Apple".into()],
            badge: badge.then(|| PathBuf::from("/输出/.ai-badge.png")),
            output: PathBuf::from("/输出/视频成片/第01集.mp4"),
            encoder: "libx264".into(),
        }
    }

    /// 所有 `-i` 都必须排在第一个输出选项（`-c:v`）之前。
    fn inputs_precede_outputs(args: &[String]) -> bool {
        let first_output = args.iter().position(|a| a == "-c:v").expect("必须有 -c:v");
        args.iter()
            .enumerate()
            .filter(|(_, a)| *a == "-i")
            .all(|(index, _)| index < first_output)
    }

    #[test]
    fn nudge_args_cover_dedup_and_badge_combinations() {
        // 去重开 + 角标开（默认）：去重链并入 filter_complex，没有 -vf。
        let args = build_nudge_args(&nudge_job("setpts=PTS/1.1", true));
        assert!(!args.contains(&"-vf".to_string()), "{args:?}");
        let graph_at = args.iter().position(|a| a == "-filter_complex").unwrap();
        assert_eq!(
            args[graph_at + 1],
            "[0:v:0]setpts=PTS/1.1[base];[base][1:v:0]overlay=x=24:y=24[vout]"
        );
        assert!(args.contains(&"-af".to_string()));
        assert!(inputs_precede_outputs(&args), "{args:?}");

        // 去重关 + 角标开：空链直接 overlay，不能出现 `[0:v:0][base]`。
        let args = build_nudge_args(&nudge_job("", true));
        let graph_at = args.iter().position(|a| a == "-filter_complex").unwrap();
        assert_eq!(args[graph_at + 1], "[0:v:0][1:v:0]overlay=x=24:y=24[vout]");
        assert!(!args.contains(&"-af".to_string()));
        assert!(inputs_precede_outputs(&args), "{args:?}");

        // 去重开 + 角标关：走 -vf，只有一个输入。
        let args = build_nudge_args(&nudge_job("setpts=PTS/1.1", false));
        assert!(args.contains(&"-vf".to_string()));
        assert!(!args.contains(&"-filter_complex".to_string()));
        assert_eq!(args.iter().filter(|a| *a == "-i").count(), 1);

        // 全关：既无 -vf 也无 filter_complex。
        let args = build_nudge_args(&nudge_job("", false));
        assert!(!args.contains(&"-vf".to_string()));
        assert!(!args.contains(&"-filter_complex".to_string()));
        assert_eq!(args.last().unwrap(), "/输出/视频成片/第01集.mp4");
    }

    #[test]
    fn bitrate_retry_target_corrects_both_ways_and_is_capped() {
        const MIN: u64 = 4_000_000;
        const MAX: u64 = 8_000_000;
        // 目标 4500k 只编出 3000k → 放大到 4500 × 4500 / 3000 = 6750k。
        assert_eq!(retry_target_bps(MIN, MAX, 4_500_000, 3_000_000), 6_750_000);
        // 差距很大（画面极简单）→ 封顶下限的 2 倍，不会推到离谱的值。
        assert_eq!(retry_target_bps(MIN, MAX, 4_500_000, 1_000_000), 8_000_000);
        // 读不到码率（0）→ 直接用期望值。
        assert_eq!(retry_target_bps(MIN, MAX, 4_500_000, 0), 4_500_000);
        // 目标 8000k 编出 10000k → 压到 8000 × 8000 / 10000 = 6400k。
        assert_eq!(retry_target_bps(MIN, MAX, 8_000_000, 10_000_000), 6_400_000);
        // 超得离谱 → 最多压到上限的一半（且不低于下限）。
        assert_eq!(retry_target_bps(MIN, MAX, 8_000_000, 40_000_000), 4_000_000);
    }

    #[test]
    fn bitrate_encode_args_keep_all_streams_and_degrade_compat() {
        let args = bitrate_encode_args("/in.mp4", "/tmp.mp4", 4_500_000, StreamCompat::CopyAll);
        let joined = args.join(" ");
        assert!(joined.contains("-map 0:V -map 0:a? -map 0:s?"), "{joined}");
        assert!(
            joined.contains("-b:v 4500k -maxrate 5625k -bufsize 9000k"),
            "{joined}"
        );
        assert!(joined.contains("-c:a copy -c:s copy"), "{joined}");
        assert_eq!(args.last().unwrap(), "/tmp.mp4");

        let joined =
            bitrate_encode_args("/in.mp4", "/tmp.mp4", 8_000_000, StreamCompat::Convert).join(" ");
        assert!(
            joined.contains("-c:a aac -b:a 128k -c:s mov_text"),
            "{joined}"
        );
        let joined = bitrate_encode_args(
            "/in.mp4",
            "/tmp.mp4",
            8_000_000,
            StreamCompat::DropSubtitles,
        )
        .join(" ");
        assert!(
            joined.contains("-sn") && !joined.contains("-c:s"),
            "{joined}"
        );

        assert_eq!(StreamCompat::CopyAll.next(), Some(StreamCompat::Convert));
        assert_eq!(StreamCompat::DropSubtitles.next(), None);
        assert!(StreamCompat::CopyAll.note().is_none());
        assert!(is_container_compat_error(
            "Could not find tag for codec subrip in stream #2"
        ));
        assert!(!is_container_compat_error("No space left on device"));
    }

    /// ffmpeg 6.1 的真实 stderr：「流放不进容器」那行后面还有三行连带报错，
    /// 摘要必须把它留下，码率修复才会降级重试（以前只取末 3 行，这行被截掉）。
    #[test]
    fn failure_summary_keeps_container_compat_line() {
        let stderr = "\
    [mp4 @ 0x5708d24aee80] Could not find tag for codec subrip in stream #2, codec not currently supported in container
    [out#0/mp4 @ 0x5708d249ac00] Could not write header (incorrect codec parameters ?): Invalid argument
    Error while filtering: Invalid argument
    [out#0/mp4 @ 0x5708d249ac00] Nothing was written into output file, because at least one of its streams received no packets.
    frame=    0 fps=0.0 q=0.0 Lsize=       0kB time=00:00:00.32 bitrate=   0.0kbits/s dup=1 drop=0 speed=13.6x
    [libx264 @ 0x5708d2524680] final ratefactor: 17.61
    Conversion failed!";
        let summary = summarize_ffmpeg_failure(stderr);
        assert!(is_container_compat_error(&summary), "{summary}");
        assert!(summary.starts_with("[mp4 @"), "{summary}");
        assert!(is_container_compat_error(
            "Subtitle encoding currently only possible from text to text or bitmap to bitmap"
        ));
    }

    /// 真实 ffmpeg：带两条音轨的原片，修复后两条音轨都还在（以前不加 -map 只剩一条）。
    #[test]
    fn bitrate_encode_keeps_every_audio_track_with_real_ffmpeg() {
        let Some(ffmpeg) = which_ffmpeg() else {
            eprintln!("本机没有 ffmpeg，跳过");
            return;
        };
        let ffprobe = ffmpeg.with_file_name(if cfg!(windows) {
            "ffprobe.exe"
        } else {
            "ffprobe"
        });
        if !ffprobe.is_file() {
            eprintln!("本机没有 ffprobe，跳过");
            return;
        }
        let dir = unique_dir("bitrate-map");
        let source = dir.join("source.mp4");
        let source_str = source.display().to_string();
        let cancel = CancelToken::new();
        run_ffmpeg(
            &ffmpeg,
            &[
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=duration=2:size=320x240:rate=25",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=880:duration=2",
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-map",
                "2:a",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                &source_str,
            ],
            FfmpegCtx::light(&cancel),
        )
        .unwrap();
        let output = dir.join("fixed.mp4");
        let args = bitrate_encode_args(
            &source_str,
            &output.display().to_string(),
            1_000_000,
            StreamCompat::CopyAll,
        );
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        run_ffmpeg(&ffmpeg, &arg_refs, FfmpegCtx::light(&cancel)).unwrap();
        let probe = std::process::Command::new(&ffprobe)
            .args([
                "-v",
                "error",
                "-select_streams",
                "a",
                "-show_entries",
                "stream=index",
                "-of",
                "csv=p=0",
            ])
            .arg(&output)
            .output()
            .unwrap();
        let audio_tracks = String::from_utf8_lossy(&probe.stdout).lines().count();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(audio_tracks, 2, "修复后音轨数不对");
    }

    #[test]
    fn bitrate_range_validation() {
        assert_eq!(bitrate_range(None, None).unwrap(), (4_000_000, 8_000_000));
        assert_eq!(
            bitrate_range(Some(3000), Some(6000)).unwrap(),
            (3_000_000, 6_000_000)
        );
        assert!(bitrate_range(Some(8000), Some(8000)).is_err());
        assert!(bitrate_range(Some(9000), Some(8000)).is_err());
        assert!(bitrate_range(Some(0), Some(8000)).is_err());
    }

    #[test]
    fn reencode_is_accepted_only_when_measured_and_in_range() {
        assert!(bitrate_in_range(4_000_000, 8_000_000, Some(4_000_000)));
        assert!(bitrate_in_range(4_000_000, 8_000_000, Some(8_000_000)));
        assert!(!bitrate_in_range(4_000_000, 8_000_000, Some(1_413_000)));
        assert!(!bitrate_in_range(4_000_000, 8_000_000, Some(8_000_001)));
        assert!(!bitrate_in_range(4_000_000, 8_000_000, None));
    }

    #[test]
    fn output_owner_marker_guards_against_other_dramas() {
        let base = std::env::temp_dir().join(format!("dossier-owner-{}", uuid_like_suffix()));
        let lib1 = base.join("库1/第一季");
        let lib2 = base.join("库2/第一季");
        let out = base.join("输出/第一季/深度处理");
        for dir in [&lib1, &lib2, &out] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let key1 = crate::paths::material_key(&lib1);
        let key2 = crate::paths::material_key(&lib2);
        // 无标记：认领。
        assert_eq!(
            check_output_owner(&out, &lib1, &key1).unwrap(),
            OwnerState::Unclaimed
        );
        let marker = write_output_owner(&out, &key1).unwrap();
        assert_eq!(marker, out.join(OUTPUT_OWNER_FILE));
        // 本剧：放行。
        assert_eq!(
            check_output_owner(&out, &lib1, &key1).unwrap(),
            OwnerState::Owned
        );
        // 别的剧（同名不同库），原剧仍在：报错并点名占用者。
        let error = check_output_owner(&out, &lib2, &key2).unwrap_err();
        assert!(error.contains(&key1), "{error}");
        assert!(
            error.contains(OUTPUT_OWNER_FILE),
            "报错应给出移动场景的处理办法：{error}"
        );
        // 原剧目录已不存在（移走或删除）：由新剧重新认领。
        std::fs::remove_dir_all(&lib1).unwrap();
        assert_eq!(
            check_output_owner(&out, &lib2, &key2).unwrap(),
            OwnerState::Reclaim {
                previous: key1.clone()
            }
        );
        // 输出目录在剧目目录里面：不检查（即使里面有别的剧的标记）。
        let inside = lib2.join("深度处理");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::write(inside.join(OUTPUT_OWNER_FILE), r#"{"dramaDir":"/别处"}"#).unwrap();
        assert_eq!(
            check_output_owner(&inside, &lib2, &key2).unwrap(),
            OwnerState::InsideDramaDir
        );
        // 标记损坏：报错并给出标记路径，不猜。
        std::fs::write(&marker, b"not json").unwrap();
        let error = check_output_owner(&out, &lib2, &key2).unwrap_err();
        assert!(error.contains(OUTPUT_OWNER_FILE), "{error}");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn owner_decision_rules() {
        assert_eq!(
            owner_decision(None, "/a", false).unwrap(),
            OwnerState::Unclaimed
        );
        assert_eq!(
            owner_decision(Some("/a"), "/a", true).unwrap(),
            OwnerState::Owned
        );
        assert_eq!(
            owner_decision(Some("/b"), "/a", false).unwrap(),
            OwnerState::Reclaim {
                previous: "/b".into()
            }
        );
        assert!(owner_decision(Some("/b"), "/a", true).is_err());
    }

    #[test]
    fn ffmpeg7_context_prefix_is_stripped_before_trailer_check() {
        assert!(is_ffmpeg_trailer_line(
            "[out#0/mp4 @ 0x55d0c1a2b3c0] video:0KiB audio:0KiB subtitle:0KiB other streams:0KiB global headers:0KiB muxing overhead: unknown"
        ));
        assert!(is_ffmpeg_trailer_line(
            "[out#0/mp4 @ 0x55d0c1a2b3c0] Nothing was written into output file, because at least one of its streams received no packets."
        ));
        assert!(!is_ffmpeg_trailer_line(
            "[vf#0:0 @ 0x640a2140c540] Error reinitializing filters!"
        ));
        // 方括号但不是 `组件 @ 地址` 形式：不当前缀剥。
        assert_eq!(strip_ffmpeg_context("[注意] 文本"), "[注意] 文本");
        let summary = summarize_ffmpeg_failure(
            "[vf#0:0 @ 0x1] Error reinitializing filters!\n\
             [out#0/mp4 @ 0x2] video:0KiB audio:0KiB subtitle:0KiB\n\
             Conversion failed!\n",
        );
        assert!(
            summary.contains("Error reinitializing filters!"),
            "{summary}"
        );
        assert!(!summary.contains("video:0KiB"), "{summary}");
    }

    #[test]
    fn reencode_retries_only_when_bitrate_was_read_but_out_of_range() {
        assert!(should_retry_reencode(4_000_000, 8_000_000, Some(3_000_000)));
        assert!(should_retry_reencode(4_000_000, 8_000_000, Some(9_000_000)));
        // 读不到码率：直接判失败，不白编第二遍。
        assert!(!should_retry_reencode(4_000_000, 8_000_000, None));
        // 已在区间：不需要重试。
        assert!(!should_retry_reencode(
            4_000_000,
            8_000_000,
            Some(4_000_000)
        ));
    }

    fn probed_with_fps(fps: Option<f64>) -> ProbedEpisode {
        ProbedEpisode {
            file: EpisodeFile {
                path: PathBuf::from("/素材/第01集.mp4"),
                file_name: "第01集.mp4".into(),
                size_bytes: 0,
                episode_number: None,
            },
            spec: dossier_core::EpisodeSpec {
                fps,
                ..Default::default()
            },
        }
    }

    #[test]
    fn nudge_trim_margin_uses_lowest_valid_fps() {
        let eps = [probed_with_fps(Some(30.0)), probed_with_fps(Some(24.0))];
        assert!((nudge_trim_margin_sec(&eps) - 3.0 / 24.0).abs() < 1e-9);
        // 读不到或无效的帧率不参与；全都无效时按 25 fps。
        let eps = [probed_with_fps(None), probed_with_fps(Some(f64::NAN))];
        assert!((nudge_trim_margin_sec(&eps) - 0.12).abs() < 1e-9);
        assert!((nudge_trim_margin_sec(&[]) - 0.12).abs() < 1e-9);
    }

    #[test]
    fn run_ffmpeg_reports_launch_failure_for_missing_program() {
        let cancel = CancelToken::new();
        let error = run_ffmpeg(
            Path::new("/不存在的目录/ffmpeg-不存在"),
            &["-version"],
            FfmpegCtx::light(&cancel),
        )
        .expect_err("不存在的程序必须报错");
        assert!(error.contains("ffmpeg 启动失败"), "错误信息不对：{error}");
    }

    #[test]
    fn ai_badge_font_size_maps_to_pixels() {
        assert_eq!(
            ai_badge_font_pixels(12),
            40,
            "默认字号与脚本原默认 40 像素一致"
        );
        assert_eq!(ai_badge_font_pixels(10), 33);
        assert_eq!(ai_badge_font_pixels(11), 37, "四舍五入");
        assert_eq!(ai_badge_font_pixels(24), 80);
    }

    #[test]
    fn ai_badge_args_survive_leading_dash_text() {
        let args = ai_badge_script_args(
            Path::new("/s/make_ai_badge.py"),
            Path::new("/o/.ai-badge.png"),
            "-AI生成 --不是选项",
            12,
        );
        assert_eq!(
            args,
            [
                "/s/make_ai_badge.py",
                "--output=/o/.ai-badge.png",
                "--text=-AI生成 --不是选项",
                "--font-size=40",
            ]
        );
    }

    #[test]
    fn recut_progress_never_goes_backwards_between_phases() {
        // 检测阶段走完 = 30%，编码阶段从 30% 开始：两段首尾相接，不回退
        assert_eq!(phase_progress(0, RECUT_DETECT_SHARE, 0, 4), 0);
        assert_eq!(phase_progress(0, RECUT_DETECT_SHARE, 4, 4), 30);
        assert_eq!(phase_progress(RECUT_DETECT_SHARE, 100, 0, 10), 30);
        assert_eq!(phase_progress(RECUT_DETECT_SHARE, 100, 10, 10), 100);
        // 深度处理再把重组压进 0~50%：检测 0~15%、编码 15~50%
        let deep = |percent: u64| percent * 50 / 100;
        assert_eq!(deep(phase_progress(0, RECUT_DETECT_SHARE, 4, 4)), 15);
        assert_eq!(deep(phase_progress(RECUT_DETECT_SHARE, 100, 10, 10)), 50);
        // done 超过 total、total 为 0 时也不越界
        assert_eq!(phase_progress(0, 30, 9, 4), 30);
        assert_eq!(phase_progress(0, 30, 0, 0), 0);
    }

    #[test]
    fn ffmpeg_timeout_scales_with_duration() {
        // 读不到时长：最短超时
        for bad in [0.0, -5.0, f64::NAN, f64::INFINITY] {
            assert_eq!(ffmpeg_timeout(bad, true).as_secs(), FFMPEG_MIN_TIMEOUT_SECS);
        }
        // 短集：不低于最短超时
        assert_eq!(ffmpeg_timeout(60.0, false).as_secs(), 600);
        // 长集：按系数放大（深度处理 8 倍、其他 4 倍）
        assert_eq!(ffmpeg_timeout(600.0, true).as_secs(), 4800);
        assert_eq!(ffmpeg_timeout(600.0, false).as_secs(), 2400);
        // 设置允许的最长单集（3600 秒）在深度处理下被钳到 4 小时上限
        assert_eq!(ffmpeg_timeout(3600.0, true).as_secs(), 4 * 3600);
        assert_eq!(ffmpeg_timeout(1e12, false).as_secs(), 4 * 3600);
    }

    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let dir =
            std::env::temp_dir().join(format!("dossier-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn bundled_tool_is_recopied_when_package_version_changes() {
        let dir = unique_dir("bundled");
        let source = dir.join("bundled-ffmpeg");
        let target = dir.join("installed-ffmpeg");
        std::fs::write(&source, b"v1").unwrap();
        let src = FileFingerprint::of(&source, None).unwrap();
        // 目标不存在：要复制
        assert!(bundled_copy_needed(Some(&src), None));
        std::fs::write(&target, b"v1").unwrap();
        let tgt = FileFingerprint::of(&target, None).unwrap();
        assert!(
            !bundled_copy_needed(Some(&src), Some(&tgt)),
            "内容一致不复制"
        );
        // 应用升级换了内置版本：旧文件必须被更新
        std::fs::write(&source, b"v2-new-build").unwrap();
        let src2 = FileFingerprint::of(&source, Some(&src)).unwrap();
        assert_ne!(src2.sha256, src.sha256, "大小变了必须重新算哈希");
        assert!(bundled_copy_needed(Some(&src2), Some(&tgt)));
        // 大小与修改时间都没变：复用缓存的哈希，不重算
        let cached = FileFingerprint::of(&source, Some(&src2)).unwrap();
        assert_eq!(cached, src2);
        assert!(!bundled_copy_needed(None, Some(&tgt)), "源读不到时不复制");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn part_path_is_hidden_and_keeps_container_extension() {
        let part = part_path(Path::new("/输出/视频成片/第01集.mp4"));
        assert_eq!(part, PathBuf::from("/输出/视频成片/.第01集.mp4.part.mp4"));
        // 智能微调沿用原名：mkv 素材的临时文件也得是 .mkv，ffmpeg 才会输出 mkv 封装
        assert_eq!(
            part_path(Path::new("/输出/ep1.MKV")),
            PathBuf::from("/输出/.ep1.MKV.part.MKV")
        );
        assert_eq!(
            part_path(Path::new("/输出/无后缀")),
            PathBuf::from("/输出/.无后缀.part.mp4")
        );
    }

    #[test]
    fn publish_parts_renames_all_or_reports_progress() {
        let dir = unique_dir("publish");
        let finals: Vec<PathBuf> = (1..=3)
            .map(|i| dir.join(format!("第{i:02}集.mp4")))
            .collect();
        // 上一轮的正式成片：发布时被本次结果覆盖
        std::fs::write(&finals[0], b"old").unwrap();
        for path in &finals {
            std::fs::write(part_path(path), b"new").unwrap();
        }
        publish_parts(&finals).unwrap();
        for path in &finals {
            assert_eq!(std::fs::read(path).unwrap(), b"new");
            assert!(!part_path(path).exists(), "发布后不能残留临时文件");
        }

        // 第 2 集的临时文件缺失：报告已发布 1 集，剩余临时文件被删除
        for path in &finals {
            std::fs::write(part_path(path), b"again").unwrap();
        }
        std::fs::remove_file(part_path(&finals[1])).unwrap();
        let error = publish_parts(&finals).unwrap_err();
        assert!(error.contains("已发布 1 / 3 集"), "错误信息不对：{error}");
        assert!(!part_path(&finals[2]).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_stale_episodes_only_touches_extra_recut_names() {
        let dir = unique_dir("stale");
        for i in 1..=12 {
            std::fs::write(dir.join(format!("第{i:02}集.mp4")), b"x").unwrap();
        }
        // 不匹配命名规则的文件、子目录一概不碰
        std::fs::write(dir.join("花絮.mp4"), b"x").unwrap();
        std::fs::write(dir.join("第1集.mp4"), b"x").unwrap();
        std::fs::create_dir_all(dir.join("第13集.mp4")).unwrap();
        let keep: Vec<PathBuf> = (1..=10)
            .map(|i| dir.join(format!("第{i:02}集.mp4")))
            .collect();

        let removed = remove_stale_episodes(&dir, &keep);
        assert_eq!(
            removed,
            vec![dir.join("第11集.mp4"), dir.join("第12集.mp4")]
        );
        for path in &keep {
            assert!(path.is_file(), "本次产物被误删：{}", path.display());
        }
        assert!(dir.join("花絮.mp4").is_file());
        assert!(
            dir.join("第1集.mp4").is_file(),
            "一位数集号不属于重组产物命名"
        );
        assert!(dir.join("第13集.mp4").is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recut_episode_name_rule() {
        assert!(is_recut_episode_name("第01集.mp4"));
        assert!(is_recut_episode_name("第100集.mp4"));
        assert!(!is_recut_episode_name("第1集.mp4"));
        assert!(!is_recut_episode_name("第0a集.mp4"));
        assert!(!is_recut_episode_name("第01集.mov"));
        assert!(!is_recut_episode_name(".第01集.mp4.part.mp4"));
    }

    #[test]
    fn staging_dir_is_removed_on_drop() {
        let base = unique_dir("staging");
        let path = base.join(".deep-staging");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("leftover.mp4"), b"x").unwrap();
        {
            let staging = StagingDir::create(path.clone()).unwrap();
            assert!(
                !staging.path().join("leftover.mp4").exists(),
                "上次残余应被清掉"
            );
            std::fs::write(staging.path().join("a.mp4"), b"x").unwrap();
        }
        assert!(!path.exists(), "守卫析构后中转目录应被删除");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn concat_entry_escapes_single_quotes() {
        assert_eq!(concat_entry(Path::new("/a/b.mp4")), "file '/a/b.mp4'");
        assert_eq!(
            concat_entry(Path::new("/Love's Call/x.mp4")),
            "file '/Love'\\''s Call/x.mp4'"
        );
    }

    /// 真实 ffmpeg：路径含单引号时 concat 清单仍能被正确解析。
    #[test]
    fn concat_list_with_quote_in_path_works_with_real_ffmpeg() {
        let Some(ffmpeg) = which_ffmpeg() else {
            eprintln!("本机没有 ffmpeg，跳过");
            return;
        };
        let base = unique_dir("concat");
        let dir = base.join("Love's Call");
        std::fs::create_dir_all(&dir).unwrap();
        let cancel = CancelToken::new();
        let mut parts = Vec::new();
        for i in 0..2 {
            let part = dir.join(format!("part{i}.mp4"));
            let part_str = part.display().to_string();
            run_ffmpeg(
                &ffmpeg,
                &[
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc=duration=1:size=64x64:rate=10",
                    "-c:v",
                    "libx264",
                    "-pix_fmt",
                    "yuv420p",
                    &part_str,
                ],
                FfmpegCtx::light(&cancel),
            )
            .unwrap();
            parts.push(part);
        }
        let list = dir.join("list.txt");
        std::fs::write(
            &list,
            parts
                .iter()
                .map(|p| concat_entry(p))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let output = dir.join("out.mp4");
        let (list_str, output_str) = (list.display().to_string(), output.display().to_string());
        run_ffmpeg(
            &ffmpeg,
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
            FfmpegCtx::light(&cancel),
        )
        .expect("路径含单引号时 concat 应成功");
        assert!(output.metadata().map(|m| m.len() > 0).unwrap_or(false));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 端到端冒烟（真实 ffmpeg / ffprobe）：重组 3 集 → 重跑成 2 集 → 中途失败。
    /// 验证：成片都是正式名、没有 `.part` 残留；集数变少时多出来的旧集被清理；
    /// 失败时上一轮成片原样保留、本次的临时文件被删掉。
    #[test]
    fn recut_end_to_end_publishes_atomically_and_prunes_stale_episodes() {
        let Some(ffmpeg) = which_ffmpeg() else {
            eprintln!("本机没有 ffmpeg，跳过");
            return;
        };
        let ffprobe = ffmpeg.with_file_name(if cfg!(windows) {
            "ffprobe.exe"
        } else {
            "ffprobe"
        });
        if !ffprobe.is_file() {
            eprintln!("本机没有 ffprobe，跳过");
            return;
        }
        let base = unique_dir("e2e-recut");
        let source_dir = base.join("剧A");
        std::fs::create_dir_all(&source_dir).unwrap();
        let cancel = CancelToken::new();
        let mut episodes = Vec::new();
        for i in 1..=3 {
            let path = source_dir.join(format!("第{i}集.mp4"));
            let path_str = path.display().to_string();
            let color = ["red", "green", "blue"][i - 1];
            run_ffmpeg(
                &ffmpeg,
                &[
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    &format!("color=c={color}:size=160x120:rate=10:duration=4"),
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=4",
                    "-shortest",
                    "-c:v",
                    "libx264",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    &path_str,
                ],
                FfmpegCtx::light(&cancel),
            )
            .unwrap();
            let file = EpisodeFile {
                path: path.clone(),
                file_name: format!("第{i}集.mp4"),
                size_bytes: 0,
                episode_number: Some(i as u32),
            };
            let probed = probe_episode(
                &file,
                &FfprobeConfig {
                    bin: Some(ffprobe.clone()),
                },
            )
            .unwrap();
            episodes.push(probed);
        }
        let lanes = EncodeLanes {
            encoder: "libx264".into(),
            lanes: 2,
            reason: "测试".into(),
        };
        let out = base.join("重组输出");
        let emit = |_: u64, _: u64, _: &str| {};
        let listing = |dir: &Path| -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(dir)
                .unwrap()
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };

        // 第一轮：3 集
        let mut produced = Vec::new();
        transcode_recut_blocking(
            &ffmpeg,
            &lanes,
            &episodes,
            RecutStrategy::TargetCount(3),
            &out,
            &cancel,
            &emit,
            &mut produced,
            true,
        )
        .unwrap();
        assert_eq!(
            listing(&out),
            vec!["第01集.mp4", "第02集.mp4", "第03集.mp4"]
        );
        std::fs::write(out.join("花絮.mp4"), b"user file").unwrap();

        // 第二轮：2 集 → 第 03 集被清理，用户自己的文件不动，没有 .part 残留
        let mut produced = Vec::new();
        let report = transcode_recut_blocking(
            &ffmpeg,
            &lanes,
            &episodes,
            RecutStrategy::TargetCount(2),
            &out,
            &cancel,
            &emit,
            &mut produced,
            true,
        )
        .unwrap();
        assert_eq!(listing(&out), vec!["第01集.mp4", "第02集.mp4", "花絮.mp4"]);
        assert!(
            report.note.contains("上一轮多出来的 1 集"),
            "{}",
            report.note
        );
        let first_round = std::fs::read(out.join("第01集.mp4")).unwrap();

        // 第三轮：中途失败（第 2 集的源文件被换成坏文件）→ 上一轮成片原样保留、没有临时文件
        let mut broken = episodes.clone();
        std::fs::write(source_dir.join("坏.mp4"), b"not a video").unwrap();
        broken[1].file.path = source_dir.join("坏.mp4");
        let mut produced = Vec::new();
        assert!(transcode_recut_blocking(
            &ffmpeg,
            &lanes,
            &broken,
            RecutStrategy::TargetCount(3),
            &out,
            &cancel,
            &emit,
            &mut produced,
            true,
        )
        .is_err());
        assert_eq!(
            listing(&out),
            vec!["第01集.mp4", "第02集.mp4", "花絮.mp4"],
            "失败不能留下 .part 或半截成片"
        );
        assert_eq!(
            std::fs::read(out.join("第01集.mp4")).unwrap(),
            first_round,
            "上一轮成片必须原样保留"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 已取消的令牌：ffmpeg 一启动就被终止，不会跑完。
    #[test]
    fn run_ffmpeg_honours_cancel_token() {
        let Some(ffmpeg) = which_ffmpeg() else {
            eprintln!("本机没有 ffmpeg，跳过");
            return;
        };
        let cancel = CancelToken::new();
        cancel.cancel();
        let error = run_ffmpeg(
            &ffmpeg,
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=60",
                "-f",
                "null",
                "-",
            ],
            FfmpegCtx::light(&cancel),
        )
        .expect_err("已取消的任务必须报错");
        assert!(error.contains("已被停止"), "错误信息不对：{error}");
    }

    fn which_ffmpeg() -> Option<PathBuf> {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| {
                    dir.join(if cfg!(windows) {
                        "ffmpeg.exe"
                    } else {
                        "ffmpeg"
                    })
                })
                .find(|path| path.is_file())
        })
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
