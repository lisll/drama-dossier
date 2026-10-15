//! 登录状态的本地存储。
//!
//! - Windows：凭据管理器；macOS：钥匙串。都通过 `keyring` 调用。
//! - Linux：沿用 `dossier_runtime::SecretStore`（权限 0600 的文件），目录是 `<app_data>/credentials`。
//!
//! 两条记录：`session`（刷新令牌、账号 ID、邮箱）和 `license`（授权凭证、见过的最晚时间）。
//! 访问令牌只放在内存里，不落盘。
//!
//! keyring 的平台 feature 漏开时，会退回到只存在内存里的假实现而且不报错。
//! 所以启动时做一次 [`CredentialStore::self_test`]：写入后用新的句柄读回来，读不回来就明确报错。

use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// 钥匙串里的服务名，和应用标识一致（Linux 的文件存储用不到）。
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
pub const SERVICE: &str = "dev.dramadossier.app";
pub const SESSION: &str = "session";
pub const LICENSE: &str = "license";
const SELF_TEST: &str = "self-test";

/// 存储出错。消息是中文，不含存储的内容。
#[derive(Debug, Clone)]
pub struct StoreError(pub String);

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

pub trait CredentialStore: Send + Sync + 'static {
    fn load(&self, name: &str) -> Result<Option<String>, StoreError>;
    fn save(&self, name: &str, value: &str) -> Result<(), StoreError>;
    /// 不存在也算成功。
    fn delete(&self, name: &str) -> Result<(), StoreError>;

    /// 写入一个随机值，再读回来比对，最后删掉。读不回来说明存储不可用（或者是假实现）。
    fn self_test(&self) -> Result<(), StoreError> {
        let probe = format!("probe-{}-{}", std::process::id(), nanos());
        self.save(SELF_TEST, &probe)?;
        let read_back = self.load(SELF_TEST);
        let _ = self.delete(SELF_TEST);
        match read_back? {
            Some(value) if value == probe => Ok(()),
            _ => Err(StoreError("系统钥匙串不可用：写进去的内容读不回来".into())),
        }
    }
}

fn nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos())
}

/// 刷新令牌和账号信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub v: u32,
    pub refresh_token: String,
    pub user_id: String,
    pub email: String,
}

/// 授权凭证和「见过的最晚时间」放在同一条记录里：删掉时间，凭证也一起没了，只能重新联网。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LicenseRecord {
    pub v: u32,
    pub token: String,
    pub high_water: u64,
}

pub const RECORD_VERSION: u32 = 1;

/// 读一条 JSON 记录。内容损坏或版本不认识时按「没有」处理并记日志：只能重新登录，不会放宽任何检查。
pub fn load_record<T: DeserializeOwned>(
    store: &dyn CredentialStore,
    name: &str,
) -> Result<Option<T>, StoreError> {
    let Some(text) = store.load(name)? else {
        return Ok(None);
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) if value.get("v").and_then(|v| v.as_u64()) == Some(RECORD_VERSION.into()) => {
            serde_json::from_value(value).map(Some).or_else(|_| {
                eprintln!("[account] 本地记录 {name} 内容损坏，已忽略");
                Ok(None)
            })
        }
        _ => {
            eprintln!("[account] 本地记录 {name} 无法识别，已忽略");
            Ok(None)
        }
    }
}

pub fn save_record<T: Serialize>(
    store: &dyn CredentialStore,
    name: &str,
    record: &T,
) -> Result<(), StoreError> {
    let text = serde_json::to_string(record)
        .map_err(|error| StoreError(format!("序列化本地记录失败：{error}")))?;
    store.save(name, &text)
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub use keyring_store::KeyringStore as PlatformStore;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub use file_store::FileStore as PlatformStore;

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod keyring_store {
    use super::{CredentialStore, StoreError, SERVICE};

    /// 系统钥匙串。每次操作新建一个 `Entry`，自检时才能分辨出「只存在句柄里」的假实现。
    pub struct KeyringStore;

    impl KeyringStore {
        pub fn open(_root: std::path::PathBuf) -> Self {
            KeyringStore
        }
    }

    fn entry(name: &str) -> Result<keyring::Entry, StoreError> {
        keyring::Entry::new(SERVICE, name)
            .map_err(|error| StoreError(format!("打开系统钥匙串失败：{error}")))
    }

    impl CredentialStore for KeyringStore {
        fn load(&self, name: &str) -> Result<Option<String>, StoreError> {
            match entry(name)?.get_password() {
                Ok(value) => Ok(Some(value)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(error) => Err(StoreError(format!("读取系统钥匙串失败：{error}"))),
            }
        }

        fn save(&self, name: &str, value: &str) -> Result<(), StoreError> {
            entry(name)?
                .set_password(value)
                .map_err(|error| StoreError(format!("写入系统钥匙串失败：{error}")))
        }

        fn delete(&self, name: &str) -> Result<(), StoreError> {
            match entry(name)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(StoreError(format!("删除系统钥匙串记录失败：{error}"))),
            }
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
mod file_store {
    use super::{CredentialStore, StoreError};
    use dossier_runtime::SecretStore;

    /// Linux：权限 0600 的文件，明确不是系统级密钥库。
    pub struct FileStore(SecretStore);

    impl FileStore {
        pub fn open(root: std::path::PathBuf) -> Self {
            FileStore(SecretStore::new(root))
        }
    }

    impl CredentialStore for FileStore {
        fn load(&self, name: &str) -> Result<Option<String>, StoreError> {
            self.0
                .get_optional(name)
                .map_err(|error| StoreError(format!("读取登录状态失败：{error}")))
        }

        fn save(&self, name: &str, value: &str) -> Result<(), StoreError> {
            self.0
                .put(name, value)
                .map(|_| ())
                .map_err(|error| StoreError(format!("保存登录状态失败：{error}")))
        }

        fn delete(&self, name: &str) -> Result<(), StoreError> {
            self.0
                .delete(name)
                .map(|_| ())
                .map_err(|error| StoreError(format!("删除登录状态失败：{error}")))
        }
    }
}
