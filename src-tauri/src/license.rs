//! 卡密授权：IPC 命令、总闸门、功能内验权、守护线程与金丝雀自检。
//!
//! 五层检查（见 docs/LICENSING.md「客户端多层验权」）：
//! 1. 前端激活页（`src/license.js`）——只是体验层；
//! 2. [`gate`]：包住 `invoke_handler`，未授权时拒绝白名单外的**所有**命令（新增命令默认被锁）；
//! 3. [`guard`]：主要功能函数第一行调用，每次从磁盘重读凭证并**完整重验签**，不信任缓存；
//! 4. 守护线程：随机 3–9 分钟重验一次，每 24 小时联网续签，状态变化即通知前端；
//! 5. 金丝雀：用内置的「已知坏签名 / 已知好签名」各验一次，验签被改成恒真就会立刻暴露。
//!
//! 状态与判定规则在 [`crate::license_state`]；这里只负责接线、联网和通知。
//! 正常使用时全部在本机计算（几十微秒），不联网、不弹窗；只有授权真的失效才会提示。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dossier_cardkey::{
    decide, display_code, parse_key_list, production_keys, read_machine_id, LockReason, TrustedKey,
    Verdict,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};

use crate::license_state::{ReadOutcome, State};
use crate::logging::log_event;

// ------------------------------------------------------------------ 配置

/// 授权服务器地址（HTTPS）。部署后填入，见 docs/LICENSING.md「上线检查清单」。
const LICENSE_SERVER: &str = "";
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const REFRESH_EVERY_SECS: u64 = 24 * 3600;
/// 续签失败后多久重试
const REFRESH_RETRY_SECS: u64 = 3600;
/// 未授权时也放行的命令：授权自身、看日志（方便把日志发给客服）、
/// 停止正在跑的长任务和查询其状态（授权中途失效时，用户必须能停下来）。
const ALLOWED_WITHOUT_LICENSE: &[&str] = &[
    "license_status",
    "license_activate",
    "license_refresh",
    "license_machine_code",
    "open_log_dir",
    "read_recent_logs",
    "transcode_stop",
    "bitrate_stop",
    "dedup_demo_stop",
    "erase_status",
];
/// 服务器明确表示「这台电脑不能再用这份授权」的错误码。只有这几种才锁定，
/// 其余任何错误（404/422/413、代理或防火墙返回的页面等）都按「暂时连不上」处理。
const REVOKING_ERRORS: &[&str] = &[
    "token_invalid",
    "machine_revoked",
    "license_expired",
    "not_activated",
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

// ------------------------------------------------------------------ 全局状态

struct Ctx {
    state: State,
    app_version: String,
}

static CTX: OnceLock<Ctx> = OnceLock::new();
static APP: OnceLock<AppHandle> = OnceLock::new();
static LAST_REFRESH: AtomicU64 = AtomicU64::new(0);
/// 上一次通知前端时的结论；只有变化时才发事件
static LAST_NOTIFIED_ACTIVE: AtomicBool = AtomicBool::new(false);

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn ctx() -> Option<&'static Ctx> {
    CTX.get()
}

// TEST-MODE：防止测试版混进正式版。填了正式公钥又没关测试开关，直接编译失败。
#[cfg(feature = "license-test-mode")]
const _: () = assert!(
    dossier_cardkey::PRODUCTION_KEYS.is_empty(),
    "已填入正式公钥：请先按 docs/LICENSING.md「删除测试版」删除 license-test-mode，再发布正式版"
);

// 正式发布包与测试版互斥：两者同开说明构建命令写错了，宁可编译失败也不出一个身份不明的包。
#[cfg(all(feature = "official-release", feature = "license-test-mode"))]
compile_error!("official-release 与 license-test-mode 不能同时开启：正式包不得包含测试授权");

// 正式发布包必须带正式公钥：没填公钥时出的包谁都激活不了，不该被发布出去。
#[cfg(feature = "official-release")]
const _: () = assert!(
    !dossier_cardkey::PRODUCTION_KEYS.is_empty(),
    "official-release 需要正式公钥：请先在 crates/dossier-cardkey/src/keys.rs 的 PRODUCTION_KEYS 中填入正式公钥"
);

/// 调试构建默认跳过授权检查，本地开发不必激活；想在调试构建里测授权，设置
/// `DD_LICENSE_DEV_BYPASS=0`。正式版（release）里没有这段代码；自动测试（cfg(test)）
/// 也不跳过，测试照样走真实检查。
#[cfg(all(debug_assertions, not(test)))]
fn dev_bypass() -> bool {
    let on = std::env::var("DD_LICENSE_DEV_BYPASS").map_or(true, |v| v != "0");
    if on {
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::Relaxed) {
            log_event(
                "授权",
                "警告：调试构建默认跳过授权检查（设置 DD_LICENSE_DEV_BYPASS=0 可启用；正式版不存在此开关）",
            );
        }
    }
    on
}

#[cfg(not(all(debug_assertions, not(test))))]
#[inline(always)]
fn dev_bypass() -> bool {
    false
}

/// 信任的公钥。优先级：debug 构建的 `DD_LICENSE_DEV_PUBKEYS`（连本地测试服务器）
/// → 测试版内置测试公钥（TEST-MODE）→ `PRODUCTION_KEYS`。
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
    #[cfg(feature = "license-test-mode")] // TEST-MODE
    {
        static TEST: OnceLock<Result<Vec<TrustedKey>, dossier_cardkey::KeyError>> = OnceLock::new();
        return TEST
            .get_or_init(|| parse_key_list(&[crate::license_test_mode::public_key().as_str()]))
            .as_ref()
            .map(Vec::as_slice)
            .map_err(Clone::clone);
    }
    #[allow(unreachable_code)]
    production_keys()
}

/// 是否为离线测试版（TEST-MODE）。
const fn test_mode() -> bool {
    cfg!(feature = "license-test-mode")
}

/// 完整判定：读盘 → 金丝雀 → 校验码 → 验签 → 机器码 → 时钟 → 有效期。不使用任何缓存。
#[inline(always)]
fn evaluate_full() -> Verdict {
    let Some(c) = ctx() else {
        return Verdict::Locked(LockReason::NotConfigured);
    };
    let (v, note) = c.state.evaluate(now_secs(), trusted_keys(), canary_ok());
    if let Some(note) = note {
        log_event("授权", note);
    }
    notify_if_changed(&v);
    v
}

/// 结论变化时通知前端（有效 ↔ 锁定）。携带完整状态，前端不必再查一次。
fn notify_if_changed(v: &Verdict) {
    let active = v.is_active();
    if LAST_NOTIFIED_ACTIVE.swap(active, Ordering::SeqCst) == active {
        return;
    }
    if let Verdict::Locked(reason) = v {
        log_event("授权", format!("授权失效：{}", reason.code()));
    } else {
        log_event("授权", "授权恢复有效");
    }
    if let Some(app) = APP.get() {
        let _ = app.emit("license-changed", status_from(v));
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
    ctx().is_some_and(|c| c.state.is_active_fast(now_secs()))
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
    match evaluate_full() {
        Verdict::Active(_) => Ok(()),
        Verdict::Locked(reason) => Err(locked_error(reason)),
    }
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
        // 快速路径不放行时再完整判一次：刚激活、或另一窗口刚续签成功时不会被误拒
        let reason = match evaluate_full() {
            Verdict::Active(_) => return inner(invoke),
            Verdict::Locked(r) => r,
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

/// 只接受 HTTPS；明文 HTTP 只允许主机名恰好是本机回环地址（本地联调用）。
fn validate_base(base: &str) -> Result<String, String> {
    let base = base.trim_end_matches('/');
    let url = reqwest::Url::parse(base).map_err(|_| "授权服务器地址格式错误".to_string())?;
    // Url 解析后主机名已规范化（小写、IPv4 点分十进制），可以直接比较
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1"));
    let ok = match url.scheme() {
        "https" => url.host_str().is_some_and(|h| !h.is_empty()),
        "http" => loopback,
        _ => false,
    };
    if !ok || !url.username().is_empty() || url.password().is_some() {
        return Err("授权服务器地址必须是 HTTPS".to_string());
    }
    Ok(base.to_string())
}

/// 服务器明确停用与「暂时连不上」要分开：前者锁定，后者凭证有效期内照常使用。
#[derive(Debug)]
enum CallError {
    /// 服务器明确拒绝：激活时原样提示；续签时锁定
    Rejected {
        code: String,
        message: String,
    },
    Network(String),
}

/// 把 HTTP 状态码 + 响应体归类。单独拆出来便于逐一测试。
fn classify_failure(status: u16, body: &str) -> CallError {
    let parsed = serde_json::from_str::<ServerError>(body).ok();
    match parsed {
        // 激活接口的业务错误（卡密不存在、已被使用等）都是 4xx + JSON，需要原样告诉用户；
        // 续签只对 REVOKING_ERRORS 锁定，由调用方用 is_revoking() 判断。
        Some(e) if (400..500).contains(&status) && status != 429 => CallError::Rejected {
            code: e.error,
            message: e.message,
        },
        Some(e) => CallError::Network(e.message),
        None => CallError::Network(format!("授权服务器返回异常（HTTP {status}）")),
    }
}

impl CallError {
    /// 续签时：是不是「这台电脑不能再用这份授权」的明确答复
    fn is_revoking(&self) -> bool {
        matches!(self, CallError::Rejected { code, .. } if REVOKING_ERRORS.contains(&code.as_str()))
    }

    fn message(&self) -> String {
        match self {
            CallError::Rejected { message, .. } | CallError::Network(message) => message.clone(),
        }
    }
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
    Err(classify_failure(status.as_u16(), &text))
}

/// 收到新凭证：先本地完整验签，通过才落盘，绝不保存一个自己验不过的凭证。
fn accept_token(c: &Ctx, token: String) -> Result<Verdict, String> {
    match c
        .state
        .accept(token, now_secs(), trusted_keys(), canary_ok())
    {
        Ok(v) => {
            LAST_REFRESH.store(now_secs(), Ordering::SeqCst);
            notify_if_changed(&v);
            Ok(v)
        }
        Err(reason) => {
            log_event(
                "授权",
                format!("服务器下发的凭证未通过本地校验：{}", reason.code()),
            );
            Err(reason.message().to_string())
        }
    }
}

fn refresh_blocking() -> Result<Verdict, String> {
    let c = ctx().ok_or("授权模块未初始化")?;
    let mid = c.state.machine_id().map_err(str::to_string)?.to_string();
    let token = match c.state.read() {
        ReadOutcome::Ok(s) => s.token,
        _ => return Err(LockReason::NotActivated.message().to_string()),
    };
    match post(
        "/v1/refresh",
        serde_json::json!({ "token": token, "machine_id": mid, "app_version": c.app_version }),
    ) {
        Ok(fresh) => accept_token(c, fresh),
        Err(e) if e.is_revoking() => {
            if let CallError::Rejected { code, .. } = &e {
                log_event("授权", format!("续签被服务器拒绝：{code}"));
            }
            if let Err(io) = c.state.mark_revoked(&token) {
                log_event(
                    "授权",
                    format!("停用标记写入失败（本次运行仍保持锁定）：{io}"),
                );
            }
            notify_if_changed(&Verdict::Locked(LockReason::Revoked));
            Err(e.message())
        }
        Err(e) => Err(e.message()),
    }
}

// ------------------------------------------------------------------ 第④层：守护线程

/// 是否该联网续签了：有凭证、距上次成功续签已满 24 小时（失败后 1 小时重试）。
fn refresh_due(now: u64) -> bool {
    now.saturating_sub(LAST_REFRESH.load(Ordering::SeqCst)) >= REFRESH_EVERY_SECS
}

fn try_refresh(trigger: &str) {
    if dev_bypass() || server_base().is_err() {
        return;
    }
    match refresh_blocking() {
        Ok(_) => log_event("授权", format!("{trigger}：续签成功")),
        Err(e) => {
            // 1 小时后重试：把「上次续签」记成 (24h - 1h) 之前
            LAST_REFRESH.store(
                now_secs().saturating_sub(REFRESH_EVERY_SECS - REFRESH_RETRY_SECS),
                Ordering::SeqCst,
            );
            log_event(
                "授权",
                format!("{trigger}：续签未成功（1 小时后重试）：{e}"),
            );
        }
    }
}

fn spawn_daemon(refresh_now: bool) {
    let spawned = std::thread::Builder::new()
        .name("dd-housekeeping".into())
        .spawn(move || {
            if refresh_now {
                // 凭证已过期（例如一周没开软件）：立即续签，不让用户干等
                try_refresh("启动");
            } else {
                // 正常情况：先等一会儿再联网，避免拖慢首屏
                std::thread::sleep(Duration::from_secs(20));
            }
            loop {
                if !dev_bypass() {
                    let _ = evaluate_full();
                    let has_token =
                        ctx().is_some_and(|c| matches!(c.state.read(), ReadOutcome::Ok(_)));
                    if has_token && refresh_due(now_secs()) {
                        try_refresh("后台");
                    }
                }
                std::thread::sleep(Duration::from_secs(jitter_secs()));
            }
        });
    if let Err(e) = spawned {
        // 守护线程起不来就直接锁定，不能悄悄少一层检查
        log_event("授权", format!("守护线程启动失败：{e}"));
        if let Some(c) = ctx() {
            c.state.mark_tampered();
        }
    }
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
        // 第二份存档放系统凭据库（Windows 凭据管理器 / macOS 钥匙串），见 license_anchor.rs。
        state: State::with_anchor(path, machine_id, crate::license_anchor::platform_anchor()),
        app_version: app.package_info().version.to_string(),
    });
    let _ = APP.set(app.clone());
    if !canary_ok() {
        log_event("授权", "启动自检失败");
    }
    let v = evaluate_full();
    LAST_NOTIFIED_ACTIVE.store(v.is_active(), Ordering::SeqCst);
    // 凭证过期但本地还有授权文件：可能只是很久没联网，立即续签
    let refresh_now = matches!(v, Verdict::Locked(LockReason::Expired));
    spawn_daemon(refresh_now);
}

// ------------------------------------------------------------------ IPC 命令

#[derive(Serialize, Clone)]
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
    /// 能否联网续签（服务器地址已配置）
    server_configured: bool,
    /// 能否在激活页输入卡密（正式版 = 服务器已配置；测试版始终可以）
    can_activate: bool,
    /// 离线测试版（TEST-MODE），界面据此显示「测试版授权」
    test_mode: bool,
}

fn status_from(v: &Verdict) -> LicenseStatus {
    let mid = ctx().and_then(|c| c.state.machine_id().ok());
    let server_configured = server_base().is_ok();
    let base = LicenseStatus {
        active: false,
        reason: None,
        message: String::new(),
        plan: None,
        plan_label: None,
        until: None,
        exp: None,
        machine_code: mid.map(display_code),
        machine_id: mid.map(str::to_string),
        server_configured,
        can_activate: server_configured || test_mode(),
        test_mode: test_mode(),
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
    // 只判断、不清任何锁：停用标记在文件里，篡改标记在内存里，都不归这里管
    status_from(&evaluate_full())
}

#[tauri::command]
pub fn license_machine_code() -> Result<String, String> {
    let c = ctx().ok_or("授权模块未初始化")?;
    c.state
        .machine_id()
        .map(str::to_string)
        .map_err(str::to_string)
}

#[tauri::command]
pub async fn license_activate(card: String) -> Result<LicenseStatus, String> {
    let code = dossier_cardkey::normalize_card(&card).ok_or("卡密格式不正确，请检查后重新输入")?;
    tauri::async_runtime::spawn_blocking(move || {
        let c = ctx().ok_or("授权模块未初始化")?;
        let mid = c.state.machine_id().map_err(str::to_string)?.to_string();
        // TEST-MODE：测试版只接受测试卡密，本机签发后走与正式版相同的 accept_token
        #[cfg(feature = "license-test-mode")]
        {
            if !crate::license_test_mode::is_test_card(&code) {
                log_event("授权", "测试版收到非测试卡密，已拒绝");
                return Err("测试版只接受内部测试卡密，请联系发放人".into());
            }
            log_event("授权", "使用测试卡密离线激活");
            let token = crate::license_test_mode::issue(&mid, now_secs());
            let v = accept_token(c, token)?;
            log_event("授权", "测试版激活成功");
            return Ok(status_from(&v));
        }
        #[allow(unreachable_code)]
        {
            log_event("授权", "提交卡密激活");
            let token = match post(
                "/v1/activate",
                serde_json::json!({ "card": code, "machine_id": mid, "app_version": c.app_version }),
            ) {
                Ok(t) => t,
                Err(e) => {
                    if let CallError::Rejected { code, .. } = &e {
                        log_event("授权", format!("激活被拒绝：{code}"));
                    }
                    return Err(e.message());
                }
            };
            let v = accept_token(c, token)?;
            log_event("授权", "激活成功");
            Ok(status_from(&v))
        }
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
            "transcode_stop2",
            "",
            "some_future_command",
        ] {
            assert!(!allowed_without_license(cmd), "{cmd} 不应在白名单里");
        }
    }

    /// 白名单里的每个命令都必须真的存在，防止拼错导致「锁定后停不下转码」又悄悄回来
    #[test]
    fn whitelist_commands_exist() {
        let lib = include_str!("lib.rs");
        for cmd in ALLOWED_WITHOUT_LICENSE {
            let found = ["license", "logging", "toolbox", "erase"]
                .iter()
                .any(|m| lib.contains(&format!("{m}::{cmd},")))
                || lib.contains(&format!("            {cmd},"));
            assert!(found, "白名单命令 {cmd} 没有在 generate_handler! 里注册");
        }
    }

    #[test]
    fn only_explicit_revoking_codes_lock() {
        let body = |code: &str| format!(r#"{{"error":"{code}","message":"x"}}"#);
        for code in REVOKING_ERRORS {
            for status in [401, 403] {
                assert!(
                    classify_failure(status, &body(code)).is_revoking(),
                    "{status} {code} 应锁定"
                );
            }
        }
        // 这些都不能让续签锁定
        let cases: &[(u16, String)] = &[
            (404, body("not_found")),
            (422, body("bad_request")),
            (413, body("too_large")),
            (411, body("too_large")),
            (400, body("bad_request")),
            (429, body("rate_limited")),
            (500, body("internal")),
            (502, "<html>Bad Gateway</html>".into()),
            (403, "<html>Forbidden by WAF</html>".into()),
            (404, "".into()),
            (403, body("card_used")),
            // 错误码在白名单里但状态码是 5xx：服务器自己出错，不算明确答复
            (500, body("machine_revoked")),
        ];
        for (status, b) in cases {
            assert!(
                !classify_failure(*status, b).is_revoking(),
                "{status} {b} 不应锁定"
            );
        }
    }

    #[test]
    fn activation_errors_are_shown_verbatim() {
        let e = classify_failure(
            409,
            r#"{"error":"card_used","message":"这张卡密已被其他电脑使用"}"#,
        );
        assert!(matches!(&e, CallError::Rejected { code, .. } if code == "card_used"));
        assert_eq!(e.message(), "这张卡密已被其他电脑使用");
        let e = classify_failure(502, "<html>");
        assert!(matches!(e, CallError::Network(_)));
        assert!(e.message().contains("502"));
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
            assert!(!is_active_fast());
        }
    }

    #[test]
    fn server_must_be_https() {
        for bad in [
            "http://evil.example",
            "http://localhost.evil.example",
            "http://127.0.0.1.evil.example",
            "http://127.0.0.2:8200",
            "ftp://license.example.com",
            "https://user:pw@license.example.com",
            "license.example.com",
            "",
        ] {
            assert!(validate_base(bad).is_err(), "{bad} 不应被接受");
        }
        assert_eq!(
            validate_base("https://license.example.com/").unwrap(),
            "https://license.example.com"
        );
        assert!(validate_base("http://127.0.0.1:8200").is_ok());
        assert!(validate_base("http://LOCALHOST:8200").is_ok());
    }

    #[test]
    fn jitter_in_range() {
        for _ in 0..100 {
            let j = jitter_secs();
            assert!((180..=540).contains(&j));
        }
    }

    /// guard 的主要开销是金丝雀两次验签 + 凭证一次验签。阈值只在 release 下断言：
    /// debug 构建里 ed25519 未优化，数字没有参考意义。
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

    /// 完整路径：金丝雀 2 次 + 读盘 + 校验码 + 凭证 1 次验签 + 按需写回，与 guard() 一致。
    #[test]
    fn full_check_path_is_fast() {
        use dossier_cardkey::{issue_token, public_key_b64, signing_key, Plan};
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../target/license-bench")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        let sk = signing_key([3u8; 32]);
        let keys = parse_key_list(&[public_key_b64(&sk).as_str()]).unwrap();
        let mid = "ab".repeat(32);
        let now = 1_800_000_000;
        let state = State::new(dir.join("license.json"), Ok(mid.clone()));
        let tok = issue_token(
            &sk,
            1,
            &mid,
            Plan::Year,
            Some(now + 9_000_000),
            now,
            now + 604_800,
        );
        assert!(state.accept(tok, now, Ok(&keys), true).unwrap().is_active());
        const N: u64 = 2_000;
        let t = std::time::Instant::now();
        for i in 0..N {
            let (v, _) = state.evaluate(now + i, Ok(&keys), canary_ok());
            assert!(v.is_active());
        }
        let per = t.elapsed() / N as u32;
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
    /// DD_E2E_CARDS=卡1,卡2 DD_E2E_DIR=<目录> DD_E2E_REVOKED_FLAG=<标记文件> \
    /// cargo test -p drama-dossier license::tests::e2e -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn e2e_against_local_server() {
        let cards: Vec<String> = std::env::var("DD_E2E_CARDS")
            .expect("需要 DD_E2E_CARDS")
            .split(',')
            .map(str::to_string)
            .collect();
        let dir = std::path::PathBuf::from(std::env::var("DD_E2E_DIR").expect("需要 DD_E2E_DIR"));
        let _ = std::fs::remove_dir_all(&dir);
        // 不依赖本机真实机器码，用固定值，便于在后台按前缀查找
        let mid = "e2e0".repeat(16);
        let path = dir.join("license.json");
        assert!(CTX
            .set(Ctx {
                state: State::new(path.clone(), Ok(mid.clone())),
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
        .unwrap_or_else(|e| panic!("激活失败：{}", e.message()));
        let v = accept_token(ctx().unwrap(), token).expect("本地验签应通过");
        assert!(v.is_active(), "{v:?}");
        assert!(guard().is_ok(), "激活后 guard 应放行");
        assert!(is_active_fast(), "激活后闸门应放行");
        eprintln!("激活成功：{v:?}");

        // 4. 授权文件被篡改：guard 立刻锁定；改回原样后自动恢复（不会一次失败锁死整次运行）
        let original = std::fs::read_to_string(&path).unwrap();
        let mut stored: serde_json::Value = serde_json::from_str(&original).unwrap();
        stored["last_seen"] = serde_json::json!(0);
        std::fs::write(&path, stored.to_string()).unwrap();
        assert!(guard().is_err(), "篡改后的授权文件必须被拒");
        assert!(!is_active_fast(), "篡改被发现后闸门也要锁定");
        std::fs::write(&path, &original).unwrap();
        assert!(guard().is_ok(), "恢复原文件后应自动放行");

        // 5. 续签：走 refresh_blocking
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
            .unwrap_or_else(|e| panic!("第二张卡激活失败：{}", e.message()));
            let v2 = accept_token(ctx().unwrap(), token).unwrap();
            eprintln!("叠加后：{v2:?}");
        }

        // 8. 后台作废（测试脚本在另一个进程里调用管理接口并创建标记文件）后：
        //    续签被拒并锁定；模拟重启（新的 State 读同一个文件）仍然锁定
        if let Ok(flag) = std::env::var("DD_E2E_REVOKED_FLAG") {
            for _ in 0..120 {
                if std::path::Path::new(&flag).exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            assert!(std::path::Path::new(&flag).exists(), "等不到作废标记文件");
            let e = refresh_blocking().expect_err("作废后续签必须失败");
            eprintln!("作废后续签：{e}");
            assert!(guard().is_err(), "作废后 guard 必须锁定");
            assert!(!is_active_fast(), "作废后闸门必须锁定");
            let rebooted = State::new(path.clone(), Ok(mid.clone()));
            let (v, _) = rebooted.evaluate(now_secs(), trusted_keys(), canary_ok());
            assert_eq!(
                v,
                Verdict::Locked(LockReason::Revoked),
                "重启后必须仍然锁定"
            );
            eprintln!("重启后仍锁定：{v:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
