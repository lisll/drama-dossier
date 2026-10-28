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

/// 按剧保存材料时用的键：规范化后的剧目目录字符串。
///
/// 同一个目录不论写成 `剧A/`、`./剧A` 还是带 `../`，都收敛成同一个键。
/// Windows 上 `canonicalize` 会返回 `\\?\C:\…` 这种扩展长度前缀（Rust 标准库文档说明它与
/// 多数程序不兼容），落进 settings.json 很难看懂，这里还原成普通写法；键只在本机内部比较，
/// 前后两次规范化结果一致即可。
pub(crate) fn material_key(drama_dir: &Path) -> String {
    let text = normalize(drama_dir).display().to_string();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        text
    }
}

/// 逐层创建目录，并返回**本次新建**的各层（从浅到深）。
///
/// 失败回滚时配合 [`remove_created_dirs`] 使用：只删自己建的，不碰原本就存在的目录。
pub(crate) fn create_dir_all_tracked(path: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut missing: Vec<PathBuf> = path
        .ancestors()
        .take_while(|dir| !dir.as_os_str().is_empty() && !dir.exists())
        .map(Path::to_path_buf)
        .collect();
    missing.reverse();
    std::fs::create_dir_all(path)?;
    Ok(missing)
}

/// 从最深一层往上删除 [`create_dir_all_tracked`] 新建的目录；只删空目录，遇到删不掉的就停
/// （里面还有东西，说明不是本次的空壳，或别处已写入内容）。
pub(crate) fn remove_created_dirs(created: &[PathBuf]) {
    for dir in created.iter().rev() {
        match std::fs::remove_dir(dir) {
            Ok(()) => {}
            // 已被别处整目录删掉（例如停止清理先 remove_dir_all 了最深一层）：继续往上收。
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => break,
        }
    }
}

/// 正在被深度处理或重做海报占用的输出目录（规范化后的路径）。
fn busy_output_dirs() -> &'static std::sync::Mutex<std::collections::HashSet<PathBuf>> {
    static BUSY: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> =
        std::sync::OnceLock::new();
    BUSY.get_or_init(Default::default)
}

/// 输出目录占用凭据：活着期间别的任务领不到同一目录；离开作用域自动释放（含出错与 panic 路径）。
#[derive(Debug)]
pub(crate) struct BusyOutputDir {
    dir: PathBuf,
}

impl Drop for BusyOutputDir {
    fn drop(&mut self) {
        busy_output_dirs()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.dir);
    }
}

/// 领取输出目录：深度处理（含材料线程）与「按新文案重做海报」都要先领，
/// 两者不会同时读写同一份清单、海报与证明。已被占用时返回 `Err(())`，由调用方组织报错文案。
pub(crate) fn claim_output_dir(dir: &Path) -> Result<BusyOutputDir, ()> {
    let dir = normalize(dir);
    let mut busy = busy_output_dirs()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !busy.insert(dir.clone()) {
        return Err(());
    }
    Ok(BusyOutputDir { dir })
}

/// 原子写入：先写到同一目录下的唯一临时文件，再 `rename` 覆盖目标。
///
/// 写到一半失败（磁盘满、被占用）时旧文件原样保留，不会留下截断的海报、清单或标记；
/// 临时文件名带进程号与序号，并发写同一目标时互不踩踏；失败时清掉临时文件。
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other(format!("不是文件路径：{}", path.display())))?;
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_file_name(format!(
        ".{}.{}.{seq}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&temp, bytes).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })?;
    std::fs::rename(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn material_key_converges_spellings_of_the_same_dir() {
        let base = std::env::temp_dir().join(format!("dossier-paths-key-{}", std::process::id()));
        let drama = base.join("剧A");
        std::fs::create_dir_all(&drama).unwrap();
        let plain = material_key(&drama);
        assert_eq!(material_key(&base.join("剧A/")), plain);
        assert_eq!(
            material_key(&base.join("剧A").join("..").join("剧A")),
            plain
        );
        assert_eq!(material_key(&base.join(".").join("剧A")), plain);
        assert!(!plain.starts_with(r"\\?\"), "不应带扩展长度前缀：{plain}");
        assert_ne!(material_key(&base.join("剧B")), plain);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn output_dir_claim_is_exclusive_and_released_on_drop() {
        let base = std::env::temp_dir().join(format!("dossier-busy-{}", std::process::id()));
        let a = base.join("甲/深度处理");
        let b = base.join("乙/深度处理");
        let first = claim_output_dir(&a).expect("第一次领取应成功");
        assert!(claim_output_dir(&a).is_err(), "同一目录不能同时领取");
        // 写法不同但是同一目录：同样被拒。
        assert!(claim_output_dir(&base.join("甲/./深度处理/")).is_err());
        let other = claim_output_dir(&b).expect("不同目录互不影响");
        drop(first);
        let again = claim_output_dir(&a).expect("释放后可以再领");
        drop((again, other));
    }

    #[test]
    fn write_atomic_replaces_target_without_leaving_temp_files() {
        let base = std::env::temp_dir().join(format!("dossier-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let target = base.join("剧目海报.png");
        write_atomic(&target, b"first").unwrap();
        write_atomic(&target, b"second").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"second");
        let leftovers: Vec<_> = std::fs::read_dir(&base)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "残留临时文件：{leftovers:?}");
        // 目录不存在：报错，旧文件（这里没有）不受影响，也不留临时文件。
        assert!(write_atomic(&base.join("不存在/x.json"), b"x").is_err());
        assert!(!base.join("不存在").exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn tracked_dirs_are_removed_only_up_to_first_non_empty_level() {
        let base =
            std::env::temp_dir().join(format!("dossier-paths-tracked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let deep = base.join("a").join("b").join("c");
        let created = create_dir_all_tracked(&deep).unwrap();
        assert_eq!(
            created,
            vec![base.join("a"), base.join("a/b"), base.join("a/b/c")]
        );
        // 中间层放了别的文件：只删到它为止。
        std::fs::write(base.join("a/keep.txt"), b"x").unwrap();
        remove_created_dirs(&created);
        assert!(!base.join("a/b").exists());
        assert!(base.join("a/keep.txt").exists());
        // 已存在的目录不算本次新建。
        assert!(create_dir_all_tracked(&base.join("a")).unwrap().is_empty());
        // 最深一层已被整目录删除：继续往上收掉空壳。
        std::fs::remove_file(base.join("a/keep.txt")).unwrap();
        let created = create_dir_all_tracked(&base.join("a/x/y")).unwrap();
        assert_eq!(created, vec![base.join("a/x"), base.join("a/x/y")]);
        std::fs::remove_dir_all(base.join("a/x/y")).unwrap();
        remove_created_dirs(&created);
        assert!(!base.join("a/x").exists());
        let _ = std::fs::remove_dir_all(base);
    }
}
