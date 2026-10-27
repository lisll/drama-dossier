//! 机器码：读取系统稳定标识 → 规范化 → 加盐摘要。只发摘要，不发原始硬件信息。
//!
//! | 平台    | 来源 |
//! |---------|------|
//! | Windows | 注册表 `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid`（强制 64 位视图）**加** SMBIOS 系统 UUID（主板固件） |
//! | macOS   | `/usr/sbin/ioreg -rd1 -c IOPlatformExpertDevice` 的 `IOPlatformUUID` |
//! | Linux   | `/etc/machine-id`，回退 `/var/lib/dbus/machine-id` |
//!
//! Windows 只用 MachineGuid 时，改一下注册表、或克隆系统盘就能让两台电脑机器码相同；
//! 加上主板 UUID 后，这两种方式都不再奏效。主板 UUID 是占位值（全 0、全 F、常见的 OEM 占位串）
//! 或读不到时，退回只用 MachineGuid（v1 公式），不锁定。
//!
//! 读不到 MachineGuid / IOPlatformUUID / machine-id 就明确报错，绝不退回到主机名之类的弱指纹。

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

fn hex_digest(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// v1：machine_id = sha256("drama-dossier/machine/v1\n" + 平台 + "\n" + 规范化值)，64 位小写十六进制。
/// macOS、Linux，以及读不到有效主板 UUID 的 Windows 用这个公式。
pub fn derive_machine_id(platform: Platform, raw: &str) -> Result<String, MachineError> {
    let norm = normalize(raw)?;
    Ok(hex_digest(&[
        b"drama-dossier/machine/v1\n",
        platform.tag().as_bytes(),
        b"\n",
        norm.as_bytes(),
    ]))
}

/// v2（Windows）：sha256("drama-dossier/machine/v2\nwindows\n" + MachineGuid + "\n" + 主板 UUID)。
/// `board` 为 `None`（读不到或是占位值）时退回 v1。
pub fn derive_windows_machine_id(
    machine_guid: &str,
    board: Option<&str>,
) -> Result<String, MachineError> {
    let guid = normalize(machine_guid)?;
    let Some(board) = board.and_then(board_uuid) else {
        return derive_machine_id(Platform::Windows, machine_guid);
    };
    Ok(hex_digest(&[
        b"drama-dossier/machine/v2\nwindows\n",
        guid.as_bytes(),
        b"\n",
        board.as_bytes(),
    ]))
}

/// 主板 UUID 规范化；占位值返回 `None`。
///
/// 常见占位值：全 0、全 F，以及很多杂牌主板出厂时写死的 `03000200-0400-0500-0006-000700080009`。
/// 这些主板之间 UUID 都一样，拿来区分电脑反而会误判为同一台。
pub fn board_uuid(raw: &str) -> Option<String> {
    const PLACEHOLDERS: &[&str] = &[
        "03000200040005000006000700080009",
        "00020003000400050006000700080009",
        "12345678123456781234567812345678",
    ];
    let norm = normalize(raw).ok()?;
    if PLACEHOLDERS.contains(&norm.as_str()) {
        return None;
    }
    // 只有极少数几个不同字节的也视为占位（如 01010101…、00000000-0000-0000-0000-000000000001）
    let mut distinct: Vec<u8> = norm.bytes().collect();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() <= 2 {
        return None;
    }
    Some(norm)
}

/// 从 `GetSystemFirmwareTable('RSMB')` 的返回数据里取出 SMBIOS Type 1（系统信息）的 UUID。
///
/// 数据布局（微软文档 RawSMBIOSData）：8 字节头（调用方式、主次版本、DMI 修订、u32 表长度），
/// 之后是 SMBIOS 结构表。每个结构 = 格式化区（`[type, length, handle_lo, handle_hi, ...]`，
/// 长度由第 2 字节给出）+ 以两个 0 结尾的字符串区。Type 1 的 UUID 在格式化区偏移 8，共 16 字节。
///
/// 这里直接把 16 个原始字节转成十六进制：机器码只要求同一台电脑每次结果一致，
/// 不需要与 `wmic` 显示的字节序一致，少做一次转换就少一处出错的可能。
/// 数据不完整或没有 Type 1 时返回 `None`。
pub fn parse_smbios_system_uuid(raw: &[u8]) -> Option<String> {
    const HEADER: usize = 8;
    let declared = u32::from_le_bytes(raw.get(4..8)?.try_into().ok()?) as usize;
    let table = raw.get(HEADER..HEADER.checked_add(declared)?)?;
    let mut pos = 0usize;
    while pos + 4 <= table.len() {
        let kind = table[pos];
        let len = table[pos + 1] as usize;
        if len < 4 || pos + len > table.len() {
            return None;
        }
        if kind == 1 && len >= 0x18 {
            let uuid = &table[pos + 8..pos + 24];
            return Some(uuid.iter().map(|b| format!("{b:02x}")).collect());
        }
        if kind == 127 {
            return None; // 表结束标记
        }
        // 跳过格式化区，再跳过以 \0\0 结尾的字符串区
        let mut next = pos + len;
        loop {
            let pair = table.get(next..next + 2)?;
            next += 1;
            if pair == [0, 0] {
                next += 1;
                break;
            }
        }
        pos = next;
    }
    None
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

#[cfg(target_os = "windows")]
fn read_machine_guid() -> Result<String, MachineError> {
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

/// 读原始 SMBIOS 表。读不到返回 `None`（虚拟机、精简系统偶有此情况），由调用方退回 v1。
#[cfg(target_os = "windows")]
fn read_smbios() -> Option<Vec<u8>> {
    use windows_sys::Win32::System::SystemInformation::{GetSystemFirmwareTable, RSMB};
    // SAFETY: 传空指针 + 0 长度只查询所需大小；第二次调用传入的缓冲区长度与 buf.len() 一致。
    let need = unsafe { GetSystemFirmwareTable(RSMB, 0, std::ptr::null_mut(), 0) };
    if need == 0 || need > 1 << 20 {
        return None;
    }
    let mut buf = vec![0u8; need as usize];
    let got = unsafe { GetSystemFirmwareTable(RSMB, 0, buf.as_mut_ptr(), need) };
    if got == 0 || got > need {
        return None;
    }
    buf.truncate(got as usize);
    Some(buf)
}

#[cfg(target_os = "windows")]
pub fn read_machine_id() -> Result<String, MachineError> {
    let guid = read_machine_guid()?;
    let board = read_smbios().and_then(|raw| parse_smbios_system_uuid(&raw));
    derive_windows_machine_id(&guid, board.as_deref())
}

#[cfg(not(target_os = "windows"))]
pub fn read_machine_id() -> Result<String, MachineError> {
    let platform = Platform::current().ok_or(MachineError::Unsupported)?;
    let raw = read_raw(platform)?;
    derive_machine_id(platform, &raw)
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

    /// 构造一份 RawSMBIOSData：8 字节头 + 若干结构（格式化区 + 字符串区）。
    fn smbios(structs: &[Vec<u8>]) -> Vec<u8> {
        let table: Vec<u8> = structs.concat();
        let mut out = vec![0, 3, 4, 0];
        out.extend_from_slice(&(table.len() as u32).to_le_bytes());
        out.extend_from_slice(&table);
        out
    }

    /// Type 0（BIOS 信息），带两条字符串
    fn type0() -> Vec<u8> {
        let mut s = vec![0, 0x18, 0x00, 0x00];
        s.resize(0x18, 0x11);
        s.extend_from_slice(b"Vendor\0Version 1.0\0\0");
        s
    }

    /// Type 1（系统信息），UUID 在偏移 8；`strings` 为空时字符串区只有 \0\0
    fn type1(uuid: [u8; 16], strings: &[u8]) -> Vec<u8> {
        let mut s = vec![1, 0x1b, 0x01, 0x00, 1, 2, 3, 4];
        s.extend_from_slice(&uuid);
        s.extend_from_slice(&[6, 5, 6]);
        assert_eq!(s.len(), 0x1b);
        if strings.is_empty() {
            s.extend_from_slice(&[0, 0]);
        } else {
            s.extend_from_slice(strings);
        }
        s
    }

    fn end() -> Vec<u8> {
        vec![127, 4, 0xff, 0xff, 0, 0]
    }

    const UUID: [u8; 16] = [
        0x4c, 0x4c, 0x45, 0x44, 0x00, 0x52, 0x31, 0x10, 0x80, 0x4e, 0xb2, 0xc0, 0x4f, 0x39, 0x52,
        0x32,
    ];
    const UUID_HEX: &str = "4c4c454400523110804eb2c04f395232";

    #[test]
    fn smbios_type1_found_after_other_structures() {
        let want = UUID_HEX;
        let raw = smbios(&[type0(), type1(UUID, b"Maker\0Model\0\0"), end()]);
        assert_eq!(parse_smbios_system_uuid(&raw).as_deref(), Some(want));
        // 字符串区为空（只有 \0\0）也能正确跳过
        let raw = smbios(&[type1(UUID, b""), end()]);
        assert_eq!(parse_smbios_system_uuid(&raw).as_deref(), Some(want));
    }

    #[test]
    fn smbios_missing_or_truncated_returns_none() {
        assert_eq!(parse_smbios_system_uuid(&[]), None);
        assert_eq!(parse_smbios_system_uuid(&smbios(&[type0(), end()])), None);
        // 声明长度超过实际数据
        let mut raw = smbios(&[type0(), type1(UUID, b""), end()]);
        raw[4..8].copy_from_slice(&10_000u32.to_le_bytes());
        assert_eq!(parse_smbios_system_uuid(&raw), None);
        // Type 1 被截断在 UUID 中间
        let full = smbios(&[type1(UUID, b"")]);
        let mut cut = full[..8 + 12].to_vec();
        cut[4..8].copy_from_slice(&12u32.to_le_bytes());
        assert_eq!(parse_smbios_system_uuid(&cut), None);
        // 结构长度字段非法（< 4）
        let raw = smbios(&[vec![0, 2, 0, 0, 0, 0], type1(UUID, b"")]);
        assert_eq!(parse_smbios_system_uuid(&raw), None);
        // 字符串区缺少结尾的 \0\0
        let mut bad = type0();
        bad.truncate(bad.len() - 2);
        assert_eq!(parse_smbios_system_uuid(&smbios(&[bad])), None);
    }

    #[test]
    fn placeholder_board_uuids_are_ignored() {
        for p in [
            "00000000-0000-0000-0000-000000000000",
            "FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF",
            "03000200-0400-0500-0006-000700080009",
            "00000000-0000-0000-0000-000000000001",
            "01010101-0101-0101-0101-010101010101",
            "not-a-uuid",
        ] {
            assert_eq!(board_uuid(p), None, "{p}");
        }
        assert!(board_uuid(UUID_HEX).is_some());
    }

    #[test]
    fn windows_v2_binds_board_and_falls_back_to_v1() {
        let guid = "{01234567-89AB-CDEF-0123-456789ABCDEF}";
        let board = UUID_HEX;
        let v1 = derive_machine_id(Platform::Windows, guid).unwrap();
        let v2 = derive_windows_machine_id(guid, Some(board)).unwrap();
        assert_ne!(v1, v2, "加入主板 UUID 后结果必须不同");
        assert_eq!(v2, derive_windows_machine_id(guid, Some(board)).unwrap());
        assert_eq!(v2.len(), 64);
        // 改注册表（同一块主板换了 MachineGuid）→ 不同
        let other_guid = "{11111111-2222-3333-4444-555555555555}";
        assert_ne!(
            v2,
            derive_windows_machine_id(other_guid, Some(board)).unwrap()
        );
        // 克隆系统盘（MachineGuid 相同，主板不同）→ 不同
        let other_board = "9c2a1f00e3b74d5a8f6e0b1c2d3e4f50";
        assert_ne!(
            v2,
            derive_windows_machine_id(guid, Some(other_board)).unwrap()
        );
        // 主板 UUID 是占位值或读不到 → 退回 v1，不报错
        assert_eq!(derive_windows_machine_id(guid, None).unwrap(), v1);
        assert_eq!(
            derive_windows_machine_id(guid, Some("03000200-0400-0500-0006-000700080009")).unwrap(),
            v1
        );
        // MachineGuid 本身无效仍然报错
        assert_eq!(
            derive_windows_machine_id("", Some(board)),
            Err(MachineError::Invalid)
        );
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
