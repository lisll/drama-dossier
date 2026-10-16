//! 受信的授权签名公钥列表。
//!
//! 做成列表、验签时逐把尝试，是为密钥轮换留位置（`COMMERCE.md` 5.1 节）：
//! 先发布同时信任新旧两把公钥的桌面端，服务端再换私钥，下一个版本去掉旧公钥。
//!
//! 公钥来源：
//! - [`BUILTIN_KEYS`]：内置在源码里的正式公钥。现在还没有正式服务端，列表是空的；
//! - 编译时环境变量 `DOSSIER_LICENSE_PUBKEYS`（逗号分隔的 base64url），追加在后面。
//!   开发和真机测试用它注入本机 `drama-commerce keyinfo` 打印的 `public_key`。
//!
//! 测试向量的签名种子是公开的，任何人都能用它签名：列表里出现测试公钥，一律按配置错误处理。

use std::sync::OnceLock;

use ed25519_dalek::VerifyingKey;
use sha2::{Digest, Sha256};

use crate::envelope::b64url_decode;

/// 内置的正式公钥（32 字节公钥的 base64url，不带填充）。
pub const BUILTIN_KEYS: &[&str] = &[];

/// 编译时注入的公钥，逗号分隔。
const INJECTED_KEYS: Option<&str> = option_env!("DOSSIER_LICENSE_PUBKEYS");

/// 测试向量 `license_vectors_v1.json` 里两个公开种子对应的公钥。绝不能被信任。
const TEST_VECTOR_KEYS: &[&str] = &[
    "A6EHv_POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbg",
    "Kay64UG8yvCyLhqU000LxzYeUm0L_hLIl5S8kyKWbdc",
];

/// 一把受信公钥和它的指纹。
#[derive(Debug, Clone)]
pub struct TrustedKey {
    key: VerifyingKey,
    kid: String,
}

impl TrustedKey {
    /// 从 32 字节公钥构造。弱公钥（小阶点）拒绝。
    pub fn from_bytes(bytes: &[u8; 32]) -> Option<Self> {
        let key = VerifyingKey::from_bytes(bytes).ok()?;
        if key.is_weak() {
            return None;
        }
        Some(Self {
            key,
            kid: key_id(bytes),
        })
    }

    /// 从 base64url（不带填充）构造，写法必须是规范形式。
    pub fn from_b64url(text: &str) -> Option<Self> {
        let raw = b64url_decode(text).ok()?;
        let bytes: [u8; 32] = raw.as_slice().try_into().ok()?;
        Self::from_bytes(&bytes)
    }

    pub fn verifying_key(&self) -> &VerifyingKey {
        &self.key
    }

    /// 公钥指纹：公钥 SHA-256 的前 16 位十六进制，和服务端 `key_id` 一致。
    pub fn kid(&self) -> &str {
        &self.kid
    }
}

/// 公钥指纹：SHA-256 的前 8 字节，转成 16 位小写十六进制。
pub fn key_id(public_key: &[u8]) -> String {
    Sha256::digest(public_key)[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 公钥配置有问题。出现这类错误时一律验签失败，界面明确提示，不会悄悄放行。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("安装包里没有配置授权公钥，请重新下载安装包")]
    Empty,
    #[error("安装包里的第 {0} 个授权公钥格式不正确，请重新下载安装包")]
    Invalid(usize),
    #[error("安装包里的第 {0} 个授权公钥是公开的测试公钥，正式环境不能信任，请重新下载安装包")]
    TestVector(usize),
}

/// 解析公钥列表：去掉空白和空项，按顺序编号（从 1 开始）报错。
pub fn parse_key_list<'a>(
    entries: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<TrustedKey>, KeyError> {
    let test_keys: Vec<Vec<u8>> = TEST_VECTOR_KEYS
        .iter()
        .filter_map(|text| b64url_decode(text).ok())
        .collect();
    let mut keys = Vec::new();
    for (index, entry) in entries
        .into_iter()
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .enumerate()
    {
        let position = index + 1;
        let raw = b64url_decode(entry).map_err(|_| KeyError::Invalid(position))?;
        if test_keys.contains(&raw) {
            return Err(KeyError::TestVector(position));
        }
        let bytes: [u8; 32] = raw
            .as_slice()
            .try_into()
            .map_err(|_| KeyError::Invalid(position))?;
        keys.push(TrustedKey::from_bytes(&bytes).ok_or(KeyError::Invalid(position))?);
    }
    if keys.is_empty() {
        return Err(KeyError::Empty);
    }
    Ok(keys)
}

/// 本程序信任的全部公钥：内置列表加编译时注入的列表。第一次调用时解析，结果缓存。
pub fn trusted_keys() -> Result<&'static [TrustedKey], KeyError> {
    static KEYS: OnceLock<Result<Vec<TrustedKey>, KeyError>> = OnceLock::new();
    KEYS.get_or_init(|| {
        let injected = INJECTED_KEYS.unwrap_or("").split(',');
        parse_key_list(BUILTIN_KEYS.iter().copied().chain(injected))
    })
    .as_deref()
    .map_err(Clone::clone)
}
