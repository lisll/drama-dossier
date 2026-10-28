//! 授权状态的第二份存档：系统凭据库（Windows 凭据管理器 / macOS 钥匙串）。
//!
//! 为什么需要：授权文件里的「见过的最晚时间」和「已停用」只存在文件里。用户备份一份停用前的
//! 授权文件，被停用后恢复备份、断网、把系统时间拨回凭证有效期内，就能继续用到凭证过期。
//! 把同样两项再写一份到凭据库后，恢复旧文件只能回滚其中一处——判定时取两边更严的那份。
//!
//! 边界：这只提高门槛，挡不住会逆向的人；真正的上限仍是凭证本身的离线有效期（服务端默认 3 天）。
//! 凭据库读写失败（用户拒绝钥匙串访问、Linux 没有后端等）时退回只看授权文件的旧逻辑，
//! 绝不因为凭据库不可用而锁死用户；本次运行也不再碰凭据库（读失败时写入会覆盖掉原有记录，
//! macOS 上每次访问还可能再弹一次授权框）。
//!
//! 这里只负责存取一段文本；记录格式、防改校验码与判定规则都在 `license_state.rs`，可单测。

/// 第二份存档的存取接口。实现不得 panic。
///
/// `load` 必须区分「确实没有记录」（`Ok(None)`）与「读不到」（`Err`）：
/// 后者不能当成没有记录，否则下一次写入会把凭据库里原有的停用记录 / 更晚的时间覆盖掉。
pub trait Anchor: Send + Sync {
    fn load(&self) -> Result<Option<String>, String>;
    fn store(&self, value: &str) -> Result<(), String>;
}

/// 没有第二份存档（Linux 等没有凭据库后端的平台，以及单测）：行为与加入锚点之前完全一致。
#[cfg(any(test, not(any(target_os = "windows", target_os = "macos"))))]
pub struct NoAnchor;

#[cfg(any(test, not(any(target_os = "windows", target_os = "macos"))))]
impl Anchor for NoAnchor {
    fn load(&self) -> Result<Option<String>, String> {
        Ok(None)
    }
    fn store(&self, _value: &str) -> Result<(), String> {
        Ok(())
    }
}

/// 系统凭据库里的条目名。
#[cfg(any(target_os = "windows", target_os = "macos"))]
const KEYRING_SERVICE: &str = "drama-dossier";
#[cfg(any(target_os = "windows", target_os = "macos"))]
const KEYRING_USER: &str = "license-anchor";

/// 系统凭据库实现（Windows 凭据管理器 / macOS 钥匙串）。
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub struct KeyringAnchor;

#[cfg(any(target_os = "windows", target_os = "macos"))]
impl Anchor for KeyringAnchor {
    fn load(&self) -> Result<Option<String>, String> {
        match keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
            .and_then(|entry| entry.get_password())
        {
            Ok(text) => Ok(Some(text)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(format!("读取系统凭据库失败：{error}")),
        }
    }

    fn store(&self, value: &str) -> Result<(), String> {
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
            .and_then(|entry| entry.set_password(value))
            .map_err(|error| format!("写入系统凭据库失败：{error}"))
    }
}

/// 当前平台的默认实现。
pub fn platform_anchor() -> Box<dyn Anchor> {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        Box::new(KeyringAnchor)
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        Box::new(NoAnchor)
    }
}

/// 测试用：进程内存里的存档，可模拟「凭据库里有更晚的记录」「凭据库读 / 写不了」。
#[cfg(test)]
#[derive(Default)]
pub struct MemoryAnchor {
    pub value: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    pub fail_reads: bool,
    pub fail_writes: bool,
}

#[cfg(test)]
impl MemoryAnchor {
    pub fn shared() -> (Self, std::sync::Arc<std::sync::Mutex<Option<String>>>) {
        let anchor = Self::default();
        let value = anchor.value.clone();
        (anchor, value)
    }
}

#[cfg(test)]
impl Anchor for MemoryAnchor {
    fn load(&self) -> Result<Option<String>, String> {
        if self.fail_reads {
            return Err("模拟凭据库读取失败".into());
        }
        Ok(self.value.lock().map_err(|_| "锁损坏".to_string())?.clone())
    }
    fn store(&self, value: &str) -> Result<(), String> {
        if self.fail_writes {
            return Err("模拟凭据库不可用".into());
        }
        *self.value.lock().map_err(|_| "锁损坏".to_string())? = Some(value.to_string());
        Ok(())
    }
}
