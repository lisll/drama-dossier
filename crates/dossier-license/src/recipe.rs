//! 处理配方的签名校验（总方案 2.2 节，参数由服务端签发）。
//!
//! 格式和授权凭证一样：`base64url(载荷 JSON) + "." + base64url(签名)`，
//! 前缀是 [`Purpose::Recipe`]，所以授权凭证的签名不能拿来冒充配方，反过来也一样。
//!
//! 「只能用一次」由服务端记账：开工前桌面端联网消费一次，第二次消费服务端拒绝。
//! 这里只做离线也能做的检查：签名、机器码、任务、有效期。两步都过才动手处理。
//!
//! 验签顺序：格式 → 签名 → 载荷 → 机器码 → 任务 → 有效期，哪一步不过就停在哪一步。

use serde_json::{Map, Value};

use crate::envelope::{self, bounded_int, non_empty_str, EnvelopeError, Purpose};
use crate::keys::TrustedKey;

/// 载荷版本号。
pub const PAYLOAD_VERSION: u64 = 1;
/// 认识的平台和强度；服务端多出来的取值这里不认，按载荷无效处理。
pub const PLATFORMS: &[&str] = &["douyin", "kuaishou", "channels"];
pub const STRENGTHS: &[&str] = &["light", "medium", "heavy"];

/// 验签通过的配方。`params` 的具体字段在 M4b 的处理管线里解析。
#[derive(Debug, Clone, PartialEq)]
pub struct Recipe {
    pub kid: String,
    pub recipe_id: String,
    pub user_id: String,
    pub device_id: String,
    /// 任务描述的哈希（服务端算，桌面端按同样规则算后比对）。
    pub task: String,
    pub platform: String,
    pub strength: String,
    pub seed: u64,
    pub params: Map<String, Value>,
    pub issued_at: u64,
    pub expires_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RecipeError {
    #[error("处理配方格式不正确")]
    Malformed,
    #[error("处理配方签名无效")]
    BadSignature,
    #[error("处理配方内容无效")]
    BadPayload,
    #[error("处理配方不属于这台电脑")]
    WrongDevice,
    #[error("处理配方不是为这个任务签发的")]
    WrongTask,
    #[error("处理配方已过期，请重新申请")]
    Expired,
}

impl RecipeError {
    pub fn code(self) -> &'static str {
        match self {
            RecipeError::Malformed => "malformed",
            RecipeError::BadSignature => "bad_signature",
            RecipeError::BadPayload => "bad_payload",
            RecipeError::WrongDevice => "wrong_device",
            RecipeError::WrongTask => "wrong_task",
            RecipeError::Expired => "expired",
        }
    }
}

impl From<EnvelopeError> for RecipeError {
    fn from(error: EnvelopeError) -> Self {
        match error {
            EnvelopeError::Malformed => RecipeError::Malformed,
            EnvelopeError::BadSignature => RecipeError::BadSignature,
        }
    }
}

/// 本地校验配方。当前时间等于 `exp` 就算过期。
pub fn verify(
    token: &str,
    trusted: &[TrustedKey],
    device_id: &str,
    task: &str,
    now: u64,
) -> Result<Recipe, RecipeError> {
    let (payload, signed_by) = envelope::open(Purpose::Recipe, token, trusted)?;
    let recipe = parse_payload(&payload, signed_by)?;
    if recipe.device_id != device_id {
        return Err(RecipeError::WrongDevice);
    }
    if recipe.task != task {
        return Err(RecipeError::WrongTask);
    }
    if now >= recipe.expires_at {
        return Err(RecipeError::Expired);
    }
    Ok(recipe)
}

fn parse_payload(raw: &[u8], signed_by: &str) -> Result<Recipe, RecipeError> {
    let bad = RecipeError::BadPayload;
    let value = envelope::parse_strict_json(raw).ok_or(bad)?;
    let object = value.as_object().ok_or(bad)?;
    if bounded_int(object.get("v")) != Some(PAYLOAD_VERSION) {
        return Err(bad);
    }
    let kid = object
        .get("kid")
        .and_then(Value::as_str)
        .filter(|kid| *kid == signed_by)
        .ok_or(bad)?;
    let text = |name: &str| {
        non_empty_str(object.get(name))
            .map(str::to_owned)
            .ok_or(bad)
    };
    let platform = text("platform")?;
    let strength = text("strength")?;
    if !PLATFORMS.contains(&platform.as_str()) || !STRENGTHS.contains(&strength.as_str()) {
        return Err(bad);
    }
    let params = object
        .get("params")
        .and_then(Value::as_object)
        .cloned()
        .ok_or(bad)?;
    let issued_at = bounded_int(object.get("iat")).ok_or(bad)?;
    let expires_at = bounded_int(object.get("exp")).ok_or(bad)?;
    if expires_at <= issued_at {
        return Err(bad);
    }
    Ok(Recipe {
        kid: kid.to_owned(),
        recipe_id: text("rid")?,
        user_id: text("sub")?,
        device_id: text("dev")?,
        task: text("task")?,
        platform,
        strength,
        seed: bounded_int(object.get("seed")).ok_or(bad)?,
        params,
        issued_at,
        expires_at,
    })
}
