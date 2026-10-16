//! 授权凭证验签。
//!
//! 格式：`base64url(载荷 JSON) + "." + base64url(签名)`，base64url 不带填充，只认这一种写法。
//! 签名：Ed25519，签名对象是 [`SIGNING_CONTEXT`] + 载荷字节。固定前缀用来隔离用途：
//! 同一把私钥以后签别的东西（配方、升级包），那些签名不能被挪来冒充授权凭证。
//!
//! 验签顺序：格式 `malformed` → 签名 `bad_signature` → 载荷 `bad_payload` →
//! 机器码 `wrong_device` → 有效期 `expired`，哪一步不过就停在哪一步。
//! 先验签名再解析载荷：没通过签名的内容，一个字节都不信。
//!
//! 和服务端参考实现（Python `license_token.verify`）相比，下面几种载荷这里会拒绝，参考实现能接受：
//! - 已知整数字段里的 `-0`（serde_json 把它读成浮点数）；
//! - 字符串里单独出现的 UTF-16 代理字符（如 `"\ud800"`）；
//! - 嵌套超过 128 层（serde_json 的默认上限）；
//! - 超出浮点范围的数字（如 `1e400`，Python 读成 inf）。
//!
//! 这些内容只有持有私钥的一方能签出来，服务端从来不输出，所以只会让验签更保守，不会放宽。

use serde::Serialize;
use serde_json::Value;

use crate::envelope::{self, bounded_int, non_empty_str, EnvelopeError, Purpose};
use crate::keys::TrustedKey;

/// 签名对象的固定前缀，和服务端 `SIGNING_CONTEXT` 一致（定义在 [`Purpose::context`]）。
pub const SIGNING_CONTEXT: &[u8] = Purpose::License.context();
/// 载荷版本号。
pub const PAYLOAD_VERSION: u64 = 1;

/// 使用权类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EntitlementKind {
    /// 没有使用权。
    None,
    /// 按天计，到 `ent_until` 为止。
    Timed,
    /// 永久。
    Lifetime,
}

impl EntitlementKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EntitlementKind::None => "none",
            EntitlementKind::Timed => "timed",
            EntitlementKind::Lifetime => "lifetime",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "none" => Some(EntitlementKind::None),
            "timed" => Some(EntitlementKind::Timed),
            "lifetime" => Some(EntitlementKind::Lifetime),
            _ => None,
        }
    }
}

/// 验签通过的授权凭证内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct License {
    /// 签名公钥的指纹（公钥 SHA-256 的前 16 位十六进制）。
    pub kid: String,
    /// 账号 ID（载荷里的 `sub`）。
    pub user_id: String,
    /// 机器码（载荷里的 `dev`）。
    pub device_id: String,
    /// 使用权类型（载荷里的 `ent`）。
    pub entitlement: EntitlementKind,
    /// 使用权到期的 Unix 秒数；只有 `Timed` 才有。
    pub entitlement_until: Option<u64>,
    /// 签发时间（服务端数据库时间）。
    pub issued_at: u64,
    /// 凭证到期时间，也就是离线宽限期的终点。
    pub expires_at: u64,
}

/// 验签失败的原因。错误码和服务端、测试向量一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    #[error("授权凭证格式不正确")]
    Malformed,
    #[error("授权凭证签名无效")]
    BadSignature,
    #[error("授权凭证内容无效")]
    BadPayload,
    #[error("授权凭证不属于这台电脑")]
    WrongDevice,
    #[error("授权凭证已过期，请联网刷新")]
    Expired,
}

impl TokenError {
    /// 机器可读的错误码。
    pub fn code(self) -> &'static str {
        match self {
            TokenError::Malformed => "malformed",
            TokenError::BadSignature => "bad_signature",
            TokenError::BadPayload => "bad_payload",
            TokenError::WrongDevice => "wrong_device",
            TokenError::Expired => "expired",
        }
    }
}

/// 本地验签：格式 → 签名 → 载荷 → 机器码 → 有效期。当前时间等于 `exp` 就算过期。
///
/// `trusted` 里的公钥逐把尝试，任意一把验过就算通过（为密钥轮换留位置）。
pub fn verify(
    token: &str,
    trusted: &[TrustedKey],
    device_id: &str,
    now: u64,
) -> Result<License, TokenError> {
    let (payload, signed_by) = envelope::open(Purpose::License, token, trusted)?;
    let license = parse_payload(&payload, signed_by)?;
    if license.device_id != device_id {
        return Err(TokenError::WrongDevice);
    }
    if now >= license.expires_at {
        return Err(TokenError::Expired);
    }
    Ok(license)
}

impl From<EnvelopeError> for TokenError {
    fn from(error: EnvelopeError) -> Self {
        match error {
            EnvelopeError::Malformed => TokenError::Malformed,
            EnvelopeError::BadSignature => TokenError::BadSignature,
        }
    }
}

fn parse_payload(raw: &[u8], signed_by: &str) -> Result<License, TokenError> {
    let bad = TokenError::BadPayload;
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
    let user_id = non_empty_str(object.get("sub")).ok_or(bad)?;
    let device_id = non_empty_str(object.get("dev")).ok_or(bad)?;
    let entitlement = object
        .get("ent")
        .and_then(Value::as_str)
        .and_then(EntitlementKind::parse)
        .ok_or(bad)?;
    let until = object.get("ent_until");
    let entitlement_until = match entitlement {
        EntitlementKind::Timed => Some(bounded_int(until).ok_or(bad)?),
        // 不是按天计的使用权：ent_until 必须是 null，缺省也算 null（和参考实现的 dict.get 一致）。
        _ if matches!(until, None | Some(Value::Null)) => None,
        _ => return Err(bad),
    };
    let issued_at = bounded_int(object.get("iat")).ok_or(bad)?;
    let expires_at = bounded_int(object.get("exp")).ok_or(bad)?;
    if expires_at <= issued_at {
        return Err(bad);
    }
    Ok(License {
        kid: kid.to_owned(),
        user_id: user_id.to_owned(),
        device_id: device_id.to_owned(),
        entitlement,
        entitlement_until,
        issued_at,
        expires_at,
    })
}
