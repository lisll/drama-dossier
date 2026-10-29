//! 长任务的取消令牌与活动任务表。
//!
//! 以前所有长任务共用一个全局停止开关：点过一次「停止转码」后码率修复也会被当成已停止，
//! 停止一类任务会波及其他类别。现在每个任务开始时登记一个 [`TaskGuard`]，
//! 拿到自己的 [`CancelToken`]；停止命令按任务类别只取消该类别的任务
//! （同一类别若有多个任务同时在跑，会一起停下——界面每类只有一个停止按钮）。
//! `TaskGuard` 析构时自动从活动表移除，任何返回路径都不会留下残余。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// 长任务类别。前三类各对应界面上一个停止按钮；其余只登记「正在运行」，
/// 供应用升级前判断能不能退出（它们有各自原来的停止方式，不走这里的令牌）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    /// 转码（微调 / 重组 / 深度处理，含材料线程）
    Transcode,
    /// 码率修复
    Bitrate,
    /// V5 去重验证
    DedupDemo,
    /// 字幕 / 水印擦除
    Erase,
    /// 一键安装运行环境、内容模型
    Install,
    /// 生成报审包
    Package,
    /// 按新文案重做海报（联网生图并写文件）
    Poster,
}

impl TaskKind {
    /// 给用户看的名称（升级被拦下时告诉用户在等哪个任务）。
    pub fn label(self) -> &'static str {
        match self {
            TaskKind::Transcode => "转码",
            TaskKind::Bitrate => "码率修复",
            TaskKind::DedupDemo => "去重验证",
            TaskKind::Erase => "擦除",
            TaskKind::Install => "安装运行环境 / 模型",
            TaskKind::Package => "生成报审包",
            TaskKind::Poster => "重做海报",
        }
    }
}

/// 取消令牌：`cancel()` 只置位自己；`is_cancelled()` 同时看父令牌。
///
/// 子令牌用于「父任务停止时连带停止、但子任务可以被单独停止」的场景，
/// 例如编码失败时只停材料线程，而用户点停止时两者一起停。
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    own: Arc<AtomicBool>,
    parent: Option<Box<CancelToken>>,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.own.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.own.load(Ordering::SeqCst)
            || self
                .parent
                .as_ref()
                .is_some_and(|parent| parent.is_cancelled())
    }

    /// 派生子令牌：父令牌取消时子令牌也视为取消；取消子令牌不影响父令牌。
    pub fn child(&self) -> Self {
        Self {
            own: Arc::new(AtomicBool::new(false)),
            parent: Some(Box::new(self.clone())),
        }
    }
}

type Registry = Mutex<HashMap<u64, (TaskKind, CancelToken)>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock() -> std::sync::MutexGuard<'static, HashMap<u64, (TaskKind, CancelToken)>> {
    // 锁中毒只说明某个线程持锁时 panic 了；表本身只有插入 / 删除，状态不会半写。
    registry().lock().unwrap_or_else(|error| error.into_inner())
}

/// 一个正在运行的任务：持有它的令牌，析构时从活动表移除。
#[derive(Debug)]
pub struct TaskGuard {
    id: u64,
    token: CancelToken,
}

impl TaskGuard {
    pub fn token(&self) -> &CancelToken {
        &self.token
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        lock().remove(&self.id);
    }
}

/// 登记一个新任务并返回它的守卫。
pub fn register(kind: TaskKind) -> TaskGuard {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    let token = CancelToken::new();
    lock().insert(id, (kind, token.clone()));
    TaskGuard { id, token }
}

/// 停止某一类的全部活动任务，返回被停止的任务数（0 = 当前没有这类任务在跑）。
pub fn cancel_kind(kind: TaskKind) -> usize {
    let tasks = lock();
    let mut count = 0;
    for (task_kind, token) in tasks.values() {
        if *task_kind == kind {
            token.cancel();
            count += 1;
        }
    }
    count
}

/// 当前正在运行的任务类别（去重，按登记顺序无关的固定顺序返回）。
pub fn running_kinds() -> Vec<TaskKind> {
    let tasks = lock();
    let mut kinds: Vec<TaskKind> = Vec::new();
    for (kind, _) in tasks.values() {
        if !kinds.contains(kind) {
            kinds.push(*kind);
        }
    }
    kinds.sort_by_key(|kind| *kind as u8);
    kinds
}

#[cfg(test)]
fn is_registered(id: u64) -> bool {
    lock().contains_key(&id)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 活动表是进程级全局状态，各用例只断言自己登记的那几个任务。

    #[test]
    fn cancel_kind_only_hits_that_kind() {
        let transcode = register(TaskKind::Transcode);
        let bitrate = register(TaskKind::Bitrate);
        assert!(cancel_kind(TaskKind::Transcode) >= 1);
        assert!(transcode.token().is_cancelled());
        assert!(!bitrate.token().is_cancelled(), "停转码不能波及码率修复");
    }

    #[test]
    fn tasks_of_same_kind_have_independent_tokens() {
        let first = register(TaskKind::DedupDemo);
        first.token().cancel();
        let second = register(TaskKind::DedupDemo);
        assert!(
            !second.token().is_cancelled(),
            "上一个任务的停止不能残留到下一个任务"
        );
    }

    #[test]
    fn child_follows_parent_but_not_the_reverse() {
        let parent = CancelToken::new();
        let child = parent.child();
        child.cancel();
        assert!(child.is_cancelled());
        assert!(!parent.is_cancelled(), "取消子令牌不影响父令牌");

        let parent = CancelToken::new();
        let child = parent.child();
        parent.cancel();
        assert!(child.is_cancelled(), "父令牌取消时子令牌也视为取消");
    }

    #[test]
    fn guard_drop_removes_task() {
        let guard = register(TaskKind::Bitrate);
        let id = guard.id;
        assert!(is_registered(id));
        drop(guard);
        assert!(!is_registered(id), "任务结束后不能残留在活动表里");
    }

    #[test]
    fn running_kinds_tracks_guards() {
        let guard = register(TaskKind::Erase);
        assert!(running_kinds().contains(&TaskKind::Erase));
        drop(guard);
        // 同一类别登记多个任务时只列一次（「是否释放」由 guard_drop_removes_task 覆盖）
        let poster = register(TaskKind::Poster);
        assert!(running_kinds().contains(&TaskKind::Poster));
        assert_eq!(TaskKind::Poster.label(), "重做海报");
        drop(poster);
        let a = register(TaskKind::Package);
        let b = register(TaskKind::Package);
        assert_eq!(
            running_kinds()
                .iter()
                .filter(|kind| **kind == TaskKind::Package)
                .count(),
            1
        );
        drop((a, b));
    }
}
