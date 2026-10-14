//! 报审包差异对比（Roadmap：报审包差异对比）。
//!
//! 报审包每次生成都会带一份 `manifest.json`（相对路径 + 大小 + SHA-256）。
//! 改稿重出之后，最常被问的两个问题是「这次变了什么」和「有哪些文件其实没动」——
//! 本模块只读两份清单并给出逐文件差异，**不读取、也不比对外部素材内容**。
//!
//! 用途：人工复核改稿前后是否只动了预期的那几个文件（例如只更新了分集清单 CSV）。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// 清单里的一个文件条目（也是 `manifest.json` 的公开读取格式）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestFileEntry {
    pub path: String,
    #[serde(default)]
    pub size_bytes: u64,
    #[serde(default)]
    pub sha256: String,
}

/// 报审包清单。字段与 `package::Manifest` 对应，另加 `Deserialize` 以便从磁盘读回。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageManifest {
    #[serde(default)]
    pub schema: String,
    #[serde(default)]
    pub drama_name: String,
    #[serde(default)]
    pub generated_at: String,
    #[serde(default)]
    pub ai_generated: bool,
    #[serde(default)]
    pub copy_episodes: bool,
    #[serde(default)]
    pub files: Vec<ManifestFileEntry>,
}

impl PackageManifest {
    /// 按相对路径索引，便于对比。
    pub fn index(&self) -> BTreeMap<&str, &ManifestFileEntry> {
        self.files.iter().map(|f| (f.path.as_str(), f)).collect()
    }

    /// 内容指纹：路径 + 大小 + SHA-256 排序后拼接。
    pub fn content_fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut files: Vec<&ManifestFileEntry> = self.files.iter().collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let mut hasher = Sha256::new();
        for file in files {
            hasher.update(file.path.as_bytes());
            hasher.update(b"\0");
            hasher.update(file.size_bytes.to_string().as_bytes());
            hasher.update(b"\0");
            hasher.update(file.sha256.as_bytes());
            hasher.update(b"\n");
        }
        format!("{:x}", hasher.finalize())
    }
}

/// 单个文件的差异类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Added,
    Removed,
    Changed,
    Unchanged,
}

/// 单个文件的差异。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDiff {
    pub path: String,
    pub kind: FileChangeKind,
    /// 右侧大小 − 左侧大小（新增为正、删除为负）。
    pub size_delta_bytes: i64,
    pub before: Option<ManifestFileEntry>,
    pub after: Option<ManifestFileEntry>,
    /// 可直接展示的中文说明。
    pub detail: String,
}

/// 两份报审包的差异报告。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageDiff {
    pub left_label: String,
    pub right_label: String,
    pub left_drama_name: String,
    pub right_drama_name: String,
    pub left_generated_at: String,
    pub right_generated_at: String,
    pub added: usize,
    pub removed: usize,
    pub changed: usize,
    pub unchanged: usize,
    /// 只列出 Added / Removed / Changed（未变文件不进列表，避免刷屏）。
    pub files: Vec<FileDiff>,
    pub summary: String,
}

impl PackageDiff {
    /// 是否有内容变化。
    pub fn has_changes(&self) -> bool {
        self.added + self.removed + self.changed > 0
    }
}

/// 解析清单 JSON 文本。
pub fn parse_manifest(json: &str) -> Result<PackageManifest, String> {
    serde_json::from_str(json).map_err(|error| format!("清单不是合法的 manifest.json：{error}"))
}

/// 从报审包目录读取 `manifest.json`。
pub fn load_manifest(package_dir: &Path) -> std::io::Result<PackageManifest> {
    let path = package_dir.join("manifest.json");
    let text = std::fs::read_to_string(&path).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("读取清单失败（{}）：{error}", path.display()),
        )
    })?;
    parse_manifest(&text).map_err(std::io::Error::other)
}

/// 对比两份清单。
pub fn diff_manifests(
    left: &PackageManifest,
    right: &PackageManifest,
    left_label: &str,
    right_label: &str,
) -> PackageDiff {
    let left_index = left.index();
    let right_index = right.index();
    let mut paths: Vec<&str> = left_index
        .keys()
        .chain(right_index.keys())
        .copied()
        .collect();
    paths.sort_unstable();
    paths.dedup();

    let mut files = Vec::new();
    let (mut added, mut removed, mut changed, mut unchanged) = (0, 0, 0, 0);
    for path in paths {
        let before = left_index.get(path).map(|f| (*f).clone());
        let after = right_index.get(path).map(|f| (*f).clone());
        let (kind, detail) = match (&before, &after) {
            (None, Some(_)) => (FileChangeKind::Added, "本次新增".to_string()),
            (Some(_), None) => (FileChangeKind::Removed, "本次移除".to_string()),
            (Some(b), Some(a)) => {
                if b.sha256 == a.sha256 && b.size_bytes == a.size_bytes {
                    (FileChangeKind::Unchanged, "内容未变".to_string())
                } else if b.sha256 == a.sha256 {
                    (
                        FileChangeKind::Changed,
                        "内容相同但大小记录不同".to_string(),
                    )
                } else {
                    (
                        FileChangeKind::Changed,
                        format!("内容变化（{} → {} 字节）", b.size_bytes, a.size_bytes),
                    )
                }
            }
            (None, None) => continue,
        };
        match kind {
            FileChangeKind::Added => added += 1,
            FileChangeKind::Removed => removed += 1,
            FileChangeKind::Changed => changed += 1,
            FileChangeKind::Unchanged => unchanged += 1,
        }
        let size_delta_bytes = after.as_ref().map(|f| f.size_bytes).unwrap_or(0) as i64
            - before.as_ref().map(|f| f.size_bytes).unwrap_or(0) as i64;
        if kind != FileChangeKind::Unchanged {
            files.push(FileDiff {
                path: path.to_string(),
                kind,
                size_delta_bytes,
                before,
                after,
                detail,
            });
        }
    }

    let summary = if added + removed + changed == 0 {
        format!("两份报审包内容一致（共比对 {} 个文件）", unchanged)
    } else {
        format!(
            "新增 {added}，移除 {removed}，修改 {changed}，未变 {unchanged}（共 {} 个文件）",
            unchanged + added + removed + changed
        )
    };

    PackageDiff {
        left_label: left_label.to_string(),
        right_label: right_label.to_string(),
        left_drama_name: left.drama_name.clone(),
        right_drama_name: right.drama_name.clone(),
        left_generated_at: left.generated_at.clone(),
        right_generated_at: right.generated_at.clone(),
        added,
        removed,
        changed,
        unchanged,
        files,
        summary,
    }
}

/// 直接对比两个报审包目录（各自读 `manifest.json`）。
pub fn diff_packages(left_dir: &Path, right_dir: &Path) -> std::io::Result<PackageDiff> {
    let left = load_manifest(left_dir)?;
    let right = load_manifest(right_dir)?;
    Ok(diff_manifests(
        &left,
        &right,
        &left_dir.display().to_string(),
        &right_dir.display().to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(files: &[(&str, u64, &str)]) -> PackageManifest {
        PackageManifest {
            schema: "1.0".into(),
            drama_name: "测试剧".into(),
            generated_at: "2024-01-01T00:00:00Z".into(),
            ai_generated: false,
            copy_episodes: false,
            files: files
                .iter()
                .map(|(path, size, sha)| ManifestFileEntry {
                    path: (*path).into(),
                    size_bytes: *size,
                    sha256: (*sha).into(),
                })
                .collect(),
        }
    }

    #[test]
    fn classifies_added_removed_changed_and_unchanged() {
        let left = manifest(&[
            ("01_剧集信息/剧名.txt", 10, "aaa"),
            ("03_分集清单/分集清单.csv", 100, "bbb"),
            ("05_权属声明/内容权属声明.txt", 50, "ccc"),
        ]);
        let right = manifest(&[
            ("01_剧集信息/剧名.txt", 10, "aaa"),
            ("03_分集清单/分集清单.csv", 120, "ddd"),
            ("06_分集视频/第01集.mp4", 999, "eee"),
        ]);
        let diff = diff_manifests(&left, &right, "改稿前", "改稿后");
        assert_eq!(
            (diff.added, diff.removed, diff.changed, diff.unchanged),
            (1, 1, 1, 1)
        );
        assert!(diff.has_changes());
        let kinds: Vec<(&str, FileChangeKind)> = diff
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.kind))
            .collect();
        assert!(kinds.contains(&("03_分集清单/分集清单.csv", FileChangeKind::Changed)));
        assert!(kinds.contains(&("05_权属声明/内容权属声明.txt", FileChangeKind::Removed)));
        assert!(kinds.contains(&("06_分集视频/第01集.mp4", FileChangeKind::Added)));
        // 未变的文件不出现在明细里，但计入 unchanged。
        assert!(!kinds.iter().any(|(p, _)| *p == "01_剧集信息/剧名.txt"));
    }

    #[test]
    fn identical_manifests_report_no_changes() {
        let m = manifest(&[("a.txt", 1, "x"), ("b.txt", 2, "y")]);
        let diff = diff_manifests(&m, &m, "a", "b");
        assert!(!diff.has_changes());
        assert!(diff.files.is_empty());
        assert_eq!(diff.unchanged, 2);
        assert!(diff.summary.contains("内容一致"));
    }

    #[test]
    fn size_delta_is_signed() {
        let left = manifest(&[("a.txt", 100, "x")]);
        let right = manifest(&[("a.txt", 40, "y")]);
        let diff = diff_manifests(&left, &right, "a", "b");
        assert_eq!(diff.files[0].size_delta_bytes, -60);
    }

    #[test]
    fn parses_real_manifest_shape() {
        let json = r#"{
            "schema": "1.0",
            "dramaName": "测试剧",
            "generatedAt": "2024-05-01T12:00:00Z",
            "aiGenerated": true,
            "copyEpisodes": true,
            "files": [{"path": "01_剧集信息/剧名.txt", "sizeBytes": 6, "sha256": "abc"}]
        }"#;
        let manifest = parse_manifest(json).expect("应能解析清单");
        assert_eq!(manifest.drama_name, "测试剧");
        assert!(manifest.ai_generated);
        assert_eq!(manifest.files[0].size_bytes, 6);
        assert_eq!(manifest.content_fingerprint().len(), 64);
    }

    #[test]
    fn rejects_invalid_manifest_json() {
        assert!(parse_manifest("not json").is_err());
    }
}
