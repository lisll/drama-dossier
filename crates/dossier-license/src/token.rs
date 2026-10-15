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

use std::fmt;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::Signature;
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::Serialize;
use serde_json::{Map, Number, Value};

use crate::keys::TrustedKey;

/// 签名对象的固定前缀，和服务端 `SIGNING_CONTEXT` 一致。
pub const SIGNING_CONTEXT: &[u8] = b"drama-dossier/license/v1\n";
/// 载荷版本号。
pub const PAYLOAD_VERSION: u64 = 1;
/// 整数字段的上限（不含）：0..2^63-1，和服务端一致。
const INT_LIMIT: u64 = 1 << 63;

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
    let (payload_part, signature_part) = token.split_once('.').ok_or(TokenError::Malformed)?;
    if signature_part.contains('.') {
        return Err(TokenError::Malformed);
    }
    let payload = b64url_decode(payload_part)?;
    let signature = b64url_decode(signature_part)?;
    let signature: [u8; 64] = signature
        .as_slice()
        .try_into()
        .map_err(|_| TokenError::Malformed)?;
    let signature = Signature::from_bytes(&signature);

    let mut message = Vec::with_capacity(SIGNING_CONTEXT.len() + payload.len());
    message.extend_from_slice(SIGNING_CONTEXT);
    message.extend_from_slice(&payload);
    let signed_by = trusted
        .iter()
        .find(|key| {
            key.verifying_key()
                .verify_strict(&message, &signature)
                .is_ok()
        })
        .ok_or(TokenError::BadSignature)?;

    let license = parse_payload(&payload, signed_by.kid())?;
    if license.device_id != device_id {
        return Err(TokenError::WrongDevice);
    }
    if now >= license.expires_at {
        return Err(TokenError::Expired);
    }
    Ok(license)
}

/// 严格的 base64url 解码：只认 `[A-Za-z0-9_-]+`，不带填充；解出来再编码一遍必须和原文一模一样。
pub(crate) fn b64url_decode(text: &str) -> Result<Vec<u8>, TokenError> {
    let charset_ok = !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if !charset_ok {
        return Err(TokenError::Malformed);
    }
    let raw = URL_SAFE_NO_PAD
        .decode(text)
        .map_err(|_| TokenError::Malformed)?;
    if URL_SAFE_NO_PAD.encode(&raw) != text {
        return Err(TokenError::Malformed);
    }
    Ok(raw)
}

fn parse_payload(raw: &[u8], signed_by: &str) -> Result<License, TokenError> {
    let bad = TokenError::BadPayload;
    let text = std::str::from_utf8(raw).map_err(|_| bad)?;
    let StrictValue(value) = serde_json::from_str(text).map_err(|_| bad)?;
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

/// JSON 整数且在 0..2^63-1。`true`、`1.0`、`"1"`、`-0` 都不算。
fn bounded_int(value: Option<&Value>) -> Option<u64> {
    value?.as_u64().filter(|number| *number < INT_LIMIT)
}

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value?.as_str().filter(|text| !text.is_empty())
}

/// 严格解析的 JSON 值：任何一层出现重复的键都拒绝。
///
/// serde_json 默认是后一个键覆盖前一个，同一段载荷就可能被两种实现读出不同的意思，所以要自己处理。
/// `NaN`、`Infinity` 这类非法常量 serde_json 本来就拒绝。
struct StrictValue(Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(StrictVisitor).map(StrictValue)
    }
}

struct StrictVisitor;

impl<'de> Visitor<'de> for StrictVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("JSON 值")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("非法数字"))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(StrictValue(item)) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        while let Some((key, StrictValue(value))) = map.next_entry::<String, StrictValue>()? {
            if object.contains_key(&key) {
                return Err(de::Error::custom("重复的键"));
            }
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }
}
