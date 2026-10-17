//! 会话测试：假服务端 + 内存钥匙串 + 可控时钟。
//!
//! 假服务端按真实服务端的规则行事：刷新令牌只能用一次，旧的刷新令牌再用一次就吊销整个会话
//! （`COMMERCE.md` 3.1 节）；授权凭证用测试私钥真签，桌面端照常验签。
//! 所以「并发刷新没排队」这种错误在这里会直接表现为会话被吊销、请求失败。

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use dossier_license::keys::key_id;
use dossier_license::{derive_device_id, MachineIdentity, Platform};
use ed25519_dalek::{Signer, SigningKey};

use super::*;
use crate::account::store::StoreError;

// ---------- 可控时钟 ----------
// 测试跑在 tokio 单线程运行时上，同一个测试里的任务都在同一个线程，线程局部变量就够用。

const T0: u64 = 1_767_225_600;

thread_local! {
    static NOW: Cell<u64> = const { Cell::new(T0) };
}

fn now() -> u64 {
    NOW.with(Cell::get)
}

fn set_now(value: u64) {
    NOW.with(|cell| cell.set(value));
}

fn advance(secs: u64) {
    set_now(now() + secs);
}

// ---------- 内存钥匙串 ----------

#[derive(Clone, Default)]
struct MemStore {
    inner: Arc<Mutex<MemInner>>,
}

#[derive(Default)]
struct MemInner {
    records: HashMap<String, String>,
    /// 自检读回不同的值：模拟 keyring 退回到只存在内存里的假实现。
    broken: bool,
    /// 写入失败：模拟钥匙串被锁住。
    fail_writes: bool,
}

impl MemStore {
    fn get(&self, name: &str) -> Option<String> {
        self.inner.lock().unwrap().records.get(name).cloned()
    }
}

impl CredentialStore for MemStore {
    fn load(&self, name: &str) -> Result<Option<String>, StoreError> {
        let inner = self.inner.lock().unwrap();
        if inner.broken && name == "self-test" {
            return Ok(Some("别的值".into()));
        }
        Ok(inner.records.get(name).cloned())
    }

    fn save(&self, name: &str, value: &str) -> Result<(), StoreError> {
        let mut inner = self.inner.lock().unwrap();
        if inner.fail_writes {
            return Err(StoreError("钥匙串被锁住了".into()));
        }
        inner.records.insert(name.to_owned(), value.to_owned());
        Ok(())
    }

    fn delete(&self, name: &str) -> Result<(), StoreError> {
        self.inner.lock().unwrap().records.remove(name);
        Ok(())
    }
}
// ---------- 假服务端 ----------

const EMAIL: &str = "user@example.com";
const PASSWORD: &str = "correct horse";
const USER_ID: &str = "00000000-0000-4000-8000-000000000001";
const LICENSE_TTL: u64 = 72 * 3600;
const ACCESS_TTL: u64 = 900;

fn server_key() -> SigningKey {
    // 不是测试向量的公开种子，也不是任何真实服务端的私钥。
    SigningKey::from_bytes(&[42u8; 32])
}

fn device_id() -> String {
    derive_device_id(Platform::Linux, "3f2a91c07b44e1d8a5b6c7d8e9f00112")
}

const OTHER_DEVICE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const OTHER_BINDING: &str = "11111111-2222-3333-4444-555555555555";
const OWN_BINDING: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

struct Server {
    offline: bool,
    revoked: bool,
    /// 当前有效的访问令牌和刷新令牌；用过的刷新令牌。
    access: Option<String>,
    refresh: Option<String>,
    used_refresh: HashSet<String>,
    serial: u32,
    /// 下一次带令牌的请求返回 401（模拟访问令牌在服务端提前失效）。
    reject_next_access: bool,
    /// 绑定：binding id → device id。
    bindings: Vec<(String, String)>,
    device_limit: usize,
    unbind_remaining: u32,
    entitlement: (&'static str, Option<u64>),
    calls: HashMap<String, u32>,
    /// 配方：签发时写进载荷的机器码（`None` = 请求里的机器码；设成别的模拟服务端发错了）。
    recipe_device: Option<String>,
    /// 已消费的配方编号（服务端记账，第二次消费 409）。
    consumed: HashSet<String>,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            offline: false,
            revoked: false,
            access: None,
            refresh: None,
            used_refresh: HashSet::new(),
            serial: 0,
            reject_next_access: false,
            bindings: Vec::new(),
            device_limit: 1,
            unbind_remaining: 3,
            entitlement: ("lifetime", None),
            calls: HashMap::new(),
            recipe_device: None,
            consumed: HashSet::new(),
        }
    }
}

#[derive(Clone, Default)]
struct FakeApi(Arc<Mutex<Server>>);

impl FakeApi {
    fn with(&self, edit: impl FnOnce(&mut Server)) {
        edit(&mut self.0.lock().unwrap());
    }

    fn calls(&self, path: &str) -> u32 {
        self.0.lock().unwrap().calls.get(path).copied().unwrap_or(0)
    }
}

fn error(status: u16, code: &str, extra: Value) -> ApiError {
    ApiError::Server {
        status,
        code: code.into(),
        message: format!("假服务端：{code}"),
        retry_after: None,
        extra,
    }
}

fn iso(secs: u64) -> String {
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .unwrap()
        .to_rfc3339()
}

impl Server {
    fn issue_tokens(&mut self) -> Value {
        self.serial += 1;
        let access = format!("at_{}", self.serial);
        let refresh = format!("rt_{}", self.serial);
        self.access = Some(access.clone());
        self.refresh = Some(refresh.clone());
        json!({ "access_token": access, "refresh_token": refresh, "token_type": "Bearer",
                "expires_in": ACCESS_TTL, "refresh_expires_in": 2_592_000 })
    }

    fn entitlement_view(&self) -> Value {
        let (kind, until) = self.entitlement;
        let active = match kind {
            "lifetime" => true,
            "timed" => until.is_some_and(|until| now() < until),
            _ => false,
        };
        json!({ "kind": kind, "active": active, "expires_at": until.map(iso) })
    }

    fn license_response(&self, device: &str) -> Value {
        let key = server_key();
        let iat = now();
        let (kind, until) = self.entitlement;
        let payload = json!({
            "v": 1, "kid": key_id(&key.verifying_key().to_bytes()), "sub": USER_ID, "dev": device,
            "ent": kind, "ent_until": if kind == "timed" { until } else { None },
            "iat": iat, "exp": iat + LICENSE_TTL,
        })
        .to_string();
        let mut message = b"drama-dossier/license/v1\n".to_vec();
        message.extend_from_slice(payload.as_bytes());
        let token = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(payload.as_bytes()),
            URL_SAFE_NO_PAD.encode(key.sign(&message).to_bytes())
        );
        json!({
            "license": { "token": token, "kid": "", "issued_at": iso(iat),
                         "expires_at": iso(iat + LICENSE_TTL), "refresh_after": iso(iat + 3600) },
            "device": { "id": OWN_BINDING, "current": true },
            "entitlement": self.entitlement_view(),
        })
    }

    fn device_list(&self) -> Value {
        let devices: Vec<Value> = self
            .bindings
            .iter()
            .map(|(id, device)| {
                json!({ "id": id, "code": "AAAA-BBBB-CCCC-DDDD", "name": "电脑", "platform": "linux",
                        "app_version": "0.2.1", "bound_at": iso(T0), "last_seen_at": iso(now()),
                        "current": *device == device_id() })
            })
            .collect();
        json!({ "devices": devices, "device_limit": self.device_limit,
                "unbind_quota": { "limit": 3, "window_days": 30, "remaining": self.unbind_remaining } })
    }

    /// 付费接口的前提：请求体里的机器码绑定在这个账号上。
    fn require_bound(&self, body: &Value) -> Result<(), ApiError> {
        let device = body["device_id"].as_str().unwrap_or("");
        if self.bindings.iter().any(|(_, bound)| bound == device) {
            Ok(())
        } else {
            Err(error(403, "device_not_bound", Value::Null))
        }
    }

    /// 和服务端 `RecipeService.issue` 一样：用途前缀 recipe，真签。
    fn recipe_response(&mut self, body: &Value) -> Value {
        self.serial += 1;
        let key = server_key();
        let iat = now();
        let rid = format!("00000000-0000-4000-8000-{:012}", self.serial);
        let device = self
            .recipe_device
            .clone()
            .unwrap_or_else(|| body["device_id"].as_str().unwrap_or("").to_owned());
        let payload = json!({
            "v": 1, "kid": key_id(&key.verifying_key().to_bytes()), "rid": rid, "sub": USER_ID,
            "dev": device, "task": body["task"], "platform": body["platform"], "strength": body["strength"],
            "seed": 7, "params": { "subtle": { "grain": 4 } }, "iat": iat, "exp": iat + 86_400,
        })
        .to_string();
        let mut message = b"drama-dossier/recipe/v1\n".to_vec();
        message.extend_from_slice(payload.as_bytes());
        let token = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(payload.as_bytes()),
            URL_SAFE_NO_PAD.encode(key.sign(&message).to_bytes())
        );
        json!({ "recipe_id": rid, "token": token, "expires_at": iat + 86_400, "params": {} })
    }

    fn check_bearer(&mut self, bearer: Option<&str>) -> Result<(), ApiError> {
        if self.revoked || bearer.is_none() || bearer != self.access.as_deref() {
            return Err(error(401, "unauthorized", Value::Null));
        }
        if std::mem::take(&mut self.reject_next_access) {
            // 这把访问令牌作废，逼客户端刷新。
            self.access = None;
            return Err(error(401, "unauthorized", Value::Null));
        }
        Ok(())
    }
}
impl Transport for FakeApi {
    async fn send(&self, request: Request) -> Result<Value, ApiError> {
        let route = format!(
            "{} {}",
            request.method,
            request.path.split('?').next().unwrap_or("")
        );
        {
            let mut server = self.0.lock().unwrap();
            *server.calls.entry(route.clone()).or_default() += 1;
            if server.offline {
                return Err(ApiError::Network("无法连接：假服务端已停".into()));
            }
        }
        // 让出一次：并发的请求在这里交错，排队有没有生效才看得出来。
        tokio::task::yield_now().await;
        let mut server = self.0.lock().unwrap();
        let body = request.body.clone().unwrap_or(Value::Null);
        let bearer = request.bearer.as_deref();
        match route.as_str() {
            "POST /auth/login" | "POST /auth/register" => {
                if body["password"] != PASSWORD {
                    return Err(error(401, "invalid_credentials", Value::Null));
                }
                server.revoked = false;
                server.used_refresh.clear();
                let tokens = server.issue_tokens();
                Ok(json!({ "user": { "id": USER_ID, "email": EMAIL }, "tokens": tokens }))
            }
            "POST /auth/refresh" => {
                let presented = body["refresh_token"].as_str().unwrap_or("").to_owned();
                if server.used_refresh.contains(&presented) {
                    // 重放：吊销整个会话（和真实服务端一样）。
                    server.revoked = true;
                    return Err(error(401, "unauthorized", Value::Null));
                }
                if server.revoked || Some(presented.as_str()) != server.refresh.as_deref() {
                    return Err(error(401, "unauthorized", Value::Null));
                }
                server.used_refresh.insert(presented);
                let tokens = server.issue_tokens();
                Ok(json!({ "tokens": tokens }))
            }
            "POST /auth/logout" => {
                server.check_bearer(bearer)?;
                server.revoked = true;
                Ok(Value::Null)
            }
            "POST /license/activate" => {
                server.check_bearer(bearer)?;
                let device = body["device_id"].as_str().unwrap_or("").to_owned();
                if !server.bindings.iter().any(|(_, bound)| *bound == device) {
                    if server.bindings.len() >= server.device_limit {
                        return Err(error(409, "device_limit", server.device_list()));
                    }
                    server.bindings.push((OWN_BINDING.into(), device.clone()));
                }
                Ok(server.license_response(&device))
            }
            "POST /license/refresh" => {
                server.check_bearer(bearer)?;
                let device = body["device_id"].as_str().unwrap_or("").to_owned();
                if !server.bindings.iter().any(|(_, bound)| *bound == device) {
                    return Err(error(403, "device_not_bound", Value::Null));
                }
                Ok(server.license_response(&device))
            }
            "GET /devices" => {
                server.check_bearer(bearer)?;
                Ok(server.device_list())
            }
            "POST /cards/redeem" => {
                server.check_bearer(bearer)?;
                server.entitlement = ("timed", Some(now() + 30 * 86_400));
                let entitlement = server.entitlement_view();
                Ok(
                    json!({ "entitlement": entitlement, "card": { "kind": "timed", "days": 30 },
                           "already_redeemed": false }),
                )
            }
            "GET /me/export" => {
                server.check_bearer(bearer)?;
                Ok(
                    json!({ "format": "drama-dossier/account-export/v1", "account": { "email": EMAIL } }),
                )
            }
            "POST /me/delete" => {
                server.check_bearer(bearer)?;
                if body["password"] != PASSWORD {
                    return Err(error(400, "wrong_password", Value::Null));
                }
                server.revoked = true;
                server.bindings.clear();
                Ok(Value::Null)
            }
            "POST /packages" => {
                server.check_bearer(bearer)?;
                server.require_bound(&body)?;
                // 会话层只关心传输和重试；清单的验签与组装在 dossier-core 的 assemble 测试里。
                Ok(
                    json!({ "package_id": "pkg-1", "manifest": "{}", "signature": "{}", "files": [] }),
                )
            }
            "POST /recipes" => {
                server.check_bearer(bearer)?;
                server.require_bound(&body)?;
                Ok(server.recipe_response(&body))
            }
            other if other.starts_with("POST /recipes/") && other.ends_with("/consume") => {
                server.check_bearer(bearer)?;
                server.require_bound(&body)?;
                let id = other
                    .trim_start_matches("POST /recipes/")
                    .trim_end_matches("/consume")
                    .to_owned();
                if !server.consumed.insert(id.clone()) {
                    return Err(error(409, "recipe_consumed", Value::Null));
                }
                Ok(json!({ "recipe_id": id, "consumed": true }))
            }
            other if other.starts_with("DELETE /devices/") => {
                server.check_bearer(bearer)?;
                if server.unbind_remaining == 0 {
                    return Err(error(
                        429,
                        "unbind_limit",
                        json!({ "unbind_quota": { "limit": 3, "window_days": 30, "remaining": 0 } }),
                    ));
                }
                let id = other.trim_start_matches("DELETE /devices/").to_owned();
                let Some(index) = server
                    .bindings
                    .iter()
                    .position(|(binding, _)| *binding == id)
                else {
                    return Err(error(404, "device_not_found", Value::Null));
                };
                let (_, device) = server.bindings.remove(index);
                server.unbind_remaining -= 1;
                // 解绑的是发起请求的这台电脑：它的会话随之吊销。
                if device == device_id() {
                    server.revoked = true;
                }
                Ok(Value::Null)
            }
            other => Err(error(404, "not_found", json!({ "route": other }))),
        }
    }
}

// ---------- 搭一个会话 ----------

type TestSession = Session<FakeApi, MemStore>;

fn setup() -> Setup {
    let key = server_key();
    Setup {
        identity: MachineIdentity {
            platform: Platform::Linux,
            device_id: device_id(),
        },
        keys: vec![TrustedKey::from_bytes(&key.verifying_key().to_bytes()).unwrap()],
        app_version: "0.2.1".into(),
    }
}

fn session_with(api: &FakeApi, store: &MemStore) -> TestSession {
    Session::new(api.clone(), store.clone(), Ok(setup()), now)
}

/// 新会话 + 登录成功（这台电脑绑定上、拿到凭证）。
async fn logged_in() -> (TestSession, FakeApi, MemStore) {
    set_now(T0);
    let api = FakeApi::default();
    let store = MemStore::default();
    let session = session_with(&api, &store);
    session.restore().await;
    session.login(EMAIL, PASSWORD).await.expect("登录成功");
    (session, api, store)
}
// ---------- 登录、刷新排队、401 ----------

#[tokio::test]
async fn login_binds_device_and_goes_online() {
    let (session, api, store) = logged_in().await;
    let status = session.status();
    assert!(status.logged_in);
    assert_eq!(status.tier, "online");
    assert_eq!(status.entitlement_kind, Some("lifetime"));
    assert_eq!(api.calls("POST /license/activate"), 1);
    // 两条记录都进了钥匙串；访问令牌不落盘。
    assert!(store.get(store::SESSION).unwrap().contains("rt_1"));
    assert!(store.get(store::LICENSE).is_some());
    assert!(!store.get(store::SESSION).unwrap().contains("at_"));
}

#[tokio::test]
async fn wrong_password_keeps_logged_out() {
    set_now(T0);
    let api = FakeApi::default();
    let store = MemStore::default();
    let session = session_with(&api, &store);
    session.restore().await;
    let error = session.login(EMAIL, "wrong").await.unwrap_err();
    assert_eq!(error.code(), "invalid_credentials");
    // 登录失败不是「会话失效」，不该写入被动退出的原因。
    let status = session.status();
    assert!(!status.logged_in);
    assert_eq!(status.lock_reason, Some("not_logged_in"));
    assert_eq!(status.signed_out_reason, None);
}

#[tokio::test(flavor = "current_thread")]
async fn concurrent_requests_refresh_only_once() {
    let (session, api, _store) = logged_in().await;
    let session = Arc::new(session);
    // 访问令牌过期：10 个请求同时发现要刷新。
    advance(ACCESS_TTL + 1);
    let local = tokio::task::LocalSet::new();
    let results = local
        .run_until(async {
            let handles: Vec<_> = (0..10)
                .map(|_| {
                    let session = Arc::clone(&session);
                    tokio::task::spawn_local(async move { session.list_devices().await })
                })
                .collect();
            let mut results = Vec::new();
            for handle in handles {
                results.push(handle.await.unwrap());
            }
            results
        })
        .await;
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    assert_eq!(
        api.calls("POST /auth/refresh"),
        1,
        "刷新必须排队，只刷新一次"
    );
    assert!(session.is_logged_in());
}

#[tokio::test]
async fn unauthorized_retries_once_after_refresh() {
    let (session, api, _store) = logged_in().await;
    api.with(|server| server.reject_next_access = true);
    let devices = session.list_devices().await.expect("刷新一次后重试成功");
    assert_eq!(devices.devices.len(), 1);
    assert_eq!(api.calls("POST /auth/refresh"), 1);
    assert_eq!(api.calls("GET /devices"), 2);
}

#[tokio::test]
async fn revoked_session_signs_out_and_clears_store() {
    let (session, api, store) = logged_in().await;
    // 在别处被解绑：服务端吊销了会话，刷新也会 401。
    api.with(|server| server.revoked = true);
    let error = session.list_devices().await.unwrap_err();
    assert!(error.is_unauthorized());
    let status = session.status();
    assert!(!status.logged_in);
    assert_eq!(status.lock_reason, Some("not_logged_in"));
    assert_eq!(
        status.signed_out_reason.as_deref(),
        Some(LockReason::DeviceUnbound.message())
    );
    assert_eq!(store.get(store::SESSION), None);
    assert_eq!(store.get(store::LICENSE), None);
}

// ---------- 报审包与处理配方 ----------

fn package_body() -> Value {
    json!({ "device_id": device_id(), "drama_name": "测试剧", "episodes": [] })
}

#[tokio::test]
async fn create_package_retries_once_after_401() {
    let (session, api, _store) = logged_in().await;
    api.with(|server| server.reject_next_access = true);
    let response = session
        .create_package(package_body())
        .await
        .expect("刷新一次后重试成功");
    assert_eq!(response.manifest, "{}");
    assert_eq!(api.calls("POST /auth/refresh"), 1);
    assert_eq!(api.calls("POST /packages"), 2);
    assert!(session.is_logged_in());
}

#[tokio::test]
async fn create_package_offline_reports_network_and_goes_offline() {
    let (session, api, _store) = logged_in().await;
    api.with(|server| server.offline = true);
    let error = session.create_package(package_body()).await.unwrap_err();
    assert!(error.is_unreachable(), "{error:?}");
    assert_eq!(error.code(), "network");
    // 断网不是被踢下线：还登录着，只是转成离线档。
    assert!(session.is_logged_in());
    assert_eq!(session.status().tier, "offline");
}

#[tokio::test]
async fn create_package_passes_server_errors_through() {
    let (session, _api, _store) = logged_in().await;
    let mut body = package_body();
    body["device_id"] = json!(OTHER_DEVICE);
    let error = session.create_package(body).await.unwrap_err();
    assert_eq!(error.code(), "device_not_bound");
    assert!(session.is_logged_in(), "403 不应清掉登录状态");
}

const TASK: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

#[tokio::test]
async fn recipe_is_verified_locally_then_consumed_once() {
    let (session, api, _store) = logged_in().await;
    let (recipe, token) = session
        .issue_recipe(TASK, "universal", "medium")
        .await
        .expect("签发并本地验签");
    // 原文留给续跑：本机重新验签（不看有效期）得到同一个配方。
    let (device, keys) = session.signing_context().unwrap();
    let again = dossier_license::version_task::verify_consumed(&token, &keys, &device, TASK)
        .expect("原文可续跑验签");
    assert_eq!(again, recipe);
    assert_eq!(recipe.device_id, device_id());
    assert_eq!(recipe.task, TASK);
    assert_eq!(recipe.platform, "universal");
    session.consume_recipe(&recipe).await.expect("第一次消费");
    let error = session.consume_recipe(&recipe).await.unwrap_err();
    assert_eq!(error.code(), "recipe_consumed");
    assert_eq!(api.calls("POST /recipes"), 1);
}

#[tokio::test]
async fn recipe_for_another_computer_is_rejected_locally() {
    let (session, api, _store) = logged_in().await;
    // 服务端（或者中间人）发回的配方签名没问题，但不是给这台电脑的。
    api.with(|server| server.recipe_device = Some(OTHER_DEVICE.into()));
    let error = session
        .issue_recipe(TASK, "universal", "medium")
        .await
        .unwrap_err();
    assert_eq!(error.code(), "recipe_wrong_device");
    assert!(session.is_logged_in());
}

// ---------- 断网、时钟 ----------

#[tokio::test]
async fn unreachable_server_falls_back_to_offline() {
    let (session, api, _store) = logged_in().await;
    api.with(|server| server.offline = true);
    advance(3600);
    session.sync().await;
    let status = session.status();
    assert_eq!(status.tier, "offline");
    // 永久授权：离线截止到凭证 exp（签发后 72 小时）。
    assert_eq!(status.offline_until, Some(T0 + LICENSE_TTL));
    assert_eq!(status.offline_remaining_secs, Some(LICENSE_TTL - 3600));
    assert!(session.check_local().is_ok());
    // 付费步骤必须联网。
    assert!(session.check_paid().await.is_err());
    // 宽限期到点：锁定。
    set_now(T0 + LICENSE_TTL);
    assert_eq!(session.status().lock_reason, Some("grace_expired"));
}

#[tokio::test]
async fn offline_clock_rollback_locks() {
    let (session, api, _store) = logged_in().await;
    api.with(|server| server.offline = true);
    advance(3600);
    session.sync().await;
    assert_eq!(session.status().tier, "offline");
    // 往回调 10 分钟以上：判定时钟倒退。
    set_now(T0 + 3600 - clock::ROLLBACK_TOLERANCE_SECS - 1);
    assert_eq!(session.status().lock_reason, Some("clock_rollback"));
    // 联网一次就恢复。
    api.with(|server| server.offline = false);
    session.sync().await;
    assert_eq!(session.status().tier, "online");
}

#[tokio::test]
async fn restart_restores_from_store() {
    let (_first, api, store) = logged_in().await;
    advance(120);
    // 模拟重启：新会话，同一个钥匙串，同一个服务端。
    let second = session_with(&api, &store);
    assert!(second.status().restoring);
    second.restore().await;
    let status = second.status();
    assert!(!status.restoring);
    assert_eq!(status.tier, "online");
    assert_eq!(status.email.as_deref(), Some(EMAIL));
    // 用掉了 rt_1，钥匙串里换成了新的刷新令牌。
    assert!(store.get(store::SESSION).unwrap().contains("rt_2"));
}

#[tokio::test]
async fn restart_offline_uses_stored_license() {
    let (_first, api, store) = logged_in().await;
    api.with(|server| server.offline = true);
    advance(600);
    let second = session_with(&api, &store);
    second.restore().await;
    assert_eq!(second.status().tier, "offline");
}

#[tokio::test]
async fn broken_keychain_locks_with_storage_unavailable() {
    set_now(T0);
    let api = FakeApi::default();
    let store = MemStore::default();
    store.inner.lock().unwrap().broken = true;
    let session = session_with(&api, &store);
    session.restore().await;
    assert_eq!(session.status().lock_reason, Some("storage_unavailable"));
    // 修好以后点「重试」恢复。
    store.inner.lock().unwrap().broken = false;
    session.retry_restore().await;
    assert_eq!(session.status().lock_reason, Some("not_logged_in"));
}

#[tokio::test]
async fn failed_keychain_write_warns_without_locking() {
    let (session, _api, store) = logged_in().await;
    store.inner.lock().unwrap().fail_writes = true;
    advance(ACCESS_TTL + 1);
    session.list_devices().await.unwrap();
    let status = session.status();
    assert_eq!(status.tier, "online");
    assert!(status.storage_warning.is_some());
}
// ---------- 名额满、解绑 ----------

/// 账号已经绑了另一台电脑，名额是 1。
async fn logged_in_with_full_quota() -> (TestSession, FakeApi, MemStore) {
    set_now(T0);
    let api = FakeApi::default();
    api.with(|server| {
        server
            .bindings
            .push((OTHER_BINDING.into(), OTHER_DEVICE.into()))
    });
    let store = MemStore::default();
    let session = session_with(&api, &store);
    session.restore().await;
    session.login(EMAIL, PASSWORD).await.expect("登录本身成功");
    (session, api, store)
}

#[tokio::test]
async fn device_limit_then_unbind_rebinds_this_device() {
    let (session, api, _store) = logged_in_with_full_quota().await;
    let status = session.status();
    assert_eq!(status.lock_reason, Some("needs_activation"));
    let pending = status.pending_activation.expect("带上了设备列表");
    assert_eq!(pending["devices"].as_array().unwrap().len(), 1);
    assert_eq!(pending["unbind_quota"]["remaining"], 3);
    // 本地功能也锁着。
    assert!(session.check_local().is_err());

    // 解绑另一台：刷新时发现本机没绑定，自动重新绑定。
    session.unbind(OTHER_BINDING).await.expect("解绑成功");
    let status = session.status();
    assert_eq!(status.tier, "online");
    assert!(status.pending_activation.is_none());
    assert_eq!(api.calls("POST /license/activate"), 2);
}

#[tokio::test]
async fn unbinding_this_device_signs_out() {
    let (session, _api, store) = logged_in().await;
    // 解绑请求本身成功；紧接着的刷新拿到 401，按「已被解绑」退出登录。
    session.unbind(OWN_BINDING).await.expect("解绑请求成功");
    let status = session.status();
    assert!(!status.logged_in);
    assert_eq!(store.get(store::SESSION), None);
}

#[tokio::test]
async fn unbind_limit_reports_quota() {
    let (session, api, _store) = logged_in_with_full_quota().await;
    api.with(|server| server.unbind_remaining = 0);
    let error = session.unbind(OTHER_BINDING).await.unwrap_err();
    assert_eq!(error.code(), "unbind_limit");
    let ApiError::Server { extra, .. } = error else {
        panic!()
    };
    assert_eq!(extra["unbind_quota"]["remaining"], 0);
    // 还登录着，仍然在等名额。
    assert_eq!(session.status().lock_reason, Some("needs_activation"));
}

#[tokio::test]
async fn malformed_binding_id_is_rejected_locally() {
    let (session, api, _store) = logged_in().await;
    let error = session.unbind("../me/delete").await.unwrap_err();
    assert_eq!(error.code(), "protocol");
    assert_eq!(api.calls("DELETE /devices/../me/delete"), 0);
}

// ---------- 兑换、注销、退出 ----------

#[tokio::test]
async fn redeem_unlocks_without_entitlement() {
    set_now(T0);
    let api = FakeApi::default();
    api.with(|server| server.entitlement = ("none", None));
    let store = MemStore::default();
    let session = session_with(&api, &store);
    session.restore().await;
    session.login(EMAIL, PASSWORD).await.unwrap();
    assert_eq!(session.status().lock_reason, Some("no_entitlement"));
    assert!(session.check_local().is_err());

    session.redeem("DD-2345-6789-ABCD-EFGH").await.unwrap();
    let status = session.status();
    assert_eq!(status.tier, "online");
    assert_eq!(status.entitlement_kind, Some("timed"));
    assert_eq!(status.entitlement_until, Some(T0 + 30 * 86_400));
    assert!(session.check_local().is_ok());
}

#[tokio::test]
async fn delete_account_clears_both_records() {
    let (session, _api, store) = logged_in().await;
    // 密码错：什么都不动。
    let error = session.delete_account("wrong").await.unwrap_err();
    assert_eq!(error.code(), "wrong_password");
    assert!(session.is_logged_in());
    assert!(store.get(store::SESSION).is_some());

    session.delete_account(PASSWORD).await.unwrap();
    let status = session.status();
    assert!(!status.logged_in);
    assert_eq!(status.signed_out_reason.as_deref(), Some("账号已注销"));
    assert_eq!(store.get(store::SESSION), None);
    assert_eq!(store.get(store::LICENSE), None);
}

#[tokio::test]
async fn logout_clears_store_even_when_offline() {
    let (session, api, store) = logged_in().await;
    api.with(|server| server.offline = true);
    session.logout().await;
    let status = session.status();
    assert!(!status.logged_in);
    // 主动退出不是「被动退出」，登录页不显示原因。
    assert_eq!(status.signed_out_reason, None);
    assert_eq!(store.get(store::SESSION), None);
    assert_eq!(store.get(store::LICENSE), None);
}

// ---------- 闸门三档 ----------

#[tokio::test]
async fn gate_tiers() {
    set_now(T0);
    let api = FakeApi::default();
    let store = MemStore::default();
    let session = session_with(&api, &store);
    session.restore().await;
    // 没登录：本地和付费都拒绝。
    assert!(session.check_local().is_err());
    assert!(session.check_paid().await.is_err());

    session.login(EMAIL, PASSWORD).await.unwrap();
    assert!(session.check_local().is_ok());
    // 60 秒内刚刷新过：付费步骤不再请求服务端。
    let before = api.calls("POST /license/refresh");
    advance(59);
    assert!(session.check_paid().await.is_ok());
    assert_eq!(api.calls("POST /license/refresh"), before);
    // 超过 60 秒：先联网刷新一次。
    advance(2);
    assert!(session.check_paid().await.is_ok());
    assert_eq!(api.calls("POST /license/refresh"), before + 1);
    // 断网：本地放行，付费拒绝。
    api.with(|server| server.offline = true);
    advance(61);
    assert!(session.check_local().is_ok());
    let rejected = session.check_paid().await.unwrap_err();
    assert!(matches!(rejected.tier, Tier::Offline { .. }));
}
