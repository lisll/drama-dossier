//! 通用的签名信封：授权凭证、报审包清单、处理配方共用。
//!
//! 一把服务端私钥签三种东西，签名对象前面加不同的固定前缀（[`Purpose::context`]），
//! 所以任何一种签名都不能被挪去冒充另一种：用授权凭证的签名去验清单，签名对象不一样，验不过。
//!
//! 两种写法：
//! - 内嵌：`base64url(载荷) + "." + base64url(签名)`（授权凭证、处理配方）；
//! - 分离：载荷是一个文件的原始字节，签名另存（报审包的 `manifest.json` + `manifest.sig`）。
//!
//! base64url 不带填充，只认一种写法；签名必须正好 64 字节，用 `verify_strict` 验；
//! 受信公钥逐把尝试，返回实际验过签的那把的指纹。

use std::fmt;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::Signature;
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};

use crate::keys::TrustedKey;

/// 签名的用途。前缀和服务端 `license_token` 里的一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    License,
    PackageManifest,
    Recipe,
}

impl Purpose {
    pub const fn context(self) -> &'static [u8] {
        match self {
            Purpose::License => b"drama-dossier/license/v1\n",
            // 下面两个前缀和服务端 license_token.PURPOSE_CONTEXTS 一致。
            Purpose::PackageManifest => b"drama-dossier/package-manifest/v1\n",
            Purpose::Recipe => b"drama-dossier/recipe/v1\n",
        }
    }
}

/// 信封层面的错误：格式不对，或者签名验不过。载荷内容的错误由各用途自己定义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeError {
    Malformed,
    BadSignature,
}

/// 严格的 base64url 解码：只认 `[A-Za-z0-9_-]+`，不带填充；解出来再编码一遍必须和原文一模一样。
pub fn b64url_decode(text: &str) -> Result<Vec<u8>, EnvelopeError> {
    let charset_ok = !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if !charset_ok {
        return Err(EnvelopeError::Malformed);
    }
    let raw = URL_SAFE_NO_PAD
        .decode(text)
        .map_err(|_| EnvelopeError::Malformed)?;
    if URL_SAFE_NO_PAD.encode(&raw) != text {
        return Err(EnvelopeError::Malformed);
    }
    Ok(raw)
}

/// 验一段字节的签名：成功时返回实际验过签的公钥指纹。`signature_b64` 必须解出正好 64 字节。
pub fn verify_detached<'k>(
    purpose: Purpose,
    payload: &[u8],
    signature_b64: &str,
    trusted: &'k [TrustedKey],
) -> Result<&'k str, EnvelopeError> {
    let signature = b64url_decode(signature_b64)?;
    let signature: [u8; 64] = signature
        .as_slice()
        .try_into()
        .map_err(|_| EnvelopeError::Malformed)?;
    let signature = Signature::from_bytes(&signature);
    let context = purpose.context();
    let mut message = Vec::with_capacity(context.len() + payload.len());
    message.extend_from_slice(context);
    message.extend_from_slice(payload);
    trusted
        .iter()
        .find(|key| {
            key.verifying_key()
                .verify_strict(&message, &signature)
                .is_ok()
        })
        .map(TrustedKey::kid)
        .ok_or(EnvelopeError::BadSignature)
}

/// 内嵌写法：拆开 `载荷.签名`，验签通过后返回载荷字节和签名公钥的指纹。
pub fn open<'k>(
    purpose: Purpose,
    token: &str,
    trusted: &'k [TrustedKey],
) -> Result<(Vec<u8>, &'k str), EnvelopeError> {
    let (payload_part, signature_part) = token.split_once('.').ok_or(EnvelopeError::Malformed)?;
    if signature_part.contains('.') {
        return Err(EnvelopeError::Malformed);
    }
    let payload = b64url_decode(payload_part)?;
    // 签名先检查格式（长度、写法），再验签：和测试向量里「格式错误优先」的顺序一致。
    let kid = verify_detached(purpose, &payload, signature_part, trusted)?;
    Ok((payload, kid))
}

/// 严格解析 JSON：任何一层出现重复的键都拒绝（serde_json 默认是后一个覆盖前一个）。
/// `NaN`、`Infinity` 这类非法常量 serde_json 本来就拒绝。
pub fn parse_strict_json(raw: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(raw).ok()?;
    serde_json::from_str::<StrictValue>(text)
        .ok()
        .map(|StrictValue(value)| value)
}

/// JSON 整数且在 0..2^63-1。`true`、`1.0`、`"1"`、`-0` 都不算。
pub fn bounded_int(value: Option<&Value>) -> Option<u64> {
    const INT_LIMIT: u64 = 1 << 63;
    value?.as_u64().filter(|number| *number < INT_LIMIT)
}

/// 非空字符串。
pub fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value?.as_str().filter(|text| !text.is_empty())
}

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
