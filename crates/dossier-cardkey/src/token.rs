//! 授权凭证解析与验签。
//!
//! 格式：`base64url(payload) "." base64url(signature)`，无填充。
//! 签名对象 = [`SIGNING_CONTEXT`] + payload 原始字节，直接对收到的字节验签，不重新序列化。
//! 本模块只判断「签名真不真、载荷对不对」；机器码、时间由 [`crate::decide`] 判断。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::Signature;
use serde_json::Value;

use crate::keys::TrustedKey;

pub const SIGNING_CONTEXT: &[u8] = b"drama-dossier/license/v1\n";
const MAX_TOKEN_LEN: usize = 2048;
const FIELDS: [&str; 8] = ["exp", "iat", "kid", "lid", "mid", "plan", "until", "v"];
const MAX_SAFE_INT: u64 = (1 << 53) - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    Trial,
    Month,
    Quarter,
    Year,
    Lifetime,
}

impl Plan {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "trial" => Self::Trial,
            "month" => Self::Month,
            "quarter" => Self::Quarter,
            "year" => Self::Year,
            "lifetime" => Self::Lifetime,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trial => "trial",
            Self::Month => "month",
            Self::Quarter => "quarter",
            Self::Year => "year",
            Self::Lifetime => "lifetime",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Trial => "试用卡",
            Self::Month => "月卡",
            Self::Quarter => "季卡",
            Self::Year => "年卡",
            Self::Lifetime => "永久卡",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claims {
    pub kid: String,
    pub lid: u64,
    pub mid: String,
    pub plan: Plan,
    pub until: Option<u64>,
    pub iat: u64,
    pub exp: u64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TokenError {
    #[error("授权凭证格式错误")]
    Malformed,
    #[error("授权凭证签名无效")]
    BadSignature,
    #[error("授权凭证内容无效：{0}")]
    BadPayload(&'static str),
}

fn b64(text: &str) -> Result<Vec<u8>, TokenError> {
    // URL_SAFE_NO_PAD 拒绝填充与非法字符；再要求能原样往返，拒绝非规范编码。
    let raw = URL_SAFE_NO_PAD
        .decode(text)
        .map_err(|_| TokenError::Malformed)?;
    if URL_SAFE_NO_PAD.encode(&raw) != text {
        return Err(TokenError::Malformed);
    }
    Ok(raw)
}

/// 验签 + 严格解析。任一步失败都返回错误，调用方据此锁定。
pub fn verify_token(token: &str, trusted: &[TrustedKey]) -> Result<Claims, TokenError> {
    if token.is_empty() || token.len() > MAX_TOKEN_LEN {
        return Err(TokenError::Malformed);
    }
    let mut parts = token.split('.');
    let (Some(p64), Some(s64), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(TokenError::Malformed);
    };
    let payload = b64(p64)?;
    let sig_bytes = b64(s64)?;
    let sig_arr: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| TokenError::Malformed)?;
    let sig = Signature::from_bytes(&sig_arr);

    let mut message = Vec::with_capacity(SIGNING_CONTEXT.len() + payload.len());
    message.extend_from_slice(SIGNING_CONTEXT);
    message.extend_from_slice(&payload);
    let matched = trusted
        .iter()
        .find(|k| k.key.verify_strict(&message, &sig).is_ok())
        .ok_or(TokenError::BadSignature)?;

    let claims = parse_payload(&payload)?;
    if claims.kid != matched.kid {
        return Err(TokenError::BadPayload("kid 与签名公钥不符"));
    }
    Ok(claims)
}

fn reject_duplicate_keys(payload: &str) -> Result<(), TokenError> {
    // serde_json 遇到重复键会静默保留最后一个；这里先用流式反序列化逐个收集键名。
    use serde::de::{Deserializer, IgnoredAny, MapAccess, Visitor};
    struct Keys;
    impl<'de> Visitor<'de> for Keys {
        type Value = Result<(), &'static str>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut seen = std::collections::BTreeSet::new();
            while let Some(k) = map.next_key::<String>()? {
                if !seen.insert(k) {
                    return Ok(Err("payload 存在重复字段"));
                }
                map.next_value::<IgnoredAny>()?;
            }
            Ok(Ok(()))
        }
    }
    let mut de = serde_json::Deserializer::from_str(payload);
    match de.deserialize_map(Keys) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(msg)) => Err(TokenError::BadPayload(msg)),
        Err(_) => Err(TokenError::BadPayload("payload 不是 JSON 对象")),
    }
}

fn int(v: &Value, what: &'static str) -> Result<u64, TokenError> {
    // 只认 JSON 整数：bool、浮点、负数、超出安全整数范围的都拒绝
    match v {
        Value::Number(n) if n.is_u64() => {
            let x = n.as_u64().unwrap_or(u64::MAX);
            if x <= MAX_SAFE_INT {
                Ok(x)
            } else {
                Err(TokenError::BadPayload(what))
            }
        }
        _ => Err(TokenError::BadPayload(what)),
    }
}

fn parse_payload(payload: &[u8]) -> Result<Claims, TokenError> {
    let text =
        std::str::from_utf8(payload).map_err(|_| TokenError::BadPayload("payload 不是 UTF-8"))?;
    if !text.is_ascii() {
        return Err(TokenError::BadPayload("payload 含非 ASCII 字符"));
    }
    reject_duplicate_keys(text)?;
    let obj: serde_json::Map<String, Value> =
        serde_json::from_str(text).map_err(|_| TokenError::BadPayload("payload 不是 JSON 对象"))?;
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    if keys != FIELDS {
        return Err(TokenError::BadPayload("payload 字段不符"));
    }
    if int(&obj["v"], "版本号无效")? != 1 {
        return Err(TokenError::BadPayload("不支持的凭证版本"));
    }
    let kid = obj["kid"]
        .as_str()
        .filter(|s| s.len() == 16)
        .ok_or(TokenError::BadPayload("kid 格式错误"))?;
    let mid = obj["mid"]
        .as_str()
        .filter(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .ok_or(TokenError::BadPayload("机器码格式错误"))?;
    let plan = obj["plan"]
        .as_str()
        .and_then(Plan::parse)
        .ok_or(TokenError::BadPayload("卡种错误"))?;
    let lid = int(&obj["lid"], "lid 无效")?;
    let iat = int(&obj["iat"], "iat 无效")?;
    let exp = int(&obj["exp"], "exp 无效")?;
    let until = match &obj["until"] {
        Value::Null => None,
        v => Some(int(v, "until 无效")?),
    };
    if (plan == Plan::Lifetime) != until.is_none() {
        return Err(TokenError::BadPayload("永久卡与 until 不一致"));
    }
    if exp <= iat {
        return Err(TokenError::BadPayload("exp 必须晚于 iat"));
    }
    Ok(Claims {
        kid: kid.to_string(),
        lid,
        mid: mid.to_string(),
        plan,
        until,
        iat,
        exp,
    })
}
