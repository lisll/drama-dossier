//! 报审包清单（`manifest.json`）的签名校验。
//!
//! 清单由服务端生成并签名，签名另存在 `manifest.sig`：`{"kid": "…", "sig": "<base64url>"}`，
//! 签的是 `manifest.json` 的原始字节，前缀是 [`Purpose::PackageManifest`]。
//! 桌面端组装报审包前先验这里：签名不对、格式不认识、不是发给这台电脑的，一律拒绝。
//!
//! 清单里的路径要能安全地拼到输出目录下：只认相对路径，用 `/` 分隔，
//! 不许出现 `..`、空段、反斜杠、盘符、控制字符；哈希必须是 64 位小写十六进制。
//! 服务端本来就不会发这种内容，这里再挡一次，免得签名私钥出事时被写到包外面去。

use serde::Deserialize;
use serde_json::Value;

use crate::envelope::{self, EnvelopeError, Purpose};
use crate::keys::TrustedKey;

/// 当前清单格式。
pub const MANIFEST_SCHEMA: &str = "drama-dossier/package@2";

/// 清单校验失败的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ManifestError {
    #[error("报审包清单的签名格式不正确")]
    Malformed,
    #[error("报审包清单的签名无效：不是服务端签发的，或者清单被改动过")]
    BadSignature,
    #[error("报审包清单的内容无法识别")]
    BadPayload,
    #[error("这份报审包清单不是发给这台电脑的")]
    WrongDevice,
}

impl ManifestError {
    pub fn code(self) -> &'static str {
        match self {
            ManifestError::Malformed => "malformed",
            ManifestError::BadSignature => "bad_signature",
            ManifestError::BadPayload => "bad_payload",
            ManifestError::WrongDevice => "wrong_device",
        }
    }
}

impl From<EnvelopeError> for ManifestError {
    fn from(error: EnvelopeError) -> Self {
        match error {
            EnvelopeError::Malformed => ManifestError::Malformed,
            EnvelopeError::BadSignature => ManifestError::BadSignature,
        }
    }
}

/// 包里的一个文件。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManifestFile {
    pub path: String,
    pub size_bytes: u64,
    pub sha256: String,
    /// `server`：服务端发回的文档；`local`：桌面端从本机放入（封面、简介、复制进包的分集）。
    pub source: FileSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileSource {
    Server,
    Local,
}

/// 不复制进包、只登记哈希的分集。`path` 是剧目目录里的文件名。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManifestReference {
    pub path: String,
    pub size_bytes: u64,
    pub sha256: String,
}

/// 签章信息。`placeholder = true` 表示用的是占位章，不具法律效力。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManifestSignature {
    pub signer: String,
    pub placeholder: bool,
    pub documents: Vec<String>,
    /// 每份文档的实际签章方式（混合签章时各不相同）。
    ///
    /// 旧版服务端不发这个字段，所以给默认值空列表——向后兼容，不至于让旧包验不过。
    #[serde(default)]
    pub per_document: Vec<ManifestDocumentSigner>,
}

/// 一份盖章文档的实际签章方式。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManifestDocumentSigner {
    pub document: String,
    /// `mock`（占位章）或 `scan`（扫描件）。
    pub signer: String,
}

/// 验签通过的清单。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    pub schema: String,
    pub package_id: String,
    pub generated_at: String,
    pub user_id: String,
    pub device_id: String,
    pub drama_name: String,
    pub ai_generated: bool,
    pub copy_episodes: bool,
    pub signature: ManifestSignature,
    pub files: Vec<ManifestFile>,
    pub references: Vec<ManifestReference>,
}

/// `manifest.sig` 的内容。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignatureFile {
    kid: String,
    sig: String,
}

/// 验 `manifest.json` 的原始字节和 `manifest.sig`，通过后返回解析好的清单。
pub fn verify(
    manifest_bytes: &[u8],
    signature_file: &[u8],
    trusted: &[TrustedKey],
    device_id: &str,
) -> Result<Manifest, ManifestError> {
    let signature = envelope::parse_strict_json(signature_file).ok_or(ManifestError::Malformed)?;
    let signature: SignatureFile =
        serde_json::from_value(signature).map_err(|_| ManifestError::Malformed)?;
    let signed_by = envelope::verify_detached(
        Purpose::PackageManifest,
        manifest_bytes,
        &signature.sig,
        trusted,
    )?;
    // 签名文件里写的 kid 只是提示，以实际验过签的公钥为准；两者对不上说明签名文件被拼接过。
    if signature.kid != signed_by {
        return Err(ManifestError::BadSignature);
    }
    let value = envelope::parse_strict_json(manifest_bytes).ok_or(ManifestError::BadPayload)?;
    if value.get("schema").and_then(Value::as_str) != Some(MANIFEST_SCHEMA) {
        return Err(ManifestError::BadPayload);
    }
    let manifest: Manifest =
        serde_json::from_value(value).map_err(|_| ManifestError::BadPayload)?;
    validate(&manifest)?;
    if manifest.device_id != device_id {
        return Err(ManifestError::WrongDevice);
    }
    Ok(manifest)
}

fn validate(manifest: &Manifest) -> Result<(), ManifestError> {
    let bad = ManifestError::BadPayload;
    let mut seen = std::collections::HashSet::new();
    for file in &manifest.files {
        if !safe_relative_path(&file.path) || !is_sha256(&file.sha256) {
            return Err(bad);
        }
        // 包里的路径不区分大小写去重：Windows / macOS 默认的文件系统不区分大小写。
        if !seen.insert(file.path.to_lowercase()) {
            return Err(bad);
        }
    }
    for reference in &manifest.references {
        // 引用的是剧目目录里的文件名：只许一段，不许带目录。
        if !safe_relative_path(&reference.path)
            || reference.path.contains('/')
            || !is_sha256(&reference.sha256)
        {
            return Err(bad);
        }
    }
    for document in &manifest.signature.documents {
        if !manifest.files.iter().any(|file| &file.path == document) {
            return Err(bad);
        }
    }
    if manifest.package_id.is_empty()
        || manifest.user_id.is_empty()
        || manifest.device_id.is_empty()
    {
        return Err(bad);
    }
    Ok(())
}

/// 能安全拼到输出目录下的相对路径。
pub fn safe_relative_path(path: &str) -> bool {
    const RESERVED: &[char] = &['\\', ':', '*', '?', '"', '<', '>', '|'];
    !path.is_empty()
        && path.len() <= 512
        && !path.starts_with('/')
        && !path
            .chars()
            .any(|c| c.is_control() || RESERVED.contains(&c))
        && path.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                // Windows 会悄悄去掉结尾的点和空格，「a.」和「a」会落到同一个文件上。
                && !segment.ends_with('.')
                && !segment.ends_with(' ')
        })
}

/// 64 位小写十六进制。
pub fn is_sha256(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
