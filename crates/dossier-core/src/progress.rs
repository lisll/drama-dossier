//! 长任务进度流（Tier 3）。
//!
//! 为什么单独做一层：在这之前桌面端**一条进度事件都没发过**（`src-tauri` 里的 emit 计数为 0），
//! 云端也只有一个任务状态字段。结果是长任务（逐帧修复、逐集转码）跑到一半时，使用者只能看到
//! 一个静止的 `RUNNING`，既不知道跑了多少，也不知道卡在哪一步。
//!
//! 这一层把「进度」定义成**可落盘、可追加、可续读**的数据，而不是内存里的一个百分比：
//!
//! - **JSONL 追加写**：长任务随时可能被杀，追加写保证已写完的行永远可解析，尾部半行直接丢弃；
//! - **序号严格递增**：外部 tailer（桌面端 / 云端 Web）靠 `seq` 去重、断点续读；
//! - **百分比是算出来的**：只存「已完成单元数 / 总单元数」，不存百分比，
//!   避免落盘的百分比与两个计数互相矛盾——这类矛盾在界面上表现为进度条来回跳，很难查。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 进度流错误。
#[derive(Debug, Error)]
pub enum ProgressError {
    #[error("写进度文件失败（{path}）：{message}")]
    Io { path: PathBuf, message: String },
    #[error("进度事件不合法：{0}")]
    Invalid(String),
    #[error("进度事件乱序：{reason}（上一条 seq={previous_seq}）")]
    OutOfOrder { reason: String, previous_seq: u64 },
}

/// 一个时间点上的任务进度（纯数据，可直接序列化给界面）。
///
/// `stage_index` 从 0 开始；`stage_total` 是阶段总数，所以「全任务进度」可以由
/// 「阶段序号 + 阶段内进度」推出来（见 [`TaskProgress::overall_fraction`]），
/// 不需要每帧再算一次权重表。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskProgress {
    pub task_id: String,
    /// 管线标识，例如 `drama.logo.erase`。
    pub pipeline: String,
    /// 阶段名，例如 `inpaint`。
    pub stage: String,
    pub stage_index: u32,
    pub stage_total: u32,
    pub unit_done: u64,
    pub unit_total: u64,
    #[serde(default)]
    pub message: String,
}

impl TaskProgress {
    /// 新建一条阶段内进度（`unit_total` 为 0 表示「该阶段还不知道总量」）。
    pub fn new(
        task_id: impl Into<String>,
        pipeline: impl Into<String>,
        stage: impl Into<String>,
        stage_index: u32,
        stage_total: u32,
    ) -> Self {
        Self {
            task_id: task_id.into(),
            pipeline: pipeline.into(),
            stage: stage.into(),
            stage_index,
            stage_total,
            unit_done: 0,
            unit_total: 0,
            message: String::new(),
        }
    }

    /// 带计数的链式写法。
    #[must_use]
    pub fn with_units(mut self, done: u64, total: u64) -> Self {
        self.unit_done = done;
        self.unit_total = total;
        self
    }

    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }

    /// 自洽性检查。**不检查顺序**——顺序是 [`ProgressRecorder`] 的职责。
    pub fn validate(&self) -> Result<(), ProgressError> {
        if self.task_id.trim().is_empty() {
            return Err(ProgressError::Invalid("task_id 不能为空".into()));
        }
        if self.pipeline.trim().is_empty() {
            return Err(ProgressError::Invalid("pipeline 不能为空".into()));
        }
        if self.stage.trim().is_empty() {
            return Err(ProgressError::Invalid("stage 不能为空".into()));
        }
        if self.stage_total == 0 {
            return Err(ProgressError::Invalid("stage_total 至少为 1".into()));
        }
        if self.stage_index >= self.stage_total {
            return Err(ProgressError::Invalid(format!(
                "stage_index({}) 必须小于 stage_total({})",
                self.stage_index, self.stage_total
            )));
        }
        if self.unit_done > self.unit_total {
            return Err(ProgressError::Invalid(format!(
                "unit_done({}) 不能大于 unit_total({})",
                self.unit_done, self.unit_total
            )));
        }
        Ok(())
    }

    /// 当前阶段内的完成比例。总量未知（`unit_total == 0`）时返回 0.0——
    /// 返回 1.0 会让界面在「还没开始」时显示满格。
    pub fn fraction(&self) -> f64 {
        if self.unit_total == 0 {
            return 0.0;
        }
        (self.unit_done as f64 / self.unit_total as f64).clamp(0.0, 1.0)
    }

    /// 百分比（0–100），界面直接可用。
    pub fn percent(&self) -> u32 {
        (self.fraction() * 100.0).round() as u32
    }

    /// 全任务进度：把「已完成的阶段」各按 1 计，当前阶段按 [`TaskProgress::fraction`] 计。
    pub fn overall_fraction(&self) -> f64 {
        let total = f64::from(self.stage_total.max(1));
        ((f64::from(self.stage_index) + self.fraction()) / total).clamp(0.0, 1.0)
    }
}

/// 事件类型。终态之后再来的事件一定是 bug，[`ProgressRecorder`] 会拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressEventKind {
    /// 一次（重新）开始：**允许**阶段内计数归零，用于重试。
    Started,
    Progress,
    Finished,
    Failed,
}

impl ProgressEventKind {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Finished | Self::Failed)
    }
}

/// 落盘的一行：`seq` 由记录器分配，`at` 是 ISO-8601 UTC 字符串。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    pub seq: u64,
    pub at: String,
    pub kind: ProgressEventKind,
    #[serde(flatten)]
    pub progress: TaskProgress,
}

impl ProgressEvent {
    pub fn is_terminal(&self) -> bool {
        self.kind.is_terminal()
    }
}

/// 读回来的进度日志。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressLog {
    pub events: Vec<ProgressEvent>,
    /// 解析不了的行数（含尾部被截断的半行）。
    pub skipped_lines: usize,
    /// 末行没有换行符 → 写入过程中被打断，这一行已被丢弃。
    pub truncated_tail: bool,
}

impl ProgressLog {
    /// 最后一条事件。
    pub fn last(&self) -> Option<&ProgressEvent> {
        self.events.last()
    }

    /// 当前进度（没有事件时为 `None`）。
    pub fn current(&self) -> Option<&TaskProgress> {
        self.events.last().map(|event| &event.progress)
    }

    /// 是否已进入终态。
    pub fn is_terminal(&self) -> bool {
        self.last().map(ProgressEvent::is_terminal).unwrap_or(false)
    }
}

/// 追加式进度记录器：一条事件一行 JSON。
///
/// 顺序约束（这正是「进度条来回跳」的根因，所以在这里一次性挡住）：
/// 1. `seq` 由记录器分配，调用方无法伪造；
/// 2. 阶段序号不得回退；同一阶段内 `unit_done` 不得回退；
/// 3. 终态之后只接受 [`ProgressEventKind::Started`]（用于「重试」这种合法重开）。
pub struct ProgressRecorder {
    path: PathBuf,
    file: std::fs::File,
    seq: u64,
    last: Option<ProgressEvent>,
    latest: Option<TaskProgress>,
    terminal: bool,
}

impl ProgressRecorder {
    /// 新建（覆盖已有文件）。
    pub fn create(path: impl Into<PathBuf>) -> Result<Self, ProgressError> {
        let path = path.into();
        let file = std::fs::File::create(&path).map_err(|error| ProgressError::Io {
            path: path.clone(),
            message: error.to_string(),
        })?;
        Ok(Self {
            path,
            file,
            seq: 0,
            last: None,
            latest: None,
            terminal: false,
        })
    }

    /// 续写已有文件（任务被杀后接着跑）。读不回来的尾部半行会被丢弃。
    pub fn resume(path: impl Into<PathBuf>) -> Result<Self, ProgressError> {
        let path = path.into();
        let log = load_progress_log(&path)?;
        let seq = log.last().map(|event| event.seq).unwrap_or(0);
        let terminal = log.is_terminal();
        let latest = log.current().cloned();
        let file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .map_err(|error| ProgressError::Io {
                path: path.clone(),
                message: error.to_string(),
            })?;
        Ok(Self {
            path,
            file,
            seq,
            last: log.last().cloned(),
            latest,
            terminal,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 已经写出的事件条数（≈ 界面上的 emit 计数）。
    pub fn emitted(&self) -> u64 {
        self.seq
    }

    /// 最近一条事件。
    pub fn last_event(&self) -> Option<&ProgressEvent> {
        self.last.as_ref()
    }

    /// 记录一条事件，返回带 `seq`/`at` 的完整事件。
    pub fn record(
        &mut self,
        kind: ProgressEventKind,
        progress: &TaskProgress,
    ) -> Result<ProgressEvent, ProgressError> {
        progress.validate()?;
        if self.terminal && kind != ProgressEventKind::Started {
            return Err(ProgressError::OutOfOrder {
                reason: "任务已进入终态，只接受 Started（重试）".to_string(),
                previous_seq: self.seq,
            });
        }
        if kind != ProgressEventKind::Started && self.latest.is_none() {
            return Err(ProgressError::OutOfOrder {
                reason: "第一条事件必须是 Started".to_string(),
                previous_seq: self.seq,
            });
        }
        if let Some(previous) = &self.latest {
            if progress.stage_index < previous.stage_index {
                return Err(ProgressError::OutOfOrder {
                    reason: format!(
                        "阶段从 {} 退回到 {}",
                        previous.stage_index, progress.stage_index
                    ),
                    previous_seq: self.seq,
                });
            }
            // Started 允许同阶段计数归零（重试），其余事件必须单调不减。
            if kind != ProgressEventKind::Started
                && progress.stage_index == previous.stage_index
                && progress.unit_done < previous.unit_done
            {
                return Err(ProgressError::OutOfOrder {
                    reason: format!(
                        "同一阶段内 unit_done 从 {} 退回到 {}",
                        previous.unit_done, progress.unit_done
                    ),
                    previous_seq: self.seq,
                });
            }
        }

        self.seq += 1;
        let event = ProgressEvent {
            seq: self.seq,
            at: now_iso8601(),
            kind,
            progress: progress.clone(),
        };
        let mut line = serde_json::to_string(&event)
            .map_err(|error| ProgressError::Invalid(format!("进度事件无法序列化：{error}")))?;
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .map_err(|error| ProgressError::Io {
                path: self.path.clone(),
                message: error.to_string(),
            })?;
        // 显式 flush：长任务的下一个动作可能是被 kill，留在缓冲区里的进度没有价值。
        self.file.flush().map_err(|error| ProgressError::Io {
            path: self.path.clone(),
            message: error.to_string(),
        })?;

        self.terminal = kind.is_terminal();
        self.latest = Some(progress.clone());
        self.last = Some(event.clone());
        Ok(event)
    }
}

/// 读取进度日志：坏行跳过并计数，末行缺换行视为「写入被打断」。
pub fn load_progress_log(path: &Path) -> Result<ProgressLog, ProgressError> {
    let text = std::fs::read_to_string(path).map_err(|error| ProgressError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    Ok(parse_progress_log(&text))
}

/// 解析进度日志文本（与文件读写分开，便于单测）。
pub fn parse_progress_log(text: &str) -> ProgressLog {
    let mut log = ProgressLog::default();
    if text.is_empty() {
        return log;
    }
    log.truncated_tail = !text.ends_with('\n');
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<ProgressEvent>(line) {
            Ok(event) => log.events.push(event),
            Err(_) => log.skipped_lines += 1,
        }
    }
    log
}

fn now_iso8601() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(task: &str) -> TaskProgress {
        TaskProgress::new(task, "drama.logo.erase", "detect", 0, 4)
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("dossier-progress-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn fraction_and_percent_come_from_the_unit_counts() {
        let progress = sample("t1").with_units(3, 4);
        assert!((progress.fraction() - 0.75).abs() < 1e-9);
        assert_eq!(progress.percent(), 75);
        // 总量未知时是 0，不是 1：界面不该在「还没开始」时显示满格。
        assert_eq!(sample("t1").fraction(), 0.0);
    }

    #[test]
    fn overall_fraction_weights_stages_equally() {
        let mut progress = sample("t1").with_units(1, 2);
        progress.stage_index = 1;
        progress.stage_total = 4;
        // 第 2/4 个阶段跑到 50% → (1 + 0.5) / 4 = 0.375
        assert!((progress.overall_fraction() - 0.375).abs() < 1e-9);
    }

    #[test]
    fn validate_rejects_impossible_numbers() {
        let mut progress = sample("t1").with_units(5, 2);
        assert!(matches!(
            progress.validate(),
            Err(ProgressError::Invalid(_))
        ));
        progress.unit_done = 1;
        progress.stage_index = 4;
        assert!(matches!(
            progress.validate(),
            Err(ProgressError::Invalid(_))
        ));
        progress.stage_index = 1;
        progress.stage_total = 0;
        assert!(matches!(
            progress.validate(),
            Err(ProgressError::Invalid(_))
        ));
    }

    #[test]
    fn recorder_assigns_monotonic_seq_and_persists_jsonl() {
        let dir = temp_dir("seq");
        let path = dir.join("progress.jsonl");
        let mut recorder = ProgressRecorder::create(&path).unwrap();
        let started = recorder
            .record(ProgressEventKind::Started, &sample("t1"))
            .unwrap();
        assert_eq!(started.seq, 1);
        let next = recorder
            .record(ProgressEventKind::Progress, &sample("t1").with_units(1, 10))
            .unwrap();
        assert_eq!(next.seq, 2);
        assert_eq!(recorder.emitted(), 2);

        let log = load_progress_log(&path).unwrap();
        assert_eq!(log.events.len(), 2);
        assert_eq!(log.skipped_lines, 0);
        assert!(!log.truncated_tail);
        assert_eq!(log.current().unwrap().unit_done, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 第一条必须是 Started：否则 tailer 会看到一条「不知道属于哪一次运行」的进度。
    #[test]
    fn first_event_must_be_started() {
        let dir = temp_dir("first");
        let path = dir.join("progress.jsonl");
        let mut recorder = ProgressRecorder::create(&path).unwrap();
        let error = recorder
            .record(ProgressEventKind::Progress, &sample("t1").with_units(1, 10))
            .unwrap_err();
        assert!(matches!(error, ProgressError::OutOfOrder { .. }));
        assert_eq!(recorder.emitted(), 0, "被拒绝的事件不该占用 seq");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recorder_rejects_backwards_progress() {
        let dir = temp_dir("backwards");
        let path = dir.join("progress.jsonl");
        let mut recorder = ProgressRecorder::create(&path).unwrap();
        recorder
            .record(ProgressEventKind::Started, &sample("t1"))
            .unwrap();
        recorder
            .record(ProgressEventKind::Progress, &sample("t1").with_units(5, 10))
            .unwrap();
        // 同一阶段内计数回退
        let error = recorder
            .record(ProgressEventKind::Progress, &sample("t1").with_units(4, 10))
            .unwrap_err();
        assert!(matches!(error, ProgressError::OutOfOrder { .. }));
        // 阶段回退
        let mut earlier_stage = sample("t1").with_units(1, 10);
        earlier_stage.stage_total = 4;
        let mut later_stage = earlier_stage.clone();
        later_stage.stage_index = 3;
        recorder
            .record(ProgressEventKind::Progress, &later_stage)
            .unwrap();
        let error = recorder
            .record(ProgressEventKind::Progress, &earlier_stage)
            .unwrap_err();
        assert!(matches!(error, ProgressError::OutOfOrder { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn started_event_allows_a_retry_to_restart_counting() {
        let dir = temp_dir("retry");
        let path = dir.join("progress.jsonl");
        let mut recorder = ProgressRecorder::create(&path).unwrap();
        recorder
            .record(ProgressEventKind::Started, &sample("t1").with_units(0, 10))
            .unwrap();
        recorder
            .record(
                ProgressEventKind::Failed,
                &sample("t1").with_units(3, 10).with_message("boom"),
            )
            .unwrap();
        // 终态之后只接受 Started
        let error = recorder
            .record(ProgressEventKind::Progress, &sample("t1").with_units(4, 10))
            .unwrap_err();
        assert!(matches!(error, ProgressError::OutOfOrder { .. }));
        // 重试：同阶段 + 计数归零，合法
        recorder
            .record(ProgressEventKind::Started, &sample("t1").with_units(0, 10))
            .unwrap();
        assert_eq!(recorder.emitted(), 3);
        let log = load_progress_log(&path).unwrap();
        assert_eq!(log.events.len(), 3);
        assert!(!log.is_terminal(), "重试后最后一条是 Started");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_continues_seq_and_survives_a_truncated_tail() {
        let dir = temp_dir("resume");
        let path = dir.join("progress.jsonl");
        {
            let mut recorder = ProgressRecorder::create(&path).unwrap();
            recorder
                .record(ProgressEventKind::Started, &sample("t1"))
                .unwrap();
            recorder
                .record(ProgressEventKind::Progress, &sample("t1").with_units(2, 10))
                .unwrap();
        }
        // 模拟「正在写第 3 行时被杀」：手写一段半行 JSON。
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(b"{\"seq\":3,\"at\":\"2026-10-14T00:0")
                .unwrap();
        }
        let log = load_progress_log(&path).unwrap();
        assert_eq!(log.events.len(), 2);
        assert_eq!(log.skipped_lines, 1);
        assert!(log.truncated_tail);

        let mut recorder = ProgressRecorder::resume(&path).unwrap();
        // seq 接着已有的最大值走，不会与旧事件撞号。
        let event = recorder
            .record(ProgressEventKind::Progress, &sample("t1").with_units(3, 10))
            .unwrap();
        assert_eq!(event.seq, 3);
        // 续写后仍然检查顺序：新记录器知道上一条是 unit_done=2。
        let error = recorder
            .record(ProgressEventKind::Progress, &sample("t1").with_units(2, 10))
            .unwrap_err();
        assert!(matches!(error, ProgressError::OutOfOrder { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_skips_broken_lines_without_losing_good_ones() {
        let text = "{\"seq\":1,\"at\":\"x\",\"kind\":\"started\",\"taskId\":\"t\",\"pipeline\":\"p\",\"stage\":\"s\",\"stageIndex\":0,\"stageTotal\":1,\"unitDone\":0,\"unitTotal\":0,\"message\":\"\"}\nnot json\n{\"seq\":2,\"at\":\"y\",\"kind\":\"finished\",\"taskId\":\"t\",\"pipeline\":\"p\",\"stage\":\"s\",\"stageIndex\":0,\"stageTotal\":1,\"unitDone\":1,\"unitTotal\":1,\"message\":\"done\"}\n";
        let log = parse_progress_log(text);
        assert_eq!(log.events.len(), 2);
        assert_eq!(log.skipped_lines, 1);
        assert!(!log.truncated_tail);
        assert!(log.is_terminal());
        assert_eq!(log.current().unwrap().percent(), 100);
    }
}
