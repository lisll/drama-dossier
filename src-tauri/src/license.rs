//! 卡密授权：IPC 命令、总闸门、功能内验权、守护线程与金丝雀自检。
//!
//! 五层检查（见 docs/LICENSING.md「客户端多层验权」）：
//! 1. 前端激活页（`src/license.js`）——只是体验层；
//! 2. [`gate`]：包住 `invoke_handler`，未授权时拒绝白名单外的**所有**命令（新增命令默认被锁）；
//! 3. [`guard`]：主要功能函数第一行调用，每次从磁盘重读凭证并**完整重验签**，不信任缓存；
//! 4. 守护线程：随机 3–9 分钟重验一次，每 24 小时联网续签，失效即全局锁定并通知前端；
//! 5. 金丝雀：用内置的「已知坏签名 / 已知好签名」各验一次，验签被改成恒真就会立刻暴露。
//!
//! 正常使用时全部在本机计算（几十微秒），不联网、不弹窗；只有授权真的失效才会提示。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dossier_cardkey::{
    decide, display_code, parse_key_list, production_keys, read_machine_id, LockReason, TrustedKey,
    Verdict,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::logging::log_event;

// ------------------------------------------------------------------ 配置

/// 授权服务器地址（HTTPS）。部署后填入，见 docs/LICENSING.md「上线检查清单」。
const LICENSE_SERVER: &str = "";
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const REFRESH_EVERY_SECS: u64 = 24 * 3600;
/// 未授权时也放行的命令：授权自身、看日志（方便把日志发给客服）。
const ALLOWED_WITHOUT_LICENSE: &[&str] = &[
    "license_status",
    "license_activate",
    "license_refresh",
    "license_machine_code",
    "open_log_dir",
    "read_recent_logs",
];

// ------------------------------------------------------------------ 字符串轻度混淆
//
// 让「在二进制里搜字符串定位检查点」这一招失效。挡不住高手，只抬高门槛。

const fn xor_bytes<const N: usize>(s: &[u8; N]) -> [u8; N] {
    let mut out = [0u8; N];
    let mut i = 0;
    while i < N {
        out[i] = s[i] ^ (0x5a ^ (i as u8).wrapping_mul(31));
        i += 1;
    }
    out
}

fn unxor(s: &[u8]) -> String {
    s.iter()
        .enumerate()
        .map(|(i, b)| (b ^ (0x5a ^ (i as u8).wrapping_mul(31))) as char)
        .collect()
}

/// 编译期把字节串字面量异或编码，运行时再还原；二进制里不出现明文。
macro_rules! hidden {
    ($s:literal) => {{
        const ENC: [u8; $s.len()] = xor_bytes($s);
        unxor(&ENC)
    }};
}

fn license_file_name() -> String {
    hidden!(b"license.json")
}

fn locked_message() -> String {
    // 与前端约定：以此前缀开头的错误由激活页接管
    hidden!(b"LICENSE_LOCKED:")
}

// ------------------------------------------------------------------ 状态

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Stored {
    token: String,
    #[serde(default)]
    last_seen: u64,
}

struct Ctx {
    path: PathBuf,
    machine_id: Result<String, String>,
    app_version: String,
}

static CTX: OnceLock<Ctx> = OnceLock::new();
static APP: OnceLock<AppHandle> = OnceLock::new();
/// 闸门的快速路径缓存：最近一次判定是否有效 + 该结论在何时之前可信（=凭证 exp）。
static ACTIVE_UNTIL: AtomicU64 = AtomicU64::new(0);
/// 只进不出的锁定标志：一旦发现篡改或失效就置位，只有联网激活/续签成功才清除。
static HARD_LOCK: AtomicBool = AtomicBool::new(false);
static IO_LOCK: Mutex<()> = Mutex::new(());
static LAST_REFRESH: AtomicU64 = AtomicU64::new(0);

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn ctx() -> Option<&'static Ctx> {
    CTX.get()
}

#[cfg(debug_assertions)]
fn dev_bypass() -> bool {
    let on = std::env::var("DD_LICENSE_DEV_BYPASS").is_ok_and(|v| v == "1");
    if on {
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            log_event(
                "授权",
                "警告：DD_LICENSE_DEV_BYPASS=1，调试构建已跳过授权检查（正式版不存在此开关）",
            );
        }
    }
    on
}

#[cfg(not(debug_assertions))]
#[inline(always)]
fn dev_bypass() -> bool {
    false
}

fn read_stored(c: &Ctx) -> Option<Stored> {
    let _g = IO_LOCK.lock().ok()?;
    let text = std::fs::read_to_string(&c.path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_stored(c: &Ctx, s: &Stored) -> Result<(), String> {
    let _g = IO_LOCK.lock().map_err(|_| "授权文件锁异常".to_string())?;
    if let Some(dir) = c.path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("无法创建配置目录：{e}"))?;
    }
    let tmp = c.path.with_extension("json.tmp");
    let text = serde_json::to_string(s).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| format!("写入授权文件失败：{e}"))?;
    std::fs::rename(&tmp, &c.path).map_err(|e| format!("保存授权文件失败：{e}"))
}

/// 信任的公钥。release 构建只认 `PRODUCTION_KEYS`；debug 构建可用
/// `DD_LICENSE_DEV_PUBKEYS`（逗号分隔 base64url）连本地测试服务器。
#[inline(always)]
fn trusted_keys() -> Result<&'static [TrustedKey], dossier_cardkey::KeyError> {
    #[cfg(debug_assertions)]
    {
        static DEV: OnceLock<Option<Vec<TrustedKey>>> = OnceLock::new();
        let dev = DEV.get_or_init(|| {
            let raw = std::env::var("DD_LICENSE_DEV_PUBKEYS").ok()?;
            let list: Vec<&str> = raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            let keys = parse_key_list(&list).ok()?;
            log_event(
                "授权",
                "警告：调试构建正在使用 DD_LICENSE_DEV_PUBKEYS（正式版不存在此开关）",
            );
            Some(keys)
        });
        if let Some(keys) = dev {
            return Ok(keys);
        }
    }
    production_keys()
}
/// 完整判定：读盘 → 金丝雀 → 验签 → 机器码 → 时钟 → 有效期。不使用任何缓存。
#[inline(always)]
fn evaluate_full() -> Verdict {
    let Some(c) = ctx() else {
        return Verdict::Locked(LockReason::NotConfigured);
    };
    if !canary_ok() {
        return Verdict::Locked(LockReason::Tampered);
    }
    let mid = match &c.machine_id {
        Ok(m) => m,
        Err(_) => return Verdict::Locked(LockReason::MachineUnavailable),
    };
    let Some(stored) = read_stored(c) else {
        return Verdict::Locked(LockReason::NotActivated);
    };
    let now = now_secs();
    let v = decide(&stored.token, mid, now, stored.last_seen, trusted_keys());
    if v.is_active() {
        if let Some(next) = dossier_cardkey::next_last_seen(now, stored.last_seen) {
            let _ = write_stored(
                c,
                &Stored {
                    token: stored.token,
                    last_seen: next,
                },
            );
        }
    }
    v
}

fn apply_verdict(v: &Verdict) {
    match v {
        Verdict::Active(a) => {
            ACTIVE_UNTIL.store(a.until.map_or(a.exp, |u| u.min(a.exp)), Ordering::SeqCst);
        }
        Verdict::Locked(reason) => {
            let was_active = ACTIVE_UNTIL.swap(0, Ordering::SeqCst) != 0;
            HARD_LOCK.store(true, Ordering::SeqCst);
            if was_active {
                log_event("授权", format!("授权失效：{}", reason.code()));
                notify_locked(*reason);
            }
        }
    }
}

fn notify_locked(reason: LockReason) {
    if let Some(app) = APP.get() {
        let _ = app.emit(
            "license-changed",
            serde_json::json!({ "active": false, "reason": reason.code(), "message": reason.message() }),
        );
    }
}

fn locked_error(reason: LockReason) -> String {
    format!("{}{}", locked_message(), reason.message())
}

/// 闸门快速路径：只比较原子变量与时间；真正的验签由 guard/守护线程负责。
fn is_active_fast() -> bool {
    if dev_bypass() {
        return true;
    }
    !HARD_LOCK.load(Ordering::SeqCst) && now_secs() < ACTIVE_UNTIL.load(Ordering::SeqCst)
}

// ------------------------------------------------------------------ 第③层：功能内验权

/// 主要功能函数的第一行：`crate::license::guard()?;`
///
/// 每次都完整重验签（不信任闸门缓存），内联展开进调用方，二进制里没有单一检查点可改。
#[inline(always)]
pub fn guard() -> Result<(), String> {
    if dev_bypass() {
        return Ok(());
    }
    if HARD_LOCK.load(Ordering::SeqCst) {
        return Err(locked_error(LockReason::Revoked));
    }
    let v = evaluate_full();
    let r = match &v {
        Verdict::Active(_) => Ok(()),
        Verdict::Locked(reason) => Err(locked_error(*reason)),
    };
    apply_verdict(&v);
    r
}

// ------------------------------------------------------------------ 第②层：总闸门

pub(crate) fn allowed_without_license(command: &str) -> bool {
    ALLOWED_WITHOUT_LICENSE.contains(&command)
}

/// 包住 `generate_handler!`：未授权时拒绝白名单外的所有命令。
pub fn gate<F>(inner: F) -> impl Fn(tauri::ipc::Invoke) -> bool + Send + Sync + 'static
where
    F: Fn(tauri::ipc::Invoke) -> bool + Send + Sync + 'static,
{
    move |invoke| {
        let command = invoke.message.command();
        if allowed_without_license(command) || is_active_fast() {
            return inner(invoke);
        }
        let reason = if ctx().is_none() {
            LockReason::NotConfigured
        } else {
            LockReason::NotActivated
        };
        invoke.resolver.reject(locked_error(reason));
        true
    }
}

// ------------------------------------------------------------------ 第⑤层：金丝雀

/// 内置的测试公钥 + 一条好凭证 + 一条坏凭证（同一公开测试种子签出，见 tests/vectors_v1.json）。
/// 只用于自检，**不会**进入生产信任列表：验签函数被改成恒真时，坏凭证也会通过，自检随即失败。
const CANARY_KEY: &str = "A6EHv_POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbg";
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/license_canary.rs"
));

#[inline(always)]
fn canary_ok() -> bool {
    // 只缓存公钥解析（点解压较慢）；两次验签每次都真做，自检不能被缓存掉。
    static KEY: OnceLock<Option<Vec<TrustedKey>>> = OnceLock::new();
    let Some(keys) = KEY.get_or_init(|| parse_key_list(&[CANARY_KEY]).ok()) else {
        return false;
    };
    let keys: &[TrustedKey] = keys;
    let good = decide(CANARY_GOOD, CANARY_MID, CANARY_NOW, 0, Ok(keys));
    let bad = decide(CANARY_BAD, CANARY_MID, CANARY_NOW, 0, Ok(keys));
    good.is_active() && matches!(bad, Verdict::Locked(LockReason::BadSignature))
}

// ------------------------------------------------------------------ 联网

#[derive(Deserialize)]
struct IssuedOut {
    token: String,
}

#[derive(Deserialize)]
struct ServerError {
    error: String,
    message: String,
}

fn server_base() -> Result<String, String> {
    #[cfg(debug_assertions)]
    if let Ok(v) = std::env::var("DD_LICENSE_SERVER") {
        if !v.trim().is_empty() {
            return validate_base(v.trim());
        }
    }
    if LICENSE_SERVER.is_empty() {
        return Err("授权服务器未配置，请联系客服获取正式版本。".to_string());
    }
    validate_base(LICENSE_SERVER)
}

fn validate_base(base: &str) -> Result<String, String> {
    let base = base.trim_end_matches('/');
    let local = base.starts_with("http://127.0.0.1") || base.starts_with("http://localhost");
    if !(base.starts_with("https://") || local) {
        return Err("授权服务器地址必须是 HTTPS".to_string());
    }
    Ok(base.to_string())
}

/// 服务器明确拒绝（4xx 业务错误）与网络失败要分开：前者锁定，后者只是暂时连不上。
enum CallError {
    Rejected { code: String, message: String },
    Network(String),
}

fn post(path: &str, body: serde_json::Value) -> Result<String, CallError> {
    let base = server_base().map_err(CallError::Network)?;
    let client = reqwest::blocking::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| CallError::Network(format!("初始化网络失败：{e}")))?;
    let resp = client
        .post(format!("{base}{path}"))
        .json(&body)
        .send()
        .map_err(|e| CallError::Network(format!("无法连接授权服务器：{e}")))?;
    let status = resp.status();
    let text = resp
        .text()
        .map_err(|e| CallError::Network(format!("读取服务器响应失败：{e}")))?;
    if status.is_success() {
        let out: IssuedOut = serde_json::from_str(&text)
            .map_err(|_| CallError::Network("服务器响应格式错误".to_string()))?;
        return Ok(out.token);
    }
    match serde_json::from_str::<ServerError>(&text) {
        Ok(e) if status.is_client_error() && status.as_u16() != 429 => Err(CallError::Rejected {
            code: e.error,
            message: e.message,
        }),
        Ok(e) => Err(CallError::Network(e.message)),
        Err(_) => Err(CallError::Network(format!(
            "授权服务器返回异常（{status}）"
        ))),
    }
}

/// 收到新凭证：先本地完整验签，通过才落盘，绝不保存一个自己验不过的凭证。
fn accept_token(c: &Ctx, token: String) -> Result<Verdict, String> {
    let mid = c.machine_id.as_ref().map_err(|e| e.clone())?;
    let now = now_secs();
    let v = decide(&token, mid, now, 0, trusted_keys());
    if let Verdict::Locked(reason) = v {
        log_event(
            "授权",
            format!("服务器下发的凭证未通过本地校验：{}", reason.code()),
        );
        return Err(reason.message().to_string());
    }
    if !canary_ok() {
        return Err(LockReason::Tampered.message().to_string());
    }
    write_stored(
        c,
        &Stored {
            token,
            last_seen: now,
        },
    )?;
    HARD_LOCK.store(false, Ordering::SeqCst);
    LAST_REFRESH.store(now, Ordering::SeqCst);
    apply_verdict(&v);
    Ok(v)
}

fn refresh_blocking() -> Result<Verdict, String> {
    let c = ctx().ok_or("授权模块未初始化")?;
    let mid = c.machine_id.clone()?;
    let stored = read_stored(c).ok_or_else(|| LockReason::NotActivated.message().to_string())?;
    match post(
        "/v1/refresh",
        serde_json::json!({ "token": stored.token, "machine_id": mid, "app_version": c.app_version }),
    ) {
        Ok(token) => accept_token(c, token),
        Err(CallError::Rejected { code, message }) => {
            log_event("授权", format!("续签被服务器拒绝：{code}"));
            apply_verdict(&Verdict::Locked(LockReason::Revoked));
            Err(message)
        }
        Err(CallError::Network(msg)) => Err(msg),
    }
}

// ------------------------------------------------------------------ 第④层：守护线程

fn spawn_daemon() {
    std::thread::Builder::new()
        .name("dd-housekeeping".into())
        .spawn(|| {
            // 启动后先等一会儿再联网，避免拖慢首屏
            std::thread::sleep(Duration::from_secs(20));
            loop {
                let v = if dev_bypass() {
                    None
                } else {
                    Some(evaluate_full())
                };
                if let Some(v) = &v {
                    apply_verdict(v);
                }
                let has_token = ctx().and_then(read_stored).is_some();
                let due = now_secs().saturating_sub(LAST_REFRESH.load(Ordering::SeqCst))
                    >= REFRESH_EVERY_SECS;
                if has_token && due && !dev_bypass() {
                    match refresh_blocking() {
                        Ok(_) => log_event("授权", "后台续签成功"),
                        // 连不上不打扰用户：凭证还在有效期内就继续用
                        Err(e) => {
                            LAST_REFRESH.store(
                                now_secs().saturating_sub(REFRESH_EVERY_SECS - 3600),
                                Ordering::SeqCst,
                            );
                            log_event("授权", format!("后台续签未成功（1 小时后重试）：{e}"));
                        }
                    }
                }
                std::thread::sleep(Duration::from_secs(jitter_secs()));
            }
        })
        .map(|_| ())
        .unwrap_or_else(|e| {
            // 守护线程起不来就直接锁定，不能悄悄少一层检查
            log_event("授权", format!("守护线程启动失败：{e}"));
            HARD_LOCK.store(true, Ordering::SeqCst);
        });
}

/// 180–540 秒的随机间隔（不引入 rand 依赖，用系统时间的纳秒部分做种子足够）。
fn jitter_secs() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0) as u64;
    180 + (nanos.wrapping_mul(2_654_435_761) % 361)
}

// ------------------------------------------------------------------ 初始化

/// 在 `setup` 里调用一次。
pub fn init(app: &AppHandle) {
    let path = app
        .path()
        .app_config_dir()
        .map(|d| d.join(license_file_name()))
        .unwrap_or_else(|_| {
            std::env::temp_dir()
                .join("drama-dossier")
                .join(license_file_name())
        });
    let machine_id = read_machine_id().map_err(|e| {
        log_event("授权", format!("读取机器码失败：{e}"));
        e.to_string()
    });
    let _ = CTX.set(Ctx {
        path,
        machine_id,
        app_version: app.package_info().version.to_string(),
    });
    let _ = APP.set(app.clone());
    if !canary_ok() {
        log_event("授权", "启动自检失败");
        HARD_LOCK.store(true, Ordering::SeqCst);
    } else {
        HARD_LOCK.store(false, Ordering::SeqCst);
        let v = evaluate_full();
        if v.is_active() {
            ACTIVE_UNTIL.store(
                match &v {
                    Verdict::Active(a) => a.until.map_or(a.exp, |u| u.min(a.exp)),
                    _ => 0,
                },
                Ordering::SeqCst,
            );
        } else {
            HARD_LOCK.store(true, Ordering::SeqCst);
        }
    }
    spawn_daemon();
}

// ------------------------------------------------------------------ IPC 命令

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LicenseStatus {
    active: bool,
    reason: Option<&'static str>,
    message: String,
    plan: Option<&'static str>,
    plan_label: Option<&'static str>,
    until: Option<u64>,
    exp: Option<u64>,
    machine_code: Option<String>,
    machine_id: Option<String>,
    server_configured: bool,
}

fn status_from(v: &Verdict) -> LicenseStatus {
    let c = ctx();
    let mid = c.and_then(|c| c.machine_id.as_ref().ok());
    let base = LicenseStatus {
        active: false,
        reason: None,
        message: String::new(),
        plan: None,
        plan_label: None,
        until: None,
        exp: None,
        machine_code: mid.map(|m| display_code(m)),
        machine_id: mid.cloned(),
        server_configured: server_base().is_ok(),
    };
    match v {
        Verdict::Active(a) => LicenseStatus {
            active: true,
            message: "已激活".into(),
            plan: Some(a.plan.as_str()),
            plan_label: Some(a.plan.label()),
            until: a.until,
            exp: Some(a.exp),
            ..base
        },
        Verdict::Locked(r) => LicenseStatus {
            reason: Some(r.code()),
            message: r.message().into(),
            ..base
        },
    }
}

#[tauri::command]
pub fn license_status() -> LicenseStatus {
    if dev_bypass() {
        let mut s = status_from(&Verdict::Locked(LockReason::NotActivated));
        s.active = true;
        s.reason = Some("dev_bypass");
        s.message = "调试构建已跳过授权检查".into();
        return s;
    }
    let v = evaluate_full();
    apply_verdict(&v);
    if v.is_active() {
        HARD_LOCK.store(false, Ordering::SeqCst);
    }
    status_from(&v)
}

#[tauri::command]
pub fn license_machine_code() -> Result<String, String> {
    let c = ctx().ok_or("授权模块未初始化")?;
    c.machine_id.clone()
}

#[tauri::command]
pub async fn license_activate(card: String) -> Result<LicenseStatus, String> {
    let code = dossier_cardkey::normalize_card(&card).ok_or("卡密格式不正确，请检查后重新输入")?;
    tauri::async_runtime::spawn_blocking(move || {
        let c = ctx().ok_or("授权模块未初始化")?;
        let mid = c.machine_id.clone()?;
        log_event("授权", "提交卡密激活");
        let token = match post(
            "/v1/activate",
            serde_json::json!({ "card": code, "machine_id": mid, "app_version": c.app_version }),
        ) {
            Ok(t) => t,
            Err(CallError::Rejected { code, message }) => {
                log_event("授权", format!("激活被拒绝：{code}"));
                return Err(message);
            }
            Err(CallError::Network(m)) => return Err(m),
        };
        let v = accept_token(c, token)?;
        log_event("授权", "激活成功");
        Ok(status_from(&v))
    })
    .await
    .map_err(|e| format!("激活任务异常：{e}"))?
}

#[tauri::command]
pub async fn license_refresh() -> Result<LicenseStatus, String> {
    tauri::async_runtime::spawn_blocking(|| refresh_blocking().map(|v| status_from(&v)))
        .await
        .map_err(|e| format!("续签任务异常：{e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitelist_is_minimal_and_exact() {
        for cmd in ALLOWED_WITHOUT_LICENSE {
            assert!(allowed_without_license(cmd));
        }
        for cmd in [
            "generate_package",
            "transcode_execute",
            "dedup_demo_execute",
            "run_erase",
            "plan_erase",
            "export_draft",
            "apply_rename",
            "fix_bitrate",
            "account_login",
            "load_app_settings",
            "LICENSE_STATUS",
            "license_status ",
            "",
            "some_future_command",
        ] {
            assert!(!allowed_without_license(cmd), "{cmd} 不应在白名单里");
        }
    }

    #[test]
    fn hidden_strings_round_trip_and_are_not_plain() {
        assert_eq!(license_file_name(), "license.json");
        assert_eq!(locked_message(), "LICENSE_LOCKED:");
        const ENC: [u8; 12] = xor_bytes(b"license.json");
        assert_ne!(&ENC, b"license.json");
    }

    #[test]
    fn canary_passes_with_genuine_verifier() {
        assert!(canary_ok());
    }

    #[test]
    fn guard_locks_when_uninitialized() {
        // 测试进程里 CTX 未初始化 → 必须锁定
        if ctx().is_none() && !dev_bypass() {
            let e = guard().unwrap_err();
            assert!(e.starts_with("LICENSE_LOCKED:"), "{e}");
        }
    }

    #[test]
    fn server_must_be_https() {
        assert!(validate_base("http://evil.example").is_err());
        assert!(validate_base("https://license.example.com/")
            .unwrap()
            .ends_with(".com"));
        assert!(validate_base("http://127.0.0.1:8200").is_ok());
    }

    #[test]
    fn jitter_in_range() {
        for _ in 0..100 {
            let j = jitter_secs();
            assert!((180..=540).contains(&j));
        }
    }

    /// guard 的主要开销是金丝雀两次验签 + 凭证一次验签。这里测真实的验签路径
    /// （canary_ok 内含两次完整 decide），而不是未初始化时的快速失败分支。
    /// 阈值只在 release 下断言：debug 构建里 ed25519 未优化，数字没有参考意义。
    #[test]
    fn guard_crypto_path_is_fast() {
        const N: u32 = 2_000;
        let t = std::time::Instant::now();
        for _ in 0..N {
            assert!(canary_ok());
        }
        let per = t.elapsed() / N;
        eprintln!("canary_ok（2 次 decide）平均耗时 {per:?}");
        if !cfg!(debug_assertions) {
            assert!(per < Duration::from_micros(500), "验签路径平均耗时 {per:?}");
        }
    }

    /// 完整路径：金丝雀 2 次 + 读盘 + 凭证 1 次验签，与 guard() 在已激活时做的事完全相同。
    #[test]
    fn full_check_path_is_fast() {
        // 用金丝雀的好凭证模拟磁盘上的授权文件
        let dir = std::env::temp_dir().join(format!("dd-license-bench-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("license.json");
        std::fs::write(
            &path,
            serde_json::to_string(&Stored {
                token: CANARY_GOOD.into(),
                last_seen: 0,
            })
            .unwrap(),
        )
        .unwrap();
        let keys = parse_key_list(&[CANARY_KEY]).unwrap();
        const N: u32 = 2_000;
        let t = std::time::Instant::now();
        for _ in 0..N {
            assert!(canary_ok());
            let s: Stored = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert!(decide(&s.token, CANARY_MID, CANARY_NOW, s.last_seen, Ok(&keys)).is_active());
        }
        let per = t.elapsed() / N;
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("完整验权路径平均耗时 {per:?}");
        if !cfg!(debug_assertions) {
            assert!(per < Duration::from_micros(500), "完整验权平均耗时 {per:?}");
        }
    }

    /// 端到端：真实客户端代码 ↔ 本地运行的 license-server。手动运行：
    ///
    /// ```text
    /// DD_LICENSE_SERVER=http://127.0.0.1:端口 DD_LICENSE_DEV_PUBKEYS=<pubkey> \
    /// DD_E2E_CARDS=卡1,卡2 cargo test -p drama-dossier license::tests::e2e -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn e2e_against_local_server() {
        let cards: Vec<String> = std::env::var("DD_E2E_CARDS")
            .expect("需要 DD_E2E_CARDS")
            .split(',')
            .map(str::to_string)
            .collect();
        let dir = std::env::temp_dir().join(format!("dd-license-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // 不依赖本机真实机器码，用固定值，便于在后台按前缀查找
        let mid = "e2e0".repeat(16);
        assert!(CTX
            .set(Ctx {
                path: dir.join("license.json"),
                machine_id: Ok(mid.clone()),
                app_version: "e2e".into(),
            })
            .is_ok());

        // 1. 未激活：guard 与闸门都拒绝
        assert!(guard().unwrap_err().starts_with("LICENSE_LOCKED:"));
        assert!(!is_active_fast());
        assert_eq!(evaluate_full(), Verdict::Locked(LockReason::NotActivated));

        // 2. 错误卡密：服务器拒绝，消息原样给用户
        let bad = post(
            "/v1/activate",
            serde_json::json!({"card": "DDK-2222-2222-2222-2222", "machine_id": mid, "app_version": "e2e"}),
        );
        assert!(matches!(bad, Err(CallError::Rejected { ref code, .. }) if code == "card_invalid"));

        // 3. 激活：走与 license_activate 相同的 post + accept_token
        let token = post(
            "/v1/activate",
            serde_json::json!({"card": cards[0], "machine_id": mid, "app_version": "e2e"}),
        )
        .unwrap_or_else(|e| match e {
            CallError::Rejected { message, .. } | CallError::Network(message) => {
                panic!("激活失败：{message}")
            }
        });
        let v = accept_token(ctx().unwrap(), token).expect("本地验签应通过");
        assert!(v.is_active(), "{v:?}");
        assert!(guard().is_ok(), "激活后 guard 应放行");
        assert!(is_active_fast(), "激活后闸门应放行");
        eprintln!("激活成功：{v:?}");

        // 4. 授权文件被篡改：guard 立刻锁定
        let path = dir.join("license.json");
        let original = std::fs::read_to_string(&path).unwrap();
        let mut stored: Stored = serde_json::from_str(&original).unwrap();
        stored.token = stored.token.replacen('e', "f", 1);
        std::fs::write(&path, serde_json::to_string(&stored).unwrap()).unwrap();
        assert!(guard().is_err(), "篡改后的授权文件必须被拒");
        assert!(!is_active_fast(), "篡改被发现后闸门也要锁定");
        std::fs::write(&path, &original).unwrap();

        // 5. 续签：走 refresh_blocking，成功后解除锁定
        let v = refresh_blocking().expect("续签应成功");
        assert!(v.is_active());
        assert!(guard().is_ok());

        // 6. 同一张卡换一台电脑：服务器拒绝
        let other = post(
            "/v1/activate",
            serde_json::json!({"card": cards[0], "machine_id": "e2e1".repeat(16), "app_version": "e2e"}),
        );
        assert!(matches!(other, Err(CallError::Rejected { ref code, .. }) if code == "card_used"));

        // 7. 第二张卡叠加时长
        if let Some(second) = cards.get(1) {
            let token = post(
                "/v1/activate",
                serde_json::json!({"card": second, "machine_id": mid, "app_version": "e2e"}),
            )
            .unwrap_or_else(|_| panic!("第二张卡激活失败"));
            let v2 = accept_token(ctx().unwrap(), token).unwrap();
            eprintln!("叠加后：{v2:?}");
        }

        // 8. 等待后台作废（由测试脚本在另一个进程里调用管理接口），然后续签必须被拒并锁定
        if let Ok(flag) = std::env::var("DD_E2E_REVOKED_FLAG") {
            for _ in 0..60 {
                if std::path::Path::new(&flag).exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            let e = refresh_blocking().expect_err("作废后续签必须失败");
            eprintln!("作废后续签：{e}");
            assert!(guard().is_err(), "作废后 guard 必须锁定");
            assert!(!is_active_fast(), "作废后闸门必须锁定");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
