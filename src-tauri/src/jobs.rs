//! 批量处理队列（M4b-1，计划 2.6 节）：先进先出，同一时间只跑一个任务。
//!
//! 视频编码本身就吃满 CPU / GPU，并行两个任务只会都变慢，所以队列只有一个工作线程。
//! 任务的最小单位是「一集」：
//! - 暂停：当前这一集做完后停下（状态「已暂停」）；
//! - 终止：立刻结束 ffmpeg 子进程，删掉这一集的半成品（状态「已终止」，可以续跑）；
//! - 续跑：重新排队，已完成且产物哈希对得上的集跳过（`dossier-core::checkpoint`）。
//!
//! 持久化：每个任务一个目录（应用数据目录 `jobs/<任务号>/`），里面是 `job.json`（任务描述，
//! 只写一次）和 `checkpoint.json`（逐集进度，每集完成后原子写）。应用重启后从磁盘读回，
//! 没做完的任务一律显示为「已暂停」，用户点「继续」才接着跑——不在用户不知情时自动开工。
//!
//! 本模块不懂视频：具体每集怎么处理由 [`JobRunner`] 实现（`version.rs`）。这样队列的状态机
//! 可以用假的执行器直接单测。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use dossier_core::{Checkpoint, StepStatus};
use serde::{Deserialize, Serialize};

use crate::encode::CancelFlag;

/// 任务状态（界面上的六种）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Paused,
    Terminated,
    Done,
    Failed,
}

/// 任务描述：创建时写一次 `job.json`，之后只读。执行器需要的全部输入都在 `spec` 里。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRecord {
    pub id: String,
    /// 任务种类（`version`；M4b-2 的擦除也会走这个队列）。
    pub kind: String,
    /// 界面显示的标题（剧名 + 档位）。
    pub title: String,
    pub created_at: String,
    /// 产物目录（「打开目录」按钮）。
    pub output_dir: String,
    /// 种类相关的任务参数（执行器自己解析）。
    pub spec: serde_json::Value,
}

/// 提交新任务的参数。
pub struct NewJob {
    pub kind: String,
    pub title: String,
    pub output_dir: String,
    pub spec: serde_json::Value,
    /// 输入指纹（处理版本用任务哈希）。
    pub fingerprint: String,
    /// 步骤名，按执行顺序，互不相同。
    pub steps: Vec<String>,
    /// 放进任务目录的附带文件（文件名、内容），如配方原文。
    pub files: Vec<(String, Vec<u8>)>,
}

/// 一集的执行结果（执行器返回，原样记进检查点和界面）。
#[derive(Debug, Clone)]
pub struct StepOutcome {
    /// 产物文件（检查点会记下它们的 SHA-256，续跑时核对）。
    pub artifacts: Vec<PathBuf>,
    /// 给界面看的一句话（画质分数、编码器）。
    pub detail: String,
}

/// 一集失败的原因。
#[derive(Debug, Clone, PartialEq)]
pub enum StepError {
    /// 用户点了终止：子进程已杀、半成品已删。
    Cancelled,
    /// 这一步不需要做（例如重新分集实际分出的集数比计划少），记原因后继续下一步。
    Skipped(String),
    /// 这一集失败（画质不达标、ffmpeg 报错……），整个任务停下，标为失败。
    Failed(String),
}

/// 每种任务的执行器。
pub trait JobRunner: Send + Sync {
    /// 开跑前的准备（重新校验配方、选编码器……）。失败则任务直接标为失败、一集都不跑。
    /// `dir` 是任务目录（执行器可以在里面放自己的文件，如配方原文、每集结果）。
    fn prepare(&self, job: &JobRecord, dir: &Path) -> Result<Box<dyn PreparedJob>, String>;
}

/// 准备好的任务：按检查点顺序逐步执行（收尾也作为最后一步，失败了可以单独续跑）。
pub trait PreparedJob: Send {
    fn run_step(&mut self, step: &str, cancel: &CancelFlag) -> Result<StepOutcome, StepError>;
}

/// 界面上的一行任务。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobView {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub created_at: String,
    pub output_dir: String,
    pub state: JobState,
    pub done: usize,
    pub total: usize,
    /// 正在处理 / 停在哪一集。
    pub current: Option<String>,
    /// 失败或终止的原因。
    pub error: Option<String>,
    /// 每集的状态与说明。
    pub steps: Vec<StepView>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepView {
    pub name: String,
    pub status: StepStatus,
    pub detail: Option<String>,
}

/// 队列操作的错误（直接展示给用户）。
#[derive(Debug, Clone, PartialEq)]
pub enum JobError {
    NotFound,
    BadState(&'static str),
    Io(String),
}

impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobError::NotFound => write!(f, "找不到这个任务"),
            JobError::BadState(message) => write!(f, "{message}"),
            JobError::Io(message) => write!(f, "{message}"),
        }
    }
}

/// 任务目录里的三个文件。
const JOB_FILE: &str = "job.json";
const CHECKPOINT_FILE: &str = "checkpoint.json";
const STATE_FILE: &str = "state.json";

/// 队列持久化的运行状态（检查点之外的部分）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedState {
    state: JobState,
    #[serde(default)]
    error: Option<String>,
    /// 每集的说明（画质分数、编码器）。
    #[serde(default)]
    details: std::collections::BTreeMap<String, String>,
}

struct Entry {
    record: JobRecord,
    dir: PathBuf,
    checkpoint: Checkpoint,
    state: JobState,
    error: Option<String>,
    details: std::collections::BTreeMap<String, String>,
    /// 运行中点了「暂停」：这一集做完后停。
    pause_requested: bool,
    /// 运行中点了「终止」时置位；每次开跑换一个新的。
    cancel: CancelFlag,
}

impl Entry {
    fn view(&self) -> JobView {
        let total = self.checkpoint.steps.len();
        let done = self
            .checkpoint
            .steps
            .iter()
            .filter(|step| step.status.is_settled())
            .count();
        let current = self
            .checkpoint
            .steps
            .iter()
            .find(|step| step.status == StepStatus::Running)
            .or_else(|| {
                self.checkpoint
                    .steps
                    .iter()
                    .find(|step| !step.status.is_settled())
            })
            .map(|step| step.name.clone());
        JobView {
            id: self.record.id.clone(),
            kind: self.record.kind.clone(),
            title: self.record.title.clone(),
            created_at: self.record.created_at.clone(),
            output_dir: self.record.output_dir.clone(),
            state: self.state,
            done,
            total,
            current,
            error: self.error.clone(),
            steps: self
                .checkpoint
                .steps
                .iter()
                .map(|step| StepView {
                    name: step.name.clone(),
                    status: step.status,
                    detail: self
                        .details
                        .get(&step.name)
                        .cloned()
                        .or_else(|| step.error.clone()),
                })
                .collect(),
        }
    }

    /// 检查点和运行状态一起落盘。检查点用原子写；状态文件同理。
    fn persist(&self) -> Result<(), String> {
        self.checkpoint
            .save(&self.dir.join(CHECKPOINT_FILE))
            .map_err(|error| format!("保存任务进度失败：{error}"))?;
        let state = PersistedState {
            state: self.state,
            error: self.error.clone(),
            details: self.details.clone(),
        };
        write_json_atomic(&self.dir.join(STATE_FILE), &state)
    }
}

struct QueueState {
    jobs: Vec<Entry>,
    pending: VecDeque<String>,
}

impl QueueState {
    fn entry_mut(&mut self, id: &str) -> Option<&mut Entry> {
        self.jobs.iter_mut().find(|entry| entry.record.id == id)
    }
}

type Notify = Arc<dyn Fn(JobView) + Send + Sync>;

struct Shared {
    state: Mutex<QueueState>,
    wake: Condvar,
    root: PathBuf,
    runners: Vec<(String, Arc<dyn JobRunner>)>,
    notify: Notify,
}

/// 任务队列（Tauri 托管状态）。克隆共享同一个队列。
#[derive(Clone)]
pub struct JobQueue {
    shared: Arc<Shared>,
}

/// 队列根目录下的任务目录。
fn job_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.join(JOB_FILE).is_file())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs
}

/// 从磁盘读回一个任务。没做完的一律视为「已暂停」：重启后不在用户不知情时自动开工。
fn load_entry(dir: &Path) -> Result<Entry, String> {
    let record: JobRecord = read_json(&dir.join(JOB_FILE))?;
    let checkpoint = Checkpoint::load(&dir.join(CHECKPOINT_FILE))
        .map_err(|error| format!("任务进度文件无法读取：{error}"))?;
    let persisted: Option<PersistedState> = read_json(&dir.join(STATE_FILE)).ok();
    let (mut state, error, details) = match persisted {
        Some(saved) => (saved.state, saved.error, saved.details),
        None => (JobState::Paused, None, Default::default()),
    };
    if matches!(state, JobState::Queued | JobState::Running) {
        state = JobState::Paused;
    }
    Ok(Entry {
        record,
        dir: dir.to_path_buf(),
        checkpoint,
        state,
        error,
        details,
        pause_requested: false,
        cancel: CancelFlag::new(),
    })
}

impl JobQueue {
    /// 读回磁盘上的任务并启动唯一的工作线程。`notify` 在任务状态变化时调用（发 `job-progress`）。
    pub fn start(
        root: PathBuf,
        runners: Vec<(String, Arc<dyn JobRunner>)>,
        notify: impl Fn(JobView) + Send + Sync + 'static,
    ) -> Self {
        let mut jobs = Vec::new();
        for dir in job_dirs(&root) {
            match load_entry(&dir) {
                Ok(entry) => jobs.push(entry),
                // 读不回来的任务不能静默消失：记日志，目录保留给用户排查。
                Err(error) => eprintln!("任务目录 {} 无法读取：{error}", dir.display()),
            }
        }
        jobs.sort_by(|a, b| a.record.created_at.cmp(&b.record.created_at));
        let queue = Self {
            shared: Arc::new(Shared {
                state: Mutex::new(QueueState {
                    jobs,
                    pending: VecDeque::new(),
                }),
                wake: Condvar::new(),
                root,
                runners,
                notify: Arc::new(notify),
            }),
        };
        let worker = queue.clone();
        std::thread::Builder::new()
            .name("dossier-jobs".into())
            .spawn(move || worker.work_loop())
            .expect("无法启动任务队列线程");
        queue
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        // 工作线程在锁外跑执行器；锁内只有简单状态更新，中毒说明有 bug，直接沿用里面的数据。
        self.shared
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn emit(&self, view: JobView) {
        (self.shared.notify)(view);
    }

    /// 提交新任务：写附带文件、`job.json` 和检查点，然后排队。
    pub fn submit(&self, job: NewJob) -> Result<JobView, JobError> {
        let NewJob {
            kind,
            title,
            output_dir,
            spec,
            fingerprint,
            steps,
            files,
        } = job;
        let mut unique = std::collections::HashSet::new();
        if steps.is_empty() || !steps.iter().all(|step| unique.insert(step.as_str())) {
            return Err(JobError::BadState("任务步骤为空或有重名"));
        }
        let id = new_job_id();
        let dir = self.shared.root.join(&id);
        std::fs::create_dir_all(&dir)
            .map_err(|error| JobError::Io(format!("创建任务目录失败：{error}")))?;
        // 执行器要用的附带文件（如配方原文）必须在排队前落盘：工作线程随时可能开跑。
        for (name, bytes) in &files {
            std::fs::write(dir.join(name), bytes)
                .map_err(|error| JobError::Io(format!("写入任务文件 {name} 失败：{error}")))?;
        }
        let names: Vec<&str> = steps.iter().map(String::as_str).collect();
        let checkpoint = Checkpoint::new(&id, kind.as_str(), "1", fingerprint, &names)
            .map_err(|error| JobError::Io(format!("创建任务进度失败：{error}")))?;
        let record = JobRecord {
            id: id.clone(),
            kind,
            title,
            created_at: dossier_core::now_timestamp(),
            output_dir,
            spec,
        };
        write_json_atomic(&dir.join(JOB_FILE), &record).map_err(JobError::Io)?;
        let entry = Entry {
            record,
            dir,
            checkpoint,
            state: JobState::Queued,
            error: None,
            details: Default::default(),
            pause_requested: false,
            cancel: CancelFlag::new(),
        };
        entry.persist().map_err(JobError::Io)?;
        let view = entry.view();
        {
            let mut state = self.lock();
            state.jobs.push(entry);
            state.pending.push_back(id);
        }
        self.shared.wake.notify_all();
        self.emit(view.clone());
        Ok(view)
    }

    pub fn list(&self) -> Vec<JobView> {
        self.lock().jobs.iter().rev().map(Entry::view).collect()
    }

    /// 暂停：排队中的直接停；运行中的做完当前这一集再停。
    pub fn pause(&self, id: &str) -> Result<JobView, JobError> {
        let view = {
            let mut state = self.lock();
            let entry = state.entry_mut(id).ok_or(JobError::NotFound)?;
            match entry.state {
                JobState::Queued => {
                    entry.state = JobState::Paused;
                    entry.persist().map_err(JobError::Io)?;
                    let view = entry.view();
                    state.pending.retain(|pending| pending != id);
                    view
                }
                JobState::Running => {
                    entry.pause_requested = true;
                    entry.view()
                }
                _ => return Err(JobError::BadState("这个任务没有在排队或运行")),
            }
        };
        self.emit(view.clone());
        Ok(view)
    }

    /// 终止：运行中的立刻结束子进程、删半成品；排队或暂停中的直接标记为已终止。
    pub fn terminate(&self, id: &str) -> Result<JobView, JobError> {
        let view = {
            let mut state = self.lock();
            let entry = state.entry_mut(id).ok_or(JobError::NotFound)?;
            match entry.state {
                JobState::Running => {
                    entry.cancel.cancel();
                    entry.view()
                }
                JobState::Queued | JobState::Paused => {
                    entry.state = JobState::Terminated;
                    entry.error = Some("已终止".into());
                    entry.persist().map_err(JobError::Io)?;
                    let view = entry.view();
                    state.pending.retain(|pending| pending != id);
                    view
                }
                _ => return Err(JobError::BadState("这个任务已经结束了")),
            }
        };
        self.emit(view.clone());
        Ok(view)
    }

    /// 继续：已暂停、已终止或失败的任务重新排队。核对已完成各集的产物，对不上的重做。
    pub fn resume(&self, id: &str) -> Result<JobView, JobError> {
        let view = {
            let mut state = self.lock();
            let entry = state.entry_mut(id).ok_or(JobError::NotFound)?;
            if !matches!(
                entry.state,
                JobState::Paused | JobState::Terminated | JobState::Failed
            ) {
                return Err(JobError::BadState("只有已暂停、已终止或失败的任务可以继续"));
            }
            entry
                .checkpoint
                .resume(None)
                .map_err(|error| JobError::Io(format!("核对已完成的分集失败：{error}")))?;
            for step in &entry.checkpoint.steps {
                if !step.status.is_settled() {
                    entry.details.remove(&step.name);
                }
            }
            entry.state = JobState::Queued;
            entry.error = None;
            entry.pause_requested = false;
            entry.persist().map_err(JobError::Io)?;
            let view = entry.view();
            state.pending.push_back(id.to_string());
            view
        };
        self.shared.wake.notify_all();
        self.emit(view.clone());
        Ok(view)
    }

    fn work_loop(&self) {
        loop {
            let id = {
                let mut state = self.lock();
                loop {
                    if let Some(id) = state.pending.pop_front() {
                        break id;
                    }
                    state = self
                        .shared
                        .wake
                        .wait(state)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
            };
            self.run_job(&id);
        }
    }

    /// 在锁里改一下任务并落盘、通知界面。落盘失败时任务转为失败（磁盘问题不能当没发生）。
    fn update(&self, id: &str, change: impl FnOnce(&mut Entry)) {
        let view = {
            let mut state = self.lock();
            let Some(entry) = state.entry_mut(id) else {
                return;
            };
            change(entry);
            if let Err(error) = entry.persist() {
                entry.state = JobState::Failed;
                entry.error = Some(error);
            }
            entry.view()
        };
        self.emit(view);
    }

    fn run_job(&self, id: &str) {
        let (record, dir, cancel) = {
            let mut state = self.lock();
            let Some(entry) = state.entry_mut(id) else {
                return;
            };
            if entry.state != JobState::Queued {
                return;
            }
            entry.state = JobState::Running;
            entry.cancel = CancelFlag::new();
            (
                entry.record.clone(),
                entry.dir.clone(),
                entry.cancel.clone(),
            )
        };
        self.update(id, |_| {});

        let runner = self
            .shared
            .runners
            .iter()
            .find(|(kind, _)| *kind == record.kind)
            .map(|(_, runner)| runner.clone());
        let prepared = match runner {
            Some(runner) => runner.prepare(&record, &dir),
            None => Err(format!("不认识的任务种类：{}", record.kind)),
        };
        let mut prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                self.update(id, |entry| {
                    entry.state = JobState::Failed;
                    entry.error = Some(error);
                });
                return;
            }
        };

        loop {
            // 取下一集（或者按用户要求停下）。
            let next = {
                let mut state = self.lock();
                let Some(entry) = state.entry_mut(id) else {
                    return;
                };
                if cancel.is_cancelled() {
                    None
                } else if entry.pause_requested {
                    entry.pause_requested = false;
                    entry.state = JobState::Paused;
                    Some(None)
                } else {
                    match entry.checkpoint.next_pending().map(str::to_string) {
                        Some(step) => match entry.checkpoint.mark_running(&step) {
                            Ok(()) => Some(Some(step)),
                            Err(error) => {
                                entry.state = JobState::Failed;
                                entry.error = Some(error.to_string());
                                Some(None)
                            }
                        },
                        None => Some(None),
                    }
                }
            };
            let step = match next {
                // 终止：在两集之间点的，直接标记。
                None => {
                    self.update(id, |entry| {
                        entry.state = JobState::Terminated;
                        entry.error = Some("已终止".into());
                    });
                    return;
                }
                // 没有下一步了（全部完成），或者按要求暂停、或登记失败：把状态落盘后结束。
                Some(None) => {
                    self.update(id, |entry| {
                        if entry.state == JobState::Running && entry.checkpoint.is_complete() {
                            entry.state = JobState::Done;
                        }
                    });
                    return;
                }
                Some(Some(step)) => step,
            };
            self.update(id, |_| {});

            let outcome = prepared.run_step(&step, &cancel);
            let mut stop = false;
            self.update(id, |entry| match outcome {
                Ok(outcome) => match entry.checkpoint.mark_done(&step, &outcome.artifacts) {
                    Ok(()) => {
                        entry.details.insert(step.clone(), outcome.detail);
                    }
                    Err(error) => {
                        let _ = entry.checkpoint.mark_failed(&step, error.to_string());
                        entry.state = JobState::Failed;
                        entry.error = Some(format!("{step}：产物登记失败：{error}"));
                        stop = true;
                    }
                },
                Err(StepError::Skipped(reason)) => {
                    if let Err(error) = entry.checkpoint.mark_skipped(&step, reason.clone()) {
                        entry.state = JobState::Failed;
                        entry.error = Some(format!("{step}：{error}"));
                        stop = true;
                    } else {
                        entry
                            .details
                            .insert(step.clone(), format!("已跳过：{reason}"));
                    }
                }
                Err(StepError::Cancelled) => {
                    let _ = entry.checkpoint.mark_failed(&step, "已终止，半成品已删除");
                    entry.state = JobState::Terminated;
                    entry.error = Some("已终止".into());
                    stop = true;
                }
                Err(StepError::Failed(message)) => {
                    let _ = entry.checkpoint.mark_failed(&step, message.clone());
                    entry.state = JobState::Failed;
                    entry.error = Some(format!("{step}：{message}"));
                    stop = true;
                }
            });
            if stop {
                return;
            }
        }
    }
}

/// 任务号：时间（便于按目录名排序）+ 进程内计数（同一纳秒提交两个也不撞）。
fn new_job_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!(
        "{nanos:024x}-{:04x}",
        COUNTER.fetch_add(1, Ordering::Relaxed) & 0xffff
    )
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("读取 {} 失败：{error}", path.display()))?;
    serde_json::from_str(&text).map_err(|error| format!("{} 格式不正确：{error}", path.display()))
}

/// 原子写 JSON：先写同目录临时文件再改名，写到一半被杀也不会留下半个文件。
pub(crate) fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| format!("序列化 {} 失败：{error}", path.display()))?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = path.with_file_name(format!(".{name}.tmp{}", std::process::id()));
    std::fs::write(&temp, text)
        .map_err(|error| format!("写入 {} 失败：{error}", path.display()))?;
    std::fs::rename(&temp, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        format!("写入 {} 失败：{error}", path.display())
    })
}

// ---------------------------------------------------------------------------
// Tauri 命令
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn job_list(queue: tauri::State<'_, JobQueue>) -> Vec<JobView> {
    queue.list()
}

#[tauri::command]
pub fn job_pause(queue: tauri::State<'_, JobQueue>, id: String) -> Result<JobView, String> {
    queue.pause(&id).map_err(|error| error.to_string())
}

#[tauri::command]
pub fn job_resume(queue: tauri::State<'_, JobQueue>, id: String) -> Result<JobView, String> {
    queue.resume(&id).map_err(|error| error.to_string())
}

#[tauri::command]
pub fn job_terminate(queue: tauri::State<'_, JobQueue>, id: String) -> Result<JobView, String> {
    queue.terminate(&id).map_err(|error| error.to_string())
}
