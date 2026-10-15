//! 机器码：系统编号加盐做 SHA-256，原始编号不出本机。
//!
//! - Windows：注册表 `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid`，强制用 64 位视图读
//!   （32 位进程默认会被重定向到 WOW6432Node）；
//! - macOS：用绝对路径执行 `/usr/sbin/ioreg -rd1 -c IOPlatformExpertDevice`，解析 `IOPlatformUUID`；
//! - Linux：`/etc/machine-id`，读不到再读 `/var/lib/dbus/machine-id`。
//!
//! 读不到时明确报错，不用随机编号兜底：兜底会让「换电脑」的判断失真。
//!
//! 规范化规则和摘要算法一改，所有用户都会被当成换了电脑，所以都用测试钉死。
//! 克隆出来的虚拟机系统编号相同，会被当成同一台电脑，这是按系统编号识别的固有限制。

use sha2::{Digest, Sha256};

/// 摘要的固定前缀，隔离用途。
const DEVICE_CONTEXT: &str = "drama-dossier/device/v1\n";

/// 平台。取值和服务端 `platform` 字段一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    Macos,
    Linux,
}

impl Platform {
    /// 当前编译目标的平台；不支持的平台返回 `None`。
    pub fn current() -> Option<Self> {
        if cfg!(target_os = "windows") {
            Some(Platform::Windows)
        } else if cfg!(target_os = "macos") {
            Some(Platform::Macos)
        } else if cfg!(target_os = "linux") {
            Some(Platform::Linux)
        } else {
            None
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Windows => "windows",
            Platform::Macos => "macos",
            Platform::Linux => "linux",
        }
    }

    /// 给人看的平台名，用在默认设备名里。
    pub fn label(self) -> &'static str {
        match self {
            Platform::Windows => "Windows 电脑",
            Platform::Macos => "Mac 电脑",
            Platform::Linux => "Linux 电脑",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MachineError {
    #[error("读不到这台电脑的系统编号（{0}），无法计算机器码")]
    Unreadable(String),
    #[error("这台电脑的系统编号无效（{0}），无法计算机器码")]
    Invalid(&'static str),
    #[error("不支持这个操作系统")]
    UnsupportedPlatform,
}

/// 本机的平台和机器码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineIdentity {
    pub platform: Platform,
    /// 64 位小写十六进制，直接作为服务端的 `device_id`。
    pub device_id: String,
}

/// 读取本机系统编号并算出机器码。
pub fn read_device_id() -> Result<MachineIdentity, MachineError> {
    let platform = Platform::current().ok_or(MachineError::UnsupportedPlatform)?;
    let raw = read_raw()?;
    let normalized = normalize(&raw)?;
    Ok(MachineIdentity {
        platform,
        device_id: derive_device_id(platform, &normalized),
    })
}

/// 规范化系统编号：去掉首尾空白和花括号、去掉短横线、转小写；必须是 32 位十六进制。
/// 全 0、全 F 这类占位值拒绝（有些虚拟机和刷过的主板会返回这种值）。
pub fn normalize(raw: &str) -> Result<String, MachineError> {
    let text: String = raw
        .trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .chars()
        .filter(|char| *char != '-')
        .collect::<String>()
        .to_ascii_lowercase();
    if text.len() != 32 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(MachineError::Invalid("格式不对"));
    }
    if text.bytes().all(|byte| byte == b'0') || text.bytes().all(|byte| byte == b'f') {
        return Err(MachineError::Invalid("占位值"));
    }
    Ok(text)
}

/// `SHA-256("drama-dossier/device/v1\n" + 平台 + "\n" + 规范化编号)`，64 位小写十六进制。
pub fn derive_device_id(platform: Platform, normalized: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(DEVICE_CONTEXT.as_bytes());
    hasher.update(platform.as_str().as_bytes());
    hasher.update(b"\n");
    hasher.update(normalized.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 给人看的机器码：前 16 位转大写、4 位一组，和服务端 `device.code` 一致。
pub fn display_code(device_id: &str) -> String {
    let head: String = device_id
        .chars()
        .take(16)
        .collect::<String>()
        .to_ascii_uppercase();
    head.as_bytes()
        .chunks(4)
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

/// 默认设备名：平台名加机器码前 4 位，例如「Windows 电脑 · 3F2A」。不读主机名：主机名里常有真实姓名。
pub fn default_device_name(platform: Platform, device_id: &str) -> String {
    let short: String = device_id
        .chars()
        .take(4)
        .collect::<String>()
        .to_ascii_uppercase();
    format!("{} · {short}", platform.label())
}

#[cfg(target_os = "windows")]
fn read_raw() -> Result<String, MachineError> {
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY};
    use winreg::RegKey;

    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(
            "SOFTWARE\\Microsoft\\Cryptography",
            KEY_READ | KEY_WOW64_64KEY,
        )
        .and_then(|key| key.get_value::<String, _>("MachineGuid"))
        .map_err(|error| MachineError::Unreadable(format!("MachineGuid：{error}")))
}

#[cfg(target_os = "macos")]
fn read_raw() -> Result<String, MachineError> {
    // 用绝对路径，不经过 PATH 查找，免得被同名程序冒充。
    let output = std::process::Command::new("/usr/sbin/ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .map_err(|error| MachineError::Unreadable(format!("ioreg：{error}")))?;
    if !output.status.success() {
        return Err(MachineError::Unreadable(format!(
            "ioreg 退出码 {}",
            output.status
        )));
    }
    parse_ioreg_uuid(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| MachineError::Unreadable("ioreg 输出里没有 IOPlatformUUID".into()))
}

#[cfg(target_os = "linux")]
fn read_raw() -> Result<String, MachineError> {
    let mut last_error = String::new();
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        match std::fs::read_to_string(path) {
            Ok(text) if !text.trim().is_empty() => return Ok(text),
            Ok(_) => last_error = format!("{path} 是空的"),
            Err(error) => last_error = format!("{path}：{error}"),
        }
    }
    Err(MachineError::Unreadable(last_error))
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn read_raw() -> Result<String, MachineError> {
    Err(MachineError::UnsupportedPlatform)
}

/// 从 `ioreg -rd1 -c IOPlatformExpertDevice` 的输出里取 `IOPlatformUUID` 的值。
/// 输出形如 `"IOPlatformUUID" = "564D1E2A-...-9F3B"`。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn parse_ioreg_uuid(output: &str) -> Option<String> {
    output.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        if key.trim().trim_matches('"') != "IOPlatformUUID" {
            return None;
        }
        Some(value.trim().trim_matches('"').to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &str = "3F2A91C0-7B44-E1D8-A5B6-C7D8E9F00112";
    const NORMALIZED: &str = "3f2a91c07b44e1d8a5b6c7d8e9f00112";

    #[test]
    fn normalize_strips_braces_dashes_and_case() {
        assert_eq!(normalize(RAW).unwrap(), NORMALIZED);
        assert_eq!(normalize(&format!("  {{{RAW}}}\n")).unwrap(), NORMALIZED);
        // Linux 的 machine-id 本来就是 32 位小写、带换行。
        assert_eq!(normalize(&format!("{NORMALIZED}\n")).unwrap(), NORMALIZED);
    }

    #[test]
    fn normalize_rejects_placeholders_and_bad_format() {
        for raw in [
            "00000000-0000-0000-0000-000000000000",
            "FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF",
        ] {
            assert_eq!(
                normalize(raw),
                Err(MachineError::Invalid("占位值")),
                "{raw}"
            );
        }
        let too_long = format!("{NORMALIZED}00");
        for raw in [
            "",
            "3f2a91c0",
            "3f2a91c07b44e1d8a5b6c7d8e9f0011z",
            too_long.as_str(),
        ] {
            assert_eq!(
                normalize(raw),
                Err(MachineError::Invalid("格式不对")),
                "{raw}"
            );
        }
    }

    /// 摘要算法一改，所有用户都会被当成换了电脑。值由独立实现（Python hashlib）算出，钉死。
    #[test]
    fn device_id_is_pinned_per_platform() {
        let cases = [
            (
                Platform::Windows,
                "af875228eb9c6d6a323e970b8c4303c2b1edfe3026ad61a616b0fb331189386e",
            ),
            (
                Platform::Macos,
                "9bb2100afc3666f4ef17644a4c703650c5c69c9fe12ffe9c5fc900af65d3753b",
            ),
            (
                Platform::Linux,
                "ce4fef5696ff4e1075fa541ec5abe889cb9973a58d33efda3a6127948d1d6299",
            ),
        ];
        for (platform, expected) in cases {
            assert_eq!(
                derive_device_id(platform, NORMALIZED),
                expected,
                "{platform:?}"
            );
        }
    }

    #[test]
    fn same_id_differs_across_platforms() {
        let ids: Vec<String> = [Platform::Windows, Platform::Macos, Platform::Linux]
            .into_iter()
            .map(|platform| derive_device_id(platform, NORMALIZED))
            .collect();
        assert_ne!(ids[0], ids[1]);
        assert_ne!(ids[1], ids[2]);
        assert_ne!(ids[0], ids[2]);
    }

    #[test]
    fn display_code_and_default_name() {
        let id = derive_device_id(Platform::Windows, NORMALIZED);
        assert_eq!(display_code(&id), "AF87-5228-EB9C-6D6A");
        assert_eq!(
            default_device_name(Platform::Windows, &id),
            "Windows 电脑 · AF87"
        );
        assert_eq!(default_device_name(Platform::Macos, &id), "Mac 电脑 · AF87");
    }

    #[test]
    fn ioreg_output_is_parsed() {
        let output = r#"+-o J316sAP  <class IOPlatformExpertDevice, id 0x100000202, registered>
    {
      "IOPlatformSerialNumber" = "XXXXXXXXXX"
      "IOPlatformUUID" = "3F2A91C0-7B44-E1D8-A5B6-C7D8E9F00112"
      "model" = <"MacBookPro18,3">
    }"#;
        assert_eq!(parse_ioreg_uuid(output).as_deref(), Some(RAW));
        assert_eq!(parse_ioreg_uuid("\"model\" = <\"x\">"), None);
    }
}
