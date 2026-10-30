//! 报审包生成流水：本机 JSONL 追加写 + 时间倒序读取。
//!
//! 用途对应竞品「设置-账单」页：本地版记录每一次生成的时间、剧目、集数、
//! 包路径与大小，作为「生成历史 / 消耗流水」的演示数据源；云端版将改为
//! 从服务端拉取积分流水，本模块保留为离线兜底。

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// 当前本地时间（ISO-8601 带时区偏移），供桌面层写记录时间戳；
/// 放在这里让各入口共用同一格式（字符串倒序即可当时间排序）。
pub fn now_timestamp() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// 单条生成流水记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerRecord {
    /// 记录时间（本地时区，ISO-8601 带时区偏移）。
    pub ts: String,
    pub drama_name: String,
    pub episode_count: usize,
    /// 生成的报审包目录。
    pub package_path: String,
    /// 报审包目录总大小（字节）。
    pub size_bytes: u64,
}

/// 追加一条流水记录到 JSONL 文件；文件不存在时自动创建（含父目录）。
/// 若上次写入中途失败留下未断行的半行，先补一个换行把它隔离，避免吞掉新记录。
pub fn append_record(path: &Path, record: &LedgerRecord) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)?;
    if file.seek(SeekFrom::End(-1)).is_ok() {
        let mut tail = [0u8; 1];
        if file.read_exact(&mut tail).is_ok() && tail[0] != b'\n' {
            file.write_all(b"\n")?;
        }
    }
    let mut line = serde_json::to_string(record).map_err(std::io::Error::other)?;
    line.push('\n');
    file.write_all(line.as_bytes())
}

/// 读取全部流水记录，按时间倒序（最新在前）返回。
/// 单行损坏时跳过该行（JSONL 追加写在极端断电下可能出现半行）。
pub fn list_records(path: &Path) -> std::io::Result<Vec<LedgerRecord>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    // 整份读入后宽松解码：一行坏字节（非 UTF-8）只影响那一行。按行读的 `lines()` 遇到非 UTF-8
    // 会返回 Err，以前用 map_while 一停就把后面所有记录都丢了。
    let bytes = std::fs::read(path)?;
    let mut records: Vec<LedgerRecord> = String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    records.sort_by(|a, b| b.ts.cmp(&a.ts));
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|v| v.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("dossier-ledger-{tag}-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("ledger.jsonl")
    }

    fn record(ts: &str, name: &str) -> LedgerRecord {
        LedgerRecord {
            ts: ts.to_string(),
            drama_name: name.to_string(),
            episode_count: 8,
            package_path: format!("/tmp/报审包/{name}"),
            size_bytes: 12345,
        }
    }

    #[test]
    fn append_then_list_is_descending() {
        let path = temp_path("desc");
        append_record(&path, &record("2026-10-14T10:00:00+08:00", "剧A")).unwrap();
        append_record(&path, &record("2026-10-14T11:30:00+08:00", "剧B")).unwrap();
        append_record(&path, &record("2026-10-14T09:00:00+08:00", "剧C")).unwrap();

        let records = list_records(&path).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].drama_name, "剧B");
        assert_eq!(records[1].drama_name, "剧A");
        assert_eq!(records[2].drama_name, "剧C");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn invalid_utf8_line_does_not_hide_later_records() {
        let path = temp_path("utf8");
        append_record(&path, &record("2026-10-14T10:00:00+08:00", "剧A")).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut file, b"{\"ts\":\"\xff\xfe\"}\n").unwrap();
        drop(file);
        append_record(&path, &record("2026-10-14T12:00:00+08:00", "剧B")).unwrap();
        let records = list_records(&path).unwrap();
        let names: Vec<&str> = records.iter().map(|r| r.drama_name.as_str()).collect();
        assert_eq!(names, ["剧B", "剧A"], "坏行前后的记录都要在");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn missing_file_returns_empty() {
        let path = temp_path("empty");
        let records = list_records(&path).unwrap();
        assert!(records.is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn corrupted_lines_are_skipped() {
        let path = temp_path("corrupt");
        append_record(&path, &record("2026-10-14T10:00:00+08:00", "剧A")).unwrap();
        // 模拟断电产生的半行。
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut file, b"{\"ts\":\"2026-10-14T1").unwrap();
        drop(file);
        append_record(&path, &record("2026-10-14T12:00:00+08:00", "剧B")).unwrap();

        let records = list_records(&path).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].drama_name, "剧B");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
