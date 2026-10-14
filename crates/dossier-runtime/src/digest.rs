//! SHA-256 摘要工具（文件级流式哈希 + 十六进制编解码）。
//!
//! 手写十六进制编解码而不是引入 `hex`：整个 workspace 的依赖都是离线缓存的，
//! 能少一个依赖就少一处解析风险；这段代码本身也被单测钉住。

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use sha2::{Digest, Sha256};

/// 读文件算 SHA-256（小写十六进制）。
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(to_hex(&hasher.finalize()))
}

/// 字节串的 SHA-256（小写十六进制）。
pub fn sha256_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    to_hex(&hasher.finalize())
}

pub fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// 解析十六进制字符串；长度为奇数或含非法字符时返回 `None`。
pub fn from_hex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let high = hex_value(bytes[index])?;
        let low = hex_value(bytes[index + 1])?;
        out.push((high << 4) | low);
        index += 2;
    }
    Some(out)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 是否是合法的 64 位小写/大写十六进制 SHA-256。
pub fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// 取摘要前 `n` 个字符，用于日志/界面展示（不泄露完整摘要时使用）。
pub fn short(digest: &str, n: usize) -> String {
    digest.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let bytes = vec![0x00, 0x0f, 0xff, 0xa5];
        let text = to_hex(&bytes);
        assert_eq!(text, "000fffa5");
        assert_eq!(from_hex("000FFFa5").unwrap(), bytes);
        assert!(from_hex("abc").is_none());
        assert!(from_hex("zz").is_none());
    }

    #[test]
    fn sha256_matches_known_vector() {
        assert_eq!(
            sha256_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(is_sha256_hex(&sha256_bytes(b"abc")));
        assert!(!is_sha256_hex("ba7816bf"));
    }

    #[test]
    fn sha256_file_hashes_streamed_bytes() {
        let dir = std::env::temp_dir().join(format!("dossier-digest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("payload.bin");
        let payload = vec![7_u8; 200_000];
        std::fs::write(&path, &payload).unwrap();
        assert_eq!(sha256_file(&path).unwrap(), sha256_bytes(&payload));
        assert_eq!(short(&sha256_bytes(b"abc"), 8), "ba7816bf");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
