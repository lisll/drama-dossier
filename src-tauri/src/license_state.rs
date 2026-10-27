//! 授权状态机（纯逻辑，不依赖 Tauri、不联网）。
//!
//! `license.rs` 负责 IPC、网络与守护线程；这里只回答「现在能不能用」，并维护授权文件。
//! 时间与文件路径都由调用方传入，单元测试可以模拟重启、时间流逝和文件被改。
//!
//! 锁定规则（见 docs/LICENSING.md「客户端多层验权」）：
//! - 每次判断都从磁盘重读授权文件并完整验签，不信任上一次的结论；
//! - 只有「程序被篡改」（金丝雀自检失败）会在整次运行内保持锁定；
//! - 「被服务器停用」写进授权文件，重启、刷新都无效，只有拿到新凭证才清除；
//! - 授权文件的 `last_seen`、`revoked` 受校验码保护，手改即判为无效。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use dossier_cardkey::{
    check_seal, decide, next_last_seen, seal, KeyError, LockReason, TrustedKey, Verdict,
};
use serde::{Deserialize, Serialize};

/// 连续这么久写不进授权文件就锁定（文件被设成只读、磁盘满等）。
pub const MAX_UNPERSISTED_SECS: u64 = 3600;

/// 授权文件内容。字段缺一不可：老格式或手改过的文件一律判为无效。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stored {
    pub token: String,
    pub last_seen: u64,
    pub revoked: bool,
    pub mac: String,
}

impl Stored {
    pub fn new(machine_id: &str, token: String, last_seen: u64, revoked: bool) -> Self {
        let mac = seal(machine_id, &token, last_seen, revoked);
        Self {
            token,
            last_seen,
            revoked,
            mac,
        }
    }

    fn sealed_ok(&self, machine_id: &str) -> bool {
        check_seal(
            machine_id,
            &self.token,
            self.last_seen,
            self.revoked,
            &self.mac,
        )
    }
}

/// 读授权文件的结果。读失败与「文件不存在」要分开：前者可能只是暂时被占用。
#[derive(Debug)]
pub enum ReadOutcome {
    Missing,
    Ok(Stored),
    /// 内容无法解析或校验码不对
    Invalid,
    /// IO 错误（权限、被占用等），附原因供日志使用
    Unreadable(String),
}

pub struct State {
    path: PathBuf,
    machine_id: Result<String, String>,
    /// 金丝雀自检失败：整次运行内保持锁定
    tampered: AtomicBool,
    /// 本次运行见过的最晚时间（防止运行中删改授权文件绕过回拨检查）
    mem_last_seen: AtomicU64,
    /// 第一次写失败的时间；0 表示当前没有未落盘的状态
    unpersisted_since: AtomicU64,
    /// 闸门快速路径：最近一次判定为有效时，结论可信到何时（= min(exp, until)）
    active_until: AtomicU64,
    /// 本次运行内被服务器停用的凭证（停用标记写盘失败时仍要锁住）
    revoked_token: Mutex<Option<String>>,
    io: Mutex<()>,
}

impl State {
    pub fn new(path: PathBuf, machine_id: Result<String, String>) -> Self {
        Self {
            path,
            machine_id,
            tampered: AtomicBool::new(false),
            mem_last_seen: AtomicU64::new(0),
            unpersisted_since: AtomicU64::new(0),
            active_until: AtomicU64::new(0),
            revoked_token: Mutex::new(None),
            io: Mutex::new(()),
        }
    }

    pub fn machine_id(&self) -> Result<&str, &str> {
        self.machine_id.as_deref().map_err(String::as_str)
    }

    pub fn mark_tampered(&self) {
        self.tampered.store(true, Ordering::SeqCst);
        self.active_until.store(0, Ordering::SeqCst);
    }

    pub fn is_tampered(&self) -> bool {
        self.tampered.load(Ordering::SeqCst)
    }

    /// 闸门用：只看原子量与时间，不读盘。
    pub fn is_active_fast(&self, now: u64) -> bool {
        !self.is_tampered() && now < self.active_until.load(Ordering::SeqCst)
    }

    fn lock_io(&self) -> std::sync::MutexGuard<'_, ()> {
        // 锁中毒只说明另一个线程在持锁时 panic 了；授权文件本身用 tmp+rename 写，不会半写。
        self.io.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn read_locked(&self) -> ReadOutcome {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ReadOutcome::Missing,
            Err(e) => return ReadOutcome::Unreadable(e.to_string()),
        };
        let Ok(mid) = self.machine_id() else {
            return ReadOutcome::Invalid;
        };
        match serde_json::from_str::<Stored>(&text) {
            Ok(s) if s.sealed_ok(mid) => ReadOutcome::Ok(s),
            _ => ReadOutcome::Invalid,
        }
    }

    fn write_locked(&self, s: &Stored) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("无法创建配置目录：{e}"))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let text = serde_json::to_string(s).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, text).map_err(|e| format!("写入授权文件失败：{e}"))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("保存授权文件失败：{e}")
        })
    }

    /// 只读当前授权文件（不判定、不写回），给续签等需要原始凭证的地方用。
    pub fn read(&self) -> ReadOutcome {
        let _g = self.lock_io();
        self.read_locked()
    }

    /// 完整判定：读盘 → 校验码 → 停用标记 → 验签 → 机器码 → 时钟 → 有效期；
    /// 有效时按需把 last_seen 写回。读、判、写在同一把锁里完成。
    ///
    /// 返回判定结果与可选的日志消息（写盘失败等），由调用方决定记不记。
    pub fn evaluate(
        &self,
        now: u64,
        keys: Result<&[TrustedKey], KeyError>,
        canary_ok: bool,
    ) -> (Verdict, Option<String>) {
        if !canary_ok {
            self.mark_tampered();
        }
        if self.is_tampered() {
            return self.finish(Verdict::Locked(LockReason::Tampered), None);
        }
        let Ok(mid) = self.machine_id() else {
            return self.finish(Verdict::Locked(LockReason::MachineUnavailable), None);
        };
        let _g = self.lock_io();
        let stored = match self.read_locked() {
            ReadOutcome::Ok(s) => s,
            ReadOutcome::Missing => {
                return self.finish(Verdict::Locked(LockReason::NotActivated), None)
            }
            ReadOutcome::Invalid => {
                return self.finish(Verdict::Locked(LockReason::Malformed), None)
            }
            ReadOutcome::Unreadable(e) => {
                return self.finish(
                    Verdict::Locked(LockReason::Unreadable),
                    Some(format!("读取授权文件失败：{e}")),
                )
            }
        };
        let revoked_in_memory = self
            .revoked_token
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_deref()
            == Some(stored.token.as_str());
        if stored.revoked || revoked_in_memory {
            return self.finish(Verdict::Locked(LockReason::Revoked), None);
        }
        let floor = stored
            .last_seen
            .max(self.mem_last_seen.load(Ordering::SeqCst));
        let v = decide(&stored.token, mid, now, floor, keys);
        if !v.is_active() {
            return self.finish(v, None);
        }
        self.mem_last_seen.fetch_max(now, Ordering::SeqCst);
        let mut note = None;
        if let Some(next) = next_last_seen(now, stored.last_seen) {
            let fresh = Stored::new(mid, stored.token, next, false);
            match self.write_locked(&fresh) {
                Ok(()) => self.unpersisted_since.store(0, Ordering::SeqCst),
                Err(e) => {
                    let since = match self.unpersisted_since.compare_exchange(
                        0,
                        now,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(_) => now,
                        Err(prev) => prev,
                    };
                    if now.saturating_sub(since) >= MAX_UNPERSISTED_SECS {
                        return self.finish(
                            Verdict::Locked(LockReason::Unpersisted),
                            Some(format!("授权状态连续 1 小时无法保存，已锁定：{e}")),
                        );
                    }
                    note = Some(format!("授权状态暂时无法保存（{e}），1 小时内恢复即可"));
                }
            }
        }
        (self.finish(v, None).0, note)
    }

    fn finish(&self, v: Verdict, note: Option<String>) -> (Verdict, Option<String>) {
        match &v {
            Verdict::Active(a) => {
                let until = a.until.map_or(a.exp, |u| u.min(a.exp));
                self.active_until.store(until, Ordering::SeqCst);
            }
            Verdict::Locked(_) => self.active_until.store(0, Ordering::SeqCst),
        }
        (v, note)
    }

    /// 收到新凭证：先本地完整验签，通过才落盘（清除停用标记）。
    /// 凭证验不过时绝不覆盖磁盘上原有的文件。
    pub fn accept(
        &self,
        token: String,
        now: u64,
        keys: Result<&[TrustedKey], KeyError>,
        canary_ok: bool,
    ) -> Result<Verdict, LockReason> {
        if !canary_ok {
            self.mark_tampered();
        }
        if self.is_tampered() {
            return Err(LockReason::Tampered);
        }
        let mid = self
            .machine_id()
            .map_err(|_| LockReason::MachineUnavailable)?;
        // 时钟只和新凭证自己的 iat 比较：用户把时间校正回来后，续签拿到的新凭证必须能用
        let v = decide(&token, mid, now, 0, keys);
        if let Verdict::Locked(r) = v {
            return Err(r);
        }
        let _g = self.lock_io();
        self.write_locked(&Stored::new(mid, token, now, false))
            .map_err(|_| LockReason::Unpersisted)?;
        self.mem_last_seen.store(now, Ordering::SeqCst);
        self.unpersisted_since.store(0, Ordering::SeqCst);
        *self.revoked_token.lock().unwrap_or_else(|e| e.into_inner()) = None;
        Ok(self.finish(v, None).0)
    }

    /// 服务器明确停用：在内存里锁住这份凭证，并把停用标记写进授权文件。
    /// 写盘失败时本次运行仍保持锁定，返回错误供日志使用。
    pub fn mark_revoked(&self, expected_token: &str) -> Result<(), String> {
        let Ok(mid) = self.machine_id() else {
            return Ok(());
        };
        // 读取与标记在同一把锁里，与 accept() 互斥。
        // 只停用发起续签时的那份凭证：期间如果用户已用新卡密激活，不能误伤新凭证。
        let _g = self.lock_io();
        let current = match self.read_locked() {
            ReadOutcome::Ok(s) if s.token == expected_token => s,
            _ => return Ok(()),
        };
        *self.revoked_token.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(expected_token.to_string());
        self.active_until.store(0, Ordering::SeqCst);
        let last = current
            .last_seen
            .max(self.mem_last_seen.load(Ordering::SeqCst));
        self.write_locked(&Stored::new(mid, current.token, last, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dossier_cardkey::{issue_token, parse_key_list, public_key_b64, signing_key, Plan};

    const MID: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const OTHER_MID: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const T0: u64 = 1_800_000_000;
    const DAY: u64 = 86_400;

    /// 每个测试一个独立目录，放在已被 git 忽略的 target/ 下，测试结束自动删除。
    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Self {
            let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../target/license-state-tests")
                .join(format!("{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn file(&self) -> PathBuf {
            self.0.join("license.json")
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct Fx {
        dir: Dir,
        keys: Vec<TrustedKey>,
        sk: dossier_cardkey::SigningKey,
    }

    impl Fx {
        fn new(name: &str) -> Self {
            let sk = signing_key([5u8; 32]);
            let keys = parse_key_list(&[public_key_b64(&sk).as_str()]).unwrap();
            Self {
                dir: Dir::new(name),
                keys,
                sk,
            }
        }
        /// 模拟一次进程启动：内存状态全新，磁盘不变
        fn boot(&self) -> State {
            State::new(self.dir.file(), Ok(MID.to_string()))
        }
        fn token(&self, mid: &str, iat: u64, exp: u64) -> String {
            issue_token(&self.sk, 1, mid, Plan::Month, Some(T0 + 30 * DAY), iat, exp)
        }
        fn eval(&self, s: &State, now: u64) -> Verdict {
            s.evaluate(now, Ok(&self.keys), true).0
        }
        fn accept(&self, s: &State, now: u64) -> Verdict {
            s.accept(
                self.token(MID, now, now + 7 * DAY),
                now,
                Ok(&self.keys),
                true,
            )
            .unwrap()
        }
        fn read(&self) -> Stored {
            serde_json::from_str(&std::fs::read_to_string(self.dir.file()).unwrap()).unwrap()
        }
        fn write_raw(&self, s: &Stored) {
            std::fs::write(self.dir.file(), serde_json::to_string(s).unwrap()).unwrap();
        }
    }

    fn locked(v: &Verdict) -> Option<LockReason> {
        match v {
            Verdict::Locked(r) => Some(*r),
            Verdict::Active(_) => None,
        }
    }

    #[test]
    fn not_activated_then_activate() {
        let fx = Fx::new("activate");
        let s = fx.boot();
        assert_eq!(locked(&fx.eval(&s, T0)), Some(LockReason::NotActivated));
        assert!(!s.is_active_fast(T0));
        assert!(fx.accept(&s, T0).is_active());
        assert!(s.is_active_fast(T0 + 1));
        assert!(fx.eval(&s, T0 + 60).is_active());
    }

    #[test]
    fn revoked_survives_restart_until_new_token() {
        let fx = Fx::new("revoked");
        let s = fx.boot();
        fx.accept(&s, T0);
        let tok = fx.read().token;
        s.mark_revoked(&tok).unwrap();
        assert!(!s.is_active_fast(T0 + 1));
        assert_eq!(locked(&fx.eval(&s, T0 + 10)), Some(LockReason::Revoked));
        // 重启（相当于刷新页面 / 关掉重开）：仍然锁定
        let s2 = fx.boot();
        assert_eq!(locked(&fx.eval(&s2, T0 + 20)), Some(LockReason::Revoked));
        // 拿到新凭证（重新激活或续签成功）才解除
        assert!(fx.accept(&s2, T0 + 30).is_active());
        assert!(fx.eval(&s2, T0 + 100).is_active());
        assert!(!fx.read().revoked);
    }

    #[test]
    fn revoke_does_not_hit_newer_token() {
        let fx = Fx::new("revoke-race");
        let s = fx.boot();
        fx.accept(&s, T0);
        let old = fx.read().token;
        // 续签请求发出后、服务器拒绝前，用户用新卡密激活了
        fx.accept(&s, T0 + 5);
        s.mark_revoked(&old).unwrap();
        assert!(!fx.read().revoked, "不能把新凭证标成停用");
        assert!(fx.eval(&s, T0 + 70).is_active());
    }

    #[test]
    fn hand_edited_file_is_invalid() {
        let fx = Fx::new("edited");
        let s = fx.boot();
        fx.accept(&s, T0 + 6 * DAY);
        // 想把 last_seen 改成 0、把时间拨回第 1 天继续用
        let mut st = fx.read();
        st.last_seen = 0;
        fx.write_raw(&st);
        let s2 = fx.boot();
        assert_eq!(locked(&fx.eval(&s2, T0 + DAY)), Some(LockReason::Malformed));
        // 想去掉停用标记
        let s3 = fx.boot();
        fx.accept(&s3, T0);
        let tok = fx.read().token;
        s3.mark_revoked(&tok).unwrap();
        let mut st = fx.read();
        st.revoked = false;
        fx.write_raw(&st);
        assert_eq!(
            locked(&fx.eval(&fx.boot(), T0 + 10)),
            Some(LockReason::Malformed)
        );
    }

    #[test]
    fn old_format_without_mac_is_invalid() {
        let fx = Fx::new("old-format");
        let tok = fx.token(MID, T0, T0 + 7 * DAY);
        std::fs::write(
            fx.dir.file(),
            serde_json::json!({ "token": tok, "last_seen": 0 }).to_string(),
        )
        .unwrap();
        assert_eq!(
            locked(&fx.eval(&fx.boot(), T0 + 10)),
            Some(LockReason::Malformed)
        );
    }

    #[test]
    fn copied_file_fails_on_other_machine() {
        let fx = Fx::new("copied");
        fx.accept(&fx.boot(), T0);
        // 同一份文件在另一台电脑上：校验码绑定机器码，直接无效
        let other = State::new(fx.dir.file(), Ok(OTHER_MID.to_string()));
        assert_eq!(
            locked(&fx.eval(&other, T0 + 10)),
            Some(LockReason::Malformed)
        );
    }

    #[test]
    fn clock_rollback_uses_memory_floor() {
        let fx = Fx::new("rollback");
        let s = fx.boot();
        fx.accept(&s, T0);
        assert!(fx.eval(&s, T0 + 3 * DAY).is_active());
        // 运行中把系统时间拨回去
        assert_eq!(
            locked(&fx.eval(&s, T0 + DAY)),
            Some(LockReason::ClockRollback)
        );
        // 校正回来就恢复，不会被一次失败锁死
        assert!(fx.eval(&s, T0 + 3 * DAY + 10).is_active());
    }

    #[test]
    fn deleting_file_mid_session_does_not_reset_floor() {
        let fx = Fx::new("delete-floor");
        let s = fx.boot();
        fx.accept(&s, T0);
        assert!(fx.eval(&s, T0 + 3 * DAY).is_active());
        // 运行中删掉文件，再放回一份「刚激活」时的旧文件
        let fresh = Stored::new(MID, fx.token(MID, T0, T0 + 7 * DAY), T0, false);
        fx.write_raw(&fresh);
        assert_eq!(
            locked(&fx.eval(&s, T0 + DAY)),
            Some(LockReason::ClockRollback)
        );
    }

    #[test]
    fn transient_read_failure_recovers() {
        let fx = Fx::new("transient");
        let s = fx.boot();
        fx.accept(&s, T0);
        // 模拟被占用：把文件换成同名目录，读取会报 IO 错误
        let saved = std::fs::read(fx.dir.file()).unwrap();
        std::fs::remove_file(fx.dir.file()).unwrap();
        std::fs::create_dir(fx.dir.file()).unwrap();
        let (v, note) = s.evaluate(T0 + 10, Ok(&fx.keys), true);
        assert_eq!(locked(&v), Some(LockReason::Unreadable));
        assert!(note.is_some());
        std::fs::remove_dir(fx.dir.file()).unwrap();
        std::fs::write(fx.dir.file(), saved).unwrap();
        // 恢复后下一次检查就放行
        assert!(fx.eval(&s, T0 + 20).is_active());
    }

    #[test]
    fn unwritable_file_locks_after_one_hour() {
        let fx = Fx::new("unwritable");
        let s = fx.boot();
        fx.accept(&s, T0);
        // 写入走 license.json.tmp → rename。在 tmp 的位置放一个同名目录，写入必然失败；
        // 读取 license.json 不受影响。这个方法对 root 同样有效（目录权限挡不住 root）。
        let tmp = fx.dir.file().with_extension("json.tmp");
        std::fs::create_dir(&tmp).unwrap();
        let (v, note) = s.evaluate(T0 + 120, Ok(&fx.keys), true);
        assert!(v.is_active(), "刚写不进去时继续放行");
        assert!(note.is_some(), "写入失败必须给出日志");
        let (v, _) = s.evaluate(T0 + 120 + MAX_UNPERSISTED_SECS - 1, Ok(&fx.keys), true);
        assert!(v.is_active(), "未满 1 小时仍放行");
        let (v, note) = s.evaluate(T0 + 120 + MAX_UNPERSISTED_SECS, Ok(&fx.keys), true);
        assert_eq!(locked(&v), Some(LockReason::Unpersisted));
        assert!(note.is_some());
        assert!(!s.is_active_fast(T0 + 121 + MAX_UNPERSISTED_SECS));
        // 恢复可写后自动放行，并且重新计时
        std::fs::remove_dir(&tmp).unwrap();
        assert!(fx.eval(&s, T0 + 200 + MAX_UNPERSISTED_SECS).is_active());
        assert_eq!(fx.read().last_seen, T0 + 200 + MAX_UNPERSISTED_SECS);
    }

    #[test]
    fn tampered_is_sticky_for_the_session() {
        let fx = Fx::new("tampered");
        let s = fx.boot();
        fx.accept(&s, T0);
        let (v, _) = s.evaluate(T0 + 10, Ok(&fx.keys), false);
        assert_eq!(locked(&v), Some(LockReason::Tampered));
        // 即使之后自检又通过，本次运行也不再放行
        assert_eq!(locked(&fx.eval(&s, T0 + 20)), Some(LockReason::Tampered));
        assert!(s
            .accept(fx.token(MID, T0, T0 + DAY), T0 + 30, Ok(&fx.keys), true)
            .is_err());
    }

    #[test]
    fn bad_token_never_overwrites_good_file() {
        let fx = Fx::new("bad-accept");
        let s = fx.boot();
        fx.accept(&s, T0);
        let before = fx.read();
        let wrong = fx.token(OTHER_MID, T0, T0 + DAY);
        assert_eq!(
            s.accept(wrong, T0 + 5, Ok(&fx.keys), true),
            Err(LockReason::WrongMachine)
        );
        assert_eq!(fx.read(), before);
    }

    #[test]
    fn expired_token_locks() {
        let fx = Fx::new("expired");
        let s = fx.boot();
        fx.accept(&s, T0);
        assert_eq!(
            locked(&fx.eval(&s, T0 + 7 * DAY)),
            Some(LockReason::Expired)
        );
        assert!(!s.is_active_fast(T0 + 7 * DAY));
    }

    #[test]
    fn concurrent_evaluate_and_accept_keep_newest_token() {
        let fx = Fx::new("concurrent");
        let s = std::sync::Arc::new(fx.boot());
        fx.accept(&s, T0);
        let keys = fx.keys.clone();
        let worker = {
            let s = s.clone();
            std::thread::spawn(move || {
                for i in 0..200 {
                    let _ = s.evaluate(T0 + 120 + i * 61, Ok(&keys), true);
                }
            })
        };
        let mut last = String::new();
        for i in 0..50 {
            let now = T0 + 100 + i;
            last = fx.token(MID, now, now + 7 * DAY);
            s.accept(last.clone(), now, Ok(&fx.keys), true).unwrap();
        }
        worker.join().unwrap();
        assert_eq!(fx.read().token, last, "后台检查不能把新凭证覆盖成旧的");
    }
}
