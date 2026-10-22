//! 自动更新：检查、下载、校验、交给系统安装（M7-4）。
//!
//! **不自己替换正在运行的可执行文件**：Windows 会锁住运行中的 exe、macOS 换掉
//! 二进制会让签名失效。所以最后一步是「把安装包交给系统」（启动安装器 / 打开 dmg），
//! 重启由安装器完成。总方案要求的能力仍然具备，只是执行者换成了系统。
//!
//! 校验分两层，缺一不可：
//!
//! 1. **SHA-256**：防「下载过程中损坏或被人换了包」；
//! 2. **Ed25519 签名**：防「有人改了服务端的下载地址」。哈希本身来自服务端，
//!    只查哈希等于只信服务端一家；签名用客户端内置的公钥验，攻击者改不了。
//!
//! 两步都过才交给系统；任何一步不过就删掉文件并如实报错。

use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

use dossier_license::envelope::{verify_detached, Purpose};

/// 下载与校验的结果。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifiedUpdate {
    /// 已经过 SHA-256 与签名校验的安装包路径。
    pub path: PathBuf,
    pub version: String,
    /// 验签通过的公钥指纹（`kid`）。客服排查时能看出是哪把钥匙签的。
    pub key_id: String,
}

#[derive(Debug)]
pub enum UpdateError {
    /// 版本策略里没有可用的下载信息（没地址 / 没哈希 / 没签名）。
    NotConfigured(String),
    /// 下载失败（网络或写盘）。
    Download(String),
    /// 整文件 SHA-256 与策略里的不符。
    HashMismatch { expected: String, actual: String },
    /// 签名验不过。
    BadSignature(String),
    /// 文件大小与预期不符。
    SizeMismatch { expected: u64, actual: u64 },
}

impl UpdateError {
    pub fn message(&self) -> String {
        match self {
            UpdateError::NotConfigured(detail) => {
                format!("暂时无法更新：{detail}")
            }
            UpdateError::Download(detail) => format!("下载升级包失败：{detail}"),
            UpdateError::HashMismatch { expected, actual } => format!(
                "升级包校验不通过（应为 {}…，实际 {}…），已删除。请重新下载。",
                &expected[..expected.len().min(12)],
                &actual[..actual.len().min(12)]
            ),
            UpdateError::BadSignature(detail) => {
                format!("升级包签名无效（{detail}），已删除。这可能不是官方发布的包，请不要安装。")
            }
            UpdateError::SizeMismatch { expected, actual } => {
                format!("升级包大小不符（应为 {expected} 字节，实际 {actual} 字节），已删除。")
            }
        }
    }
}

/// 检查一个候选升级包的元信息是否齐全。
///
/// 三项缺一不可：缺地址没法下、缺哈希没法验完整性、缺签名没法验来源。
/// 缺任何一项都**不做「降级成只验哈希」**——那等于把安全降到只信服务端。
pub fn check_metadata(
    download_url: &str,
    sha256_hex: &str,
    signature_b64: &str,
) -> Result<(), UpdateError> {
    if download_url.trim().is_empty() {
        return Err(UpdateError::NotConfigured(
            "管理员还没有提供下载地址".into(),
        ));
    }
    // 只接受 https：明文 http 下载的包，路上的任何人都能替换。
    if !download_url
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("https://")
    {
        return Err(UpdateError::NotConfigured(
            "下载地址必须是 https（明文 http 下载的包路上可能被替换）".into(),
        ));
    }
    if sha256_hex.len() != 64 || !sha256_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(UpdateError::NotConfigured("缺少有效的整文件校验值".into()));
    }
    if signature_b64.trim().is_empty() {
        return Err(UpdateError::NotConfigured(
            "升级包没有签名，拒绝使用".into(),
        ));
    }
    Ok(())
}

/// 校验已经下载到本地的安装包：大小、SHA-256、Ed25519 签名。
///
/// 单独成一个函数（不在这里下载）是为了能对真实文件做单测：
/// 下载是网络行为，校验是纯本地判断，分开测才不会为了测校验去联网。
pub fn verify_downloaded(
    path: &Path,
    expected_sha256: &str,
    signature_b64: &str,
    expected_size: Option<u64>,
) -> Result<String, UpdateError> {
    let bytes = std::fs::read(path)
        .map_err(|error| UpdateError::Download(format!("读取下载的文件失败：{error}")))?;

    if let Some(expected) = expected_size {
        if bytes.len() as u64 != expected {
            return Err(UpdateError::SizeMismatch {
                expected,
                actual: bytes.len() as u64,
            });
        }
    }

    let actual = format!("{:x}", Sha256::digest(&bytes));
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(UpdateError::HashMismatch {
            expected: expected_sha256.to_ascii_lowercase(),
            actual,
        });
    }

    let keys = dossier_license::trusted_keys()
        .map_err(|error| UpdateError::BadSignature(format!("内置公钥不可用：{error}")))?;
    match verify_detached(Purpose::Release, &bytes, signature_b64, keys) {
        Ok(kid) => Ok(kid.to_owned()),
        Err(error) => Err(UpdateError::BadSignature(format!("{error:?}"))),
    }
}

/// 下载目录：放在应用数据目录下的 `updates/`。
pub fn update_dir(home: &Path) -> PathBuf {
    home.join("updates")
}

/// 下载升级包到 `updates/`，返回落盘路径。
///
/// **一次性读完再写盘**：安装包几十 MB，分块流式下载需要额外的进度与断点
/// 状态管理；这一版先保证「下完、验完、能装」这条链路是对的（YAGNI）。
/// 下载失败时删除半截文件——留着会让人以为下载过了。
///
/// 下载跟随重定向（见 [`redirect_policy`]）：GitHub Releases 这类发布源一律先 302
/// 跳到存储域名，不跟就下不了。安全性不靠跳转规则，靠后面的哈希 + 签名。
pub async fn download(url: &str, home: &Path, file_name: &str) -> Result<PathBuf, UpdateError> {
    let directory = update_dir(home);
    std::fs::create_dir_all(&directory)
        .map_err(|error| UpdateError::Download(format!("创建更新目录失败：{error}")))?;
    let target = directory.join(sanitize_file_name(file_name));

    // 建客户端前先装 rustls 后端（正常启动时账号模块已经装过，重复调用无副作用）。
    crate::account::api::install_crypto_provider();
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(600))
        .redirect(redirect_policy())
        .build()
        .map_err(|error| UpdateError::Download(format!("初始化网络模块失败：{error}")))?;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| UpdateError::Download(format!("{}", error.without_url())))?;
    if !response.status().is_success() {
        return Err(UpdateError::Download(format!(
            "服务端返回 {}",
            response.status()
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| UpdateError::Download(format!("{}", error.without_url())))?;
    if let Err(error) = std::fs::write(&target, &bytes) {
        // 写失败时不要留半截文件。
        let _ = std::fs::remove_file(&target);
        return Err(UpdateError::Download(format!(
            "保存下载的文件失败：{error}"
        )));
    }
    Ok(target)
}

/// 跟随重定向的上限。5 次对发布源足够（GitHub Releases 是 1 跳），
/// 又能挡住无限互相踢皮球的环。
const MAX_REDIRECTS: usize = 5;

/// 决定一次跳转跟不跟：目标必须是 https，且已跳次数没到上限。
///
/// 抽成纯函数是为了**不联网就能测这两条规则**；真发 302 的接线测试另有一条。
/// 初始地址的协议不在这里管——[`check_metadata`] 已要求 https。
fn follows_redirect(hops_done: usize, target_scheme: &str) -> bool {
    hops_done < MAX_REDIRECTS && target_scheme == "https"
}

/// 重定向策略：只跟 https 跳转、最多 [`MAX_REDIRECTS`] 次，其余按错误报出。
///
/// 哈希 + 签名在跳转**之后**仍然全量校验，所以跟重定向不降低安全性；
/// 拒绝 https 以外的目标是让「302 跳明文地址」这种把包降级成可替换的行为走不通。
fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        let target = attempt.url().clone();
        if follows_redirect(attempt.previous().len(), target.scheme()) {
            attempt.follow()
        } else if target.scheme() != "https" {
            let reason: Box<dyn std::error::Error + Send + Sync> =
                format!("下载地址跳转到 {}，不是 https，已拒绝", target.scheme()).into();
            attempt.error(reason)
        } else {
            let reason: Box<dyn std::error::Error + Send + Sync> =
                format!("下载地址重定向超过 {MAX_REDIRECTS} 次，已拒绝").into();
            attempt.error(reason)
        }
    })
}

/// 只取文件名部分，并挡掉可能穿越目录的写法。
fn sanitize_file_name(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("update.bin")
        .trim();
    // 去掉控制字符与 ..：`..` 单独出现会让 join 走出 updates 目录。
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .replace("..", "_");
    if cleaned.is_empty() {
        "update.bin".to_owned()
    } else {
        cleaned
    }
}

/// 不碰文件系统的路径规范化：展开 `.` 与 `..`，**不解析符号链接**。
///
/// `/..` 再往上走，POSIX 规定等于 `/`，这里照此处理——规范化结果一定还是绝对路径，
/// 挡住 `updates/../../evil.exe` 这类写法靠的是后面的前缀比较，不靠这里报错。
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                result.push(component.as_os_str());
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                result.pop();
            }
            std::path::Component::Normal(name) => result.push(name),
        }
    }
    result
}

/// 「启动安装器」的放行检查：候选文件必须真的落在 `update_dir` 里。
///
/// 两层检查，缺一不可：
/// 1. **字面路径**展开 `..` 后仍在目录内——挡住直接传 `../evil.exe`；
/// 2. **真实路径**（canonicalize，会解析符号链接）也在目录内——挡住把目录里
///    的一个名字做成指向目录外文件的链接（字面合规、实际执行的是别的东西）。
///
/// `update_dir` 自身也按真实路径比较，目录被整体做成链接时不会误放行。
/// 抽成独立函数是为了能直接测这几条规则；真目录、真符号链接的测试见下。
pub fn is_inside_dir(update_dir: &Path, candidate: &Path) -> bool {
    if !lexical_normalize(candidate).starts_with(lexical_normalize(update_dir)) {
        return false;
    }
    // 目录都不存在（还没下载过任何包）就没有可放行的文件。
    let Ok(real_base) = std::fs::canonicalize(update_dir) else {
        return false;
    };
    std::fs::canonicalize(candidate).is_ok_and(|real| real.starts_with(real_base))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn tempdir() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("dossier-update-{nanos}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn metadata_requires_all_three_pieces() {
        let url = "https://example.com/a.exe";
        let sha = "a".repeat(64);
        assert!(check_metadata(url, &sha, "sig").is_ok());
        // 缺地址。
        assert!(matches!(
            check_metadata("", &sha, "sig"),
            Err(UpdateError::NotConfigured(_))
        ));
        // 缺哈希（或形状不对）。
        assert!(matches!(
            check_metadata(url, "", "sig"),
            Err(UpdateError::NotConfigured(_))
        ));
        assert!(matches!(
            check_metadata(url, "abc", "sig"),
            Err(UpdateError::NotConfigured(_))
        ));
        // 缺签名。
        assert!(matches!(
            check_metadata(url, &sha, ""),
            Err(UpdateError::NotConfigured(_))
        ));
    }

    #[test]
    fn plain_http_is_refused() {
        let sha = "a".repeat(64);
        let error = check_metadata("http://example.com/a.exe", &sha, "sig").unwrap_err();
        assert!(error.message().contains("https"));
    }

    #[test]
    fn a_wrong_hash_is_rejected() {
        let dir = tempdir();
        let path = dir.join("pkg.bin");
        std::fs::write(&path, b"payload").unwrap();
        let error = verify_downloaded(&path, &"0".repeat(64), "sig", None).unwrap_err();
        assert!(matches!(error, UpdateError::HashMismatch { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_wrong_size_is_rejected() {
        let dir = tempdir();
        let path = dir.join("pkg.bin");
        std::fs::write(&path, b"payload").unwrap();
        let digest = format!("{:x}", Sha256::digest(b"payload"));
        let error = verify_downloaded(&path, &digest, "sig", Some(999)).unwrap_err();
        assert!(matches!(error, UpdateError::SizeMismatch { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 哈希对但签名不对：必须拒绝。这是「服务端被改」的场景——
    /// 只验哈希会放过它，所以签名是必要的一层。
    #[test]
    fn a_bad_signature_is_rejected_even_when_the_hash_matches() {
        let dir = tempdir();
        let path = dir.join("pkg.bin");
        std::fs::write(&path, b"payload").unwrap();
        let digest = format!("{:x}", Sha256::digest(b"payload"));
        let bogus = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0u8; 64]);
        let error = verify_downloaded(&path, &digest, &bogus, None).unwrap_err();
        assert!(
            matches!(error, UpdateError::BadSignature(_)),
            "哈希对但签名错时必须报签名错：{error:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_file_is_reported_not_panicked() {
        let dir = tempdir();
        let error =
            verify_downloaded(&dir.join("nope.bin"), &"0".repeat(64), "sig", None).unwrap_err();
        assert!(matches!(error, UpdateError::Download(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---------- 重定向策略 ----------

    #[test]
    fn redirects_stay_on_https_within_the_cap() {
        assert!(follows_redirect(0, "https"));
        assert!(
            follows_redirect(MAX_REDIRECTS - 1, "https"),
            "第 5 跳本身仍允许"
        );
        assert!(
            !follows_redirect(MAX_REDIRECTS, "https"),
            "再跳一跳超过上限，拒绝"
        );
        assert!(
            !follows_redirect(0, "http"),
            "跳到明文 http 拒绝——发布源 302 降级成可替换的传输"
        );
        assert!(!follows_redirect(0, "ftp"));
    }

    /// 应答 302 的迷你服务器：验证策略真的接进了下载（不只是规则对）。
    /// tokio 没开 net 特性，用标准库现写。
    fn redirecting_server(location: String) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).unwrap() == 0 {
                    break;
                }
                head.push(byte[0]);
            }
            let reply = format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(reply.as_bytes()).unwrap();
        });
        (base, handle)
    }

    /// 302 跳到明文地址：下载必须失败，且不落半截文件。
    #[tokio::test]
    async fn a_redirect_to_plain_http_is_refused() {
        let (start, server) = redirecting_server("http://127.0.0.1:9/pkg.bin".into());
        let home = tempdir();
        let error = download(&format!("{start}/pkg.bin"), &home, "pkg.bin")
            .await
            .unwrap_err();
        assert!(matches!(error, UpdateError::Download(_)), "{error:?}");
        assert!(
            !update_dir(&home).join("pkg.bin").exists(),
            "拒绝后不能留下半截文件"
        );
        server.join().unwrap();
        std::fs::remove_dir_all(&home).ok();
    }

    // ---------- 启动安装器的放行检查 ----------

    #[test]
    fn only_files_inside_the_update_dir_are_allowed() {
        let home = tempdir();
        let dir = update_dir(&home);
        std::fs::create_dir_all(&dir).unwrap();
        let inside = dir.join("pkg.exe");
        std::fs::write(&inside, b"pkg").unwrap();

        // 目录内的正常文件放行（字面与真实路径都在目录里）。
        assert!(is_inside_dir(&dir, &inside));
        // 字面在目录外：拒绝（第二层都不用看）。
        assert!(!is_inside_dir(&dir, &home.join("elsewhere.exe")));
        // `..` 穿越展开后跑出目录。
        let escape = dir.join("..").join("escaped.exe");
        assert!(!is_inside_dir(&dir, &escape));
        // 目录不存在（还没下载过任何包）：一律拒绝。
        assert!(!is_inside_dir(&home.join("missing"), &inside));
        std::fs::remove_dir_all(&home).ok();
    }

    /// 字面在目录里、实际指向目录外的符号链接：不放行。
    /// （Windows 建符号链接要特权，这条只在 unix 上测。）
    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_the_dir_pointing_out_is_refused() {
        let home = tempdir();
        let dir = update_dir(&home);
        std::fs::create_dir_all(&dir).unwrap();
        let outside = home.join("real.exe");
        std::fs::write(&outside, b"evil").unwrap();
        let link = dir.join("link.exe");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(
            !is_inside_dir(&dir, &link),
            "目录里的名字指向目录外的文件，等于执行任意路径"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
