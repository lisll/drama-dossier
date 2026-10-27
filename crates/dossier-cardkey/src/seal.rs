//! 授权文件防改校验码。
//!
//! 授权文件里除了服务器签名的凭证，还有两项本机状态：见过的最晚时间 `last_seen`
//! 和「已被服务器停用」标记 `revoked`。这两项没有服务器签名，用户可以手改。
//! 这里用 HMAC-SHA256 把它们和凭证、本机机器码绑在一起：改了任何一项，校验码就对不上。
//!
//! 边界：密钥写在程序里，熟悉逆向的人能找出来并自己重算校验码。
//! 它挡住的是「用记事本改个数字」，不是逆向工程。

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

const SEAL_KEY: &[u8] = b"drama-dossier/stored/v1/7b1f0c9e4a2d48f6b3e5a18c";

fn mac_for(machine_id: &str, token: &str, last_seen: u64, revoked: bool) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(SEAL_KEY).expect("HMAC 接受任意长度密钥");
    mac.update(b"drama-dossier/stored/v1\n");
    mac.update(machine_id.as_bytes());
    mac.update(b"\n");
    mac.update(token.as_bytes());
    mac.update(b"\n");
    mac.update(last_seen.to_string().as_bytes());
    mac.update(if revoked { b"\n1" } else { b"\n0" });
    mac
}

/// 计算校验码（64 位小写十六进制）。
pub fn seal(machine_id: &str, token: &str, last_seen: u64, revoked: bool) -> String {
    mac_for(machine_id, token, last_seen, revoked)
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 常量时间比较校验码；格式不对直接返回 false。
pub fn check_seal(machine_id: &str, token: &str, last_seen: u64, revoked: bool, mac: &str) -> bool {
    let Some(bytes) = decode_hex32(mac) else {
        return false;
    };
    mac_for(machine_id, token, last_seen, revoked)
        .verify_slice(&bytes)
        .is_ok()
}

fn decode_hex32(text: &str) -> Option<[u8; 32]> {
    let raw = text.as_bytes();
    if raw.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in raw.chunks(2).enumerate() {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out[i] = (hi * 16 + lo) as u8;
    }
    // 只接受小写，与 seal 的输出保持唯一形式
    if text.bytes().any(|b| b.is_ascii_uppercase()) {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MID: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    #[test]
    fn round_trip() {
        let mac = seal(MID, "tok.sig", 1_800_000_000, false);
        assert_eq!(mac.len(), 64);
        assert!(check_seal(MID, "tok.sig", 1_800_000_000, false, &mac));
    }

    #[test]
    fn any_field_change_breaks_seal() {
        let mac = seal(MID, "tok.sig", 1_800_000_000, false);
        let other_mid = "2".repeat(64);
        assert!(!check_seal(
            &other_mid,
            "tok.sig",
            1_800_000_000,
            false,
            &mac
        ));
        assert!(!check_seal(MID, "tok.sih", 1_800_000_000, false, &mac));
        assert!(!check_seal(MID, "tok.sig", 0, false, &mac));
        assert!(!check_seal(MID, "tok.sig", 1_800_000_000, true, &mac));
    }

    #[test]
    fn malformed_mac_is_rejected() {
        let mac = seal(MID, "t", 1, false);
        for bad in [
            String::new(),
            "zz".repeat(32),
            mac[..62].to_string(),
            mac.to_ascii_uppercase(),
            format!("{mac}00"),
        ] {
            assert!(!check_seal(MID, "t", 1, false, &bad), "{bad}");
        }
    }
}
