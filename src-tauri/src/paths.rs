//! 桌面层的路径工具：比较用的归一化，以及在系统文件管理器里打开目录。
//!
//! 为什么不放在 `dossier-core`：这些函数**会碰文件系统**（`canonicalize`、`spawn`），
//! 而 `dossier-core::tools` 的约定是「只做纯计算」。规则本身（该怎么判定冲突）
//! 仍然是 core 里的纯函数 `same_path` / `output_dir_hits_sources`，这里只负责
//! 把路径变成可以比较、可以交给系统打开的形式。

use std::path::{Path, PathBuf};
use std::process::Command;

/// 把路径归一化成可比较的绝对形式。
///
/// 先尝试整条 `canonicalize`；失败（输出目录通常还不存在）就向上找到最近的已存在
/// 祖先做 `canonicalize`，再把剩下的成分原样拼回去——这样 `../`、大小写、
/// 结尾分隔符这些写法差异都能收敛到同一个结果。
pub(crate) fn normalize(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut current = path;
    loop {
        let Some(parent) = current.parent() else {
            return path.to_path_buf();
        };
        if parent.as_os_str().is_empty() {
            return path.to_path_buf();
        }
        if let Some(name) = current.file_name() {
            suffix.push(name.to_os_string());
        }
        if let Ok(real) = parent.canonicalize() {
            let mut result = real;
            for part in suffix.iter().rev() {
                result.push(part);
            }
            return result;
        }
        current = parent;
    }
}

/// 平台对应的「打开文件管理器」命令；不支持的平台返回 `None`。
fn opener_command() -> Option<Command> {
    #[cfg(target_os = "windows")]
    {
        Some(Command::new("explorer"))
    }
    #[cfg(target_os = "macos")]
    {
        Some(Command::new("open"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Some(Command::new("xdg-open"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    {
        None
    }
}

/// 在系统文件管理器里打开 `path`（目录本身，或文件所在目录）。
///
/// 只做「打开」这一个动作：不猜路径用途、不拼 shell 字符串、参数以 argv 传递。
/// Windows 的 `explorer` 即使成功也会返回非 0 退出码，所以这里只 `spawn`，
/// 不检查退出状态；失败信息返回给界面提示。
pub(crate) fn open_in_file_manager(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("路径为空，无法打开。".into());
    }
    if !path.exists() {
        return Err(format!("路径不存在：{}", path.display()));
    }
    let target: PathBuf = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.to_path_buf())
    };
    let Some(mut command) = opener_command() else {
        return Err("当前平台暂不支持打开文件管理器。".into());
    };
    command.arg(&target);
    match command.spawn() {
        Ok(_) => Ok(()),
        Err(error) => Err(format!(
            "无法打开文件管理器（{}）：{error}。请手动打开该目录。",
            target.display()
        )),
    }
}
