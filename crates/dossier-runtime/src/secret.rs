//! 本地凭据存储。
//!
//! **这不是 OS 级密钥库**。Linux 上没有可用的 keyring 后端（本机无 `libsecret`
//! 绑定、也没有桌面会话），所以这里做的是「权限收紧 + 原子写 + 不明文外泄」的
//! 文件存储：目录 `0700`、文件 `0600`，写入走临时文件 + `rename`，读取接口返回
//! `Result` 而不是 `Option`，避免「读不到就当没有」把配置错误当成空值。
//!
//! 明确的能力边界（写进文档，不假装具备）：
//! - 不能抵御拿到本机 root/同用户权限的攻击者；
//! - 不加密内容（加密需要密钥，而密钥又要有地方放，属于自欺欺人）；
//! - 不参与任何网络传输；值永不进入日志（提供 [`mask`] 用于展示）。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::digest::{sha256_bytes, short};

const MAX_NAME_LEN: usize = 128;
const MAX_VALUE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Error)]
pub enum SecretError {
    #[error(
        "凭据名 `{name}` 非法：只允许字母、数字、点、下划线和短横线，长度 1–128，不能以点开头"
    )]
    InvalidName { name: String },
    #[error("凭据 `{name}` 不存在")]
    NotFound { name: String },
    #[error("凭据 `{name}` 超过 65536 字节上限")]
    TooLarge { name: String },
    #[error("凭据文件操作失败（{path}）：{source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("无法确定凭据目录：既没有 XDG_DATA_HOME / HOME，也没有 APPDATA")]
    NoHome,
    #[error("凭据文件权限过宽（{path}，mode {mode:o}）：同机其它用户可能读到内容")]
    InsecurePermissions { path: PathBuf, mode: u32 },
}

/// 一条凭据的元信息（**不含值**）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretMeta {
    pub name: String,
    pub updated_at: Option<String>,
    pub bytes: u64,
    /// 值摘要的前 12 位，用于「是不是同一个值」的比对，不泄露内容。
    pub digest_prefix: String,
}

/// 文件型凭据存储。
#[derive(Debug, Clone)]
pub struct SecretStore {
    root: PathBuf,
}

impl SecretStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 默认位置：`$XDG_DATA_HOME/dossier/secrets`，退回 `$HOME/.local/share/dossier/secrets`，
    /// Windows 上退回 `%APPDATA%\dossier\secrets`（解析规则见 [`crate::paths`]）。
    ///
    /// 不放在仓库目录里——凭据绝不能进版本库。
    pub fn with_default_root() -> Result<Self, SecretError> {
        let root = crate::paths::default_data_root()
            .ok_or(SecretError::NoHome)?
            .join("dossier")
            .join("secrets");
        Ok(Self::new(root))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 写入（或覆盖）一条凭据，返回不含值的元信息。
    pub fn put(&self, name: &str, value: &str) -> Result<SecretMeta, SecretError> {
        validate_name(name)?;
        if value.len() as u64 > MAX_VALUE_BYTES {
            return Err(SecretError::TooLarge {
                name: name.to_string(),
            });
        }
        self.ensure_root()?;
        let path = self.path_for(name);
        // 原子写：同目录临时文件 → fsync 后 rename，避免断电留下半个凭据。
        let temp = self
            .root
            .join(format!(".{name}.{}.tmp", std::process::id()));
        write_private(&temp, value.as_bytes())?;
        fs::rename(&temp, &path).map_err(|source| {
            let _ = fs::remove_file(&temp);
            SecretError::Io {
                path: path.clone(),
                source,
            }
        })?;
        Ok(meta_for(name, value.as_bytes(), Some(Utc::now())))
    }

    /// 读取一条凭据的值。
    pub fn get(&self, name: &str) -> Result<String, SecretError> {
        validate_name(name)?;
        let path = self.path_for(name);
        let bytes = fs::read(&path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                SecretError::NotFound {
                    name: name.to_string(),
                }
            } else {
                SecretError::Io {
                    path: path.clone(),
                    source,
                }
            }
        })?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// 读取一条凭据；不存在时返回 `None`（存在但读失败仍报错）。
    pub fn get_optional(&self, name: &str) -> Result<Option<String>, SecretError> {
        match self.get(name) {
            Ok(value) => Ok(Some(value)),
            Err(SecretError::NotFound { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn has(&self, name: &str) -> Result<bool, SecretError> {
        validate_name(name)?;
        Ok(self.path_for(name).is_file())
    }

    /// 删除一条凭据；返回「原本是否存在」。
    pub fn delete(&self, name: &str) -> Result<bool, SecretError> {
        validate_name(name)?;
        let path = self.path_for(name);
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(SecretError::Io { path, source }),
        }
    }

    /// 列出所有凭据的元信息（不含值）。
    pub fn list(&self) -> Result<Vec<SecretMeta>, SecretError> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(source) => {
                return Err(SecretError::Io {
                    path: self.root.clone(),
                    source,
                })
            }
        };
        for entry in entries {
            let entry = entry.map_err(|source| SecretError::Io {
                path: self.root.clone(),
                source,
            })?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if !path.is_file() || !is_valid_name(name) {
                continue;
            }
            let bytes = fs::read(&path).map_err(|source| SecretError::Io {
                path: path.clone(),
                source,
            })?;
            let modified = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .map(DateTime::<Utc>::from);
            out.push(meta_for(name, &bytes, modified));
        }
        out.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(out)
    }

    /// 检查目录与文件权限是否够紧；返回需要提醒的说明。
    ///
    /// 由 `dossier-runtime selfcheck` 调用：权限过宽不算致命错误（Windows 上就没有
    /// POSIX 权限），但必须显式告知，而不是默认「安全」。
    pub fn permission_warnings(&self) -> Result<Vec<String>, SecretError> {
        let mut warnings = Vec::new();
        if !self.root.exists() {
            return Ok(warnings);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let root_mode = fs::metadata(&self.root)
                .map_err(|source| SecretError::Io {
                    path: self.root.clone(),
                    source,
                })?
                .permissions()
                .mode()
                & 0o777;
            if root_mode & 0o077 != 0 {
                warnings.push(format!(
                    "凭据目录 {} 权限为 {root_mode:o}，建议 700（同机其它用户可能读到）",
                    self.root.display()
                ));
            }
            for meta in self.list()? {
                let path = self.path_for(&meta.name);
                let mode = fs::metadata(&path)
                    .map_err(|source| SecretError::Io {
                        path: path.clone(),
                        source,
                    })?
                    .permissions()
                    .mode()
                    & 0o777;
                if mode & 0o077 != 0 {
                    warnings.push(format!(
                        "凭据文件 {} 权限为 {mode:o}，建议 600",
                        path.display()
                    ));
                }
            }
        }
        #[cfg(not(unix))]
        {
            warnings.push(
                "当前平台没有 POSIX 权限模型，凭据文件仅依赖用户目录 ACL；\
                 不要把高价值长期令牌放进来"
                    .to_string(),
            );
        }
        Ok(warnings)
    }

    fn path_for(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn ensure_root(&self) -> Result<(), SecretError> {
        if self.root.is_dir() {
            return Ok(());
        }
        create_dir_private(&self.root)
    }
}

fn meta_for(name: &str, bytes: &[u8], updated_at: Option<DateTime<Utc>>) -> SecretMeta {
    SecretMeta {
        name: name.to_string(),
        updated_at: updated_at.map(|time| time.to_rfc3339()),
        bytes: bytes.len() as u64,
        digest_prefix: short(&sha256_bytes(bytes), 12),
    }
}

/// 校验凭据名。
pub fn validate_name(name: &str) -> Result<(), SecretError> {
    if is_valid_name(name) {
        Ok(())
    } else {
        Err(SecretError::InvalidName {
            name: name.to_string(),
        })
    }
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// 写文件并把权限收到 0600（POSIX）。
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), SecretError> {
    let io = |source: std::io::Error| SecretError::Io {
        path: path.to_path_buf(),
        source,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(io)?;
        file.write_all(bytes).map_err(io)?;
        file.sync_all().map_err(io)?;
    }
    #[cfg(not(unix))]
    {
        let mut file = fs::File::create(path).map_err(io)?;
        file.write_all(bytes).map_err(io)?;
        file.sync_all().map_err(io)?;
    }
    Ok(())
}

fn create_dir_private(path: &Path) -> Result<(), SecretError> {
    let io = |source: std::io::Error| SecretError::Io {
        path: path.to_path_buf(),
        source,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(io)?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path).map_err(io)?;
    }
    Ok(())
}

/// 展示用的脱敏形式：只暴露长度，绝不暴露内容片段。
pub fn mask(value: &str) -> String {
    format!("***（{} 字符）", value.chars().count())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("dossier-secret-{tag}-{}", std::process::id()))
    }

    #[test]
    fn round_trips_a_secret() {
        let root = temp_root("roundtrip");
        let _ = fs::remove_dir_all(&root);
        let store = SecretStore::new(&root);

        assert!(store.get_optional("cloud.token").unwrap().is_none());
        assert!(!store.has("cloud.token").unwrap());

        let meta = store.put("cloud.token", "super-secret-value").unwrap();
        assert_eq!(meta.name, "cloud.token");
        assert_eq!(meta.bytes, "super-secret-value".len() as u64);
        assert_eq!(meta.digest_prefix.len(), 12);
        assert!(meta.updated_at.is_some());
        // 元信息里绝不能出现值本身。
        let serialized = serde_json::to_string(&meta).unwrap();
        assert!(!serialized.contains("super-secret-value"));

        assert_eq!(store.get("cloud.token").unwrap(), "super-secret-value");
        assert_eq!(
            store.get_optional("cloud.token").unwrap().as_deref(),
            Some("super-secret-value")
        );
        assert!(store.has("cloud.token").unwrap());

        // 覆盖写：值变了，摘要前缀也要变。
        let updated = store.put("cloud.token", "rotated-value").unwrap();
        assert_ne!(updated.digest_prefix, meta.digest_prefix);
        assert_eq!(store.get("cloud.token").unwrap(), "rotated-value");

        assert!(store.delete("cloud.token").unwrap());
        assert!(!store.delete("cloud.token").unwrap());
        assert!(matches!(
            store.get("cloud.token").unwrap_err(),
            SecretError::NotFound { .. }
        ));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_unsafe_names_and_missing_secret() {
        let root = temp_root("names");
        let _ = fs::remove_dir_all(&root);
        let store = SecretStore::new(&root);
        for bad in [
            "",
            ".hidden",
            "with/slash",
            "with\\backslash",
            "with space",
            "../escape",
        ] {
            assert!(
                matches!(
                    store.put(bad, "x").unwrap_err(),
                    SecretError::InvalidName { .. }
                ),
                "名字 {bad:?} 应当被拒绝"
            );
            assert!(matches!(
                store.get(bad).unwrap_err(),
                SecretError::InvalidName { .. }
            ));
        }
        let long = "a".repeat(MAX_NAME_LEN + 1);
        assert!(matches!(
            store.put(&long, "x").unwrap_err(),
            SecretError::InvalidName { .. }
        ));
        assert!(matches!(
            store.get("missing").unwrap_err(),
            SecretError::NotFound { .. }
        ));
    }

    #[test]
    fn lists_metadata_without_values() {
        let root = temp_root("list");
        let _ = fs::remove_dir_all(&root);
        let store = SecretStore::new(&root);
        store.put("b.token", "bbb").unwrap();
        store.put("a.token", "aaa").unwrap();
        let list = store.list().unwrap();
        assert_eq!(
            list.iter()
                .map(|meta| meta.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a.token", "b.token"]
        );
        assert!(list.iter().all(|meta| meta.bytes == 3));
        assert_eq!(store.list().unwrap().len(), 2);
        // 临时文件不应当出现在列表里。
        assert!(list.iter().all(|meta| !meta.name.ends_with(".tmp")));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn permissions_are_tightened_on_unix() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let root = temp_root("perms");
            let _ = fs::remove_dir_all(&root);
            let store = SecretStore::new(&root);
            store.put("t", "v").unwrap();
            let dir_mode = fs::metadata(&root).unwrap().permissions().mode() & 0o777;
            let file_mode = fs::metadata(root.join("t")).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700);
            assert_eq!(file_mode, 0o600);
            assert!(store.permission_warnings().unwrap().is_empty());

            // 人为放宽权限后必须报警。
            fs::set_permissions(root.join("t"), fs::Permissions::from_mode(0o644)).unwrap();
            let warnings = store.permission_warnings().unwrap();
            assert_eq!(warnings.len(), 1);
            assert!(warnings[0].contains("建议 600"));
            let _ = fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn mask_never_leaks_content() {
        let masked = mask("abcdefgh");
        assert!(!masked.contains("abcd"));
        assert!(masked.contains("8 字符"));
    }

    #[test]
    fn default_root_uses_the_platform_data_dir() {
        // 不改环境，只验证本机能给出一个绝对路径。各来源的优先级由
        // `paths::tests` 覆盖（那里注入环境，不依赖真实机器）。
        let store = SecretStore::with_default_root().unwrap();
        assert!(store.root().is_absolute());
        assert!(store.root().ends_with("secrets"));
    }

    #[test]
    fn too_large_value_is_rejected() {
        let root = temp_root("large");
        let _ = fs::remove_dir_all(&root);
        let store = SecretStore::new(&root);
        let big = "x".repeat((MAX_VALUE_BYTES + 1) as usize);
        assert!(matches!(
            store.put("big", &big).unwrap_err(),
            SecretError::TooLarge { .. }
        ));
        assert!(!root.exists() || store.list().unwrap().is_empty());
        let _ = fs::remove_dir_all(&root);
    }
}
