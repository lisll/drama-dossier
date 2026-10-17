//! 任务队列状态机的测试（M4b-1，计划第 5 节「队列」）。
//!
//! 队列本身不懂视频：用一个假执行器（[`Fake`]）就能把暂停、终止、续跑、重启恢复全测到，
//! 不需要真的编码。真实 ffmpeg 子进程的终止路径在 `encode.rs` 的测试里。
//!
//! 每个测试用自己的临时目录当队列根目录，测完删掉。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dossier_core::StepStatus;

use crate::encode::CancelFlag;
use crate::jobs::{
    JobError, JobQueue, JobRecord, JobRunner, JobState, JobView, NewJob, PreparedJob, StepError,
    StepOutcome,
};

/// 等待队列到达某个状态的上限：假执行器每步都是毫秒级，1 秒绰绰有余。
const WAIT: Duration = Duration::from_secs(5);

/// 一次性临时目录。
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("dossier-jobs-{tag}-{nanos:x}"));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 假执行器：每跑一步就往 `done` 发一条消息，并等 `go` 放行。
///
/// 这样测试能精确控制「现在正停在某一集中间」，不靠 sleep 猜时机。
struct Fake {
    /// 每步开始时发出步骤名。
    started: Sender<String>,
    /// 每步都等这里收到一个指令才继续。
    go: Arc<Mutex<Receiver<Order>>>,
    /// 产物写在这个目录下（续跑要核对哈希）。
    out: PathBuf,
}

/// 测试对某一步的安排。
enum Order {
    /// 正常完成，产物写入 `<out>/<步骤名>.txt`，内容是这个字符串。
    Finish(String),
    /// 这一步跳过。
    Skip(String),
    /// 这一步失败。
    Fail(String),
    /// 模拟「跑到一半被终止」：返回 Cancelled。
    Cancel,
}

impl JobRunner for Fake {
    fn prepare(&self, _job: &JobRecord, _dir: &Path) -> Result<Box<dyn PreparedJob>, String> {
        Ok(Box::new(FakeJob {
            started: self.started.clone(),
            go: self.go.clone(),
            out: self.out.clone(),
        }))
    }
}

struct FakeJob {
    started: Sender<String>,
    go: Arc<Mutex<Receiver<Order>>>,
    out: PathBuf,
}

impl PreparedJob for FakeJob {
    fn run_step(&mut self, step: &str, cancel: &CancelFlag) -> Result<StepOutcome, StepError> {
        let _ = self.started.send(step.to_string());
        let order = {
            let rx = self.go.lock().expect("假执行器的指令通道");
            rx.recv_timeout(WAIT)
                .unwrap_or_else(|_| panic!("测试没有给 {step} 下指令"))
        };
        match order {
            Order::Finish(body) => {
                std::fs::create_dir_all(&self.out).map_err(|e| StepError::Failed(e.to_string()))?;
                let artifact = self.out.join(format!("{step}.txt"));
                std::fs::write(&artifact, body).map_err(|e| StepError::Failed(e.to_string()))?;
                Ok(StepOutcome {
                    artifacts: vec![artifact],
                    detail: format!("{step} 完成"),
                })
            }
            Order::Skip(reason) => Err(StepError::Skipped(reason)),
            Order::Fail(message) => Err(StepError::Failed(message)),
            Order::Cancel => {
                // 真执行器此时已经杀掉子进程并删了半成品。
                assert!(cancel.is_cancelled(), "收到终止指令时控制标志应该已经置位");
                Err(StepError::Cancelled)
            }
        }
    }
}

/// 一套测试用的队列：根目录 + 控制通道。
struct Harness {
    dir: TempDir,
    queue: JobQueue,
    /// 执行器报告「开始跑这一步」。
    started: Receiver<String>,
    /// 测试给执行器下指令。
    orders: Sender<Order>,
    /// 队列推给界面的每一次状态。
    views: Receiver<JobView>,
}

impl Harness {
    fn new(tag: &str) -> Self {
        let dir = TempDir::new(tag);
        let out = dir.path().join("out");
        let (started_tx, started_rx) = channel();
        let (order_tx, order_rx) = channel();
        let (view_tx, view_rx) = channel();
        let runner: Arc<dyn JobRunner> = Arc::new(Fake {
            started: started_tx,
            go: Arc::new(Mutex::new(order_rx)),
            out,
        });
        let queue = JobQueue::start(
            dir.path().to_path_buf(),
            vec![("test".to_string(), runner)],
            move |view| {
                let _ = view_tx.send(view);
            },
        );
        Self {
            dir,
            queue,
            started: started_rx,
            orders: order_tx,
            views: view_rx,
        }
    }

    fn submit(&self, steps: &[&str]) -> JobView {
        self.queue
            .submit(NewJob {
                kind: "test".into(),
                title: "测试任务".into(),
                output_dir: self.dir.path().join("out").display().to_string(),
                spec: serde_json::json!({}),
                fingerprint: "f".repeat(64),
                steps: steps.iter().map(|s| s.to_string()).collect(),
                files: vec![("note.txt".into(), b"hi".to_vec())],
            })
            .expect("提交任务")
    }

    /// 等执行器报告开始跑某一步。
    fn next_started(&self) -> String {
        self.started
            .recv_timeout(WAIT)
            .expect("执行器应该开始跑下一步")
    }

    /// 执行器不应该再开新的一步（例如暂停之后）。
    fn no_more_steps(&self) {
        assert!(
            self.started
                .recv_timeout(Duration::from_millis(300))
                .is_err(),
            "不该再开始新的步骤"
        );
    }

    fn order(&self, order: Order) {
        self.orders.send(order).expect("下指令");
    }

    /// 轮询到任务进入指定状态为止（队列是异步的，状态由工作线程改）。
    fn wait_state(&self, id: &str, state: JobState) -> JobView {
        let deadline = Instant::now() + WAIT;
        loop {
            let view = self
                .queue
                .list()
                .into_iter()
                .find(|view| view.id == id)
                .expect("任务还在列表里");
            if view.state == state {
                return view;
            }
            assert!(
                Instant::now() < deadline,
                "等不到状态 {state:?}，现在是 {:?}（{:?}）",
                view.state,
                view.error
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn step_status(view: &JobView, name: &str) -> StepStatus {
        view.steps
            .iter()
            .find(|step| step.name == name)
            .unwrap_or_else(|| panic!("视图里没有步骤 {name}"))
            .status
    }
}

#[test]
fn submit_rejects_empty_or_duplicated_steps() {
    let h = Harness::new("badsteps");
    for steps in [vec![], vec!["第01集", "第01集"]] {
        let error = h
            .queue
            .submit(NewJob {
                kind: "test".into(),
                title: "测试任务".into(),
                output_dir: h.dir.path().display().to_string(),
                spec: serde_json::json!({}),
                fingerprint: "f".repeat(64),
                steps: steps.iter().map(|s| s.to_string()).collect(),
                files: vec![],
            })
            .expect_err("步骤为空或有重名必须被拒");
        assert_eq!(error, JobError::BadState("任务步骤为空或有重名"));
    }
    // 被拒的任务不该在磁盘上留下任务目录。
    let dirs: Vec<_> = std::fs::read_dir(h.dir.path())
        .expect("读队列根目录")
        .flatten()
        .filter(|entry| entry.path().join("job.json").is_file())
        .collect();
    assert!(dirs.is_empty(), "被拒的任务不该留下任务目录");
}

#[test]
fn unknown_kind_fails_the_job_without_running_anything() {
    let h = Harness::new("badkind");
    let view = h
        .queue
        .submit(NewJob {
            kind: "erase".into(),
            title: "测试任务".into(),
            output_dir: h.dir.path().display().to_string(),
            spec: serde_json::json!({}),
            fingerprint: "f".repeat(64),
            steps: vec!["第01集".into()],
            files: vec![],
        })
        .expect("提交本身是合法的");
    let failed = h.wait_state(&view.id, JobState::Failed);
    assert_eq!(
        failed.error.as_deref(),
        Some("不认识的任务种类：erase"),
        "认不出的种类要如实报错，不能静默跳过"
    );
    h.no_more_steps();
}

#[test]
fn pause_takes_effect_between_steps_only() {
    let h = Harness::new("pause");
    let view = h.submit(&["第01集", "第02集", "第03集"]);
    // 第 1 集已经开跑：此时点暂停，这一集要做完才停。
    assert_eq!(h.next_started(), "第01集");
    let paused_request = h.queue.pause(&view.id).expect("运行中可以暂停");
    assert_eq!(
        paused_request.state,
        JobState::Running,
        "点暂停的那一刻还在跑这一集"
    );
    h.order(Order::Finish("一".into()));

    let paused = h.wait_state(&view.id, JobState::Paused);
    assert_eq!(
        Harness::step_status(&paused, "第01集"),
        StepStatus::Done,
        "暂停前那一集已经做完"
    );
    assert_eq!(Harness::step_status(&paused, "第02集"), StepStatus::Pending);
    assert_eq!(paused.done, 1);
    assert_eq!(paused.total, 3);
    h.no_more_steps();

    // 继续：从第 2 集接着跑，第 1 集不重做。
    h.queue.resume(&view.id).expect("已暂停的可以继续");
    assert_eq!(h.next_started(), "第02集");
    h.order(Order::Finish("二".into()));
    assert_eq!(h.next_started(), "第03集");
    h.order(Order::Finish("三".into()));
    let done = h.wait_state(&view.id, JobState::Done);
    assert_eq!(done.done, 3);
    assert!(done.error.is_none());
}

#[test]
fn pause_on_queued_job_stops_it_before_it_starts() {
    let h = Harness::new("pausequeued");
    let first = h.submit(&["第01集"]);
    assert_eq!(h.next_started(), "第01集");
    // 第二个任务排在后面（队列同一时间只跑一个）。
    let second = h.submit(&["第01集"]);
    assert_eq!(second.state, JobState::Queued);
    let paused = h.queue.pause(&second.id).expect("排队中的可以暂停");
    assert_eq!(paused.state, JobState::Paused);

    h.order(Order::Finish("一".into()));
    h.wait_state(&first.id, JobState::Done);
    // 被暂停的任务已经从待跑队列里摘掉：第一个做完也不会轮到它。
    h.no_more_steps();
    assert_eq!(
        h.wait_state(&second.id, JobState::Paused).done,
        0,
        "暂停的排队任务一步都不该跑"
    );
}

#[test]
fn terminate_marks_job_and_keeps_it_resumable() {
    let h = Harness::new("terminate");
    let view = h.submit(&["第01集", "第02集"]);
    assert_eq!(h.next_started(), "第01集");
    h.order(Order::Finish("一".into()));
    assert_eq!(h.next_started(), "第02集");

    // 正在跑第 2 集时终止：标志置位，执行器返回 Cancelled。
    h.queue.terminate(&view.id).expect("运行中可以终止");
    h.order(Order::Cancel);

    let stopped = h.wait_state(&view.id, JobState::Terminated);
    assert_eq!(stopped.error.as_deref(), Some("已终止"));
    assert_eq!(
        Harness::step_status(&stopped, "第01集"),
        StepStatus::Done,
        "终止不影响已经做完的集"
    );
    assert_eq!(
        Harness::step_status(&stopped, "第02集"),
        StepStatus::Failed,
        "被终止的那一集记为失败，续跑时重做"
    );
    h.no_more_steps();

    // 已终止的任务可以续跑：只重做第 2 集。
    h.queue.resume(&view.id).expect("已终止的可以继续");
    assert_eq!(h.next_started(), "第02集");
    h.order(Order::Finish("二".into()));
    h.wait_state(&view.id, JobState::Done);
}

#[test]
fn terminate_then_resume_rejects_wrong_states() {
    let h = Harness::new("states");
    let view = h.submit(&["第01集"]);
    assert_eq!(h.next_started(), "第01集");
    // 运行中不能「继续」。
    assert_eq!(
        h.queue.resume(&view.id).expect_err("运行中不能继续"),
        JobError::BadState("只有已暂停、已终止或失败的任务可以继续")
    );
    h.order(Order::Finish("一".into()));
    h.wait_state(&view.id, JobState::Done);
    // 完成的任务不能暂停、不能终止。
    assert_eq!(
        h.queue.pause(&view.id).expect_err("完成的不能暂停"),
        JobError::BadState("这个任务没有在排队或运行")
    );
    assert_eq!(
        h.queue.terminate(&view.id).expect_err("完成的不能终止"),
        JobError::BadState("这个任务已经结束了")
    );
    // 认不出的任务号一律 NotFound，不会误伤别的任务。
    assert_eq!(
        h.queue.pause("nope").expect_err("没有这个任务"),
        JobError::NotFound
    );
    assert_eq!(
        h.queue.resume("nope").expect_err("没有这个任务"),
        JobError::NotFound
    );
    assert_eq!(
        h.queue.terminate("nope").expect_err("没有这个任务"),
        JobError::NotFound
    );
}

#[test]
fn resume_redoes_steps_whose_artifacts_changed() {
    let h = Harness::new("resume");
    let view = h.submit(&["第01集", "第02集", "第03集"]);
    assert_eq!(h.next_started(), "第01集");
    h.order(Order::Finish("一".into()));
    assert_eq!(h.next_started(), "第02集");
    h.order(Order::Finish("二".into()));
    assert_eq!(h.next_started(), "第03集");
    h.queue.pause(&view.id).expect("暂停");
    h.order(Order::Fail("先让第 3 集失败".into()));
    let failed = h.wait_state(&view.id, JobState::Failed);
    assert_eq!(failed.done, 2);

    // 把第 2 集的产物改掉：续跑时它的哈希对不上，必须重做。
    let artifact = h.dir.path().join("out").join("第02集.txt");
    std::fs::write(&artifact, "被改过了").expect("改产物");

    h.queue.resume(&view.id).expect("失败的任务可以继续");
    // 第 1 集产物没动，跳过；第 2 集重做；第 3 集接着做。
    assert_eq!(h.next_started(), "第02集");
    h.order(Order::Finish("二改".into()));
    assert_eq!(h.next_started(), "第03集");
    h.order(Order::Finish("三".into()));
    let done = h.wait_state(&view.id, JobState::Done);
    assert_eq!(done.done, 3);
    assert_eq!(
        std::fs::read_to_string(&artifact).expect("读重做后的产物"),
        "二改",
        "重做应该覆盖掉被改过的产物"
    );
}

#[test]
fn resume_redoes_steps_whose_artifacts_are_missing() {
    let h = Harness::new("missing");
    let view = h.submit(&["第01集", "第02集"]);
    assert_eq!(h.next_started(), "第01集");
    h.order(Order::Finish("一".into()));
    assert_eq!(h.next_started(), "第02集");
    h.queue.terminate(&view.id).expect("终止");
    h.order(Order::Cancel);
    h.wait_state(&view.id, JobState::Terminated);

    // 删掉第 1 集的产物（用户手动清理了输出目录）。
    std::fs::remove_file(h.dir.path().join("out").join("第01集.txt")).expect("删产物");
    h.queue.resume(&view.id).expect("继续");
    assert_eq!(h.next_started(), "第01集", "产物没了就得重做这一集");
    h.order(Order::Finish("一".into()));
    assert_eq!(h.next_started(), "第02集");
    h.order(Order::Finish("二".into()));
    h.wait_state(&view.id, JobState::Done);
}

#[test]
fn skipped_step_is_recorded_and_job_still_completes() {
    let h = Harness::new("skip");
    let view = h.submit(&["第01集", "第02集"]);
    assert_eq!(h.next_started(), "第01集");
    h.order(Order::Skip("重新分集实际分出的集数比计划少".into()));
    assert_eq!(h.next_started(), "第02集");
    h.order(Order::Finish("二".into()));

    let done = h.wait_state(&view.id, JobState::Done);
    assert_eq!(
        Harness::step_status(&done, "第01集"),
        StepStatus::Skipped,
        "跳过要如实记下来，不能算成完成"
    );
    let detail = done
        .steps
        .iter()
        .find(|step| step.name == "第01集")
        .and_then(|step| step.detail.clone())
        .expect("跳过的步骤要有说明");
    assert!(
        detail.contains("已跳过"),
        "说明里要写明跳过的原因：{detail}"
    );
    assert_eq!(done.done, 2, "跳过也算收口");
}

#[test]
fn failed_step_stops_the_job_and_keeps_the_reason() {
    let h = Harness::new("fail");
    let view = h.submit(&["第01集", "第02集"]);
    assert_eq!(h.next_started(), "第01集");
    h.order(Order::Fail(
        "画质低于下限（SSIM 0.8000 / 下限 0.92）".into(),
    ));
    let failed = h.wait_state(&view.id, JobState::Failed);
    assert!(
        failed
            .error
            .as_deref()
            .is_some_and(|error| error.contains("第01集") && error.contains("画质低于下限")),
        "失败原因要带上是哪一集、为什么：{:?}",
        failed.error
    );
    // 失败就停下，后面的集不跑。
    h.no_more_steps();
    assert_eq!(Harness::step_status(&failed, "第02集"), StepStatus::Pending);
}

#[test]
fn jobs_survive_restart_and_come_back_paused() {
    let h = Harness::new("restart");
    let view = h.submit(&["第01集", "第02集"]);
    assert_eq!(h.next_started(), "第01集");
    h.order(Order::Finish("一".into()));
    assert_eq!(h.next_started(), "第02集");
    // 第 2 集正跑着就「关掉应用」：不给指令，直接用同一个根目录新建一个队列。
    let root = h.dir.path().to_path_buf();
    let (started_tx, started_rx) = channel();
    let (order_tx, order_rx) = channel();
    let runner: Arc<dyn JobRunner> = Arc::new(Fake {
        started: started_tx,
        go: Arc::new(Mutex::new(order_rx)),
        out: root.join("out"),
    });
    let reopened = JobQueue::start(root, vec![("test".to_string(), runner)], |_| {});

    let listed = reopened.list();
    assert_eq!(listed.len(), 1, "任务要从磁盘读回来");
    let restored = &listed[0];
    assert_eq!(restored.id, view.id);
    assert_eq!(
        restored.state,
        JobState::Paused,
        "重启后未完成的任务显示为已暂停，不自动开工"
    );
    assert_eq!(restored.title, "测试任务");
    assert_eq!(restored.total, 2);
    // 读回来之后不自动跑：没有任何一步开始。
    assert!(
        started_rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "重启后不该在用户不知情时自动开工"
    );

    // 用户点「继续」才接着跑：第 1 集跳过，从第 2 集开始。
    reopened.resume(&view.id).expect("继续");
    assert_eq!(
        started_rx.recv_timeout(WAIT).expect("应该开始跑"),
        "第02集",
        "续跑从没做完的那一集开始"
    );
    order_tx.send(Order::Finish("二".into())).expect("下指令");
    let deadline = Instant::now() + WAIT;
    loop {
        let view = reopened.list().into_iter().next().expect("任务在列表里");
        if view.state == JobState::Done {
            break;
        }
        assert!(Instant::now() < deadline, "等不到完成：{:?}", view.state);
        std::thread::sleep(Duration::from_millis(20));
    }
    // 原来那个队列的执行器还卡在第 2 集上等指令，放它走，避免测试结束时线程还握着临时目录。
    h.order(Order::Cancel);
}

#[test]
fn job_files_are_written_before_the_job_can_start() {
    let h = Harness::new("files");
    let view = h.submit(&["第01集"]);
    // 附带文件（真实任务里是配方原文）必须在排队前就落盘，否则执行器可能读不到。
    let dir = h.dir.path().join(&view.id);
    assert_eq!(
        std::fs::read_to_string(dir.join("note.txt")).expect("附带文件已落盘"),
        "hi"
    );
    assert!(dir.join("job.json").is_file(), "任务描述要落盘");
    assert!(dir.join("checkpoint.json").is_file(), "检查点要落盘");
    assert!(dir.join("state.json").is_file(), "运行状态要落盘");
    h.order(Order::Finish("一".into()));
    h.wait_state(&view.id, JobState::Done);

    // 任务描述只写一次，内容就是提交时给的那份。
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("job.json")).unwrap()).unwrap();
    assert_eq!(record["kind"], "test");
    assert_eq!(record["title"], "测试任务");
}

#[test]
fn list_is_newest_first_and_notifies_on_every_change() {
    let h = Harness::new("list");
    let first = h.submit(&["第01集"]);
    assert_eq!(h.next_started(), "第01集");
    let second = h.submit(&["第01集"]);
    let listed = h.queue.list();
    assert_eq!(
        listed
            .iter()
            .map(|view| view.id.clone())
            .collect::<Vec<_>>(),
        vec![second.id.clone(), first.id.clone()],
        "列表是新到旧"
    );
    h.order(Order::Finish("一".into()));
    h.wait_state(&first.id, JobState::Done);
    assert_eq!(h.next_started(), "第01集", "第一个做完才轮到第二个");
    h.order(Order::Finish("一".into()));
    h.wait_state(&second.id, JobState::Done);

    // 界面通知：至少要收到两个任务各自的提交、开跑和完成。
    let mut seen: Vec<(String, JobState)> = Vec::new();
    while let Ok(view) = h.views.recv_timeout(Duration::from_millis(200)) {
        seen.push((view.id, view.state));
    }
    for id in [&first.id, &second.id] {
        assert!(
            seen.iter()
                .any(|(seen_id, state)| seen_id == id && *state == JobState::Queued),
            "缺少 {id} 的排队通知"
        );
        assert!(
            seen.iter()
                .any(|(seen_id, state)| seen_id == id && *state == JobState::Running),
            "缺少 {id} 的开跑通知"
        );
        assert!(
            seen.iter()
                .any(|(seen_id, state)| seen_id == id && *state == JobState::Done),
            "缺少 {id} 的完成通知"
        );
    }
}
