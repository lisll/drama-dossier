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

pub const MOUNT_COPY_HEADER: &str = "集号,成片文件,短标题,描述,挂载文案,剧集检索名,状态,结果备注";

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

/// UTF-8 BOM：Windows 上的 Excel 靠它识别 UTF-8，没有 BOM 时按系统代码页（GBK）打开，中文全是乱码。
pub const UTF8_BOM: &str = "\u{feff}";

/// 把 CSV 文件字节解码成文本：去掉 UTF-8 BOM；不是合法 UTF-8 时按 GBK 解码一次
/// （中文 Windows 上用 Excel 直接「保存」得到的就是 GBK）。GBK 也解不干净时报错，提示怎么另存。
pub fn decode_csv_bytes(bytes: &[u8]) -> std::io::Result<String> {
    let bytes = bytes.strip_prefix(UTF8_BOM.as_bytes()).unwrap_or(bytes);
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok(text.to_string());
    }
    let (text, _, had_errors) = encoding_rs::GBK.decode(bytes);
    if had_errors {
        return Err(invalid_csv(
            "CSV 编码无法识别（既不是 UTF-8 也不是 GBK）；请在 Excel 里「另存为 → CSV UTF-8（逗号分隔）」后再导入",
        ));
    }
    Ok(text.into_owned())
}

/// 读取 CSV 文件并按 [`decode_csv_bytes`] 解码。
pub fn read_csv_text(path: &Path) -> std::io::Result<String> {
    decode_csv_bytes(&fs::read(path)?)
}

/// 写出给 Excel 打开的 CSV 文本：开头补一个 UTF-8 BOM（已有就不重复加）。
pub fn csv_text_with_bom(text: &str) -> String {
    if text.starts_with(UTF8_BOM) {
        text.to_string()
    } else {
        format!("{UTF8_BOM}{text}")
    }
}

/// 写入 UTF-8（带 BOM）CSV。原子写入（[`crate::fsio::write_atomic`]）：中途崩溃不留半份文案，
/// 也不会顺着输出位置预先放好的符号链接写到别处。
pub fn write_mount_copy_csv(
    path: &Path,
    rows: &[MountCopyRow],
) -> std::io::Result<MountCopyWriteResult> {
    validate_rows(rows)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut output = String::from(UTF8_BOM);
    output.push_str(MOUNT_COPY_HEADER);
    output.push('\n');
    for row in rows {
        output.push_str(&row_to_csv(row));
        output.push('\n');
    }
    crate::fsio::write_atomic(path, output.as_bytes())?;
    Ok(MountCopyWriteResult {
        path: path.to_path_buf(),
        row_count: rows.len(),
        sha256: sha256_bytes(output.as_bytes()),
    })
}

/// 读取并校验挂载文案 CSV。
pub fn load_mount_copy_csv(path: &Path) -> std::io::Result<Vec<MountCopyRow>> {
    let text = read_csv_text(path)?;
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
            Some(
                record[0]
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| invalid_csv("集号必须是非负整数"))?,
            )
        };
        let field = |index: usize| unguard_formula(&record[index]);
        rows.push(MountCopyRow {
            episode_number,
            source_file: field(1),
            short_title: field(2),
            description: field(3),
            mount_copy: field(4),
            drama_search_name: field(5),
            status: field(6),
            result_note: field(7),
        });
    }
    validate_rows(&rows)?;
    Ok(rows)
}

/// 构造一份本地发布批次描述。发布器应在真正提交前重新读取 CSV 并人工确认。
pub fn make_publish_batch(
    batch_id: &str,
    drama_name: &str,
    csv_path: &Path,
) -> std::io::Result<PublishBatch> {
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
                reason: if eligible {
                    "本地前置条件满足".to_string()
                } else {
                    missing.join("；")
                },
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
        if !matches!(
            row.status.as_str(),
            "draft" | "ready" | "submitted" | "confirmed" | "failed"
        ) {
            return Err(invalid_csv(
                "状态必须是 draft/ready/submitted/confirmed/failed",
            ));
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
        row.episode_number
            .map(|value| value.to_string())
            .unwrap_or_default(),
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

/// 以这些字符开头的单元格会被 Excel / WPS 当成公式执行（CSV 公式注入）。
const FORMULA_TRIGGERS: [char; 6] = ['=', '+', '-', '@', '\t', '\r'];

/// 「若干个英文单引号 + 公式触发字符」开头（含零个单引号）：这类值写出时要加保护前缀，读回时要去掉一个。
/// 写、读用同一个判断，保证任意文本都能原样往返。
fn formula_like(value: &str) -> bool {
    value.trim_start_matches('\'').starts_with(FORMULA_TRIGGERS)
}

/// 写出前给可能被当成公式的单元格加一个英文单引号（OWASP 推荐做法：Excel 把它当文本前缀，不显示）。
/// 用户原本就写成「单引号 + 触发字符」开头的文本（如 `'-_-`）也补一个，读回时只去掉一个。
fn guard_formula(value: &str) -> String {
    if formula_like(value) {
        format!("'{value}")
    } else {
        value.to_string()
    }
}

/// 读回时去掉 [`guard_formula`] 加的那个单引号；用户自己写的、以单引号开头的普通文本原样保留。
fn unguard_formula(value: &str) -> String {
    match value.strip_prefix('\'') {
        Some(rest) if formula_like(rest) => rest.to_string(),
        _ => value.to_string(),
    }
}

/// CSV 单元格转义：先做公式注入防护（[`guard_formula`]），再按 RFC 4180 加引号。
/// 挂载文案与报审包分集清单共用这一份。
pub(crate) fn csv_escape(value: &str) -> String {
    let value = guard_formula(value);
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value
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
        // 写出的文件带 BOM：Windows 上双击用 Excel 打开时中文不乱码
        assert!(fs::read(&path).unwrap().starts_with(UTF8_BOM.as_bytes()));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn decode_csv_handles_bom_gbk_and_garbage() {
        assert_eq!(
            decode_csv_bytes("\u{feff}集号,文案\n".as_bytes()).unwrap(),
            "集号,文案\n"
        );
        assert_eq!(
            decode_csv_bytes("集号,文案\n".as_bytes()).unwrap(),
            "集号,文案\n"
        );
        // 中文 Windows 上 Excel 直接保存得到的 GBK
        let (gbk, _, _) = encoding_rs::GBK.encode("集号,文案\n1,第一集\n");
        assert_eq!(decode_csv_bytes(&gbk).unwrap(), "集号,文案\n1,第一集\n");
        // 既不是 UTF-8 也不是 GBK：报错并告诉用户怎么另存
        let error = decode_csv_bytes(&[0xff, 0xff, 0xff]).unwrap_err();
        assert!(error.to_string().contains("CSV UTF-8"), "{error}");
        assert_eq!(csv_text_with_bom("a"), "\u{feff}a");
        assert_eq!(csv_text_with_bom("\u{feff}a"), "\u{feff}a", "不重复加 BOM");
    }

    #[test]
    fn formula_prefix_is_guarded_and_round_trips() {
        for text in [
            "=HYPERLINK(\"http://x\")",
            "+1",
            "-2",
            "@SUM(A1)",
            "\t制表符开头",
        ] {
            let escaped = csv_escape(text);
            assert!(
                escaped.trim_start_matches('"').starts_with('\''),
                "公式前缀没有被转义：{escaped}"
            );
            assert_eq!(unguard_formula(&format!("'{text}")), text);
        }
        // 用户本来就写的单引号开头文本：读回时不能被误删
        assert_eq!(unguard_formula("'普通引号"), "'普通引号");
        assert_eq!(csv_escape("普通文本"), "普通文本");

        let dir = std::env::temp_dir().join(format!("publish-formula-{}", std::process::id()));
        let path = dir.join("挂载文案.csv");
        let mut rows = build_mount_copy_rows("测试剧", &[episode(1, "第1集.mp4")]);
        rows[0].mount_copy = "=1+1 不能被当成公式".into();
        rows[0].description = "'用户自己的单引号".into();
        // 用户原本就写成「单引号 + 触发字符」开头：读回不能少掉那个单引号
        rows[0].short_title = "'-_- 颜文字".into();
        write_mount_copy_csv(&path, &rows).unwrap();
        let raw = read_csv_text(&path).unwrap();
        assert!(
            raw.contains("'=1+1"),
            "写出的文件里公式前缀没有被转义：{raw}"
        );
        assert_eq!(
            load_mount_copy_csv(&path).unwrap(),
            rows,
            "读回内容必须与写入一致"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn duplicate_source_file_is_rejected() {
        let rows = vec![
            MountCopyRow {
                source_file: "a.mp4".into(),
                status: "draft".into(),
                ..Default::default()
            },
            MountCopyRow {
                source_file: "a.mp4".into(),
                status: "draft".into(),
                ..Default::default()
            },
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
        let previous = MountCopyRow {
            source_file: "a.mp4".into(),
            status: "draft".into(),
            ..Default::default()
        };
        let next = MountCopyRow {
            source_file: "a.mp4".into(),
            status: "confirmed".into(),
            ..Default::default()
        };
        assert!(validate_publish_update(&previous, &next).is_err());
    }
}
