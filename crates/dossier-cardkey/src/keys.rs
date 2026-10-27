//! 受信任的授权公钥。
//!
//! `PRODUCTION_KEYS` 是正式包唯一信任的公钥列表（base64url，无填充，32 字节）。
//! 支持多把：密钥轮换期间同时信任新旧公钥。初始为空——为空时一律锁定，
//! 绝不放行；部署服务器后用 `license-admin pubkey` 的输出填入。

use std::sync::OnceLock;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::VerifyingKey;
use sha2::{Digest, Sha256};

/// 正式包信任的公钥。上线前必须填入，见 docs/LICENSING.md「上线检查清单」。
pub const PRODUCTION_KEYS: &[&str] = &[];

/// 公开的测试公钥（种子为 0..32 与 32..64）。出现在生产列表里即视为配置错误。
pub const TEST_VECTOR_KEYS: &[&str] = &[
    "A6EHv_POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbg",
    "Kay64UG8yvCyLhqU000LxzYeUm0L_hLIl5S8kyKWbdc",
];

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum KeyError {
    #[error("授权公钥未配置")]
    Empty,
    #[error("授权公钥格式错误：{0}")]
    Invalid(String),
    #[error("生产公钥列表里混入了公开测试公钥")]
    TestVector,
}

#[derive(Clone)]
pub struct TrustedKey {
    pub kid: String,
    pub key: VerifyingKey,
}

impl TrustedKey {
    pub fn from_b64url(text: &str) -> Result<Self, KeyError> {
        let raw = URL_SAFE_NO_PAD
            .decode(text.trim())
            .map_err(|_| KeyError::Invalid(text.to_string()))?;
        let bytes: [u8; 32] = raw
            .as_slice()
            .try_into()
            .map_err(|_| KeyError::Invalid(text.to_string()))?;
        let key =
            VerifyingKey::from_bytes(&bytes).map_err(|_| KeyError::Invalid(text.to_string()))?;
        Ok(Self {
            kid: key_id(&bytes),
            key,
        })
    }
}

/// kid = sha256(公钥原始 32 字节) 的前 16 位十六进制，与服务端一致。
pub fn key_id(raw: &[u8; 32]) -> String {
    let digest = Sha256::digest(raw);
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_key_list(list: &[&str]) -> Result<Vec<TrustedKey>, KeyError> {
    if list.is_empty() {
        return Err(KeyError::Empty);
    }
    list.iter().map(|k| TrustedKey::from_b64url(k)).collect()
}

/// 生产公钥（进程内缓存）。
pub fn production_keys() -> Result<&'static [TrustedKey], KeyError> {
    static CACHE: OnceLock<Result<Vec<TrustedKey>, KeyError>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            if PRODUCTION_KEYS
                .iter()
                .any(|k| TEST_VECTOR_KEYS.contains(&k.trim()))
            {
                return Err(KeyError::TestVector);
            }
            parse_key_list(PRODUCTION_KEYS)
        })
        .as_ref()
        .map(Vec::as_slice)
        .map_err(Clone::clone)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_keys_never_contain_test_vectors() {
        for k in PRODUCTION_KEYS {
            assert!(
                !TEST_VECTOR_KEYS.contains(&k.trim()),
                "生产公钥列表里出现了测试公钥 {k}"
            );
        }
    }

    #[test]
    fn test_vector_keys_parse() {
        let keys = parse_key_list(TEST_VECTOR_KEYS).unwrap();
        assert_eq!(keys.len(), 2);
        assert_ne!(keys[0].kid, keys[1].kid);
    }

    #[test]
    fn empty_and_invalid_lists_error() {
        assert!(matches!(parse_key_list(&[]), Err(KeyError::Empty)));
        assert!(matches!(
            parse_key_list(&["not-a-key"]),
            Err(KeyError::Invalid(_))
        ));
        assert!(matches!(
            parse_key_list(&["AAAA"]),
            Err(KeyError::Invalid(_))
        ));
    }
}
