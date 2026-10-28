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
/// 串行化写日志与轮转：多个线程同时写时不会在轮转瞬间互相覆盖。
static WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// 日志超过这个大小就轮转为 `dossier.log.1`（只保留一份旧日志），避免无限增长。
const ROTATE_BYTES: u64 = 10 * 1024 * 1024;
/// 读日志尾部时每次往回读的块大小。
const TAIL_CHUNK: u64 = 64 * 1024;

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

/// 读日志尾部 N 行（供界面「运行日志」面板展示与一键复制；对应 Grove 的
/// 可复制日志面板）。文件不存在或未初始化时返回空。
pub fn read_recent_logs(limit: usize) -> Vec<String> {
    let Some(path) = LOG_PATH.get() else {
        return Vec::new();
    };
    tail_lines(path, limit).unwrap_or_default()
}

/// 从文件尾部按块往回读，直到凑够 `limit` 行：不再把整个日志读进内存。
fn tail_lines(path: &Path, limit: usize) -> std::io::Result<Vec<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let mut start = len;
    let mut buffer: Vec<u8> = Vec::new();
    // 多读一行：最前面那一行可能是被块边界截断的半行
    while start > 0 && buffer.iter().filter(|b| **b == b'\n').count() <= limit {
        let step = TAIL_CHUNK.min(start);
        start -= step;
        file.seek(SeekFrom::Start(start))?;
        let mut chunk = vec![0_u8; step as usize];
        file.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&buffer);
        buffer = chunk;
    }
    let text = String::from_utf8_lossy(&buffer);
    let mut lines: Vec<&str> = text.lines().collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    let from = lines.len().saturating_sub(limit);
    Ok(lines[from..].iter().map(|line| line.to_string()).collect())
}

/// 日志里的邮箱打码成 `a***@example.com`：日志面板未登录也能打开，也常被整份发给维护者。
pub fn mask_email(email: &str) -> String {
    match email.trim().split_once('@') {
        Some((local, domain)) if !local.is_empty() && !domain.is_empty() => {
            let first: String = local.chars().take(1).collect();
            format!("{first}***@{domain}")
        }
        _ => "***".to_string(),
    }
}

/// 写入前检查大小，超过上限就把当前文件改名为 `.1`（覆盖更旧的那份）。
fn rotate_if_needed(path: &Path) {
    let too_big = std::fs::metadata(path).is_ok_and(|meta| meta.len() >= ROTATE_BYTES);
    if too_big {
        let mut rotated = path.as_os_str().to_owned();
        rotated.push(".1");
        let _ = std::fs::rename(path, PathBuf::from(rotated));
    }
}

/// 记一条关键日志：`[时间] [操作] 说明`。文件写失败静默（测试辅助，不该影响业务）。
pub fn log_event(op: &str, message: impl AsRef<str>) {
    let line = format!("[{}] [{}] {}", now_timestamp(), op, message.as_ref());
    eprintln!("{line}");
    if let Some(path) = LOG_PATH.get() {
        let _guard = WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        rotate_if_needed(path);
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

/// 家目录下的「文稿」（Finder / 资源管理器侧边栏可见，方便使用者取日志）。
/// macOS/Linux 用 `HOME`；Windows 一般不设 `HOME`，用 `USERPROFILE`。
fn home_documents() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(Path::new(&home).join("Documents"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_email_keeps_only_first_char_and_domain() {
        assert_eq!(mask_email("alice@example.com"), "a***@example.com");
        assert_eq!(mask_email("张三@例子.cn"), "张***@例子.cn");
        assert_eq!(mask_email("x@y.z"), "x***@y.z");
        assert_eq!(mask_email("不是邮箱"), "***");
        assert_eq!(mask_email("@nolocal.com"), "***");
    }

    #[test]
    fn tail_matches_full_read_across_chunk_boundaries() {
        let dir = std::env::temp_dir().join(format!("dossier-logtail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dossier.log");
        // 足够长、跨过多个 64 KB 块的日志（含中文多字节字符）
        let content: String = (0..20_000)
            .map(|i| format!("[时间] [操作] 第 {i} 行日志内容\n"))
            .collect();
        std::fs::write(&path, &content).unwrap();
        let expected: Vec<String> = content.lines().map(str::to_string).collect();
        for limit in [1, 200, 5_000, 30_000] {
            let tail = tail_lines(&path, limit).unwrap();
            let want = &expected[expected.len().saturating_sub(limit)..];
            assert_eq!(tail, want, "limit={limit}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_moves_large_log_aside() {
        let dir = std::env::temp_dir().join(format!("dossier-logrotate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dossier.log");
        std::fs::write(&path, vec![b'x'; ROTATE_BYTES as usize]).unwrap();
        rotate_if_needed(&path);
        assert!(!path.exists());
        assert!(dir.join("dossier.log.1").is_file());
        // 小文件不轮转
        std::fs::write(&path, b"small").unwrap();
        rotate_if_needed(&path);
        assert!(path.is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
