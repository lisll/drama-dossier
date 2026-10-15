//! 发布素材与挂载文案清单。
//!
//! 这个模块只负责把已存在的成片映射到可人工复核的发布元数据：短标题、描述、
//! 挂载文案和剧集检索名。它不访问第三方平台，也不保存登录会话；外部发布器只能
//! 消费这里生成的 CSV 和状态模型。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::types::ProbedEpisode;

pub const MOUNT_COPY_HEADER: &str =
    "集号,成片文件,短标题,描述,挂载文案,剧集检索名,状态,结果备注";

/// 单条发布素材元数据。字段故意与媒体文件分离，便于改文案而不重新处理视频。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MountCopyRow {
    pub episode_number: Option<u32>,
    pub source_file: String,
    pub short_title: String,
    pub description: String,
    pub mount_copy: String,
    pub drama_search_name: String,
    /// draft / ready / submitted / confirmed / failed。
    pub status: String,
    pub result_note: String,
}

/// 发布批次的本地描述；不包含任何网络凭据或平台会话。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PublishBatch {
    pub schema: String,
    pub batch_id: String,
    pub drama_name: String,
    pub csv_path: PathBuf,
    pub rows: Vec<MountCopyRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MountCopyWriteResult {
    pub path: PathBuf,
    pub row_count: usize,
    pub sha256: String,
}

/// 模拟发布结果。它只说明哪些行满足本地提交前置条件，`would_submit` 固定为 false；
/// 不创建平台单号、不伪造“已发布”结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PublishPreview {
    pub schema: String,
    pub batch_id: String,
    pub row_count: usize,
    pub eligible_count: usize,
    pub can_submit: bool,
    pub would_submit: bool,
    pub rows: Vec<PublishPreviewRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PublishPreviewRow {
    pub source_file: String,
    pub status: String,
    pub eligible: bool,
    pub reason: String,
}

/// 限制发布记录的本地状态迁移，避免把“已提交”误当成“结果已确认”。
pub fn transition_publish_row(
    row: &mut MountCopyRow,
    next_status: &str,
    result_note: impl Into<String>,
) -> std::io::Result<()> {
    let allowed = match row.status.as_str() {
        "draft" => matches!(next_status, "ready"),
        "ready" => matches!(next_status, "draft" | "submitted"),
        "submitted" => matches!(next_status, "confirmed" | "failed"),
        "failed" => matches!(next_status, "ready"),
        "confirmed" => false,
        _ => false,
    };
    if !allowed {
        return Err(invalid_csv(&format!(
            "不允许的发布状态迁移：{} -> {}",
            row.status, next_status
        )));
    }
    row.status = next_status.to_string();
    row.result_note = result_note.into();
    Ok(())
}

/// 校验 CSV 保存时的状态变化。相同状态表示仅编辑文案，不触发状态机；
/// 状态变化必须走允许的显式迁移，避免 UI 直接把 draft 改成 confirmed。
pub fn validate_publish_update(
    previous: &MountCopyRow,
    next: &MountCopyRow,
) -> std::io::Result<()> {
    if previous.source_file != next.source_file {
        return Err(invalid_csv("保存时不能更换成片文件；请生成新的发布批次"));
    }
    if previous.episode_number != next.episode_number {
        return Err(invalid_csv("保存时不能更换集号；请生成新的发布批次"));
    }
    if previous.status != next.status {
        let mut checked = previous.clone();
        transition_publish_row(&mut checked, &next.status, next.result_note.clone())?;
    }
    Ok(())
}

/// 根据已探测分集生成一份可编辑的挂载文案 CSV。
pub fn build_mount_copy_rows(drama_name: &str, episodes: &[ProbedEpisode]) -> Vec<MountCopyRow> {
    episodes
        .iter()
        .map(|episode| {
            let number = episode.file.episode_number;
            let suffix = number
                .map(|value| format!("第{value}集"))
                .unwrap_or_else(|| episode.file.file_name.clone());
            MountCopyRow {
                episode_number: number,
                source_file: episode.file.file_name.clone(),
                short_title: format!("{drama_name} {suffix}"),
                description: String::new(),
                mount_copy: String::new(),
                drama_search_name: drama_name.to_string(),
                status: "draft".to_string(),
                result_note: String::new(),
            }
        })
        .collect()
}

/// 写入 UTF-8 CSV。写入采用临时文件替换，避免中途崩溃留下半份文案。
pub fn write_mount_copy_csv(path: &Path, rows: &[MountCopyRow]) -> std::io::Result<MountCopyWriteResult> {
    validate_rows(rows)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("csv.tmp");
    let mut output = String::new();
    output.push_str(MOUNT_COPY_HEADER);
    output.push('\n');
    for row in rows {
        output.push_str(&row_to_csv(row));
        output.push('\n');
    }
    fs::write(&tmp, output.as_bytes())?;
    fs::rename(&tmp, path)?;
    Ok(MountCopyWriteResult {
        path: path.to_path_buf(),
        row_count: rows.len(),
        sha256: sha256_bytes(output.as_bytes()),
    })
}

/// 读取并校验挂载文案 CSV。
pub fn load_mount_copy_csv(path: &Path) -> std::io::Result<Vec<MountCopyRow>> {
    let text = fs::read_to_string(path)?;
    let mut records = parse_csv(&text)?;
    if records.is_empty() {
        return Ok(Vec::new());
    }
    let header = records.remove(0);
    let expected: Vec<String> = MOUNT_COPY_HEADER.split(',').map(str::to_string).collect();
    if header != expected {
        return Err(invalid_csv("CSV 表头不匹配，期望挂载文案模板"));
    }
    let mut rows = Vec::with_capacity(records.len());
    for record in records {
        if record.len() != 8 {
            return Err(invalid_csv("CSV 行字段数量不正确"));
        }
        let episode_number = if record[0].trim().is_empty() {
            None
        } else {
            Some(record[0].trim().parse::<u32>().map_err(|_| {
                invalid_csv("集号必须是非负整数")
            })?)
        };
        rows.push(MountCopyRow {
            episode_number,
            source_file: record[1].clone(),
            short_title: record[2].clone(),
            description: record[3].clone(),
            mount_copy: record[4].clone(),
            drama_search_name: record[5].clone(),
            status: record[6].clone(),
            result_note: record[7].clone(),
        });
    }
    validate_rows(&rows)?;
    Ok(rows)
}

/// 构造一份本地发布批次描述。发布器应在真正提交前重新读取 CSV 并人工确认。
pub fn make_publish_batch(batch_id: &str, drama_name: &str, csv_path: &Path) -> std::io::Result<PublishBatch> {
    if batch_id.trim().is_empty() || drama_name.trim().is_empty() {
        return Err(invalid_csv("批次 ID 和剧名不能为空"));
    }
    let rows = load_mount_copy_csv(csv_path)?;
    Ok(PublishBatch {
        schema: "drama-dossier/publish-batch@1".to_string(),
        batch_id: batch_id.to_string(),
        drama_name: drama_name.to_string(),
        csv_path: csv_path.to_path_buf(),
        rows,
    })
}

/// 生成不联网的提交前预览。预览不会改变 CSV 状态，也不会声称已经发布。
pub fn preview_publish_batch(batch_id: &str, batch: &PublishBatch) -> PublishPreview {
    let rows = batch
        .rows
        .iter()
        .map(|row| {
            let mut missing = Vec::new();
            if row.status != "ready" {
                missing.push(format!("状态为 {}，需要 ready", row.status));
            }
            if row.short_title.trim().is_empty() {
                missing.push("短标题为空".to_string());
            }
            if row.description.trim().is_empty() {
                missing.push("描述为空".to_string());
            }
            if row.mount_copy.trim().is_empty() {
                missing.push("挂载文案为空".to_string());
            }
            if row.drama_search_name.trim().is_empty() {
                missing.push("剧集检索名为空".to_string());
            }
            let eligible = missing.is_empty();
            PublishPreviewRow {
                source_file: row.source_file.clone(),
                status: row.status.clone(),
                eligible,
                reason: if eligible { "本地前置条件满足".to_string() } else { missing.join("；") },
            }
        })
        .collect::<Vec<_>>();
    let eligible_count = rows.iter().filter(|row| row.eligible).count();
    PublishPreview {
        schema: "drama-dossier/publish-preview@1".to_string(),
        batch_id: batch_id.to_string(),
        row_count: rows.len(),
        eligible_count,
        can_submit: eligible_count == rows.len() && !rows.is_empty(),
        would_submit: false,
        rows,
    }
}

fn validate_rows(rows: &[MountCopyRow]) -> std::io::Result<()> {
    let mut seen_files = std::collections::HashSet::new();
    for row in rows {
        if row.source_file.trim().is_empty() {
            return Err(invalid_csv("成片文件不能为空"));
        }
        if !seen_files.insert(row.source_file.clone()) {
            return Err(invalid_csv("成片文件不能重复"));
        }
        if !matches!(row.status.as_str(), "draft" | "ready" | "submitted" | "confirmed" | "failed") {
            return Err(invalid_csv("状态必须是 draft/ready/submitted/confirmed/failed"));
        }
        for (label, value) in [
            ("短标题", &row.short_title),
            ("描述", &row.description),
            ("挂载文案", &row.mount_copy),
            ("剧集检索名", &row.drama_search_name),
            ("结果备注", &row.result_note),
        ] {
            if value.contains(['\n', '\r']) {
                return Err(invalid_csv(&format!("{label}不能包含换行")));
            }
        }
    }
    Ok(())
}

fn row_to_csv(row: &MountCopyRow) -> String {
    [
        row.episode_number.map(|value| value.to_string()).unwrap_or_default(),
        row.source_file.clone(),
        row.short_title.clone(),
        row.description.clone(),
        row.mount_copy.clone(),
        row.drama_search_name.clone(),
        row.status.clone(),
        row.result_note.clone(),
    ]
    .into_iter()
    .map(|value| csv_escape(&value))
    .collect::<Vec<_>>()
    .join(",")
}

fn csv_escape(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn parse_csv(input: &str) -> std::io::Result<Vec<Vec<String>>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cell.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => {
                row.push(std::mem::take(&mut cell));
            }
            '\n' if !quoted => {
                row.push(std::mem::take(&mut cell));
                if !(row.len() == 1 && row[0].is_empty()) {
                    rows.push(std::mem::take(&mut row));
                } else {
                    row.clear();
                }
            }
            '\r' if !quoted => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                row.push(std::mem::take(&mut cell));
                if !(row.len() == 1 && row[0].is_empty()) {
                    rows.push(std::mem::take(&mut row));
                } else {
                    row.clear();
                }
            }
            _ => cell.push(ch),
        }
    }
    if quoted {
        return Err(invalid_csv("CSV 引号未闭合"));
    }
    if !row.is_empty() || !cell.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    Ok(rows)
}

fn invalid_csv(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    format!("{:x}", digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EpisodeFile, EpisodeSpec};

    fn episode(number: u32, name: &str) -> ProbedEpisode {
        ProbedEpisode {
            file: EpisodeFile {
                path: PathBuf::from(name),
                file_name: name.to_string(),
                size_bytes: 1,
                episode_number: Some(number),
            },
            spec: EpisodeSpec::default(),
        }
    }

    #[test]
    fn generated_csv_round_trips_quotes_and_unicode() {
        let dir = std::env::temp_dir().join(format!("publish-csv-{}", std::process::id()));
        let path = dir.join("挂载文案.csv");
        let mut rows = build_mount_copy_rows("测试剧", &[episode(1, "第1集,终.mp4")]);
        rows[0].description = "含 \"引号\" 的描述".into();
        write_mount_copy_csv(&path, &rows).unwrap();
        let loaded = load_mount_copy_csv(&path).unwrap();
        assert_eq!(loaded, rows);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn duplicate_source_file_is_rejected() {
        let rows = vec![
            MountCopyRow { source_file: "a.mp4".into(), status: "draft".into(), ..Default::default() },
            MountCopyRow { source_file: "a.mp4".into(), status: "draft".into(), ..Default::default() },
        ];
        assert!(write_mount_copy_csv(&std::env::temp_dir().join("bad.csv"), &rows).is_err());
    }

    #[test]
    fn publish_status_requires_explicit_result_confirmation() {
        let mut row = MountCopyRow {
            source_file: "a.mp4".into(),
            status: "draft".into(),
            ..Default::default()
        };
        assert!(transition_publish_row(&mut row, "confirmed", "").is_err());
        transition_publish_row(&mut row, "ready", "人工检查完成").unwrap();
        transition_publish_row(&mut row, "submitted", "dry-run submission").unwrap();
        transition_publish_row(&mut row, "confirmed", "人工确认结果").unwrap();
        assert!(transition_publish_row(&mut row, "failed", "").is_err());
    }

    #[test]
    fn saving_cannot_skip_status_machine() {
        let previous = MountCopyRow { source_file: "a.mp4".into(), status: "draft".into(), ..Default::default() };
        let next = MountCopyRow { source_file: "a.mp4".into(), status: "confirmed".into(), ..Default::default() };
        assert!(validate_publish_update(&previous, &next).is_err());
    }
}
