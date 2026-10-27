//! 机器码：读取系统稳定标识 → 规范化 → 加盐摘要。只发摘要，不发原始硬件信息。
//!
//! | 平台    | 来源 |
//! |---------|------|
//! | Windows | 注册表 `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid`（强制 64 位视图） |
//! | macOS   | `/usr/sbin/ioreg -rd1 -c IOPlatformExpertDevice` 的 `IOPlatformUUID` |
//! | Linux   | `/etc/machine-id`，回退 `/var/lib/dbus/machine-id` |
//!
//! 读不到就明确报错，绝不退回到主机名之类的弱指纹。

use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    MacOs,
    Linux,
}

impl Platform {
    pub fn current() -> Option<Self> {
        if cfg!(target_os = "windows") {
            Some(Self::Windows)
        } else if cfg!(target_os = "macos") {
            Some(Self::MacOs)
        } else if cfg!(target_os = "linux") {
            Some(Self::Linux)
        } else {
            None
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::MacOs => "macos",
            Self::Linux => "linux",
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MachineError {
    #[error("不支持的操作系统")]
    Unsupported,
    #[error("读取本机标识失败：{0}")]
    Read(String),
    #[error("本机标识格式无效")]
    Invalid,
}

/// 去花括号/短横线、转小写；必须是 32 位十六进制，拒绝全 0 / 全 F。
pub fn normalize(raw: &str) -> Result<String, MachineError> {
    let s: String = raw
        .trim()
        .chars()
        .filter(|c| !matches!(c, '{' | '}' | '-'))
        .collect::<String>()
        .to_ascii_lowercase();
    if s.len() != 32 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(MachineError::Invalid);
    }
    if s.bytes().all(|b| b == b'0') || s.bytes().all(|b| b == b'f') {
        return Err(MachineError::Invalid);
    }
    Ok(s)
}

/// machine_id = sha256("drama-dossier/machine/v1\n" + 平台 + "\n" + 规范化值)，64 位小写十六进制。
pub fn derive_machine_id(platform: Platform, raw: &str) -> Result<String, MachineError> {
    let norm = normalize(raw)?;
    let mut h = Sha256::new();
    h.update(b"drama-dossier/machine/v1\n");
    h.update(platform.tag().as_bytes());
    h.update(b"\n");
    h.update(norm.as_bytes());
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// 界面展示：前 16 位，4 位一组。
pub fn display_code(machine_id: &str) -> String {
    machine_id
        .get(..16)
        .unwrap_or(machine_id)
        .to_ascii_uppercase()
        .as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

pub fn read_machine_id() -> Result<String, MachineError> {
    let platform = Platform::current().ok_or(MachineError::Unsupported)?;
    let raw = read_raw(platform)?;
    derive_machine_id(platform, &raw)
}

#[cfg(target_os = "windows")]
fn read_raw(_: Platform) -> Result<String, MachineError> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY};
    use winreg::RegKey;
    let key = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(
            r"SOFTWARE\Microsoft\Cryptography",
            KEY_READ | KEY_WOW64_64KEY,
        )
        .map_err(|e| MachineError::Read(e.to_string()))?;
    key.get_value::<String, _>("MachineGuid")
        .map_err(|e| MachineError::Read(e.to_string()))
}

#[cfg(target_os = "macos")]
fn read_raw(_: Platform) -> Result<String, MachineError> {
    let out = std::process::Command::new("/usr/sbin/ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .map_err(|e| MachineError::Read(e.to_string()))?;
    if !out.status.success() {
        return Err(MachineError::Read(format!(
            "ioreg 退出码 {:?}",
            out.status.code()
        )));
    }
    parse_ioreg(&String::from_utf8_lossy(&out.stdout))
        .ok_or(MachineError::Read("ioreg 输出中没有 IOPlatformUUID".into()))
}

#[cfg(target_os = "linux")]
fn read_raw(_: Platform) -> Result<String, MachineError> {
    let mut last = String::new();
    for p in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        match std::fs::read_to_string(p) {
            Ok(s) if !s.trim().is_empty() => return Ok(s),
            Ok(_) => last = format!("{p} 为空"),
            Err(e) => last = format!("{p}: {e}"),
        }
    }
    Err(MachineError::Read(last))
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn read_raw(_: Platform) -> Result<String, MachineError> {
    Err(MachineError::Unsupported)
}

/// 从 `ioreg` 输出中取出 `"IOPlatformUUID" = "XXXXXXXX-..."` 的值。
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn parse_ioreg(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.contains("\"IOPlatformUUID\""))?;
    let value = line.split('=').nth(1)?.trim();
    Some(value.trim_matches('"').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_variants() {
        let want = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            normalize("0123456789abcdef0123456789abcdef\n").unwrap(),
            want
        );
        assert_eq!(
            normalize("{01234567-89AB-CDEF-0123-456789ABCDEF}").unwrap(),
            want
        );
        assert_eq!(
            normalize("01234567-89ab-cdef-0123-456789abcdef").unwrap(),
            want
        );
    }

    #[test]
    fn normalize_rejects_bad() {
        for bad in [
            "",
            "xyz",
            "0".repeat(32).as_str(),
            "F".repeat(32).as_str(),
            "0123456789abcdef",
        ] {
            assert_eq!(normalize(bad), Err(MachineError::Invalid), "{bad}");
        }
    }

    #[test]
    fn derive_is_stable_and_platform_salted() {
        let raw = "0123456789abcdef0123456789abcdef";
        let a = derive_machine_id(Platform::Linux, raw).unwrap();
        assert_eq!(a, derive_machine_id(Platform::Linux, raw).unwrap());
        assert_ne!(a, derive_machine_id(Platform::Windows, raw).unwrap());
        assert_eq!(a.len(), 64);
        assert!(!a.contains(raw));
    }

    #[test]
    fn display_code_groups() {
        assert_eq!(display_code("abcdef0123456789ffff"), "ABCD-EF01-2345-6789");
    }

    #[test]
    fn parse_ioreg_sample() {
        let sample = r#"+-o J316sAP  <class IOPlatformExpertDevice>
    {
      "IOPlatformSerialNumber" = "XXXX"
      "IOPlatformUUID" = "12345678-9ABC-DEF0-1234-56789ABCDEF0"
    }"#;
        assert_eq!(
            parse_ioreg(sample).unwrap(),
            "12345678-9ABC-DEF0-1234-56789ABCDEF0"
        );
        assert!(normalize(&parse_ioreg(sample).unwrap()).is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reads_real_machine_id_on_linux() {
        if std::path::Path::new("/etc/machine-id").exists() {
            let id = read_machine_id().unwrap();
            assert_eq!(id.len(), 64);
        }
    }
}
