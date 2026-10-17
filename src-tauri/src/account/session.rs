//! 会话：登录状态、刷新令牌的单飞、授权凭证、三档判定。
//!
//! 刷新令牌只能用一次，并发刷新会被服务端当成重放，整个会话都会被吊销。所以所有刷新都排队
//! 经过同一把异步锁 [`Session::tokens`]：拿到锁后先看别人是不是已经刷新过了，新令牌写进钥匙串之后才放锁。
//!
//! 两把锁：`tokens`（tokio 异步锁，只在刷新时跨 await 持有）和 `state`（std 锁，绝不跨 await 持有）。
//! 拿锁顺序固定：要两把都拿时先 `tokens` 后 `state`。

use std::sync::{Mutex, MutexGuard};

use dossier_license::access::{decide, Connectivity, LockReason, ServerEntitlement, Tier};
use dossier_license::clock;
use dossier_license::token::{self, EntitlementKind, License};
use dossier_license::{MachineIdentity, TrustedKey};
use reqwest::Method;
use serde::Serialize;
use serde_json::{json, Value};

use dossier_license::recipe::{self, Recipe};

use super::api::{
    decode, ApiError, AuthResponse, DeviceList, LicenseResponse, PackageResponse, RecipeResponse,
    RedeemResponse, RefreshResponse, Request, Transport,
};
use super::store::{self, CredentialStore, LicenseRecord, SessionRecord, RECORD_VERSION};

/// 访问令牌提前这么多秒视为过期，免得请求发出去的路上过期。
const ACCESS_SKEW_SECS: u64 = 30;
/// 付费步骤前的联网刷新：这么多秒内刚刷新过就不再请求。
const PAID_FRESH_SECS: u64 = 60;
/// 授权凭证的刷新间隔限制在 1 分钟到 1 小时之间。
const REFRESH_MIN_SECS: u64 = 60;
const REFRESH_MAX_SECS: u64 = 3600;
/// 连不上服务端时的重试间隔：30 秒起，每次翻倍，最长 10 分钟。
const RETRY_MIN_SECS: u64 = 30;
const RETRY_MAX_SECS: u64 = 600;
/// 后台循环最长隔这么久醒一次：离线宽限期到点时界面要及时变成锁定。
const LOOP_MAX_SLEEP_SECS: u64 = 300;

/// 启动时就确定的东西：机器码和受信公钥。任何一项拿不到，都锁定为「安装包配置有误」。
#[derive(Debug, Clone)]
pub struct Setup {
    pub identity: MachineIdentity,
    pub keys: Vec<TrustedKey>,
    pub app_version: String,
}

#[derive(Default)]
struct TokenSlot {
    access: Option<(String, u64)>,
    refresh: Option<String>,
}

#[derive(Debug, Clone)]
struct User {
    id: String,
    email: String,
}

#[derive(Debug, Clone, Copy)]
struct OnlineSnapshot {
    entitlement: ServerEntitlement,
    /// 最近一次联网刷新成功的时间（本机时间，只用来判断「刚刷新过」）。
    at: u64,
}

#[derive(Default)]
struct State {
    /// 系统钥匙串自检失败：锁定，界面提供「重试」。
    storage_unavailable: Option<String>,
    /// 登录后写钥匙串失败：不锁定，只提示（这次运行还能用，下次启动要重新登录）。
    storage_warning: Option<String>,
    /// 服务端签发的凭证在本机验不过（多半是安装包里的公钥和服务端不一致）。
    config_error: Option<String>,
    user: Option<User>,
    license: Option<LicenseRecord>,
    /// 钥匙串里存的最晚时间，用来判断要不要写回。
    persisted_high_water: u64,
    online: Option<OnlineSnapshot>,
    /// 名额满了：服务端返回的设备列表和解绑次数，等用户先解绑一台。
    pending_activation: Option<Value>,
    /// 最近一次失败的说明（给界面显示），成功后清空。
    last_error: Option<String>,
    /// 被动退出登录的原因（会话失效、被解绑），登录页据此提示。
    signed_out_reason: Option<String>,
    next_sync_at: u64,
    retry_secs: u64,
    restored: bool,
}

/// 一次判定的结果。
#[derive(Debug, Clone)]
pub struct Evaluation {
    pub tier: Tier,
    pub renew_soon: bool,
    pub message: Option<String>,
}

/// `account_status` 的回包，也是 `account-changed` 事件的负载。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusPayload {
    pub logged_in: bool,
    pub email: Option<String>,
    /// `online` / `offline` / `locked`。
    pub tier: &'static str,
    pub lock_reason: Option<&'static str>,
    /// 锁定时给用户看的说明。
    pub message: Option<String>,
    /// 离线档的截止时间（Unix 秒）和剩余秒数。
    pub offline_until: Option<u64>,
    pub offline_remaining_secs: Option<u64>,
    pub renew_soon: bool,
    /// 使用权：`none` / `timed` / `lifetime`；到期时间是 Unix 秒。
    pub entitlement_kind: Option<&'static str>,
    pub entitlement_until: Option<u64>,
    /// 本机机器码（4 位一组）。
    pub device_code: Option<String>,
    /// 名额满了时服务端给的设备列表（`devices`、`device_limit`、`unbind_quota`）。
    pub pending_activation: Option<Value>,
    pub storage_warning: Option<String>,
    pub last_error: Option<String>,
    pub signed_out_reason: Option<String>,
    /// 启动时的恢复还没做完：界面先显示「正在恢复登录」，不要急着跳登录页。
    pub restoring: bool,
}

pub struct Session<T: Transport, S: CredentialStore> {
    api: T,
    store: S,
    setup: Result<Setup, String>,
    tokens: tokio::sync::Mutex<TokenSlot>,
    state: Mutex<State>,
    /// 当前 Unix 秒数。测试里换成可控的时钟。
    now: fn() -> u64,
    /// 登录、退出后叫醒后台循环，让它按新状态重新排时间。
    pub wake: tokio::sync::Notify,
}

fn unauthorized() -> ApiError {
    ApiError::Server {
        status: 401,
        code: "unauthorized".into(),
        message: "请先登录".into(),
        retry_after: None,
        extra: Value::Null,
    }
}

impl<T: Transport, S: CredentialStore> Session<T, S> {
    pub fn new(api: T, store: S, setup: Result<Setup, String>, now: fn() -> u64) -> Self {
        Self {
            api,
            store,
            setup,
            tokens: tokio::sync::Mutex::new(TokenSlot::default()),
            state: Mutex::new(State::default()),
            now,
            wake: tokio::sync::Notify::new(),
        }
    }

    /// std 锁中毒（持锁时 panic）也照样拿：里面只是普通数据，宁可继续用，也不让整个账号模块失灵。
    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn setup(&self) -> Option<&Setup> {
        self.setup.as_ref().ok()
    }

    /// 安装包配置有误时不登录、不写存储，直接报配置错误。
    fn require_setup(&self) -> Result<&Setup, ApiError> {
        self.setup.as_ref().map_err(|problem| ApiError::Server {
            status: 0,
            code: LockReason::ConfigError.code().into(),
            message: problem.clone(),
            retry_after: None,
            extra: Value::Null,
        })
    }

    pub fn is_logged_in(&self) -> bool {
        self.state().user.is_some()
    }

    // ---------- 启动恢复 ----------

    /// 启动时恢复登录：自检钥匙串 → 读本地记录 → 联网刷新（连不上就用本地凭证进入离线档）。
    pub async fn restore(&self) {
        if let Err(error) = self.store.self_test() {
            let mut state = self.state();
            state.storage_unavailable = Some(error.to_string());
            state.restored = true;
            return;
        }
        let session = store::load_record::<SessionRecord>(&self.store, store::SESSION);
        let license = store::load_record::<LicenseRecord>(&self.store, store::LICENSE);
        let (session, license) = match (session, license) {
            (Ok(session), Ok(license)) => (session, license),
            (Err(error), _) | (_, Err(error)) => {
                let mut state = self.state();
                state.storage_unavailable = Some(error.to_string());
                state.restored = true;
                return;
            }
        };
        let Some(session) = session else {
            // 没登录：残留的授权凭证没有意义，顺手删掉。
            let _ = self.store.delete(store::LICENSE);
            self.state().restored = true;
            return;
        };
        self.tokens.lock().await.refresh = Some(session.refresh_token);
        {
            let mut state = self.state();
            state.user = Some(User {
                id: session.user_id,
                email: session.email,
            });
            state.persisted_high_water = license.as_ref().map_or(0, |record| record.high_water);
            state.license = license;
        }
        self.sync().await;
        self.state().restored = true;
    }

    /// 钥匙串自检失败后，用户点「重试」。
    pub async fn retry_restore(&self) {
        self.state().storage_unavailable = None;
        self.restore().await;
    }

    // ---------- 令牌 ----------

    /// 拿一个可用的访问令牌，必要时用刷新令牌换一对新的。
    ///
    /// `stale` 是调用方刚被服务端拒绝（401）的令牌：如果锁里的令牌已经不是它，说明别人刚刷新过，直接用新的。
    async fn access_token(&self, stale: Option<&str>) -> Result<String, ApiError> {
        let mut slot = self.tokens.lock().await;
        let now = (self.now)();
        if let Some((token, expires_at)) = &slot.access {
            let replaced = stale.is_some_and(|stale| stale != token);
            let fresh = stale.is_none() && now + ACCESS_SKEW_SECS < *expires_at;
            if replaced || fresh {
                return Ok(token.clone());
            }
        }
        let refresh = slot.refresh.clone().ok_or_else(unauthorized)?;
        let response = self
            .api
            .send(
                Request::new(Method::POST, "/auth/refresh")
                    .json(json!({ "refresh_token": refresh })),
            )
            .await?;
        let tokens = decode::<RefreshResponse>(response)?.tokens;
        // 先写钥匙串再放锁：刷新令牌只能用一次，旧的已经作废了。
        self.persist_session(&tokens.refresh_token);
        slot.refresh = Some(tokens.refresh_token);
        slot.access = Some((tokens.access_token.clone(), now + tokens.expires_in));
        Ok(tokens.access_token)
    }

    fn persist_session(&self, refresh_token: &str) {
        let mut state = self.state();
        let Some(user) = state.user.clone() else {
            return;
        };
        let record = SessionRecord {
            v: RECORD_VERSION,
            refresh_token: refresh_token.to_owned(),
            user_id: user.id,
            email: user.email,
        };
        if let Err(error) = store::save_record(&self.store, store::SESSION, &record) {
            state.storage_warning =
                Some(format!("{error}。这次还能正常使用，下次启动需要重新登录"));
        }
    }

    /// 带访问令牌调用接口。401 时刷新一次再试；仍然 401 就退出登录。
    async fn authed(&self, request: Request) -> Result<Value, ApiError> {
        let result = self.authed_inner(request).await;
        if let Err(error) = &result {
            self.on_auth_error(error).await;
        }
        result
    }

    async fn authed_inner(&self, request: Request) -> Result<Value, ApiError> {
        let token = self.access_token(None).await?;
        match self.api.send(request.clone().bearer(&token)).await {
            Err(error) if error.is_unauthorized() => {
                let token = self.access_token(Some(&token)).await?;
                self.api.send(request.bearer(&token)).await
            }
            other => other,
        }
    }

    async fn on_auth_error(&self, error: &ApiError) {
        if error.is_unauthorized() {
            if self.is_logged_in() {
                self.clear_local(Some(LockReason::DeviceUnbound.message().into()))
                    .await;
            }
        } else if error.is_unreachable() {
            self.mark_offline();
        }
    }

    fn mark_offline(&self) {
        let now = (self.now)();
        let mut state = self.state();
        state.online = None;
        state.retry_secs = (state.retry_secs * 2).clamp(RETRY_MIN_SECS, RETRY_MAX_SECS);
        state.next_sync_at = now + state.retry_secs;
    }

    /// 清掉本机的登录状态：内存里的令牌、钥匙串里的两条记录。
    async fn clear_local(&self, reason: Option<String>) {
        *self.tokens.lock().await = TokenSlot::default();
        let failures: Vec<String> = [store::SESSION, store::LICENSE]
            .into_iter()
            .filter_map(|name| self.store.delete(name).err().map(|error| error.to_string()))
            .collect();
        let mut state = self.state();
        let storage_unavailable = state.storage_unavailable.take();
        *state = State {
            storage_unavailable,
            storage_warning: (!failures.is_empty()).then(|| failures.join("；")),
            signed_out_reason: reason,
            restored: true,
            ..State::default()
        };
        drop(state);
        self.wake.notify_one();
    }

    // ---------- 授权凭证 ----------

    /// 联网刷新授权凭证（顺带拿回最新的使用权）。这台电脑没绑定就先绑定。
    pub async fn sync(&self) {
        let Some(setup) = self.setup() else {
            return;
        };
        if !self.is_logged_in() {
            return;
        }
        let request = Request::new(Method::POST, "/license/refresh").json(json!({
            "device_id": setup.identity.device_id,
            "app_version": setup.app_version,
        }));
        match self.authed(request).await {
            Ok(value) => self.accept_license(value),
            Err(error) if error.code() == "device_not_bound" => self.activate().await,
            Err(error) => self.note_error(&error),
        }
    }

    /// 绑定这台电脑。名额满了就记下服务端给的设备列表，等用户先解绑一台。
    pub async fn activate(&self) {
        let Some(setup) = self.setup() else {
            return;
        };
        let identity = &setup.identity;
        let request = Request::new(Method::POST, "/license/activate").json(json!({
            "device_id": identity.device_id,
            "name": dossier_license::default_device_name(identity.platform, &identity.device_id),
            "platform": identity.platform.as_str(),
            "app_version": setup.app_version,
        }));
        match self.authed(request).await {
            Ok(value) => {
                self.state().pending_activation = None;
                self.accept_license(value);
            }
            Err(ApiError::Server { code, extra, .. }) if code == "device_limit" => {
                let now = (self.now)();
                let mut state = self.state();
                state.pending_activation = Some(extra);
                state.last_error = None;
                state.next_sync_at = now + REFRESH_MAX_SECS;
            }
            Err(error) => self.note_error(&error),
        }
    }

    /// 记下失败原因。断网的重试时间已经在 `mark_offline` 里排好；会话失效已经清掉了登录状态。
    fn note_error(&self, error: &ApiError) {
        if error.is_unauthorized() {
            return;
        }
        let now = (self.now)();
        let mut state = self.state();
        state.last_error = Some(error.message());
        if !error.is_unreachable() {
            state.next_sync_at = now + RETRY_MAX_SECS;
        }
    }

    /// 收下服务端签发的新凭证：先在本机验一遍，再写钥匙串。
    fn accept_license(&self, value: Value) {
        let response = match decode::<LicenseResponse>(value) {
            Ok(response) => response,
            Err(error) => return self.note_error(&error),
        };
        let Some(setup) = self.setup() else {
            return;
        };
        let now = (self.now)();
        let verified = token::verify(
            &response.license.token,
            &setup.keys,
            &setup.identity.device_id,
            now,
        );
        let license = match verified {
            Ok(license) => license,
            Err(error) => {
                let message = match error {
                    token::TokenError::Expired => {
                        "服务端刚签发的授权凭证在本机显示已过期，请检查系统时间是否正确".to_owned()
                    }
                    other => format!(
                        "服务端签发的授权凭证在本机验不过（{}），多半是安装包里的公钥和服务端不一致，请重新下载安装包",
                        other.code()
                    ),
                };
                let mut state = self.state();
                state.config_error = Some(message);
                state.next_sync_at = now + RETRY_MAX_SECS;
                return;
            }
        };
        let high_water = clock::reset_after_online(now, license.issued_at);
        let record = LicenseRecord {
            v: RECORD_VERSION,
            token: response.license.token,
            high_water,
        };
        let saved = store::save_record(&self.store, store::LICENSE, &record);
        let entitlement = server_entitlement(&response.entitlement);
        let refresh_after = parse_time(&response.license.refresh_after).unwrap_or(now);

        let mut state = self.state();
        match saved {
            Ok(()) => state.persisted_high_water = high_water,
            Err(error) => {
                state.storage_warning =
                    Some(format!("{error}。这次还能正常使用，断网后可能需要重新联网"))
            }
        }
        state.config_error = None;
        state.license = Some(record);
        state.online = Some(OnlineSnapshot {
            entitlement,
            at: now,
        });
        state.last_error = None;
        state.signed_out_reason = None;
        state.retry_secs = 0;
        // 服务端的 refresh_after 是服务端时钟；两边时钟有偏差也不会刷得太勤或太久。
        let wait = refresh_after
            .saturating_sub(now)
            .clamp(REFRESH_MIN_SECS, REFRESH_MAX_SECS);
        state.next_sync_at = now + wait;
    }

    // ---------- 三档判定 ----------

    /// 当前档位。每次都重新算：离线宽限期、月卡到期都是「时间一到就变」，不能缓存结论。
    pub fn evaluate(&self) -> Evaluation {
        let now = (self.now)();
        let mut state = self.state();
        let locked = |reason: LockReason, message: Option<String>| Evaluation {
            tier: Tier::Locked(reason),
            renew_soon: false,
            message: Some(message.unwrap_or_else(|| reason.message().to_owned())),
        };
        let setup = match &self.setup {
            Ok(setup) => setup,
            Err(problem) => return locked(LockReason::ConfigError, Some(problem.clone())),
        };
        if let Some(problem) = &state.storage_unavailable {
            return locked(LockReason::StorageUnavailable, Some(problem.clone()));
        }
        if let Some(problem) = &state.config_error {
            return locked(LockReason::ConfigError, Some(problem.clone()));
        }
        let Some(user) = &state.user else {
            return locked(LockReason::NotLoggedIn, state.signed_out_reason.clone());
        };
        if state.pending_activation.is_some() {
            return locked(LockReason::NeedsActivation, None);
        }
        let Some(record) = &state.license else {
            return locked(LockReason::LicenseInvalid, state.last_error.clone());
        };
        let verified = token::verify(&record.token, &setup.keys, &setup.identity.device_id, now)
            // 凭证必须属于当前账号：换账号登录后残留的旧凭证不算数。
            .and_then(|license| {
                if license.user_id == user.id {
                    Ok(license)
                } else {
                    Err(token::TokenError::BadPayload)
                }
            });
        let connectivity = match state.online {
            Some(snapshot) => Connectivity::Online(snapshot.entitlement),
            None => Connectivity::Offline,
        };
        let access = decide(
            verified.as_ref().map_err(|error| *error),
            record.high_water,
            now,
            connectivity,
        );

        // 联网成功会把最晚时间重置（可能往回调），离线时只会往前走；两种情况都以判定结果为准。
        if access.high_water != record.high_water {
            let mut updated = record.clone();
            updated.high_water = access.high_water;
            let moved = access.high_water.abs_diff(state.persisted_high_water);
            if moved >= clock::PERSIST_STEP_SECS {
                match store::save_record(&self.store, store::LICENSE, &updated) {
                    Ok(()) => state.persisted_high_water = updated.high_water,
                    Err(error) => eprintln!("[account] 保存最晚时间失败：{error}"),
                }
            }
            state.license = Some(updated);
        }
        let message = match access.tier {
            Tier::Locked(reason) => Some(reason.message().to_owned()),
            _ => None,
        };
        Evaluation {
            tier: access.tier,
            renew_soon: access.renew_soon,
            message,
        }
    }

    /// 退出程序前把最晚时间写回钥匙串（平时最多每分钟写一次）。
    pub fn flush(&self) {
        let state = self.state();
        if let Some(record) = &state.license {
            if record.high_water != state.persisted_high_water {
                let _ = store::save_record(&self.store, store::LICENSE, record);
            }
        }
    }

    /// 给前端的完整状态。
    pub fn status(&self) -> StatusPayload {
        let evaluation = self.evaluate();
        let now = (self.now)();
        let state = self.state();
        let setup = self.setup();
        let offline_license: Option<License> = match (&state.license, setup) {
            (Some(record), Some(setup)) => {
                token::verify(&record.token, &setup.keys, &setup.identity.device_id, now).ok()
            }
            _ => None,
        };
        let (entitlement_kind, entitlement_until) = match (&state.online, &offline_license) {
            (Some(snapshot), _) => (
                Some(snapshot.entitlement.kind.as_str()),
                snapshot.entitlement.expires_at,
            ),
            (None, Some(license)) => (
                Some(license.entitlement.as_str()),
                license.entitlement_until,
            ),
            (None, None) => (None, None),
        };
        let offline_until = match evaluation.tier {
            Tier::Offline { until } => Some(until),
            _ => None,
        };
        let lock_reason = match evaluation.tier {
            Tier::Locked(reason) => Some(reason.code()),
            _ => None,
        };
        StatusPayload {
            logged_in: state.user.is_some(),
            email: state.user.as_ref().map(|user| user.email.clone()),
            tier: evaluation.tier.code(),
            lock_reason,
            message: evaluation.message,
            offline_until,
            offline_remaining_secs: offline_until.map(|until| until.saturating_sub(now)),
            renew_soon: evaluation.renew_soon,
            entitlement_kind,
            entitlement_until,
            device_code: setup
                .map(|setup| dossier_license::display_code(&setup.identity.device_id)),
            pending_activation: state.pending_activation.clone(),
            storage_warning: state.storage_warning.clone(),
            last_error: state.last_error.clone(),
            signed_out_reason: state.signed_out_reason.clone(),
            restoring: !state.restored,
        }
    }

    /// 本地功能的闸门：联网或离线档都放行。
    pub fn check_local(&self) -> Result<(), Evaluation> {
        let evaluation = self.evaluate();
        if evaluation.tier.allows_local() {
            Ok(())
        } else {
            Err(evaluation)
        }
    }

    /// 付费步骤的闸门：执行前联网刷新一次（60 秒内刚刷新过就不再请求），必须是联网档。
    pub async fn check_paid(&self) -> Result<(), Evaluation> {
        let now = (self.now)();
        let fresh = self
            .state()
            .online
            .is_some_and(|snapshot| now.saturating_sub(snapshot.at) < PAID_FRESH_SECS);
        if !fresh {
            self.sync().await;
        }
        let evaluation = self.evaluate();
        if evaluation.tier.allows_paid() {
            Ok(())
        } else {
            Err(evaluation)
        }
    }

    // ---------- 账号操作 ----------

    pub async fn send_code(&self, email: &str, purpose: &str) -> Result<Value, ApiError> {
        self.require_setup()?;
        self.api
            .send(
                Request::new(Method::POST, "/auth/codes")
                    .json(json!({ "email": email, "purpose": purpose })),
            )
            .await
    }

    pub async fn register(
        &self,
        email: &str,
        code: &str,
        password: &str,
        accept_terms: bool,
    ) -> Result<(), ApiError> {
        self.require_setup()?;
        let value = self
            .api
            .send(Request::new(Method::POST, "/auth/register").json(json!({
                "email": email,
                "code": code,
                "password": password,
                "accept_terms": accept_terms,
            })))
            .await?;
        self.signed_in(decode(value)?).await
    }

    pub async fn login(&self, email: &str, password: &str) -> Result<(), ApiError> {
        self.require_setup()?;
        let value = self
            .api
            .send(
                Request::new(Method::POST, "/auth/login")
                    .json(json!({ "email": email, "password": password })),
            )
            .await?;
        self.signed_in(decode(value)?).await
    }

    /// 登录或注册成功：换掉本机的旧登录状态，写钥匙串，再绑定这台电脑。
    async fn signed_in(&self, auth: AuthResponse) -> Result<(), ApiError> {
        let now = (self.now)();
        {
            let mut slot = self.tokens.lock().await;
            slot.access = Some((auth.tokens.access_token, now + auth.tokens.expires_in));
            slot.refresh = Some(auth.tokens.refresh_token.clone());
        }
        // 换账号时旧凭证属于别人，不能留着。
        let _ = self.store.delete(store::LICENSE);
        {
            let mut state = self.state();
            let storage_unavailable = state.storage_unavailable.take();
            *state = State {
                storage_unavailable,
                user: Some(User {
                    id: auth.user.id,
                    email: auth.user.email,
                }),
                restored: true,
                ..State::default()
            };
        }
        self.persist_session(&auth.tokens.refresh_token);
        self.activate().await;
        self.wake.notify_one();
        Ok(())
    }

    /// 退出登录：通知服务端吊销会话（连不上也照样清掉本机状态），不归还设备名额。
    pub async fn logout(&self) {
        if self.is_logged_in() {
            if let Ok(token) = self.access_token(None).await {
                let _ = self
                    .api
                    .send(Request::new(Method::POST, "/auth/logout").bearer(&token))
                    .await;
            }
        }
        self.clear_local(None).await;
    }

    pub async fn reset_password(
        &self,
        email: &str,
        code: &str,
        new_password: &str,
    ) -> Result<(), ApiError> {
        self.require_setup()?;
        self.api
            .send(
                Request::new(Method::POST, "/auth/password/reset").json(json!({
                    "email": email,
                    "code": code,
                    "new_password": new_password,
                })),
            )
            .await
            .map(|_| ())
    }

    pub async fn change_password(&self, current: &str, new_password: &str) -> Result<(), ApiError> {
        self.authed(
            Request::new(Method::POST, "/auth/password/change").json(json!({
                "current_password": current,
                "new_password": new_password,
            })),
        )
        .await
        .map(|_| ())
    }

    /// 兑换卡密，成功后立即刷新授权，让新使用权生效。
    pub async fn redeem(&self, code: &str) -> Result<RedeemResponse, ApiError> {
        let value = self
            .authed(Request::new(Method::POST, "/cards/redeem").json(json!({ "code": code })))
            .await?;
        let response = decode::<RedeemResponse>(value)?;
        self.sync().await;
        Ok(response)
    }

    pub async fn list_devices(&self) -> Result<DeviceList, ApiError> {
        let mut path = "/devices".to_owned();
        if let Some(setup) = self.setup() {
            // 机器码是 64 位十六进制，直接拼进查询参数是安全的。
            path.push_str(&format!("?current_device={}", setup.identity.device_id));
        }
        decode(self.authed(Request::new(Method::GET, path)).await?)
    }

    /// 解绑一台电脑。解绑的如果是本机，接下来的刷新会 401，按「已被解绑」退出登录；
    /// 如果是在等名额（名额满了），接下来的刷新会发现没绑定，自动重新绑定本机。
    pub async fn unbind(&self, binding_id: &str) -> Result<(), ApiError> {
        let valid = binding_id.len() == 36
            && binding_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-');
        if !valid {
            return Err(ApiError::Protocol("设备 ID 格式不正确".into()));
        }
        self.authed(Request::new(
            Method::DELETE,
            format!("/devices/{binding_id}"),
        ))
        .await?;
        self.sync().await;
        Ok(())
    }

    pub async fn export(&self) -> Result<Value, ApiError> {
        self.authed(Request::new(Method::GET, "/me/export")).await
    }

    /// 注销账号：服务端立即匿名化，成功后清掉本机全部登录状态。
    pub async fn delete_account(&self, password: &str) -> Result<(), ApiError> {
        self.authed(Request::new(Method::POST, "/me/delete").json(json!({ "password": password })))
            .await?;
        self.clear_local(Some("账号已注销".into())).await;
        Ok(())
    }

    // ---------- 报审包与处理配方 ----------

    /// 本机机器码和受信公钥：组装报审包、校验配方都要用。安装包配置有误时报错。
    pub fn signing_context(&self) -> Result<(String, Vec<TrustedKey>), ApiError> {
        let setup = self.require_setup()?;
        Ok((setup.identity.device_id.clone(), setup.keys.clone()))
    }

    /// 让服务端生成报审包文档。`body` 由调用方按接口格式拼好（含本机机器码）。
    pub async fn create_package(&self, body: Value) -> Result<PackageResponse, ApiError> {
        decode(
            self.authed(Request::new(Method::POST, "/packages").json(body))
                .await?,
        )
    }

    /// 申请处理配方，拿到后立刻在本机验一遍（签名、机器码、任务、有效期），验不过不交给调用方。
    /// 返回验过的配方和签名原文：原文要存进任务目录，续跑时用 `verify_consumed` 重新验。
    pub async fn issue_recipe(
        &self,
        task: &str,
        platform: &str,
        strength: &str,
    ) -> Result<(Recipe, String), ApiError> {
        let (device_id, keys) = self.signing_context()?;
        let body = json!({
            "device_id": device_id,
            "task": task,
            "platform": platform,
            "strength": strength,
        });
        let response: RecipeResponse = decode(
            self.authed(Request::new(Method::POST, "/recipes").json(body))
                .await?,
        )?;
        let recipe = recipe::verify(&response.token, &keys, &device_id, task, (self.now)())
            .map_err(|error| ApiError::Server {
                status: 0,
                code: format!("recipe_{}", error.code()),
                message: error.to_string(),
                retry_after: None,
                extra: Value::Null,
            })?;
        if recipe.recipe_id != response.recipe_id {
            return Err(ApiError::Protocol("配方编号和签名内容不一致".into()));
        }
        Ok((recipe, response.token))
    }

    /// 开工前消费配方：同一个配方只能成功一次（服务端记账）。
    pub async fn consume_recipe(&self, recipe: &Recipe) -> Result<(), ApiError> {
        let (device_id, _) = self.signing_context()?;
        // 配方编号来自验过签的载荷；仍按 UUID 的字符集检查一遍再拼进路径。
        let valid = recipe.recipe_id.len() == 36
            && recipe
                .recipe_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-');
        if !valid {
            return Err(ApiError::Protocol("配方编号格式不正确".into()));
        }
        self.authed(
            Request::new(
                Method::POST,
                format!("/recipes/{}/consume", recipe.recipe_id),
            )
            .json(json!({ "device_id": device_id })),
        )
        .await?;
        Ok(())
    }

    // ---------- 后台刷新 ----------

    /// 距离下一次该做事还有多少秒（后台循环用）。最长 5 分钟醒一次，让宽限期到点时界面及时变化。
    pub fn sleep_secs(&self) -> u64 {
        let now = (self.now)();
        let state = self.state();
        if state.user.is_none() {
            return LOOP_MAX_SLEEP_SECS;
        }
        state
            .next_sync_at
            .saturating_sub(now)
            .clamp(1, LOOP_MAX_SLEEP_SECS)
    }

    /// 到了该联网刷新的时间。
    pub fn sync_due(&self) -> bool {
        let now = (self.now)();
        let state = self.state();
        state.user.is_some() && now >= state.next_sync_at
    }
}

/// 服务端的使用权视图转成判定用的结构。认不出的类型按「没有使用权」处理，只收紧不放宽。
fn server_entitlement(view: &super::api::EntitlementView) -> ServerEntitlement {
    ServerEntitlement {
        kind: EntitlementKind::parse(&view.kind).unwrap_or(EntitlementKind::None),
        active: view.active,
        expires_at: view.expires_at.as_deref().and_then(parse_time),
    }
}

/// ISO 8601（带时区）转 Unix 秒。
fn parse_time(text: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .and_then(|time| u64::try_from(time.timestamp()).ok())
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
