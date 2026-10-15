//! 本地演示版账号：单账号注册 / 登录 / 改密 / 登出。
//!
//! 设计边界：
//! - **本地演示**：账号保存在本机 JSON 文件（桌面层负责路径），验证码由前端生成回显，
//!   不发邮件、不联网；报审包始终只生成在本地目录，与账号无关。
//! - **预留云端接入口**：对外函数签名（入参 / 出参）与未来云端版本一致，
//!   届时只替换内部实现为 HTTP 调用，调用方（src-tauri 命令层）不动。
//! - 密码哈希用 `SHA256(salt + password)`：本地单机演示足够，云端版会换成服务端方案。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 账号操作失败原因（文案直接面向界面展示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountError {
    /// 邮箱格式不合法。
    InvalidEmail,
    /// 密码不满足「≥8 位且同时含字母与数字」。
    WeakPassword,
    /// 该邮箱已注册（本地演示版仅支持一个账号）。
    AlreadyRegistered,
    /// 尚未注册任何账号。
    NotRegistered,
    /// 邮箱与注册时不一致。
    EmailMismatch,
    /// 原密码不正确。
    WrongPassword,
    /// 当前未登录却执行了需要登录的操作。
    NotLoggedIn,
}

impl std::fmt::Display for AccountError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            AccountError::InvalidEmail => "邮箱格式不正确",
            AccountError::WeakPassword => "密码需至少 8 位，且同时包含字母和数字",
            AccountError::AlreadyRegistered => "该邮箱已注册，请直接登录",
            AccountError::NotRegistered => "尚未注册账号",
            AccountError::EmailMismatch => "邮箱与注册邮箱不一致",
            AccountError::WrongPassword => "密码不正确",
            AccountError::NotLoggedIn => "尚未登录",
        };
        f.write_str(text)
    }
}

impl std::error::Error for AccountError {}

/// 单个账号记录（持久化在账号文件中）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AccountRecord {
    pub email: String,
    /// 16 字节随机数的十六进制串。
    pub salt: String,
    /// `SHA256(salt + password)` 的十六进制串。
    pub password_hash: String,
    pub created_at: String,
}

/// 账号文件整体结构（桌面层读写 JSON）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountStore {
    /// 本地演示版只支持一个账号；`None` 表示尚未注册。
    pub account: Option<AccountRecord>,
    pub logged_in: bool,
}

/// 返回给界面的账号状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountStatus {
    pub logged_in: bool,
    /// 未登录时为 `None`。
    pub email: Option<String>,
}

impl AccountStatus {
    fn from_store(store: &AccountStore) -> Self {
        Self {
            logged_in: store.logged_in,
            email: store
                .logged_in
                .then(|| store.account.as_ref().map(|a| a.email.clone()))
                .flatten(),
        }
    }
}

/// 邮箱格式校验：宽松版（本地段 + `@` + 域名带点），云端版会换成服务端校验。
pub fn validate_email(email: &str) -> Result<(), AccountError> {
    let email = email.trim();
    let ok = {
        let mut parts = email.split('@');
        let local = parts.next().unwrap_or("");
        let domain = parts.next().unwrap_or("");
        parts.next().is_none()
            && !local.is_empty()
            && domain.contains('.')
            && !domain.starts_with('.')
            && !domain.ends_with('.')
            && !email.contains(char::is_whitespace)
    };
    if ok {
        Ok(())
    } else {
        Err(AccountError::InvalidEmail)
    }
}

/// 密码规则：≥8 位且同时含字母与数字（与竞品一致）。
pub fn validate_password(password: &str) -> Result<(), AccountError> {
    let ok = password.len() >= 8
        && password.chars().any(|c| c.is_ascii_alphabetic())
        && password.chars().any(|c| c.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        Err(AccountError::WeakPassword)
    }
}

/// `SHA256(salt + password)` 的十六进制串。
pub fn hash_password(salt: &str, password: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt.as_bytes());
    hasher.update(password.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// 生成 16 字节随机盐的十六进制串：优先读系统熵源，失败时回退到时间纳秒。
pub fn generate_salt() -> String {
    let mut bytes = [0u8; 16];
    let mut filled = false;
    // 注意：/dev/urandom 是无限流，不能用 fs::read（永不 EOF），必须限定读取字节数。
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        if std::io::Read::read_exact(&mut file, &mut bytes).is_ok() {
            filled = true;
        }
    }
    if !filled {
        // 回退熵源：时间纳秒 + 进程 id 拼接后哈希（本地演示可接受）。
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let pid = std::process::id();
        let mut hasher = Sha256::new();
        hasher.update(nanos.to_le_bytes());
        hasher.update(pid.to_le_bytes());
        let digest = hasher.finalize();
        bytes.copy_from_slice(&digest[..16]);
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn now_iso() -> String {
    // 无 chrono 依赖诉求时用 std 时间戳（秒）；仅作展示用途。
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    format!("{secs}")
}

/// 注册：校验邮箱 / 密码格式；本地演示版已有账号时拒绝。
/// 成功后视为已登录（注册即登录，与竞品一致）。
pub fn register(
    store: &mut AccountStore,
    email: &str,
    password: &str,
) -> Result<AccountStatus, AccountError> {
    if store.account.is_some() {
        return Err(AccountError::AlreadyRegistered);
    }
    let email = email.trim().to_string();
    validate_email(&email)?;
    validate_password(password)?;
    let salt = generate_salt();
    store.account = Some(AccountRecord {
        email,
        salt: salt.clone(),
        password_hash: hash_password(&salt, password),
        created_at: now_iso(),
    });
    store.logged_in = true;
    Ok(AccountStatus::from_store(store))
}

/// 登录：要求已注册、邮箱一致、密码哈希匹配。
pub fn login(
    store: &mut AccountStore,
    email: &str,
    password: &str,
) -> Result<AccountStatus, AccountError> {
    let record = store.account.as_ref().ok_or(AccountError::NotRegistered)?;
    if record.email != email.trim() {
        return Err(AccountError::EmailMismatch);
    }
    if record.password_hash != hash_password(&record.salt, password) {
        return Err(AccountError::WrongPassword);
    }
    store.logged_in = true;
    Ok(AccountStatus::from_store(store))
}

/// 登出：幂等，未登录也返回成功状态。
pub fn logout(store: &mut AccountStore) -> AccountStatus {
    store.logged_in = false;
    AccountStatus::from_store(store)
}

/// 修改密码：需已登录且原密码正确；成功后保持登录态。
pub fn change_password(
    store: &mut AccountStore,
    old_password: &str,
    new_password: &str,
) -> Result<AccountStatus, AccountError> {
    if !store.logged_in {
        return Err(AccountError::NotLoggedIn);
    }
    let record = store.account.as_ref().ok_or(AccountError::NotRegistered)?;
    if record.password_hash != hash_password(&record.salt, old_password) {
        return Err(AccountError::WrongPassword);
    }
    validate_password(new_password)?;
    let salt = generate_salt();
    let record = store.account.as_mut().expect("已检查存在");
    record.salt = salt.clone();
    record.password_hash = hash_password(&salt, new_password);
    Ok(AccountStatus::from_store(store))
}

/// 读取当前状态（只读，不出错）。
pub fn status(store: &AccountStore) -> AccountStatus {
    AccountStatus::from_store(store)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_store() -> AccountStore {
        AccountStore::default()
    }

    #[test]
    fn register_login_roundtrip() {
        let mut store = fresh_store();
        let status = register(&mut store, " user@example.com ", "abc12345").unwrap();
        assert!(status.logged_in);
        assert_eq!(status.email.as_deref(), Some("user@example.com"));

        // 注册即登录；登出后能用相同密码登录回来。
        let logged_out = logout(&mut store);
        assert!(!logged_out.logged_in);
        let logged_in = login(&mut store, "user@example.com", "abc12345").unwrap();
        assert_eq!(logged_in.email.as_deref(), Some("user@example.com"));
    }

    #[test]
    fn duplicate_register_rejected() {
        let mut store = fresh_store();
        register(&mut store, "a@b.co", "abc12345").unwrap();
        assert_eq!(
            register(&mut store, "a@b.co", "xyz98765"),
            Err(AccountError::AlreadyRegistered)
        );
    }

    #[test]
    fn login_wrong_password_and_email() {
        let mut store = fresh_store();
        register(&mut store, "a@b.co", "abc12345").unwrap();
        logout(&mut store);
        assert_eq!(
            login(&mut store, "a@b.co", "wrong123"),
            Err(AccountError::WrongPassword)
        );
        assert_eq!(
            login(&mut store, "other@b.co", "abc12345"),
            Err(AccountError::EmailMismatch)
        );
    }

    #[test]
    fn login_before_register() {
        let mut store = fresh_store();
        assert_eq!(
            login(&mut store, "a@b.co", "abc12345"),
            Err(AccountError::NotRegistered)
        );
    }

    #[test]
    fn weak_and_invalid_inputs_rejected() {
        let mut store = fresh_store();
        assert_eq!(
            validate_email("no-at-sign"),
            Err(AccountError::InvalidEmail)
        );
        assert_eq!(validate_email("a@b"), Err(AccountError::InvalidEmail));
        assert_eq!(validate_email("a b@c.d"), Err(AccountError::InvalidEmail));
        assert_eq!(validate_password("short1"), Err(AccountError::WeakPassword));
        assert_eq!(
            validate_password("nodigitslong"),
            Err(AccountError::WeakPassword)
        );
        assert_eq!(
            validate_password("12345678"),
            Err(AccountError::WeakPassword)
        );
        assert_eq!(
            register(&mut store, "bad-email", "abc12345"),
            Err(AccountError::InvalidEmail)
        );
    }

    #[test]
    fn change_password_flow() {
        let mut store = fresh_store();
        register(&mut store, "a@b.co", "abc12345").unwrap();
        // 原密码错。
        assert_eq!(
            change_password(&mut store, "wrong0000", "newpass99"),
            Err(AccountError::WrongPassword)
        );
        // 新密码弱。
        assert_eq!(
            change_password(&mut store, "abc12345", "short1"),
            Err(AccountError::WeakPassword)
        );
        // 成功改密后旧密码失效、新密码可用。
        change_password(&mut store, "abc12345", "newpass99").unwrap();
        logout(&mut store);
        assert_eq!(
            login(&mut store, "a@b.co", "abc12345"),
            Err(AccountError::WrongPassword)
        );
        assert!(login(&mut store, "a@b.co", "newpass99").is_ok());
    }

    #[test]
    fn store_serializes_with_camel_case() {
        let mut store = fresh_store();
        register(&mut store, "a@b.co", "abc12345").unwrap();
        let json = serde_json::to_string(&store).unwrap();
        assert!(json.contains("\"loggedIn\":true"));
        assert!(json.contains("\"passwordHash\""));
        let back: AccountStore = serde_json::from_str(&json).unwrap();
        assert_eq!(back.account.as_ref().unwrap().email, "a@b.co");
    }

    #[test]
    fn salt_is_unique_and_hash_differs_by_salt() {
        assert_ne!(generate_salt(), generate_salt());
        assert_ne!(hash_password("s1", "pw"), hash_password("s2", "pw"));
        assert_eq!(hash_password("s1", "pw"), hash_password("s1", "pw"));
    }
}
