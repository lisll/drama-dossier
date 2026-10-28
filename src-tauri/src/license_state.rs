//! 授权状态机（纯逻辑，不依赖 Tauri、不联网）。
//!
//! `license.rs` 负责 IPC、网络与守护线程；这里只回答「现在能不能用」，并维护授权文件。
//! 时间与文件路径都由调用方传入，单元测试可以模拟重启、时间流逝和文件被改。
//!
//! 锁定规则（见 docs/LICENSING.md「客户端多层验权」）：
//! - 每次判断都从磁盘重读授权文件并完整验签，不信任上一次的结论；
//! - 只有「程序被篡改」（金丝雀自检失败）会在整次运行内保持锁定；
//! - 「被服务器停用」写进授权文件，重启、刷新都无效，只有拿到新凭证才清除；
//! - 授权文件的 `last_seen`、`revoked` 受校验码保护，手改即判为无效；
//! - 同样两项再存一份到系统凭据库（[`crate::license_anchor`]），判定取两边更严的那份：
//!   恢复一份旧的授权文件只能回滚其中一处。凭据库不可用时退回只看授权文件。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use dossier_cardkey::{
    check_seal, decide, next_last_seen, seal, KeyError, LockReason, TrustedKey, Verdict,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::license_anchor::Anchor;

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

/// 凭据库里的第二份存档：见过的最晚时间 + 被停用的凭证（存 sha256，不存凭证原文）。
///
/// 和授权文件一样用校验码绑定本机机器码：手改或从别的电脑拷来的记录一律视为不存在
/// （不锁定，只是不再提供额外保护）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnchorRecord {
    last_seen: u64,
    revoked_token_sha256: Option<String>,
    mac: String,
}

impl AnchorRecord {
    /// 校验码复用授权文件的 HMAC：把停用凭证的哈希放在「凭证」位置，区分出这是锚点记录。
    fn sealed(machine_id: &str, last_seen: u64, revoked_token_sha256: Option<String>) -> Self {
        let mac = seal(
            machine_id,
            &Self::payload(revoked_token_sha256.as_deref()),
            last_seen,
            false,
        );
        Self {
            last_seen,
            revoked_token_sha256,
            mac,
        }
    }

    fn payload(revoked: Option<&str>) -> String {
        format!("anchor:{}", revoked.unwrap_or("-"))
    }

    fn parse(text: &str, machine_id: &str) -> Option<Self> {
        let record: Self = serde_json::from_str(text).ok()?;
        check_seal(
            machine_id,
            &Self::payload(record.revoked_token_sha256.as_deref()),
            record.last_seen,
            false,
            &record.mac,
        )
        .then_some(record)
    }
}

fn token_sha256(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

/// 同一次判定里的多条说明合成一条日志；没有说明时为 None。
fn join_notes(notes: Vec<String>) -> Option<String> {
    (!notes.is_empty()).then(|| notes.join("；"))
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
    /// 系统凭据库里的第二份存档；只在第一次用到时读一次，之后以内存为准、变化时写回。
    anchor: Box<dyn Anchor>,
    anchor_slot: Mutex<AnchorSlot>,
}

/// 第二份存档在本次运行里的状态。
#[derive(Debug)]
enum AnchorSlot {
    /// 还没读过凭据库
    Unloaded,
    /// 已读（`None` = 确实没有记录，或记录被改过 / 来自别的电脑），之后以内存为准
    Ready(Option<AnchorRecord>),
    /// 本次运行不再碰凭据库：读失败（写入会覆盖掉读不到的原有记录）或写失败
    /// （macOS 上反复访问会反复弹授权框）。已经读到的记录仍留在内存里参与判定。
    Disabled(Option<AnchorRecord>),
}

/// 对第二份存档的一次更新，与已有记录合并后写回。
enum AnchorUpdate {
    /// 判定通过：见过的最晚时间往后推（只増不减），停用记录原样保留
    Seen(u64),
    /// 服务器停用了这份凭证（sha256）；时间同样只増不减
    Revoke {
        last_seen: u64,
        token_sha256: String,
    },
    /// 激活了新凭证：时钟下限重置为现在（用户校正时钟后续签必须能用）；
    /// 旧凭证的停用记录保留——恢复旧凭证的备份文件仍然拦得住，除非新凭证就是它。
    Accept { now: u64, token_sha256: String },
}

impl AnchorUpdate {
    fn merge(self, previous: Option<&AnchorRecord>) -> (u64, Option<String>) {
        let previous_seen = previous.map_or(0, |record| record.last_seen);
        let previous_revoked = previous.and_then(|record| record.revoked_token_sha256.clone());
        match self {
            Self::Seen(now) => (now.max(previous_seen), previous_revoked),
            Self::Revoke {
                last_seen,
                token_sha256,
            } => (last_seen.max(previous_seen), Some(token_sha256)),
            Self::Accept { now, token_sha256 } => (
                now,
                previous_revoked.filter(|revoked| *revoked != token_sha256),
            ),
        }
    }
}

impl State {
    /// 不带第二份存档：单测模拟「只有授权文件」的旧行为用。
    #[cfg(test)]
    pub fn new(path: PathBuf, machine_id: Result<String, String>) -> Self {
        Self::with_anchor(path, machine_id, Box::new(crate::license_anchor::NoAnchor))
    }

    pub fn with_anchor(
        path: PathBuf,
        machine_id: Result<String, String>,
        anchor: Box<dyn Anchor>,
    ) -> Self {
        Self {
            path,
            machine_id,
            tampered: AtomicBool::new(false),
            mem_last_seen: AtomicU64::new(0),
            unpersisted_since: AtomicU64::new(0),
            active_until: AtomicU64::new(0),
            revoked_token: Mutex::new(None),
            io: Mutex::new(()),
            anchor,
            anchor_slot: Mutex::new(AnchorSlot::Unloaded),
        }
    }

    fn lock_anchor(&self) -> std::sync::MutexGuard<'_, AnchorSlot> {
        self.anchor_slot.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 读第二份存档（每次运行只真正读一次凭据库）。返回记录与需要记日志的说明：
    /// 没有记录、校验码不对都算没有记录；读失败则本次运行停用存档（只报告这一次）。
    fn anchor_record(&self, machine_id: &str) -> (Option<AnchorRecord>, Option<String>) {
        let mut slot = self.lock_anchor();
        let mut note = None;
        if matches!(*slot, AnchorSlot::Unloaded) {
            *slot = match self.anchor.load() {
                Ok(text) => {
                    AnchorSlot::Ready(text.and_then(|text| AnchorRecord::parse(&text, machine_id)))
                }
                Err(error) => {
                    note = Some(format!(
                        "授权状态的第二份存档读取失败，本次运行不再使用（不影响使用）：{error}"
                    ));
                    AnchorSlot::Disabled(None)
                }
            };
        }
        let record = match &*slot {
            AnchorSlot::Ready(record) | AnchorSlot::Disabled(record) => record.clone(),
            AnchorSlot::Unloaded => None,
        };
        (record, note)
    }

    /// 把一次更新合并进第二份存档并写回凭据库。写失败只返回说明，由调用方记日志——绝不因此锁定；
    /// 失败一次后本次运行不再尝试写入（只更新内存）。
    fn write_anchor(&self, machine_id: &str, update: AnchorUpdate) -> Option<String> {
        let (_, mut note) = self.anchor_record(machine_id);
        let mut slot = self.lock_anchor();
        let previous = match &*slot {
            AnchorSlot::Ready(record) | AnchorSlot::Disabled(record) => record.clone(),
            AnchorSlot::Unloaded => None,
        };
        let (last_seen, revoked) = update.merge(previous.as_ref());
        let record = AnchorRecord::sealed(machine_id, last_seen, revoked);
        if matches!(*slot, AnchorSlot::Disabled(_)) {
            *slot = AnchorSlot::Disabled(Some(record));
            return note;
        }
        let result = serde_json::to_string(&record)
            .map_err(|error| error.to_string())
            .and_then(|text| self.anchor.store(&text));
        *slot = match result {
            Ok(()) => AnchorSlot::Ready(Some(record)),
            Err(error) => {
                note = Some(format!(
                    "授权状态的第二份存档未更新（不影响使用，本次运行不再重试）：{error}"
                ));
                AnchorSlot::Disabled(Some(record))
            }
        };
        note
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
        let (anchor, anchor_note) = self.anchor_record(mid);
        let mut notes: Vec<String> = anchor_note.into_iter().collect();
        // 第二份存档记录了「这份凭证已被停用」：授权文件被换成停用前的备份也照样锁定。
        let revoked_in_anchor = anchor
            .as_ref()
            .and_then(|record| record.revoked_token_sha256.as_deref())
            == Some(token_sha256(&stored.token).as_str());
        if stored.revoked || revoked_in_memory || revoked_in_anchor {
            return self.finish(Verdict::Locked(LockReason::Revoked), join_notes(notes));
        }
        // 时钟下限取三处最晚：授权文件、本次运行内存、第二份存档。恢复旧文件只能回滚其中一处。
        let anchor_seen = anchor.as_ref().map_or(0, |record| record.last_seen);
        let floor = stored
            .last_seen
            .max(self.mem_last_seen.load(Ordering::SeqCst))
            .max(anchor_seen);
        let v = decide(&stored.token, mid, now, floor, keys);
        if !v.is_active() {
            return self.finish(v, join_notes(notes));
        }
        self.mem_last_seen.fetch_max(now, Ordering::SeqCst);
        if next_last_seen(now, anchor_seen).is_some() {
            notes.extend(self.write_anchor(mid, AnchorUpdate::Seen(now)));
        }
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
                        notes.push(format!("授权状态连续 1 小时无法保存，已锁定：{e}"));
                        return self
                            .finish(Verdict::Locked(LockReason::Unpersisted), join_notes(notes));
                    }
                    notes.push(format!("授权状态暂时无法保存（{e}），1 小时内恢复即可"));
                }
            }
        }
        (self.finish(v, None).0, join_notes(notes))
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
        let accepted_sha256 = token_sha256(&token);
        self.write_locked(&Stored::new(mid, token, now, false))
            .map_err(|_| LockReason::Unpersisted)?;
        self.mem_last_seen.store(now, Ordering::SeqCst);
        self.unpersisted_since.store(0, Ordering::SeqCst);
        *self.revoked_token.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // 新凭证由服务器签发、刚通过验签：第二份存档的时钟下限重置为现在；
        // 旧凭证的停用记录保留（恢复旧凭证的备份仍然锁定）。写失败不影响激活结果。
        let accepted = AnchorUpdate::Accept {
            now,
            token_sha256: accepted_sha256,
        };
        if let Some(note) = self.write_anchor(mid, accepted) {
            crate::logging::log_event("授权", note);
        }
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
        // 先写第二份存档：即使授权文件之后被换回停用前的备份，这份凭证仍然锁定。
        let revoke = AnchorUpdate::Revoke {
            last_seen: last,
            token_sha256: token_sha256(&current.token),
        };
        if let Some(note) = self.write_anchor(mid, revoke) {
            crate::logging::log_event("授权", note);
        }
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

    use crate::license_anchor::MemoryAnchor;

    /// 带第二份存档启动：`store` 跨「重启」共享，模拟系统凭据库。
    fn boot_with(fx: &Fx, store: &std::sync::Arc<std::sync::Mutex<Option<String>>>) -> State {
        State::with_anchor(
            fx.dir.file(),
            Ok(MID.to_string()),
            Box::new(MemoryAnchor {
                value: store.clone(),
                ..Default::default()
            }),
        )
    }

    /// 回滚攻击：备份旧授权文件 → 用到很晚 → 恢复备份、重启、把时间拨回去。
    /// 没有第二份存档时能用（旧行为）；有了它，凭据库里更晚的时间仍然拦住回拨。
    #[test]
    fn restored_old_license_file_cannot_roll_back_clock() {
        let fx = Fx::new("anchor-rollback");
        let (_, store) = MemoryAnchor::shared();
        let s = boot_with(&fx, &store);
        fx.accept(&s, T0);
        let backup = std::fs::read(fx.dir.file()).unwrap();
        assert!(fx.eval(&s, T0 + 5 * DAY).is_active());

        std::fs::write(fx.dir.file(), &backup).unwrap();
        let rebooted = boot_with(&fx, &store);
        assert_eq!(
            locked(&fx.eval(&rebooted, T0 + DAY)),
            Some(LockReason::ClockRollback),
            "凭据库记得见过 T0+5 天，拨回 T0+1 天必须被拦住"
        );

        // 对照：没有第二份存档（旧行为）时同样的操作能用，说明测试确实打在了漏洞上。
        std::fs::write(fx.dir.file(), &backup).unwrap();
        assert!(fx.eval(&fx.boot(), T0 + DAY).is_active());
    }

    /// 被停用后恢复停用前的授权文件：凭据库里有停用记录，仍然锁定。
    #[test]
    fn restored_pre_revocation_file_stays_revoked() {
        let fx = Fx::new("anchor-revoked");
        let (_, store) = MemoryAnchor::shared();
        let s = boot_with(&fx, &store);
        fx.accept(&s, T0);
        let backup = std::fs::read(fx.dir.file()).unwrap();
        let tok = fx.read().token;
        s.mark_revoked(&tok).unwrap();

        std::fs::write(fx.dir.file(), &backup).unwrap();
        assert!(!fx.read().revoked, "备份里没有停用标记");
        let rebooted = boot_with(&fx, &store);
        assert_eq!(
            locked(&fx.eval(&rebooted, T0 + 10)),
            Some(LockReason::Revoked)
        );

        // 拿到新凭证后解除：新凭证不受旧停用记录影响
        assert!(fx.accept(&rebooted, T0 + 20).is_active());
        assert!(fx.eval(&boot_with(&fx, &store), T0 + 30).is_active());
    }

    /// 凭据库里的记录被手改或来自别的电脑：视为不存在（不锁定，只是不再提供额外保护）。
    #[test]
    fn tampered_anchor_record_is_ignored_not_fatal() {
        let fx = Fx::new("anchor-tampered");
        let (_, store) = MemoryAnchor::shared();
        let s = boot_with(&fx, &store);
        fx.accept(&s, T0);
        // 把凭据库里的时间改成很晚（想把用户锁死）——校验码对不上，必须被忽略
        let mut record: AnchorRecord =
            serde_json::from_str(store.lock().unwrap().as_deref().unwrap()).unwrap();
        record.last_seen = T0 + 100 * DAY;
        *store.lock().unwrap() = Some(serde_json::to_string(&record).unwrap());
        assert!(fx.eval(&boot_with(&fx, &store), T0 + 60).is_active());

        // 别的电脑的记录（机器码不同）同样无效
        let foreign = AnchorRecord::sealed(OTHER_MID, T0 + 100 * DAY, None);
        *store.lock().unwrap() = Some(serde_json::to_string(&foreign).unwrap());
        assert!(fx.eval(&boot_with(&fx, &store), T0 + 120).is_active());
    }

    /// 凭据库不可用（拒绝钥匙串访问等）：行为与没有第二份存档完全一致，不锁死用户。
    #[test]
    fn unavailable_anchor_falls_back_to_file_only() {
        let fx = Fx::new("anchor-unavailable");
        // 先用没有存档的实例激活，再换成「凭据库写不进去」的实例判定
        fx.accept(&fx.boot(), T0);
        let s = State::with_anchor(
            fx.dir.file(),
            Ok(MID.to_string()),
            Box::new(MemoryAnchor {
                fail_writes: true,
                ..Default::default()
            }),
        );
        let (verdict, note) = s.evaluate(T0 + DAY, Ok(&fx.keys), true);
        assert!(verdict.is_active());
        assert!(
            note.is_some_and(|n| n.contains("不影响使用")),
            "第一次失败要报告"
        );
        let (verdict, note) = s.evaluate(T0 + DAY + 120, Ok(&fx.keys), true);
        assert!(verdict.is_active());
        assert!(note.is_none(), "同一次运行内不重复报告");
    }

    /// 凭据库一时读不到（钥匙串锁定、拒绝访问）：不能当成「没有记录」再写一份新的，
    /// 否则原有的停用记录被覆盖；本次运行改为只读授权文件，凭据库原样不动。
    #[test]
    fn unreadable_anchor_is_never_overwritten() {
        let fx = Fx::new("anchor-unreadable");
        let (_, store) = MemoryAnchor::shared();
        let s = boot_with(&fx, &store);
        fx.accept(&s, T0);
        let tok = fx.read().token;
        s.mark_revoked(&tok).unwrap();
        let before = store.lock().unwrap().clone();
        assert!(before.is_some());

        // 换一份凭证激活（新实例、凭据库读失败），然后判定几次
        let flaky = State::with_anchor(
            fx.dir.file(),
            Ok(MID.to_string()),
            Box::new(MemoryAnchor {
                value: store.clone(),
                fail_reads: true,
                ..Default::default()
            }),
        );
        let (verdict, note) = flaky.evaluate(T0 + 10, Ok(&fx.keys), true);
        assert_eq!(
            verdict,
            Verdict::Locked(LockReason::Revoked),
            "授权文件里的停用标记照常生效"
        );
        assert!(
            note.is_some_and(|n| n.contains("读取失败")),
            "第一次读失败要报告"
        );
        assert!(fx.accept(&flaky, T0 + 20).is_active());
        let (verdict, note) = flaky.evaluate(T0 + DAY, Ok(&fx.keys), true);
        assert!(verdict.is_active(), "读不到凭据库不能锁死用户");
        assert!(note.is_none(), "读失败只报告一次");
        assert_eq!(*store.lock().unwrap(), before, "凭据库里的记录必须原样保留");
    }

    /// 激活新凭证后恢复「已停用的旧凭证」的备份：停用记录不能随新凭证激活被清掉。
    #[test]
    fn revoked_token_stays_blocked_after_new_activation() {
        let fx = Fx::new("anchor-keep-revoked");
        let (_, store) = MemoryAnchor::shared();
        let s = boot_with(&fx, &store);
        fx.accept(&s, T0);
        let old_backup = std::fs::read(fx.dir.file()).unwrap();
        let old = fx.read().token;
        s.mark_revoked(&old).unwrap();

        assert!(fx.accept(&s, T0 + 20).is_active(), "新凭证正常激活");
        assert!(fx.eval(&s, T0 + 30).is_active());

        std::fs::write(fx.dir.file(), &old_backup).unwrap();
        assert_eq!(
            locked(&fx.eval(&boot_with(&fx, &store), T0 + 40)),
            Some(LockReason::Revoked)
        );
    }

    /// 停用时写入的时间不能低于凭据库里已有的更晚时间（只增不减）。
    #[test]
    fn revocation_never_lowers_anchor_clock() {
        let fx = Fx::new("anchor-revoke-clock");
        let (_, store) = MemoryAnchor::shared();
        fx.accept(&boot_with(&fx, &store), T0);
        assert!(fx.eval(&boot_with(&fx, &store), T0 + 5 * DAY).is_active());
        // 恢复早期文件后重启（内存里没有更晚时间），这时被停用
        let early = Stored::new(MID, fx.read().token, T0, false);
        fx.write_raw(&early);
        let s = boot_with(&fx, &store);
        s.mark_revoked(&early.token).unwrap();
        let record = AnchorRecord::parse(store.lock().unwrap().as_deref().unwrap(), MID).unwrap();
        assert_eq!(record.last_seen, T0 + 5 * DAY);
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
