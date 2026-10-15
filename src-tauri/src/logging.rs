//! 测试期关键日志：同时写 stderr 与 `~/Documents/剧备日志/dossier.log`。
//!
//! 放在「文稿」目录是为了方便使用者拿到日志（Finder 侧边栏直接可见），
//! 遇到问题时把整个文件发给维护者即可；双击启动打包版看不到 stderr，
//! 所以关键链路（环境检测 / 安装、扫描、探测、生成、账号、挂载、转码）
//! 统一走 [`log_event`]。正式接入 `log`/`tracing` 时可整体替换本模块。

use dossier_core::now_timestamp;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 日志文件位置；应用启动时 `init` 一次，之后只读。
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// 应用启动时调用：确定日志文件位置（`~/Documents/剧备日志/`，找不到家目录时
/// 退回应用配置目录）并记第一条。
pub fn init(fallback_dir: PathBuf) {
    let dir = home_documents().map(|home| home.join("剧备日志"));
    let dir = match dir {
        Some(dir) if std::fs::create_dir_all(&dir).is_ok() => dir,
        _ => fallback_dir,
    };
    let _ = LOG_PATH.set(dir.join("dossier.log"));
    log_event(
        "启动",
        format!(
            "日志初始化完成，文件位置 {}",
            log_file().unwrap_or_default()
        ),
    );
}

/// 日志文件完整路径（未初始化时为 `None`）。
pub fn log_file() -> Option<String> {
    LOG_PATH.get().map(|path| path.display().to_string())
}

/// 记一条关键日志：`[时间] [操作] 说明`。文件写失败静默（测试辅助，不该影响业务）。
pub fn log_event(op: &str, message: impl AsRef<str>) {
    let line = format!("[{}] [{}] {}", now_timestamp(), op, message.as_ref());
    eprintln!("{line}");
    if let Some(path) = LOG_PATH.get() {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{line}");
        }
    }
}

/// 把命令结果转成可读文案（成功带摘要，失败带原因）。
pub fn result_text<T, E: std::fmt::Display>(
    result: &Result<T, E>,
    ok_text: impl FnOnce(&T) -> String,
) -> String {
    match result {
        Ok(value) => ok_text(value),
        Err(error) => format!("失败：{error}"),
    }
}

/// 透传 `Result` 并只在失败时记日志（成功摘要由调用方自行补）。
pub fn log_err<T>(op: &str, result: Result<T, String>) -> Result<T, String> {
    if let Err(error) = &result {
        log_event(op, format!("失败：{error}"));
    }
    result
}

/// 家目录下的「文稿」（Finder 侧边栏可见，方便使用者取日志）。
fn home_documents() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| Path::new(&home).join("Documents"))
}
