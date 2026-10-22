//! 账号与授权服务的 HTTP 客户端（接口见 `cloud/docs/COMMERCE.md` 第 3 节）。
//!
//! - 服务端地址编译时用 `DOSSIER_API_BASE` 写进程序；调试版运行时也能用同名环境变量覆盖，正式版不能
//!   （让用户随便改地址，等于允许别人把登录引到钓鱼服务器）。不是本机地址的，必须用 HTTPS。
//! - 服务端处理（上传、建任务、取产物）发给**媒体控制面**，地址是 `DOSSIER_MEDIA_BASE`，规则同上；
//!   不设时与 `DOSSIER_API_BASE` 相同——部署时两个服务经 Caddy 共用一个入口，什么都不用配。
//!   只有本地开发两个服务各占一个端口时才需要设它。
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
///
/// CI 从仓库变量注入，未配置时是**空字符串**——当作没配、退回默认值，
/// 而不是把空串当地址报「格式不正确」让程序一启动就锁死。
pub fn resolve_base() -> Result<Url, String> {
    #[cfg(debug_assertions)]
    if let Ok(text) = std::env::var("DOSSIER_API_BASE") {
        return parse_base(&text);
    }
    parse_base(
        option_env!("DOSSIER_API_BASE")
            .filter(|text| !text.trim().is_empty())
            .unwrap_or(DEFAULT_BASE),
    )
}

/// 决定媒体控制面的地址。没单独配就用账号服务那个地址。
pub fn resolve_media_base() -> Result<Url, String> {
    #[cfg(debug_assertions)]
    let runtime = std::env::var("DOSSIER_MEDIA_BASE").ok();
    #[cfg(not(debug_assertions))]
    let runtime: Option<String> = None;
    match pick_media_base(runtime.as_deref(), option_env!("DOSSIER_MEDIA_BASE")) {
        Some(text) => parse_base(text),
        None => resolve_base(),
    }
}

/// 媒体地址的取值顺序：调试版的运行时环境变量 → 编译时写入的值 → 都没有就返回 `None`
/// （调用方退回账号服务地址）。空字符串当作没配：CI 里变量没设时展开出来就是空串。
fn pick_media_base<'a>(runtime: Option<&'a str>, compiled: Option<&'a str>) -> Option<&'a str> {
    [runtime, compiled]
        .into_iter()
        .flatten()
        .find(|text| !text.trim().is_empty())
}

/// 把媒体控制面返回的站内路径（如产物下载链接 `/api/v1/artifacts/…/download?token=…`）
/// 补成完整地址，界面才能把它做成可点的链接。已经是 http/https 完整地址的原样返回；
/// 其余形状（不以 `/` 开头）也原样返回，由界面按「不可信地址」处理。
pub fn absolute_media_url(base: &Url, url: &str) -> String {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || !url.starts_with('/') {
        return url.to_owned();
    }
    format!("{}{url}", base.as_str().trim_end_matches('/'))
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

/// `POST /version-recipes` 的结果。`params` 只是方便查看；处理时以验过签的 `token` 里的为准。
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

/// 请求发给哪个服务。两个服务是不同的进程，地址可能不同（见文件开头）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Service {
    /// 账号与授权服务（drama-commerce）：登录、授权、积分、报审包、版本配方。
    #[default]
    Commerce,
    /// 媒体控制面（drama-cloud）：分片上传、服务端任务、产物。
    Media,
}

/// 一次请求：方法、路径（不含 `/api/v1`）、可选的访问令牌和请求体。
///
/// 请求体有两种：JSON（绝大多数接口）与**原始字节**（M5b 的分片上传）。
/// 两者互斥；都用同一个字段，是为了不让 `Transport` trait 长出一个平行的方法，
/// 那样每个测试里的假实现都得跟着改。
#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub path: String,
    pub bearer: Option<String>,
    pub body: Option<Value>,
    /// 原始字节体（分片上传）。为 `Some` 时忽略 `body`。
    pub raw: Option<Vec<u8>>,
    /// 发给哪个服务。默认账号服务；媒体控制面的请求用 [`Request::media`] 标出来。
    pub service: Service,
}

impl Request {
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            bearer: None,
            body: None,
            raw: None,
            service: Service::Commerce,
        }
    }

    pub fn json(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }

    /// 原始字节体。用于分片上传：分片是视频的一部分，不能当 JSON 传。
    pub fn bytes(mut self, payload: Vec<u8>) -> Self {
        self.raw = Some(payload);
        self
    }

    pub fn bearer(mut self, token: &str) -> Self {
        self.bearer = Some(token.to_owned());
        self
    }

    /// 这个请求发给媒体控制面。
    pub fn media(mut self) -> Self {
        self.service = Service::Media;
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
    inner: Result<Connection, String>,
}

/// 一个共用的 HTTP 客户端 + 两个服务各自的地址。
struct Connection {
    client: reqwest::Client,
    commerce: Url,
    media: Url,
}

impl Connection {
    fn base(&self, service: Service) -> &Url {
        match service {
            Service::Commerce => &self.commerce,
            Service::Media => &self.media,
        }
    }
}

impl HttpTransport {
    pub fn new() -> Self {
        let inner = resolve_base().and_then(|commerce| {
            let media = resolve_media_base()?;
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
            Ok(Connection {
                client,
                commerce,
                media,
            })
        });
        Self { inner }
    }

    /// 指定服务端地址（端到端测试用：模拟「服务端停了」时指向一个没人监听的端口，不用改环境变量）。
    /// 两个服务用同一个地址：端到端测试只碰账号服务。
    #[cfg(test)]
    pub fn with_base(base: &str) -> Self {
        Self::with_bases(base, base)
    }

    /// 两个服务各给一个地址（测试「请求发对了服务」用）。
    #[cfg(test)]
    pub fn with_bases(commerce: &str, media: &str) -> Self {
        install_crypto_provider();
        let inner = parse_base(commerce).and_then(|commerce| {
            let media = parse_base(media)?;
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map(|client| Connection {
                    client,
                    commerce,
                    media,
                })
                .map_err(|error| format!("初始化网络模块失败：{error}"))
        });
        Self { inner }
    }

    /// 构造时遇到的问题；没有问题时为 `None`。
    pub fn problem(&self) -> Option<&str> {
        self.inner.as_ref().err().map(String::as_str)
    }
}

/// 接口完整地址：服务地址 + `/api/v1` + 路径。
fn endpoint(base: &Url, path: &str) -> Result<Url, ApiError> {
    Url::parse(&format!(
        "{}{API_PREFIX}{path}",
        base.as_str().trim_end_matches('/')
    ))
    .map_err(|error| ApiError::Protocol(format!("接口地址拼接失败：{error}")))
}

impl Transport for HttpTransport {
    async fn send(&self, request: Request) -> Result<Value, ApiError> {
        let connection = self
            .inner
            .as_ref()
            .map_err(|problem| ApiError::Protocol(problem.clone()))?;
        let url = endpoint(connection.base(request.service), &request.path)?;
        let mut builder = connection.client.request(request.method, url);
        if let Some(token) = &request.bearer {
            builder = builder.bearer_auth(token);
        }
        if let Some(raw) = &request.raw {
            // 分片上传：整个分片一次放进内存。8 MiB 的分片对内存没压力，
            // 而流式上传需要额外的读文件句柄生命周期管理，这里不值得（YAGNI）。
            builder = builder
                .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
                .body(raw.clone());
        } else if let Some(body) = &request.body {
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
/// `reqwest` 的 `rustls-no-provider` 要求建 Client 之前必须装好，否则会 panic。装一次就够，
/// 重复调用返回 Err（已经装过），忽略即可。updater.rs 建下载客户端前也会调用。
pub(crate) fn install_crypto_provider() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn media_base_prefers_runtime_then_compiled_then_falls_back() {
        assert_eq!(
            pick_media_base(Some("http://127.0.0.1:8000"), Some("https://m.example")),
            Some("http://127.0.0.1:8000")
        );
        assert_eq!(
            pick_media_base(None, Some("https://m.example")),
            Some("https://m.example")
        );
        // CI 里变量没设时展开成空串：当作没配，退回账号服务地址，而不是把空串当地址报错。
        assert_eq!(pick_media_base(Some(""), Some("  ")), None);
        assert_eq!(pick_media_base(None, None), None);
    }

    #[test]
    fn relative_artifact_links_become_absolute_on_the_media_base() {
        let base = Url::parse("http://127.0.0.1:8000").unwrap();
        assert_eq!(
            absolute_media_url(&base, "/api/v1/artifacts/af_1/download?token=t"),
            "http://127.0.0.1:8000/api/v1/artifacts/af_1/download?token=t"
        );
        // 以 `//` 开头也只会落在媒体服务自己的主机上，不会被当成「换一台主机」。
        assert_eq!(
            absolute_media_url(&base, "//evil.example/x"),
            "http://127.0.0.1:8000//evil.example/x"
        );
        // 地址带路径前缀和末尾斜杠：不丢前缀、不多斜杠。
        let prefixed = Url::parse("https://example.com/dossier/").unwrap();
        assert_eq!(
            absolute_media_url(&prefixed, "/api/v1/artifacts/a/download"),
            "https://example.com/dossier/api/v1/artifacts/a/download"
        );
        // 已是完整地址的原样返回；其余形状不猜，原样交给界面按不可信地址处理。
        for kept in [
            "https://cdn.example/x",
            "HTTP://127.0.0.1/x",
            "javascript:alert(1)",
            "relative/x",
            "",
        ] {
            assert_eq!(absolute_media_url(&base, kept), kept);
        }
    }

    /// 起一个只应答一次的本机 HTTP 服务，回 `{"service": 名字, "path": 请求路径}`。
    /// 用标准库现写：tokio 没开 net 特性，不为一条测试加依赖。
    fn one_shot_server(name: &'static str) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).unwrap() == 0 {
                    break;
                }
                head.push(byte[0]);
            }
            let text = String::from_utf8_lossy(&head);
            let path = text
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_owned();
            let body = json!({ "service": name, "path": path }).to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).unwrap();
        });
        (base, handle)
    }

    /// 两个服务地址不同时，请求必须发到自己标记的那个服务（曾经全部发给账号服务）。
    /// 用 GET：假服务只读请求头，带请求体时提前关连接可能让客户端收到 RST。
    #[tokio::test]
    async fn each_request_goes_to_the_service_it_is_marked_for() {
        let (commerce, commerce_server) = one_shot_server("commerce");
        let (media, media_server) = one_shot_server("media");
        let transport = HttpTransport::with_bases(&commerce, &media);
        assert_eq!(transport.problem(), None);

        let reply = transport
            .send(Request::new(Method::GET, "/me"))
            .await
            .unwrap();
        assert_eq!(
            reply,
            json!({ "service": "commerce", "path": "/api/v1/me" })
        );

        let reply = transport
            .send(Request::new(Method::GET, "/tasks/tsk_1").media())
            .await
            .unwrap();
        assert_eq!(
            reply,
            json!({ "service": "media", "path": "/api/v1/tasks/tsk_1" })
        );

        commerce_server.join().unwrap();
        media_server.join().unwrap();
    }

    #[test]
    fn an_invalid_media_base_is_reported_not_ignored() {
        let transport = HttpTransport::with_bases("http://127.0.0.1:8100", "http://media.example");
        let problem = transport.problem().expect("非本机 http 地址必须报出来");
        assert!(problem.contains("HTTPS"), "{problem}");
    }
}
