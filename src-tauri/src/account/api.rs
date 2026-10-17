//! 账号与授权服务的 HTTP 客户端（接口见 `cloud/docs/COMMERCE.md` 第 3 节）。
//!
//! - 服务端地址编译时用 `DOSSIER_API_BASE` 写进程序；调试版运行时也能用同名环境变量覆盖，正式版不能
//!   （让用户随便改地址，等于允许别人把登录引到钓鱼服务器）。不是本机地址的，必须用 HTTPS。
//! - 证书校验走系统信任库；不自动跟随重定向（令牌在请求头里，不能被带到别的地址）。
//! - 日志和错误里不出现令牌、密码、卡密、授权凭证和响应体。

use std::future::Future;
use std::time::Duration;

use reqwest::{Method, StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 编译时没给地址时的默认值：本机开发服务（`drama-commerce serve --port 8100`）。
const DEFAULT_BASE: &str = "http://127.0.0.1:8100";
const API_PREFIX: &str = "/api/v1";

/// 决定服务端地址。返回的错误是中文，界面直接展示。
pub fn resolve_base() -> Result<Url, String> {
    #[cfg(debug_assertions)]
    if let Ok(text) = std::env::var("DOSSIER_API_BASE") {
        return parse_base(&text);
    }
    parse_base(option_env!("DOSSIER_API_BASE").unwrap_or(DEFAULT_BASE))
}

/// 校验服务端地址：只认 http / https；http 只允许本机地址；不能带查询参数和片段。
pub fn parse_base(text: &str) -> Result<Url, String> {
    let url = Url::parse(text.trim()).map_err(|_| format!("服务端地址格式不正确：{text}"))?;
    let host = url
        .host_str()
        .ok_or_else(|| format!("服务端地址缺少主机名：{text}"))?;
    // IPv6 地址在 host_str 里带方括号。
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = host.eq_ignore_ascii_case("localhost")
        || bare
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        "http" => return Err(format!("服务端地址必须使用 HTTPS（本机地址除外）：{text}")),
        _ => return Err(format!("服务端地址只支持 http / https：{text}")),
    }
    if url.query().is_some() || url.fragment().is_some() || !url.username().is_empty() {
        return Err(format!("服务端地址不能带查询参数、片段或用户名：{text}"));
    }
    Ok(url)
}

/// 接口调用失败。
#[derive(Debug, Clone)]
pub enum ApiError {
    /// 服务端返回的错误：`{"error", "message", ...}`。
    Server {
        status: u16,
        code: String,
        message: String,
        retry_after: Option<u64>,
        /// 附加字段（如 `device_limit` 带的设备列表），原样保留。
        extra: Value,
    },
    /// 连不上服务端（断网、超时、TLS 失败）。
    Network(String),
    /// 服务端返回的内容和约定不符。
    Protocol(String),
}

impl ApiError {
    pub fn code(&self) -> &str {
        match self {
            ApiError::Server { code, .. } => code,
            ApiError::Network(_) => "network",
            ApiError::Protocol(_) => "protocol",
        }
    }

    pub fn message(&self) -> String {
        match self {
            ApiError::Server { message, .. } => message.clone(),
            ApiError::Network(detail) => format!("连不上服务端（{detail}），请检查网络后重试"),
            ApiError::Protocol(detail) => format!("服务端返回的内容无法识别（{detail}）"),
        }
    }

    /// 会话失效（令牌无效、过期、被吊销，包括这台电脑被解绑）。
    /// 只看错误码：登录失败的 `invalid_credentials` 也是 401，但不是会话失效。
    pub fn is_unauthorized(&self) -> bool {
        self.code() == "unauthorized"
    }

    /// 服务端不可达：断网，或者服务端自己报不可用（5xx）。这两种都按断网处理，走离线宽限期。
    pub fn is_unreachable(&self) -> bool {
        match self {
            ApiError::Network(_) => true,
            ApiError::Server { status, .. } => *status >= 500,
            ApiError::Protocol(_) => false,
        }
    }
}

// ---------- 响应结构（字段名和服务端一致，snake_case） ----------

#[derive(Debug, Clone, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// 访问令牌还有多少秒过期。
    pub expires_in: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserView {
    pub id: String,
    pub email: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthResponse {
    pub user: UserView,
    pub tokens: Tokens,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RefreshResponse {
    pub tokens: Tokens,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntitlementView {
    pub kind: String,
    pub active: bool,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LicenseView {
    pub token: String,
    pub refresh_after: String,
}

/// 设备信息，原样转给前端（前端按 snake_case 读）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceView {
    pub id: String,
    pub code: String,
    pub name: String,
    pub platform: String,
    pub app_version: String,
    pub bound_at: String,
    pub last_seen_at: String,
    pub current: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LicenseResponse {
    pub license: LicenseView,
    pub entitlement: EntitlementView,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnbindQuota {
    pub limit: u32,
    pub window_days: u32,
    pub remaining: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceList {
    pub devices: Vec<DeviceView>,
    pub device_limit: u32,
    pub unbind_quota: UnbindQuota,
}

/// 兑换结果。新的使用权以兑换后那次授权刷新为准，这里只取「是不是重复兑换」。
#[derive(Debug, Clone, Deserialize)]
pub struct RedeemResponse {
    pub already_redeemed: bool,
}

/// 服务端生成的一个文件：包内路径 + base64（标准字母表）内容。
#[derive(Debug, Clone, Deserialize)]
pub struct PackageFileView {
    pub path: String,
    pub content: String,
}

/// `POST /packages` 的结果。`manifest` 是原样字符串：签名针对的就是这些字节，不能重新序列化。
#[derive(Debug, Clone, Deserialize)]
pub struct PackageResponse {
    pub manifest: String,
    /// `manifest.sig` 的内容（`{"kid","sig"}` 的 JSON 文本）。
    pub signature: String,
    pub files: Vec<PackageFileView>,
}

/// `POST /recipes` 的结果。`params` 只是方便查看；处理时以验过签的 `token` 里的为准。
#[derive(Debug, Clone, Deserialize)]
pub struct RecipeResponse {
    pub recipe_id: String,
    pub token: String,
    /// 这次要花多少积分（M5a）。签发时定价，消费时扣。
    ///
    /// 不回填 `episode_count`：桌面端本来就知道自己报了多少集，
    /// 服务端把它再传回来只会多一个字段、多一处需要同步的地方。
    #[serde(default)]
    pub credits: i64,
}

// ---------- 传输层 ----------

/// 一次请求：方法、路径（不含 `/api/v1`）、可选的访问令牌和 JSON 请求体。
#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub path: String,
    pub bearer: Option<String>,
    pub body: Option<Value>,
}

impl Request {
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            bearer: None,
            body: None,
        }
    }

    pub fn json(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }

    pub fn bearer(mut self, token: &str) -> Self {
        self.bearer = Some(token.to_owned());
        self
    }
}

/// 发请求、拿回 JSON（204 时是 `Value::Null`）。测试用假实现替换它。
pub trait Transport: Send + Sync + 'static {
    fn send(&self, request: Request) -> impl Future<Output = Result<Value, ApiError>> + Send;
}

/// 把 JSON 解成目标结构，解不开按协议错误处理。
pub fn decode<T: DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value).map_err(|error| ApiError::Protocol(error.to_string()))
}

/// 真实的 HTTP 传输：reqwest + rustls。
///
/// 地址不合规、网络模块初始化失败时照样能构造出来，[`HttpTransport::problem`] 给出原因：
/// 界面锁定为「安装包配置有误」，而不是让程序启动就崩溃。
pub struct HttpTransport {
    inner: Result<(reqwest::Client, Url), String>,
}

impl HttpTransport {
    pub fn new() -> Self {
        let inner = resolve_base().and_then(|base| {
            install_crypto_provider();
            let client = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(format!(
                    "DramaDossier/{} ({})",
                    env!("CARGO_PKG_VERSION"),
                    dossier_license::Platform::current().map_or("unknown", |p| p.as_str())
                ))
                .build()
                .map_err(|error| format!("初始化网络模块失败：{error}"))?;
            Ok((client, base))
        });
        Self { inner }
    }

    /// 指定服务端地址（端到端测试用：模拟「服务端停了」时指向一个没人监听的端口，不用改环境变量）。
    #[cfg(test)]
    pub fn with_base(base: &str) -> Self {
        install_crypto_provider();
        let inner = parse_base(base).and_then(|base| {
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map(|client| (client, base))
                .map_err(|error| format!("初始化网络模块失败：{error}"))
        });
        Self { inner }
    }

    /// 构造时遇到的问题；没有问题时为 `None`。
    pub fn problem(&self) -> Option<&str> {
        self.inner.as_ref().err().map(String::as_str)
    }
}

impl Transport for HttpTransport {
    async fn send(&self, request: Request) -> Result<Value, ApiError> {
        let (client, base) = self
            .inner
            .as_ref()
            .map_err(|problem| ApiError::Protocol(problem.clone()))?;
        let url = Url::parse(&format!(
            "{}{API_PREFIX}{}",
            base.as_str().trim_end_matches('/'),
            request.path
        ))
        .map_err(|error| ApiError::Protocol(format!("接口地址拼接失败：{error}")))?;
        let mut builder = client.request(request.method, url);
        if let Some(token) = &request.bearer {
            builder = builder.bearer_auth(token);
        }
        if let Some(body) = &request.body {
            builder = builder.json(body);
        }
        // without_url：错误信息里不带地址，免得以后地址里出现敏感参数被打进日志。
        let response = builder.send().await.map_err(network_error)?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|text| text.trim().parse::<u64>().ok());
        let bytes = response.bytes().await.map_err(network_error)?;
        if status == StatusCode::NO_CONTENT {
            return Ok(Value::Null);
        }
        let value: Option<Value> = serde_json::from_slice(&bytes).ok();
        if status.is_success() {
            return value.ok_or_else(|| ApiError::Protocol("响应不是 JSON".into()));
        }
        Err(server_error(status, retry_after, value))
    }
}

/// 装上 rustls 的加密后端。
///
/// 用 `ring` 而不是默认的 `aws-lc-rs`：后者在 Windows 上要装 NASM，GitHub 的 Windows runner 没有预装。
/// `reqwest` 的 `rustls-no-provider` 要求建 Client 之前必须装好，否则会 panic。
/// 装一次就够，重复调用返回 Err（已经装过），忽略即可。
fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn network_error(error: reqwest::Error) -> ApiError {
    let kind = if error.is_timeout() {
        "超时"
    } else if error.is_connect() {
        "无法连接"
    } else {
        "网络错误"
    };
    ApiError::Network(format!("{kind}：{}", error.without_url()))
}

fn server_error(status: StatusCode, retry_after: Option<u64>, body: Option<Value>) -> ApiError {
    let mut extra = body.unwrap_or(Value::Null);
    let take = |extra: &mut Value, key: &str| {
        extra
            .as_object_mut()
            .and_then(|object| object.remove(key))
            .and_then(|value| value.as_str().map(str::to_owned))
    };
    let code = take(&mut extra, "error");
    let message = take(&mut extra, "message");
    match code {
        Some(code) => ApiError::Server {
            status: status.as_u16(),
            code,
            message: message.unwrap_or_else(|| format!("服务端返回 {status}")),
            retry_after,
            extra,
        },
        // 反向代理之类返回的非约定错误页：5xx 按服务不可用处理，其余按协议错误。
        None if status.is_server_error() => ApiError::Server {
            status: status.as_u16(),
            code: "service_unavailable".into(),
            message: "服务暂时不可用，请稍后重试".into(),
            retry_after,
            extra: Value::Null,
        },
        None => ApiError::Protocol(format!("服务端返回 {status}，但没有错误码")),
    }
}
