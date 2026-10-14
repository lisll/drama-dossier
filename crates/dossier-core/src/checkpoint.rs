//! 检查点与断点续跑（Tier 3）。
//!
//! 解决的问题：一次擦除/转码任务可能跑几十分钟，中途断电、被杀、或者使用者主动中断之后，
//! 之前算完的东西**没有任何记录**，只能从头再来。这一层把「跑到哪了、产物是什么、产物还对不对」
//! 落成一份可校验的检查点。
//!
//! 两条关键设计（都是为了避免「续跑续出错误结果」这种最难查的故障）：
//!
//! 1. **输入指纹**：检查点里记着它所对应输入的摘要。换了素材或改了参数还想续跑，
//!    会直接报 [`CheckpointError::InputChanged`]，而不是把两批产物混在一起；
//! 2. **产物校验**：标成完成的步骤，续跑前要重新核对产物**存在且摘要一致**；
//!    对不上就把该步骤降级回待执行（并记下原因），而不是拿着坏文件往下走。
//!
//! 「运行中」的步骤在续跑时一律视为未完成：进程被杀的那一刻它的产物就是不可信的半成品。

use std::path::{Path, PathBuf};

use dossier_runtime::sha256_file;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 检查点文件的结构版本。加了新字段且**不兼容**时必须递增，并在加载处拒绝旧文件。
pub const CHECKPOINT_VERSION: u32 = 1;

/// 检查点错误。
#[derive(Debug, Error)]
pub enum CheckpointError {
    #[error("检查点文件读写失败（{path}）：{message}")]
    Io { path: PathBuf, message: String },
    #[error("检查点内容无法解析：{0}")]
    Parse(String),
    #[error("检查点结构版本不支持：文件为 v{found}，本程序支持 v{expected}")]
    VersionUnsupported { found: u32, expected: u32 },
    #[error("检查点里没有步骤 `{0}`")]
    UnknownStep(String),
    #[error("步骤 `{step}` 的状态不允许该操作：当前 {from:?}")]
    IllegalTransition { step: String, from: StepStatus },
    #[error("输入已变化，拒绝续跑：检查点记录 {expected}，当前 {actual}")]
    InputChanged { expected: String, actual: String },
    #[error("产物不存在（步骤 {step}）：{path}")]
    ArtifactMissing { step: String, path: PathBuf },
    #[error("产物摘要不一致（步骤 {step}）：{path} 期望 {expected}，实际 {actual}")]
    ArtifactDigestMismatch {
        step: String,
        path: PathBuf,
        expected: String,
        actual: String,
    },
    #[error("产物无法读取（{path}）：{message}")]
    ArtifactUnreadable { path: PathBuf, message: String },
    #[error("检查点必须至少有一个步骤")]
    EmptySteps,
}

/// 一步的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    /// 正在跑。**续跑时视为未完成**：被中断的步骤产物不可信。
    Running,
    Done,
    Failed,
    /// 主动跳过（例如该素材本来就没有硬字幕）。
    Skipped,
}

impl StepStatus {
    /// 是否算「已经做完，不用再跑」。
    pub fn is_settled(self) -> bool {
        matches!(self, Self::Done | Self::Skipped)
    }
}

/// 一个产物的引用：路径 + 摘要 + 字节数。
///
/// 单独存字节数是有用的：续跑时能立刻发现「文件被截断了」而不必读完整文件算摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRef {
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
}

impl ArtifactRef {
    /// 立刻读文件并算摘要。产物必须是**已经写完并落盘**的。
    pub fn capture(path: impl Into<PathBuf>) -> Result<Self, CheckpointError> {
        let path = path.into();
        let bytes = std::fs::metadata(&path)
            .map_err(|error| CheckpointError::ArtifactUnreadable {
                path: path.clone(),
                message: error.to_string(),
            })?
            .len();
        let sha256 = sha256_file(&path).map_err(|error| CheckpointError::ArtifactUnreadable {
            path: path.clone(),
            message: error.to_string(),
        })?;
        Ok(Self {
            path,
            sha256,
            bytes,
        })
    }

    /// 核对产物是否还是当初那一份。
    pub fn verify(&self, step: &str) -> Result<(), CheckpointError> {
        let metadata =
            std::fs::metadata(&self.path).map_err(|_| CheckpointError::ArtifactMissing {
                step: step.to_string(),
                path: self.path.clone(),
            })?;
        if metadata.len() != self.bytes {
            return Err(CheckpointError::ArtifactDigestMismatch {
                step: step.to_string(),
                path: self.path.clone(),
                expected: format!("{} 字节 / {}", self.bytes, self.sha256),
                actual: format!("{} 字节", metadata.len()),
            });
        }
        let actual =
            sha256_file(&self.path).map_err(|error| CheckpointError::ArtifactUnreadable {
                path: self.path.clone(),
                message: error.to_string(),
            })?;
        if actual != self.sha256 {
            return Err(CheckpointError::ArtifactDigestMismatch {
                step: step.to_string(),
                path: self.path.clone(),
                expected: self.sha256.clone(),
                actual,
            });
        }
        Ok(())
    }
}

/// 一步的记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepState {
    pub name: String,
    pub status: StepStatus,
    /// 已经尝试过几次（含失败与重试）。
    pub attempts: u32,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub finished_at: Option<String>,
    #[serde(default)]
    pub artifacts: Vec<ArtifactRef>,
    #[serde(default)]
    pub error: Option<String>,
    /// 续跑时被降级的原因（「产物丢了」「上次跑到一半被杀」），便于对账。
    #[serde(default)]
    pub reset_reason: Option<String>,
}

/// 整份检查点。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    pub version: u32,
    pub task_id: String,
    pub pipeline: String,
    pub pipeline_version: String,
    /// 输入指纹（素材 + 关键参数的摘要，见 [`input_fingerprint`]）。
    pub input_fingerprint: String,
    pub steps: Vec<StepState>,
    pub created_at: String,
    pub updated_at: String,
}

impl Checkpoint {
    /// 用步骤名建一份新检查点；顺序即执行顺序。
    pub fn new(
        task_id: impl Into<String>,
        pipeline: impl Into<String>,
        pipeline_version: impl Into<String>,
        input_fingerprint: impl Into<String>,
        step_names: &[&str],
    ) -> Result<Self, CheckpointError> {
        if step_names.is_empty() {
            return Err(CheckpointError::EmptySteps);
        }
        let now = now_iso8601();
        Ok(Self {
            version: CHECKPOINT_VERSION,
            task_id: task_id.into(),
            pipeline: pipeline.into(),
            pipeline_version: pipeline_version.into(),
            input_fingerprint: input_fingerprint.into(),
            steps: step_names
                .iter()
                .map(|name| StepState {
                    name: (*name).to_string(),
                    status: StepStatus::Pending,
                    attempts: 0,
                    started_at: None,
                    finished_at: None,
                    artifacts: Vec::new(),
                    error: None,
                    reset_reason: None,
                })
                .collect(),
            created_at: now.clone(),
            updated_at: now,
        })
    }

    fn step_mut(&mut self, name: &str) -> Result<&mut StepState, CheckpointError> {
        self.steps
            .iter_mut()
            .find(|step| step.name == name)
            .ok_or_else(|| CheckpointError::UnknownStep(name.to_string()))
    }

    pub fn step(&self, name: &str) -> Option<&StepState> {
        self.steps.iter().find(|step| step.name == name)
    }

    /// 标记开始执行（`attempts` +1）。
    pub fn mark_running(&mut self, name: &str) -> Result<(), CheckpointError> {
        let now = now_iso8601();
        let step = self.step_mut(name)?;
        step.attempts += 1;
        step.status = StepStatus::Running;
        step.started_at = Some(now.clone());
        step.finished_at = None;
        step.error = None;
        step.reset_reason = None;
        self.updated_at = now;
        Ok(())
    }

    /// 标记完成，并登记产物（产物必须先落盘，否则这里会报错而不是记下一个假摘要）。
    pub fn mark_done(&mut self, name: &str, artifacts: &[PathBuf]) -> Result<(), CheckpointError> {
        let captured = artifacts
            .iter()
            .map(ArtifactRef::capture)
            .collect::<Result<Vec<_>, _>>()?;
        let now = now_iso8601();
        let step = self.step_mut(name)?;
        step.status = StepStatus::Done;
        step.artifacts = captured;
        step.finished_at = Some(now.clone());
        step.error = None;
        self.updated_at = now;
        Ok(())
    }

    pub fn mark_failed(
        &mut self,
        name: &str,
        error: impl Into<String>,
    ) -> Result<(), CheckpointError> {
        let now = now_iso8601();
        let step = self.step_mut(name)?;
        step.status = StepStatus::Failed;
        step.error = Some(error.into());
        step.finished_at = Some(now.clone());
        self.updated_at = now;
        Ok(())
    }

    /// 主动跳过（例如素材没有硬字幕）。跳过的步骤不产出产物，但**记原因**。
    pub fn mark_skipped(
        &mut self,
        name: &str,
        reason: impl Into<String>,
    ) -> Result<(), CheckpointError> {
        let now = now_iso8601();
        let step = self.step_mut(name)?;
        step.status = StepStatus::Skipped;
        step.error = Some(reason.into());
        step.artifacts.clear();
        step.finished_at = Some(now.clone());
        self.updated_at = now;
        Ok(())
    }

    /// 下一个待执行的步骤名（跳过 `Done`/`Skipped`）。
    pub fn next_pending(&self) -> Option<&str> {
        self.steps
            .iter()
            .find(|step| !step.status.is_settled())
            .map(|step| step.name.as_str())
    }

    /// 待执行步骤名（按原顺序）。
    pub fn pending_steps(&self) -> Vec<&str> {
        self.steps
            .iter()
            .filter(|step| !step.status.is_settled())
            .map(|step| step.name.as_str())
            .collect()
    }

    /// 是否全部收口（完成或跳过）。
    pub fn is_complete(&self) -> bool {
        self.steps.iter().all(|step| step.status.is_settled())
    }

    /// 已收口的步骤数 / 总数，可直接喂给进度条。
    pub fn settled_ratio(&self) -> f64 {
        if self.steps.is_empty() {
            return 0.0;
        }
        let settled = self
            .steps
            .iter()
            .filter(|step| step.status.is_settled())
            .count();
        settled as f64 / self.steps.len() as f64
    }

    /// 逐条核对已完成步骤的产物。返回每个被降级的步骤名。
    ///
    /// 不在这里直接改状态：校验与「怎么恢复」是两个决定，分开更好测也更好审。
    pub fn verify_artifacts(&self) -> Result<Vec<String>, CheckpointError> {
        let mut broken = Vec::new();
        for step in &self.steps {
            if step.status != StepStatus::Done {
                continue;
            }
            for artifact in &step.artifacts {
                if artifact.verify(&step.name).is_err() {
                    broken.push(step.name.clone());
                    break;
                }
            }
        }
        Ok(broken)
    }

    /// 续跑：核对输入指纹 → 核对产物 → 回退不可信的步骤 → 返回还要跑的步骤名。
    ///
    /// `actual_fingerprint` 传 `None` 表示「这次不校验输入」（例如只想看看还剩哪几步）。
    pub fn resume(
        &mut self,
        actual_fingerprint: Option<&str>,
    ) -> Result<Vec<String>, CheckpointError> {
        if let Some(actual) = actual_fingerprint {
            if actual != self.input_fingerprint {
                return Err(CheckpointError::InputChanged {
                    expected: self.input_fingerprint.clone(),
                    actual: actual.to_string(),
                });
            }
        }
        let broken = self.verify_artifacts()?;
        let now = now_iso8601();
        for step in &mut self.steps {
            let reason = match step.status {
                StepStatus::Running => Some("上次执行到一半被中断".to_string()),
                StepStatus::Failed => Some("上次执行失败".to_string()),
                StepStatus::Done if broken.contains(&step.name) => {
                    Some("产物缺失或摘要不一致".to_string())
                }
                _ => None,
            };
            if let Some(reason) = reason {
                step.status = StepStatus::Pending;
                step.artifacts.clear();
                step.started_at = None;
                step.finished_at = None;
                step.reset_reason = Some(reason);
            }
        }
        let plan: Vec<String> = self
            .pending_steps()
            .into_iter()
            .map(|name| name.to_string())
            .collect();
        if !plan.is_empty() {
            self.updated_at = now;
        }
        Ok(plan)
    }

    /// 原子保存：先写临时文件再 `rename`。
    ///
    /// 直接覆盖写的话，一旦在写的时候被杀，检查点就变成半截 JSON —— 而检查点本身
    /// 正是「被中断之后用来恢复」的东西，它不能是第一个被中断损坏的文件。
    pub fn save(&self, path: &Path) -> Result<(), CheckpointError> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|error| CheckpointError::Parse(error.to_string()))?;
        let temp = temporary_sibling(path);
        std::fs::write(&temp, text).map_err(|error| CheckpointError::Io {
            path: temp.clone(),
            message: error.to_string(),
        })?;
        std::fs::rename(&temp, path).map_err(|error| CheckpointError::Io {
            path: path.to_path_buf(),
            message: error.to_string(),
        })
    }

    /// 从磁盘读取并校验结构版本。
    pub fn load(path: &Path) -> Result<Self, CheckpointError> {
        let text = std::fs::read_to_string(path).map_err(|error| CheckpointError::Io {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        Self::from_json(&text)
    }

    /// 从 JSON 文本解析（与文件读写分开，便于单测）。
    pub fn from_json(text: &str) -> Result<Self, CheckpointError> {
        let checkpoint: Self = serde_json::from_str(text)
            .map_err(|error| CheckpointError::Parse(error.to_string()))?;
        if checkpoint.version != CHECKPOINT_VERSION {
            return Err(CheckpointError::VersionUnsupported {
                found: checkpoint.version,
                expected: CHECKPOINT_VERSION,
            });
        }
        if checkpoint.steps.is_empty() {
            return Err(CheckpointError::EmptySteps);
        }
        Ok(checkpoint)
    }
}

/// 保存检查点（[`Checkpoint::save`] 的自由函数形式）。
pub fn save_checkpoint(checkpoint: &Checkpoint, path: &Path) -> Result<(), CheckpointError> {
    checkpoint.save(path)
}

/// 读取检查点（[`Checkpoint::load`] 的自由函数形式）。
pub fn load_checkpoint(path: &Path) -> Result<Checkpoint, CheckpointError> {
    Checkpoint::load(path)
}

/// 计算输入指纹：把「跑这条管线真正会改变结果的输入」按固定顺序拼起来再取摘要。
///
/// 只传**会影响结果**的东西（素材摘要、关键参数），不要把时间戳、主机名这类无关项混进来——
/// 那会让「输入其实没变」的续跑被误判成输入变化，从而白白重跑。
pub fn input_fingerprint(parts: &[(&str, &str)]) -> String {
    let mut sorted: Vec<&(&str, &str)> = parts.iter().collect();
    sorted.sort_by(|left, right| left.0.cmp(right.0));
    let mut buffer = String::new();
    for (key, value) in sorted {
        buffer.push_str(key);
        buffer.push('=');
        buffer.push_str(value);
        buffer.push('\n');
    }
    dossier_runtime::sha256_bytes(buffer.as_bytes())
}

fn temporary_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "checkpoint".to_string());
    path.with_file_name(format!(".{name}.tmp{}", std::process::id()))
}

fn now_iso8601() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dossier-ckpt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_artifact(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    fn new_checkpoint(fingerprint: &str) -> Checkpoint {
        Checkpoint::new(
            "task-1",
            "drama.logo.erase",
            "1.0.0",
            fingerprint,
            &["detect", "mask", "inpaint", "verify"],
        )
        .unwrap()
    }

    #[test]
    fn a_new_checkpoint_starts_at_the_first_step() {
        let checkpoint = new_checkpoint("fp");
        assert_eq!(checkpoint.next_pending(), Some("detect"));
        assert_eq!(
            checkpoint.pending_steps(),
            vec!["detect", "mask", "inpaint", "verify"]
        );
        assert!(!checkpoint.is_complete());
        assert_eq!(checkpoint.settled_ratio(), 0.0);
    }

    #[test]
    fn empty_step_list_is_rejected() {
        assert!(matches!(
            Checkpoint::new("t", "p", "1", "fp", &[]),
            Err(CheckpointError::EmptySteps)
        ));
    }

    #[test]
    fn unknown_step_names_are_rejected() {
        let mut checkpoint = new_checkpoint("fp");
        assert!(matches!(
            checkpoint.mark_running("nope"),
            Err(CheckpointError::UnknownStep(_))
        ));
    }

    #[test]
    fn marking_done_records_artifact_digests_and_advances() {
        let dir = temp_dir("done");
        let checkpoint_path = dir.join("checkpoint.json");
        let mut checkpoint = new_checkpoint("fp");
        let artifact = write_artifact(&dir, "detect.json", b"{\"logo\":true}");

        checkpoint.mark_running("detect").unwrap();
        checkpoint.mark_done("detect", &[artifact]).unwrap();
        assert_eq!(checkpoint.next_pending(), Some("mask"));

        let step = checkpoint.step("detect").unwrap();
        assert_eq!(step.attempts, 1);
        assert_eq!(step.artifacts.len(), 1);
        assert_eq!(step.artifacts[0].bytes, 13);
        assert_eq!(
            step.artifacts[0].sha256,
            dossier_runtime::sha256_bytes(b"{\"logo\":true}")
        );

        checkpoint.save(&checkpoint_path).unwrap();
        let reloaded = load_checkpoint(&checkpoint_path).unwrap();
        assert_eq!(reloaded, checkpoint);
        assert_eq!(reloaded.settled_ratio(), 0.25);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 产物不在磁盘上就不能标完成：否则检查点会记下一个「Done 但没有产物」的步骤，
    /// 续跑时谁也发现不了。
    #[test]
    fn marking_done_fails_when_the_artifact_is_not_on_disk() {
        let dir = temp_dir("missing-artifact");
        let mut checkpoint = new_checkpoint("fp");
        checkpoint.mark_running("detect").unwrap();
        let error = checkpoint
            .mark_done("detect", &[dir.join("nope.json")])
            .unwrap_err();
        assert!(matches!(error, CheckpointError::ArtifactUnreadable { .. }));
        // 产物登记失败发生在改状态之前，所以状态仍是 Running，不会被误标成 Done。
        assert_eq!(
            checkpoint.step("detect").unwrap().status,
            StepStatus::Running
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 保存必须原子：临时文件不能被留在目录里，也不能出现「写到一半」的检查点。
    #[test]
    fn save_is_atomic_so_a_half_written_file_never_replaces_a_good_one() {
        let dir = temp_dir("atomic");
        let path = dir.join("checkpoint.json");
        let mut checkpoint = new_checkpoint("fp");
        checkpoint.save(&path).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        checkpoint.mark_running("detect").unwrap();
        checkpoint.save(&path).unwrap();
        assert_ne!(std::fs::read_to_string(&path).unwrap(), first);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_mismatch_is_refused_instead_of_guessed() {
        let text = serde_json::to_string(&new_checkpoint("fp"))
            .unwrap()
            .replace("\"version\":1", "\"version\":99");
        let error = Checkpoint::from_json(&text).unwrap_err();
        assert!(matches!(
            error,
            CheckpointError::VersionUnsupported { found: 99, .. }
        ));
    }

    #[test]
    fn resume_rejects_a_different_input_fingerprint() {
        let mut checkpoint = new_checkpoint("fp-old");
        let error = checkpoint.resume(Some("fp-new")).unwrap_err();
        assert!(matches!(error, CheckpointError::InputChanged { .. }));
        // 拒绝之后检查点必须原封不动。
        assert_eq!(checkpoint.next_pending(), Some("detect"));
    }

    #[test]
    fn resume_skips_settled_steps_and_reruns_interrupted_ones() {
        let dir = temp_dir("resume");
        let mut checkpoint = new_checkpoint("fp");
        let artifact = write_artifact(&dir, "detect.json", b"ok");
        checkpoint.mark_running("detect").unwrap();
        checkpoint.mark_done("detect", &[artifact]).unwrap();
        // 第二、三步跑过但没标完成（模拟被杀）
        checkpoint.mark_running("mask").unwrap();
        checkpoint.mark_running("inpaint").unwrap();
        checkpoint.mark_skipped("verify", "素材没有硬字幕").unwrap();

        let plan = checkpoint.resume(Some("fp")).unwrap();
        assert_eq!(plan, vec!["mask", "inpaint"]);
        assert_eq!(
            checkpoint.step("mask").unwrap().reset_reason.as_deref(),
            Some("上次执行到一半被中断")
        );
        // 已完成且产物完好的步骤不受影响；跳过的步骤不会被重新捡起来。
        assert_eq!(checkpoint.step("detect").unwrap().status, StepStatus::Done);
        assert_eq!(
            checkpoint.step("verify").unwrap().status,
            StepStatus::Skipped
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 这条是续跑的安全底线：产物被改动过就绝不能当「已完成」用。
    #[test]
    fn resume_invalidates_a_step_whose_artifact_changed() {
        let dir = temp_dir("tampered");
        let mut checkpoint = new_checkpoint("fp");
        let artifact = write_artifact(&dir, "detect.json", b"good");
        checkpoint.mark_running("detect").unwrap();
        checkpoint
            .mark_done("detect", std::slice::from_ref(&artifact))
            .unwrap();
        std::fs::write(&artifact, b"tampered-content").unwrap();

        let plan = checkpoint.resume(Some("fp")).unwrap();
        assert_eq!(plan, vec!["detect", "mask", "inpaint", "verify"]);
        let step = checkpoint.step("detect").unwrap();
        assert_eq!(step.status, StepStatus::Pending);
        assert_eq!(step.reset_reason.as_deref(), Some("产物缺失或摘要不一致"));
        assert!(step.artifacts.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_invalidates_a_step_whose_artifact_disappeared() {
        let dir = temp_dir("gone");
        let mut checkpoint = new_checkpoint("fp");
        let artifact = write_artifact(&dir, "detect.json", b"good");
        checkpoint.mark_running("detect").unwrap();
        checkpoint
            .mark_done("detect", std::slice::from_ref(&artifact))
            .unwrap();
        std::fs::remove_file(&artifact).unwrap();

        let plan = checkpoint.resume(None).unwrap();
        assert!(plan.contains(&"detect".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_complete_checkpoint_has_nothing_left_to_run() {
        let dir = temp_dir("complete");
        let mut checkpoint = new_checkpoint("fp");
        for name in ["detect", "mask", "inpaint"] {
            let artifact = write_artifact(&dir, &format!("{name}.json"), b"x");
            checkpoint.mark_running(name).unwrap();
            checkpoint.mark_done(name, &[artifact]).unwrap();
        }
        checkpoint.mark_skipped("verify", "无需校验").unwrap();
        assert!(checkpoint.is_complete());
        assert_eq!(checkpoint.next_pending(), None);
        assert_eq!(checkpoint.settled_ratio(), 1.0);
        assert!(checkpoint.resume(Some("fp")).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_steps_carry_their_error_and_can_be_retried() {
        let dir = temp_dir("failed");
        let mut checkpoint = new_checkpoint("fp");
        checkpoint.mark_running("detect").unwrap();
        checkpoint.mark_failed("detect", "worker 崩了").unwrap();
        assert_eq!(
            checkpoint.step("detect").unwrap().error.as_deref(),
            Some("worker 崩了")
        );
        let plan = checkpoint.resume(Some("fp")).unwrap();
        assert_eq!(plan.first().map(String::as_str), Some("detect"));
        // 重试时 attempts 累加，能看出到底试了几次。
        checkpoint.mark_running("detect").unwrap();
        assert_eq!(checkpoint.step("detect").unwrap().attempts, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 指纹只认「会影响结果」的键值，且与传入顺序无关：否则续跑会被无关的顺序差异打断。
    #[test]
    fn input_fingerprint_ignores_argument_order() {
        let left = input_fingerprint(&[("source", "abc"), ("dilate", "4")]);
        let right = input_fingerprint(&[("dilate", "4"), ("source", "abc")]);
        assert_eq!(left, right);
        let changed = input_fingerprint(&[("source", "abc"), ("dilate", "8")]);
        assert_ne!(left, changed);
        assert!(dossier_runtime::is_sha256_hex(&left));
    }
}
