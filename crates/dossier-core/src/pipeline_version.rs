//! 管线版本化与跨产物一致性校验（Tier 3）。
//!
//! 起因是一次真实的线上故障（见 `docs/PROJECT_REVIEW.md` §7.1）：云端按新版契约给
//! `dossier-inpaint` 传了 `--verify-ssim-warn`，而现场装的是不认识这个开关的旧二进制，
//! 结果整条任务直接解析失败。这类问题的共同点是——**两个发布物的版本不同步，而契约只覆盖了
//! 「工具在不在」，没覆盖「工具是不是我以为的那个版本」**。
//!
//! 所以这里做两件事：
//! 1. 每个产物旁边落一份 `*.stamp.json`（管线 + 版本 + 工具版本 + 参数摘要 + 输入摘要）；
//! 2. 给一组产物做一致性校验，把「混用不同版本」这种问题变成**可读的数据**而不是事后猜。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::checkpoint::ArtifactRef;

/// 硬字幕擦除管线。
pub const ERASE_PIPELINE: &str = "drama.logo.erase";
/// 软字幕擦除管线。
pub const SUBTITLE_PIPELINE: &str = "drama.subtitle.erase";
/// 报审包生成管线。
pub const REPORT_PIPELINE: &str = "drama.report.package";

/// 产物旁边的戳记文件后缀。
pub const STAMP_SUFFIX: &str = ".stamp.json";

/// 已知管线的当前版本。**改了会影响产物内容的逻辑就要在这里递增**，否则新旧产物会被判成同一版。
const PIPELINE_VERSIONS: &[(&str, &str)] = &[
    (ERASE_PIPELINE, "1.0.0"),
    (SUBTITLE_PIPELINE, "1.0.0"),
    (REPORT_PIPELINE, "1.0.0"),
];

/// 查一个管线的当前版本；未登记的管线返回 `None`（调用方应显式报错而不是猜一个）。
pub fn pipeline_version(pipeline: &str) -> Option<&'static str> {
    PIPELINE_VERSIONS
        .iter()
        .find(|(name, _)| *name == pipeline)
        .map(|(_, version)| *version)
}

/// 参与生产的工具及其版本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolStamp {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    /// 实际调用的程序路径（同名工具装了两份时，这是唯一能分辨的线索）。
    #[serde(default)]
    pub program: Option<String>,
}

/// 产物戳记。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactStamp {
    pub pipeline: String,
    pub pipeline_version: String,
    pub task_id: String,
    pub produced_at: String,
    #[serde(default)]
    pub tool: Option<ToolStamp>,
    /// 关键参数的摘要（用 [`crate::checkpoint::input_fingerprint`] 算）。
    pub params_sha256: String,
    /// 输入摘要，按出现顺序排列。
    #[serde(default)]
    pub inputs: Vec<String>,
    /// 本次产出的所有文件（含摘要与字节数）。
    pub outputs: Vec<ArtifactRef>,
}

impl ArtifactStamp {
    /// 生产时间用当前时间填充。
    pub fn new(
        pipeline: impl Into<String>,
        pipeline_version: impl Into<String>,
        task_id: impl Into<String>,
        params_sha256: impl Into<String>,
    ) -> Self {
        Self {
            pipeline: pipeline.into(),
            pipeline_version: pipeline_version.into(),
            task_id: task_id.into(),
            produced_at: now_iso8601(),
            tool: None,
            params_sha256: params_sha256.into(),
            inputs: Vec::new(),
            outputs: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_tool(mut self, tool: ToolStamp) -> Self {
        self.tool = Some(tool);
        self
    }

    #[must_use]
    pub fn with_inputs(mut self, inputs: Vec<String>) -> Self {
        self.inputs = inputs;
        self
    }

    /// 登记产物（读盘算摘要；文件不在就直接报错，不记假摘要）。
    pub fn add_output(&mut self, path: impl Into<PathBuf>) -> Result<(), ConsistencyError> {
        self.outputs.push(
            ArtifactRef::capture(path)
                .map_err(|error| ConsistencyError::ArtifactUnreadable(error.to_string()))?,
        );
        Ok(())
    }

    /// 戳记应该落在哪：产物路径后加后缀（`out.mp4` → `out.mp4.stamp.json`）。
    pub fn stamp_path_for(output: &Path) -> PathBuf {
        let mut name = output
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "artifact".to_string());
        name.push_str(STAMP_SUFFIX);
        output.with_file_name(name)
    }

    /// 保存到产物旁边（原子写，理由同检查点：戳记本身就是用来判断完整性的）。
    pub fn save_next_to(&self, output: &Path) -> Result<PathBuf, ConsistencyError> {
        let path = Self::stamp_path_for(output);
        self.save(&path)?;
        Ok(path)
    }

    pub fn save(&self, path: &Path) -> Result<(), ConsistencyError> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|error| ConsistencyError::Parse(error.to_string()))?;
        let temp = path.with_file_name(format!(
            ".{}.tmp{}",
            path.file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| "stamp".to_string()),
            std::process::id()
        ));
        std::fs::write(&temp, text).map_err(|error| ConsistencyError::Io {
            path: temp.clone(),
            message: error.to_string(),
        })?;
        std::fs::rename(&temp, path).map_err(|error| ConsistencyError::Io {
            path: path.to_path_buf(),
            message: error.to_string(),
        })
    }

    pub fn load(path: &Path) -> Result<Self, ConsistencyError> {
        let text = std::fs::read_to_string(path).map_err(|error| ConsistencyError::Io {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        serde_json::from_str(&text).map_err(|error| ConsistencyError::Parse(error.to_string()))
    }

    /// 核对本次登记的产物是否还在、是否还是当初那一份。
    pub fn verify_outputs(&self) -> Result<(), ConsistencyError> {
        for output in &self.outputs {
            output
                .verify(&self.pipeline)
                .map_err(|error| ConsistencyError::ArtifactUnreadable(error.to_string()))?;
        }
        Ok(())
    }

    /// 戳记本身是否自洽（管线已登记、版本与登记值一致、有产物）。
    pub fn validate(&self) -> Result<(), ConsistencyError> {
        let known = pipeline_version(&self.pipeline)
            .ok_or_else(|| ConsistencyError::UnknownPipeline(self.pipeline.clone()))?;
        if known != self.pipeline_version {
            return Err(ConsistencyError::PipelineVersionSkew {
                pipeline: self.pipeline.clone(),
                stamped: self.pipeline_version.clone(),
                expected: known.to_string(),
            });
        }
        if self.outputs.is_empty() {
            return Err(ConsistencyError::Parse(format!(
                "戳记没有任何产物：{}",
                self.pipeline
            )));
        }
        Ok(())
    }
}

/// 不一致的类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkewKind {
    /// 同一管线出现了多个版本。
    PipelineVersionMixed,
    /// 同一管线用了多个工具版本。
    ToolVersionMixed,
    /// 参数摘要不一致（同一批产物不是同一组参数跑出来的）。
    ParamsMismatch,
    /// 输入摘要不一致（产物之间的来源不同）。
    InputMismatch,
}

/// 一条不一致记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsistencyIssue {
    pub kind: SkewKind,
    pub pipeline: String,
    /// 人可读的说明（直接可以贴到日志或界面上）。
    pub detail: String,
}

/// 一致性报告。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsistencyReport {
    pub stamps_checked: usize,
    pub pipelines: Vec<String>,
    pub issues: Vec<ConsistencyIssue>,
}

impl ConsistencyReport {
    pub fn is_consistent(&self) -> bool {
        self.issues.is_empty()
    }

    /// 所有不一致的管线名（去重、保序）。
    pub fn affected_pipelines(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for issue in &self.issues {
            if !names.contains(&issue.pipeline) {
                names.push(issue.pipeline.clone());
            }
        }
        names
    }
}

/// 校验协议错误。
#[derive(Debug, Error)]
pub enum ConsistencyError {
    #[error("戳记读写失败（{path}）：{message}")]
    Io { path: PathBuf, message: String },
    #[error("戳记无法解析：{0}")]
    Parse(String),
    #[error("管线未登记版本：{0}（请在 pipeline_version 里补上，不要猜一个版本号）")]
    UnknownPipeline(String),
    #[error("管线 {pipeline} 的戳记版本 {stamped} 与当前登记的 {expected} 不一致")]
    PipelineVersionSkew {
        pipeline: String,
        stamped: String,
        expected: String,
    },
    #[error("产物无法读取：{0}")]
    ArtifactUnreadable(String),
}

/// 对一组戳记做一致性检查。
///
/// 规则都是「同一批产物应当同源」的直接推论：
/// - 同一管线的所有戳记版本必须一致；
/// - 工具的版本（带路径）必须一致；
/// - 参数摘要必须一致（不一致说明这批产物是不同参数跑出来的，混在一起交付会很难解释）；
/// - 输入摘要集合必须一致。
pub fn check_consistency(stamps: &[ArtifactStamp]) -> ConsistencyReport {
    let mut pipelines: Vec<String> = Vec::new();
    let mut issues: Vec<ConsistencyIssue> = Vec::new();
    for stamp in stamps {
        if !pipelines.contains(&stamp.pipeline) {
            pipelines.push(stamp.pipeline.clone());
        }
    }

    for pipeline in &pipelines {
        let group: Vec<&ArtifactStamp> = stamps
            .iter()
            .filter(|stamp| &stamp.pipeline == pipeline)
            .collect();
        let first = group[0];

        let versions: Vec<&str> = dedup(group.iter().map(|s| s.pipeline_version.as_str()));
        if versions.len() > 1 {
            issues.push(ConsistencyIssue {
                kind: SkewKind::PipelineVersionMixed,
                pipeline: pipeline.clone(),
                detail: format!("同一管线出现多个版本：{}", versions.join(" / ")),
            });
        }

        let mut tool_labels: Vec<String> = Vec::new();
        for stamp in &group {
            let label = tool_label(stamp.tool.as_ref());
            if !tool_labels.contains(&label) {
                tool_labels.push(label);
            }
        }
        if tool_labels.len() > 1 {
            issues.push(ConsistencyIssue {
                kind: SkewKind::ToolVersionMixed,
                pipeline: pipeline.clone(),
                detail: format!("同一管线用了多个工具版本：{}", tool_labels.join(" / ")),
            });
        }

        if group.iter().any(|s| s.params_sha256 != first.params_sha256) {
            issues.push(ConsistencyIssue {
                kind: SkewKind::ParamsMismatch,
                pipeline: pipeline.clone(),
                detail: format!(
                    "参数摘要不一致：{} 与 {}",
                    dossier_runtime::digest::short(&first.params_sha256, 12),
                    group
                        .iter()
                        .find(|s| s.params_sha256 != first.params_sha256)
                        .map(|s| dossier_runtime::digest::short(&s.params_sha256, 12))
                        .unwrap_or_default()
                ),
            });
        }

        if group.iter().any(|s| s.inputs != first.inputs) {
            issues.push(ConsistencyIssue {
                kind: SkewKind::InputMismatch,
                pipeline: pipeline.clone(),
                detail: "输入摘要不一致：这组产物不是同一批输入跑出来的".to_string(),
            });
        }
    }

    ConsistencyReport {
        stamps_checked: stamps.len(),
        pipelines,
        issues,
    }
}

/// 从磁盘读一组戳记并校验。
pub fn load_and_check(paths: &[PathBuf]) -> Result<ConsistencyReport, ConsistencyError> {
    let mut stamps = Vec::with_capacity(paths.len());
    for path in paths {
        stamps.push(ArtifactStamp::load(path)?);
    }
    Ok(check_consistency(&stamps))
}

fn tool_label(tool: Option<&ToolStamp>) -> String {
    match tool {
        None => "(未记录工具)".to_string(),
        Some(tool) => {
            let version = tool
                .version
                .clone()
                .unwrap_or_else(|| "未知版本".to_string());
            match &tool.program {
                Some(program) => format!("{}@{version}({program})", tool.name),
                None => format!("{}@{version}", tool.name),
            }
        }
    }
}

fn dedup<'a>(items: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut seen: Vec<&str> = Vec::new();
    for item in items {
        if !seen.contains(&item) {
            seen.push(item);
        }
    }
    seen
}

fn now_iso8601() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::input_fingerprint;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dossier-stamp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn stamp_for(
        dir: &Path,
        pipeline: &str,
        params: &str,
        output: &str,
        tool: &str,
    ) -> ArtifactStamp {
        let path = dir.join(output);
        std::fs::write(&path, output.as_bytes()).unwrap();
        let mut stamp = ArtifactStamp::new(
            pipeline,
            pipeline_version(pipeline).unwrap(),
            "task-1",
            input_fingerprint(&[("params", params)]),
        )
        .with_tool(ToolStamp {
            name: "dossier-inpaint".to_string(),
            version: Some(tool.to_string()),
            program: Some("/usr/local/bin/dossier-inpaint".to_string()),
        })
        .with_inputs(vec![input_fingerprint(&[("source", "abc")])]);
        stamp.add_output(&path).unwrap();
        stamp
    }

    #[test]
    fn registered_pipelines_have_versions_and_unknown_ones_do_not() {
        assert_eq!(pipeline_version(ERASE_PIPELINE), Some("1.0.0"));
        assert_eq!(pipeline_version(SUBTITLE_PIPELINE), Some("1.0.0"));
        assert_eq!(pipeline_version(REPORT_PIPELINE), Some("1.0.0"));
        // 未登记就返回 None，让调用方显式报错，而不是猜一个版本号。
        assert_eq!(pipeline_version("drama.nope"), None);
    }

    #[test]
    fn stamp_path_is_derived_from_the_artifact_name() {
        assert_eq!(
            ArtifactStamp::stamp_path_for(Path::new("/tmp/out.mp4")),
            PathBuf::from("/tmp/out.mp4.stamp.json")
        );
    }

    #[test]
    fn stamp_roundtrips_next_to_its_artifact_and_verifies_outputs() {
        let dir = temp_dir("roundtrip");
        let output = dir.join("ep01.mp4");
        std::fs::write(&output, b"video-bytes").unwrap();
        let mut stamp = ArtifactStamp::new(
            ERASE_PIPELINE,
            "1.0.0",
            "task-1",
            input_fingerprint(&[("dilate", "4")]),
        );
        stamp.add_output(&output).unwrap();
        stamp.validate().unwrap();
        let path = stamp.save_next_to(&output).unwrap();
        assert_eq!(path, dir.join("ep01.mp4.stamp.json"));

        let reloaded = ArtifactStamp::load(&path).unwrap();
        assert_eq!(reloaded, stamp);
        reloaded.verify_outputs().unwrap();
        // 目录里只有产物和戳记，没有残留的临时文件。
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 产物被改过 → 戳记必须报错（这正是「事后来查是哪个版本产的」要能站住的前提）。
    #[test]
    fn verify_outputs_catches_a_modified_artifact() {
        let dir = temp_dir("modified");
        let output = dir.join("ep01.mp4");
        std::fs::write(&output, b"video-bytes").unwrap();
        let mut stamp = ArtifactStamp::new(ERASE_PIPELINE, "1.0.0", "task-1", "params");
        stamp.add_output(&output).unwrap();
        std::fs::write(&output, b"something-else-entirely").unwrap();
        assert!(stamp.verify_outputs().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_refuses_unregistered_pipelines_and_version_skew() {
        let dir = temp_dir("validate");
        let output = dir.join("x.bin");
        std::fs::write(&output, b"x").unwrap();

        let mut unknown = ArtifactStamp::new("drama.nope", "1.0.0", "t", "params");
        unknown.add_output(&output).unwrap();
        assert!(matches!(
            unknown.validate(),
            Err(ConsistencyError::UnknownPipeline(_))
        ));

        let mut skewed = ArtifactStamp::new(ERASE_PIPELINE, "0.9.0", "t", "params");
        skewed.add_output(&output).unwrap();
        assert!(matches!(
            skewed.validate(),
            Err(ConsistencyError::PipelineVersionSkew { .. })
        ));

        let mut empty = ArtifactStamp::new(ERASE_PIPELINE, "1.0.0", "t", "params");
        assert!(matches!(empty.validate(), Err(ConsistencyError::Parse(_))));
        // 登记一个不存在的产物也要报错，不能记下假摘要。
        assert!(empty.add_output(dir.join("missing.bin")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_uniform_batch_is_consistent() {
        let dir = temp_dir("uniform");
        let stamps = vec![
            stamp_for(&dir, ERASE_PIPELINE, "dilate=4", "ep01.mp4", "0.3.0"),
            stamp_for(&dir, ERASE_PIPELINE, "dilate=4", "ep02.mp4", "0.3.0"),
        ];
        let report = check_consistency(&stamps);
        assert!(report.is_consistent(), "{report:?}");
        assert_eq!(report.stamps_checked, 2);
        assert_eq!(report.pipelines, vec![ERASE_PIPELINE.to_string()]);
        assert!(report.affected_pipelines().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真实故障场景：同一批产物里混了新旧两个版本的二进制。
    #[test]
    fn mixing_tool_versions_in_one_batch_is_reported() {
        let dir = temp_dir("tool-skew");
        let stamps = vec![
            stamp_for(&dir, ERASE_PIPELINE, "dilate=4", "ep01.mp4", "0.3.0"),
            stamp_for(&dir, ERASE_PIPELINE, "dilate=4", "ep02.mp4", "0.2.1"),
        ];
        let report = check_consistency(&stamps);
        assert!(!report.is_consistent());
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].kind, SkewKind::ToolVersionMixed);
        assert!(report.issues[0].detail.contains("0.3.0"));
        assert!(report.issues[0].detail.contains("0.2.1"));
        assert_eq!(
            report.affected_pipelines(),
            vec![ERASE_PIPELINE.to_string()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mixing_pipeline_versions_or_params_is_reported() {
        let dir = temp_dir("mixed");
        let mut older = stamp_for(&dir, ERASE_PIPELINE, "dilate=4", "ep01.mp4", "0.3.0");
        older.pipeline_version = "0.9.0".to_string();
        let different_params = stamp_for(&dir, ERASE_PIPELINE, "dilate=8", "ep02.mp4", "0.3.0");
        let report = check_consistency(&[older, different_params]);
        let kinds: Vec<SkewKind> = report.issues.iter().map(|issue| issue.kind).collect();
        assert!(kinds.contains(&SkewKind::PipelineVersionMixed), "{kinds:?}");
        assert!(kinds.contains(&SkewKind::ParamsMismatch), "{kinds:?}");

        let mut other_input = stamp_for(&dir, ERASE_PIPELINE, "dilate=4", "ep03.mp4", "0.3.0");
        other_input.inputs = vec![input_fingerprint(&[("source", "different")])];
        let mut base = stamp_for(&dir, ERASE_PIPELINE, "dilate=4", "ep04.mp4", "0.3.0");
        base.inputs = vec![input_fingerprint(&[("source", "abc")])];
        let report = check_consistency(&[base, other_input]);
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].kind, SkewKind::InputMismatch);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 不同管线混在一批里不算不一致（报审包和擦除产物本来就该一起交付）。
    #[test]
    fn different_pipelines_do_not_conflict_with_each_other() {
        let dir = temp_dir("cross");
        let stamps = vec![
            stamp_for(&dir, ERASE_PIPELINE, "dilate=4", "ep01.mp4", "0.3.0"),
            stamp_for(&dir, REPORT_PIPELINE, "platform=x", "report.json", "1.2.0"),
        ];
        let report = check_consistency(&stamps);
        assert!(report.is_consistent(), "{report:?}");
        assert_eq!(report.pipelines.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_and_check_reads_stamps_from_disk() {
        let dir = temp_dir("load");
        let first = stamp_for(&dir, ERASE_PIPELINE, "dilate=4", "ep01.mp4", "0.3.0");
        let second = stamp_for(&dir, ERASE_PIPELINE, "dilate=4", "ep02.mp4", "0.3.0");
        let first_path = first.save_next_to(&dir.join("ep01.mp4")).unwrap();
        let second_path = second.save_next_to(&dir.join("ep02.mp4")).unwrap();
        let report = load_and_check(&[first_path, second_path]).unwrap();
        assert!(report.is_consistent());
        assert_eq!(report.stamps_checked, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
