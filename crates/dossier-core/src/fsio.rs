//! 写进用户目录 / 输出目录时用的文件原语。
//!
//! 素材目录与输出目录不归本程序独占：里面可能预先放着符号链接。这里的两个函数保证
//! 写入与清理都作用在**路径本身**，不会顺着链接写到 / 删到别处。

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// 临时文件名被占用时最多换几个序号。
const TEMP_ATTEMPTS: u32 = 16;

/// 原子写入：先写到同目录下的独占临时文件，再 `rename` 覆盖目标。
///
/// - 临时文件用 `create_new` 创建（O_EXCL）：同名文件或链接（包括悬空链接）已存在时直接失败、
///   换下一个序号，不会顺着预先放好的链接写到别处；
/// - `rename` 替换的是目标路径本身：目标是链接时换掉的是链接，链接指向的文件不受影响；
/// - 写到一半失败时旧文件原样保留，临时文件被清掉；
/// - 不创建目录：父目录不存在时报错（调用方负责建目录）。
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    write_atomic_with(path, bytes, &mut || SEQ.fetch_add(1, Ordering::Relaxed))
}

/// 临时文件名：点开头、`.tmp` 结尾（扫描器跳过点开头的文件，残留检查认 `.tmp`）。
fn temp_name(name: &str, seq: u64) -> String {
    format!(".{name}.{}.{seq}.tmp", std::process::id())
}

/// [`write_atomic`] 的实现；序号来源可注入，测试据此精确预占临时文件名。
fn write_atomic_with(
    path: &Path,
    bytes: &[u8],
    next_seq: &mut dyn FnMut() -> u64,
) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other(format!("不是文件路径：{}", path.display())))?
        .to_string_lossy()
        .into_owned();
    for _ in 0..TEMP_ATTEMPTS {
        let temp = path.with_file_name(temp_name(&name, next_seq()));
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => file,
            // 名字被占（同名文件或链接，含悬空链接）：换下一个序号，绝不打开它。
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let written = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file);
        if let Err(error) = written.and_then(|()| std::fs::rename(&temp, path)) {
            let _ = std::fs::remove_file(&temp);
            return Err(error);
        }
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!(
            "{} 旁的临时文件名连续 {TEMP_ATTEMPTS} 个都被占用",
            path.display()
        ),
    ))
}

/// 编码 / 渲染前清掉同名的旧临时文件：上次崩溃留下的残件，或别人预先放好的链接。
///
/// - 不存在：成功；
/// - 普通文件或链接：删除路径本身（删的是链接，不跟随）；
/// - 真实目录：报错，不删（不是我们留下的东西）；
/// - 其他错误一律上报。
pub fn clear_stale_temp(path: &Path) -> io::Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if meta.is_dir() {
        return Err(io::Error::other(format!(
            "临时文件位置被一个目录占用，未删除：{}",
            path.display()
        )));
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        // Windows 上指向目录的符号链接要用 remove_dir 删（删的仍是链接本身）。
        Err(error) if meta.file_type().is_symlink() => std::fs::remove_dir(path).map_err(|_| error),
        Err(error) => Err(error),
    }
}

/// 路径上是否已经有东西（包括悬空链接）。`Path::exists` 会跟随链接，悬空链接会被当成"不存在"。
pub fn path_occupied(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_base(tag: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        let base = std::env::temp_dir().join(format!("dossier-fsio-{tag}-{unique}"));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect()
    }

    #[test]
    fn write_atomic_replaces_and_leaves_no_temp() {
        let base = temp_base("replace");
        let target = base.join("清单.json");
        write_atomic(&target, b"first").unwrap();
        write_atomic(&target, b"second").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"second");
        assert!(leftovers(&base).is_empty());
        assert!(write_atomic(&base.join("不存在/x.json"), b"x").is_err());
        assert!(!base.join("不存在").exists());
        std::fs::remove_dir_all(base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_replaces_link_itself_not_its_target() {
        use std::os::unix::fs::symlink;
        let base = temp_base("symlink");
        let victim = base.join("victim.txt");
        std::fs::write(&victim, b"original").unwrap();
        let target = base.join("挂载文案.csv");
        symlink(&victim, &target).unwrap();
        write_atomic(&target, b"new").unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"original");
        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(!std::fs::symlink_metadata(&target)
            .unwrap()
            .file_type()
            .is_symlink());
        std::fs::remove_dir_all(base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_skips_temp_names_occupied_by_links() {
        use std::os::unix::fs::symlink;
        let base = temp_base("occupied");
        let victim = base.join("victim.txt");
        std::fs::write(&victim, b"original").unwrap();
        let target = base.join("c.csv");
        // 前 3 个序号预先放成链接（一个指向受害文件，一个悬空，一个指向受害文件）。
        symlink(&victim, base.join(temp_name("c.csv", 0))).unwrap();
        symlink(base.join("nowhere"), base.join(temp_name("c.csv", 1))).unwrap();
        symlink(&victim, base.join(temp_name("c.csv", 2))).unwrap();
        let mut seq = 0;
        write_atomic_with(&target, b"final", &mut || {
            seq += 1;
            seq - 1
        })
        .unwrap();
        assert_eq!(seq, 4, "跳过 3 个被占的名字，第 4 个写成功");
        assert_eq!(std::fs::read(&target).unwrap(), b"final");
        assert_eq!(std::fs::read(&victim).unwrap(), b"original");
        assert!(
            !base.join("nowhere").exists(),
            "悬空链接不能被顺着创建出目标"
        );

        // 16 个序号全被占：报错，目标不变，受害文件不变。
        for n in 100..116 {
            symlink(&victim, base.join(temp_name("c.csv", n))).unwrap();
        }
        let mut seq = 100;
        assert!(write_atomic_with(&target, b"again", &mut || {
            seq += 1;
            seq - 1
        })
        .is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"final");
        assert_eq!(std::fs::read(&victim).unwrap(), b"original");
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn clear_stale_temp_handles_missing_file_and_dir() {
        let base = temp_base("clear");
        let missing = base.join(".x.part");
        clear_stale_temp(&missing).unwrap();
        let file = base.join(".y.part");
        std::fs::write(&file, b"old").unwrap();
        clear_stale_temp(&file).unwrap();
        assert!(!path_occupied(&file));
        let dir = base.join(".z.part");
        std::fs::create_dir(&dir).unwrap();
        assert!(clear_stale_temp(&dir).is_err());
        assert!(dir.is_dir(), "真实目录不能被删");
        std::fs::remove_dir_all(base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn clear_stale_temp_removes_link_not_target() {
        use std::os::unix::fs::symlink;
        let base = temp_base("clear-link");
        let victim = base.join("victim.txt");
        std::fs::write(&victim, b"original").unwrap();
        let link = base.join(".ep.mp4.part");
        symlink(&victim, &link).unwrap();
        clear_stale_temp(&link).unwrap();
        assert!(!path_occupied(&link));
        assert_eq!(std::fs::read(&victim).unwrap(), b"original");
        // 悬空链接也算"已占用"，也能清掉。
        let dangling = base.join(".dangling.part");
        symlink(base.join("nowhere"), &dangling).unwrap();
        assert!(path_occupied(&dangling));
        assert!(!dangling.exists());
        clear_stale_temp(&dangling).unwrap();
        assert!(!path_occupied(&dangling));
        std::fs::remove_dir_all(base).ok();
    }
}
