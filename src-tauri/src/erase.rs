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
    build_plan, resolve_tool, run_plan, ErasePlan, EraseRequest, EraseRun,
};
use serde::Serialize;

use crate::paths::normalize;

/// 擦除工具的可用状态，供界面在启动时展示。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EraseStatus {
    pub available: bool,
    pub tool: Option<String>,
    pub message: String,
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
#[tauri::command]
pub fn plan_erase(request: EraseRequest, tool_path: Option<String>) -> Result<ErasePlan, String> {
    ensure_distinct_io(&request)?;
    let tool = locate(tool_path.as_deref())?;
    build_plan(&request, &tool)
}

/// 执行擦除并按确认流退出码回报结论。
#[tauri::command]
pub fn run_erase(
    request: EraseRequest,
    tool_path: Option<String>,
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
    let tool = locate(tool_path.as_deref())?;
    let plan = build_plan(&request, &tool)?;
    run_plan(&plan, timeout_seconds.unwrap_or(1800))
}
