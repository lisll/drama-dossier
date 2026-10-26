//! 桌面端擦除入口：把 `dossier-inpaint` 这条命令行管线接到界面上。
//!
//! 命令层只做三件事：定位可执行文件、调用库里的参数拼装、跑进程并回传结论。
//! **参数规则与确认流语义全部在 `dossier-inpaint::erase` 里**（并有单元测试覆盖），
//! 这一层不做任何业务判断，只把错误转成中文提示。
//!
//! 注意：本模块依赖 Tauri 系统库（Linux 上需要 `glib >= 2.70`），在装不了桌面壳的
//! 机器上无法编译验证，只做人工审阅 + `rustfmt` 语法校验。擦除管线的核心逻辑
//! （参数拼装 / 退出码语义 / 工具定位）都在 `crates/dossier-inpaint` 里单测过。

use std::path::{Path, PathBuf};

use dossier_core::same_path;
use dossier_inpaint::erase::{
    build_plan, resolve_tool, run_plan_with_progress, ErasePlan, EraseRequest, EraseRun,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::paths::normalize;

/// 擦除工具的可用状态，供界面在启动时展示。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EraseStatus {
    pub available: bool,
    pub tool: Option<String>,
    pub message: String,
}

/// 擦除进度的界面负载（`erase-progress` 事件）。
///
/// 直接对应进度流里一条 `TaskProgress`：`stageIndex/stageTotal` 给「第几段」，
/// `unitDone/unitTotal` 给「这一段跑到第几帧」。擦除的进度单位是**帧**，
/// 与最终报告里的 `framesOut` 同源。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EraseProgress {
    stage: String,
    stage_index: u32,
    stage_total: u32,
    unit_done: u64,
    unit_total: u64,
    message: String,
    /// 终态事件（finished / failed）：界面据此收尾，不再等下一次轮询。
    terminal: bool,
    failed: bool,
}

/// 本次运行的进度流落点。
///
/// 放在系统临时目录里，而不是输出目录旁边：它只是「跑到哪了」的实时快照，
/// 结论在 `<输出>.inpaint-report.json` 里，跑完就删（失败也删），不该出现在使用者的素材/产物目录里。
///
/// 路径**按输出文件名派生**，不掺时间戳或随机数：这样「预览命令」和真正执行拼出的是同一个路径，
/// 预览才算名副其实的「将要执行的命令」。
///
/// 代价是同名不同目录的输出会共用同一个文件——界面同一时刻只允许一个长任务
/// （`beginTask` 会拦住第二次点击），所以这条不会真的撞上；换掉它反而不划算。
fn erase_progress_path(output: &Path) -> PathBuf {
    let stem = output
        .file_stem()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "erase".to_string());
    std::env::temp_dir().join(format!("dossier-erase-progress-{stem}.jsonl"))
}

/// 开发期仓库根目录（用于在没装到 PATH 时找到 `target/` 里的构建产物）。
///
/// 这是编译期常量，打包后的安装版通常不存在这个目录——那时的正确做法是让使用者在
/// 界面上指定工具路径，或在 PATH 里安装 `dossier-inpaint`。
fn dev_repo_root() -> Option<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
}

fn locate(tool_path: Option<&str>) -> Result<PathBuf, String> {
    let explicit = tool_path
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    resolve_tool(explicit.as_deref(), dev_repo_root().as_deref())
}

/// 选择视频文件（擦除的输入）。
#[tauri::command]
pub async fn pick_video() -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        rfd::FileDialog::new()
            .set_title("选择要处理的视频（仅限自有素材）")
            .add_filter("视频", &["mp4", "mov", "mkv", "flv", "ts", "m4v", "webm"])
            .pick_file()
            .map(|path| path.display().to_string())
    })
    .await
    .map_err(|error| format!("文件选择任务失败：{error}"))
}

/// 查询擦除工具是否可用。
#[tauri::command]
pub fn erase_status(tool_path: Option<String>) -> EraseStatus {
    match locate(tool_path.as_deref()) {
        Ok(tool) => EraseStatus {
            available: true,
            tool: Some(tool.display().to_string()),
            message: format!("已找到擦除工具：{}", tool.display()),
        },
        Err(message) => EraseStatus {
            available: false,
            tool: None,
            message,
        },
    }
}

/// 输入与输出指向同一个文件时会边读边写、直接毁掉源素材；预览与执行前都先拦住。
fn ensure_distinct_io(request: &EraseRequest) -> Result<(), String> {
    if same_path(&normalize(&request.input), &normalize(&request.output)) {
        return Err(format!(
            "输出视频不能是输入视频本身（{}）：那会覆盖原素材。请换一个输出路径。",
            request.input.display()
        ));
    }
    Ok(())
}

/// 只拼装命令，不执行——界面据此展示「将要运行什么」。
///
/// 这里也要挂上 `--progress-json`：预览的意义就是「和真正执行的是同一条命令」，
/// 少一个参数就不是同一条了。
#[tauri::command]
pub fn plan_erase(
    app: AppHandle,
    mut request: EraseRequest,
    tool_path: Option<String>,
    ffprobe_path: Option<String>,
) -> Result<ErasePlan, String> {
    crate::license::guard()?;
    ensure_distinct_io(&request)?;
    request.progress_json = Some(erase_progress_path(&request.output));
    // 预览里的命令必须和真正执行的同一条：ffprobe 也在这一步解析好填进去。
    request.ffprobe = Some(
        crate::toolbox::resolve_ffprobe(&app, ffprobe_path.as_deref())?
            .display()
            .to_string(),
    );
    // Python 解释器同理：设置里配过（Windows 一键配置装好）或 PATH 上有就注入，
    // 没有则交由 dossier-inpaint 的校验给出提示（不在这里硬失败）。
    if request.worker_program.is_none() {
        if let Ok(python) = crate::toolbox::resolve_python(&app) {
            request.worker_program = Some(python.display().to_string());
        }
    }
    let tool = locate(tool_path.as_deref())?;
    build_plan(&request, &tool)
}

/// 执行擦除并按确认流退出码回报结论。
///
/// **必须是 async + `spawn_blocking`**：擦除要跑几分钟到几十分钟，而同步命令是在
/// IPC 处理线程上直接调用的——那样会卡住主线程，进度事件也排到进程结束才送出去，
/// 进度条就白做了。工具箱的转码走的是同一条路子（`transcode_execute`）。
#[tauri::command]
pub async fn run_erase(
    app: AppHandle,
    request: EraseRequest,
    tool_path: Option<String>,
    ffprobe_path: Option<String>,
    timeout_seconds: Option<u64>,
) -> Result<EraseRun, String> {
    crate::license::guard()?;
    tauri::async_runtime::spawn_blocking(move || {
        run_erase_blocking(&app, request, tool_path, ffprobe_path, timeout_seconds)
    })
    .await
    .map_err(|error| format!("擦除任务失败：{error}"))?
}

/// 真正干活的那一半：给工具挂上 `--progress-json`，边跑边把进度流里的新事件
/// 转成 `erase-progress` 推给界面。
fn run_erase_blocking(
    app: &AppHandle,
    mut request: EraseRequest,
    tool_path: Option<String>,
    ffprobe_path: Option<String>,
    timeout_seconds: Option<u64>,
) -> Result<EraseRun, String> {
    if !request.input.is_file() {
        return Err(format!("输入视频不存在：{}", request.input.display()));
    }
    ensure_distinct_io(&request)?;
    if let Some(parent) = request.output.parent() {
        if !parent.as_os_str().is_empty() && !parent.is_dir() {
            return Err(format!("输出目录不存在：{}", parent.display()));
        }
    }
    let progress_path = erase_progress_path(&request.output);
    request.progress_json = Some(progress_path.clone());
    // GUI 子进程的 PATH 里几乎肯定没有 ffprobe（用户环境里的 9.0.2-tessus 也是这么被
    // 探测到的），所以必须把应用解析好的绝对路径显式交给管线。
    request.ffprobe = Some(
        crate::toolbox::resolve_ffprobe(app, ffprobe_path.as_deref())?
            .display()
            .to_string(),
    );
    // Python 解释器同理：与 plan_erase 保持同一条命令，注入逻辑一致。
    if request.worker_program.is_none() {
        if let Ok(python) = crate::toolbox::resolve_python(app) {
            request.worker_program = Some(python.display().to_string());
        }
    }

    let tool = locate(tool_path.as_deref())?;
    let plan = build_plan(&request, &tool)?;
    let result = run_plan_with_progress(&plan, timeout_seconds.unwrap_or(1800), |event| {
        let _ = app.emit(
            "erase-progress",
            EraseProgress {
                stage: event.progress.stage.clone(),
                stage_index: event.progress.stage_index,
                stage_total: event.progress.stage_total,
                unit_done: event.progress.unit_done,
                unit_total: event.progress.unit_total,
                message: event.progress.message.clone(),
                terminal: event.is_terminal(),
                failed: matches!(event.kind, dossier_core::ProgressEventKind::Failed),
            },
        );
    });
    let _ = std::fs::remove_file(&progress_path);
    result
}
