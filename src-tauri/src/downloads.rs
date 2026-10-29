//! 大体积下载：多源回退 + sha256 校验 + 失败分类。
//!
//! 一键安装要拉的东西（ffmpeg、Python、tesseract、语言包、模型）都从这里走：
//! - **多源**：每个资源给一组候选地址，按顺序尝试，前一个失败自动换下一个；
//! - **校验**：每个地址配一份写死的 sha256，下载后先算哈希再用，不匹配就删掉重试；
//! - **失败分类**：把"连不上 / 内容不对 / 格式错误"分开，界面据此给出不同提示。
//!
//! 内置资源（安装包里带的二进制）同样走校验：`verify_file` 对已有文件算哈希，
//! 避免"安装包里的文件被替换"这类问题被静默放过。
//!
//! 边界：哈希写死在源码里，升级资源版本时必须同步更新；这是有意的——
//! 宁可下载失败，也不允许悄悄换成别的文件。

// 本模块只在 Windows / macOS 的一键安装里有调用方（Linux 不走这条路径）。
// 为了让 Linux 上的 clippy 不报"从未使用"，统一放开 dead_code；测试也在这两个平台上跑。
#![allow(dead_code)]

use std::path::Path;

use sha2::{Digest, Sha256};

/// 下载失败的类别。界面按类别给不同提示，日志按类别记录，便于定位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// 连不上、超时、DNS 失败、HTTP 非 2xx
    Network,
    /// 下载完成但 sha256 与预期不符
    Checksum,
    /// 下载完成但内容不是预期的格式（例如该是 zip 的解析不了）
    Format,
}

impl Failure {
    pub fn label(self) -> &'static str {
        match self {
            Failure::Network => "网络失败",
            Failure::Checksum => "文件校验失败",
            Failure::Format => "文件格式错误",
        }
    }
}

/// 一个下载源的完整性凭据。
///
/// 能固定哈希的源一律用 sha256。以前 macOS 的 evermeet 源只校验字节数（数值取自同一台
/// 服务器），服务器被攻破时文件和字节数可以一起换掉，下载的程序随后会被执行——已改为
/// 钉版本化地址 + sha256。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Integrity {
    Sha256(&'static str),
    /// 没有可用的固定凭据：上游每日重建、不发布哈希。
    /// 只能靠 HTTPS 与调用方的自检把关；用的时候必须在代码里写明为什么。
    None,
}

impl Integrity {
    pub fn describe(&self) -> &'static str {
        match self {
            Integrity::Sha256(_) => "sha256",
            Integrity::None => "无（仅 HTTPS 与自检）",
        }
    }
}

/// 一个下载源：地址 + 完整性凭据。
#[derive(Debug, Clone)]
pub struct Source {
    pub url: String,
    pub integrity: Integrity,
}

impl Source {
    /// 有 sha256 的源（首选）
    pub fn sha256(url: impl Into<String>, sha256: &'static str) -> Self {
        Self {
            url: url.into(),
            integrity: Integrity::Sha256(sha256),
        }
    }
}

/// 一个资源：名字（用于进度与日志）+ 一组候选源。
///
/// 用 `Vec` 而不是 `&'static [Source]`：源列表很短，每次构造的分配可以忽略，
/// 换来的是普通函数就能构造（不需要 const 上下文或静态泄漏）。
#[derive(Debug, Clone)]
pub struct Asset {
    pub name: &'static str,
    /// 候选源，顺序即优先级
    pub sources: Vec<Source>,
}

impl Asset {
    pub fn new(name: &'static str, sources: Vec<Source>) -> Self {
        Self { name, sources }
    }
}

/// 一次多源下载的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 已经就位（目标文件存在且哈希匹配），没有下载
    Existing,
    /// 从第 `index` 个源下载成功
    Downloaded { index: usize },
}

/// 计算文件 sha256（小写十六进制）。
pub fn file_sha256(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("无法读取 {}：{error}", path.display()))?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)
        .map_err(|error| format!("读取 {} 失败：{error}", path.display()))?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// 按凭据校验文件：sha256 逐一字节比对；
/// `None` 表示没有可校验的凭据，只要文件存在且非空就放行（调用方必须自己再自检）。
pub fn verify_file(path: &Path, integrity: &Integrity) -> bool {
    match integrity {
        Integrity::Sha256(expected) => match file_sha256(path) {
            Ok(actual) => actual.eq_ignore_ascii_case(expected),
            Err(_) => false,
        },
        Integrity::None => std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.len() > 0)
            .unwrap_or(false),
    }
}

/// 逐个源尝试下载到 `target`。任一源成功即返回。
///
/// `fetch` 由调用方提供（实际是 curl），签名是 `(url, target) -> Result<(), Failure>`；
/// 这样本模块不依赖具体下载工具，也能在测试里替换成假实现。
pub fn fetch_with_fallback<F>(
    asset: &Asset,
    target: &Path,
    mut fetch: F,
) -> Result<Outcome, (Failure, String)>
where
    F: FnMut(&str, &Path) -> Result<(), Failure>,
{
    // 目标已存在且哈希正确：直接复用，不重复下载
    if target.is_file() {
        if let Some(first) = asset.sources.first() {
            if verify_file(target, &first.integrity) {
                return Ok(Outcome::Existing);
            }
        }
        // 存在但不可信：删掉重新下载
        let _ = std::fs::remove_file(target);
    }
    let mut last: Option<(Failure, String)> = None;
    for (index, source) in asset.sources.iter().enumerate() {
        match fetch(&source.url, target) {
            Ok(()) => {
                if verify_file(target, &source.integrity) {
                    return Ok(Outcome::Downloaded { index });
                }
                let _ = std::fs::remove_file(target);
                last = Some((
                    Failure::Checksum,
                    format!(
                        "{} 的下载内容与预期不一致（源：{}）",
                        asset.name, source.url
                    ),
                ));
            }
            Err(failure) => {
                let _ = std::fs::remove_file(target);
                last = Some((
                    failure,
                    format!(
                        "{} 从 {} 下载失败（{}）",
                        asset.name,
                        source.url,
                        failure.label()
                    ),
                ));
            }
        }
    }
    Err(last.unwrap_or_else(|| {
        (
            Failure::Network,
            format!("{} 没有配置可用的下载源", asset.name),
        )
    }))
}

/// 运行环境（ffmpeg / ffprobe）的下载源表，顺序即优先级。
///
/// 取舍依据（2026-10-28 核对）：
/// - **Windows 只用 gyan.dev essentials（单一来源）**：钉**版本化的包名**，哈希取自它自己的
///   `.sha256` 接口 `/packages/<包名>.sha256`。升级版本时必须同步改这里。
///   以前排在它前面的 BtbN 用的是滚动的 `latest` 地址却钉了固定哈希：上游每次重建（几乎每天）
///   校验就必然失败，实际每次都退回 gyan.dev，只是白白多下一遍 —— 已移除。
///   正常安装路径用的是安装包内置的 ffmpeg，这里只是兜底。
/// - **macOS 只有 evermeet.cx** 提供 ffmpeg/ffprobe 单文件包，它只发布 GPG 签名、不发布
///   sha256。这里钉死版本化地址，sha256 由我们下载核对后写死（见 [`EVERMEET`]）。
///   正常安装路径用的是安装包内置的 ffmpeg，这里只作兜底。
#[cfg(target_os = "windows")]
pub fn runtime_ffmpeg() -> Asset {
    Asset::new(
        "ffmpeg",
        vec![
            // gyan.dev：essentials（GPLv3），哈希来自其 .sha256 接口
            Source::sha256(
                "https://www.gyan.dev/ffmpeg/builds/packages/ffmpeg-9.0.2-essentials_build.zip",
                "60f467265b1e312373dbcd92200c2618a74850f98d3d078e94296bb3fa2047ba",
            ),
        ],
    )
}

/// macOS：ffmpeg 与 ffprobe 分成两个包，各自一个源（evermeet 只有这一家）。
#[cfg(target_os = "macos")]
pub fn runtime_ffmpeg() -> Asset {
    Asset::new(
        "ffmpeg",
        vec![evermeet_source("ffmpeg", EVERMEET.ffmpeg_sha256)],
    )
}

#[cfg(target_os = "macos")]
pub fn runtime_ffprobe() -> Asset {
    Asset::new(
        "ffprobe",
        vec![evermeet_source("ffprobe", EVERMEET.ffprobe_sha256)],
    )
}

/// evermeet 的固定版本与 zip 的 sha256（2026-10-29 下载核对，字节数 26198325 / 26104193）。
///
/// 为什么写死而不跟随它的 `info/*/release` 接口：接口和文件在同一台服务器上，跟着它走就
/// 等于没有校验。代价是 evermeet 清理旧版本后这条兜底会 404（下载失败、明确报错，不会装上
/// 未经核对的文件），届时人工升级这里的版本与哈希，并同步 `.github/workflows/desktop.yml`
/// 「预置 ffmpeg 到安装包资源（macOS）」步骤里的同一组值。
#[cfg(any(target_os = "macos", test))]
struct EvermeetPin {
    version: &'static str,
    ffmpeg_sha256: &'static str,
    ffprobe_sha256: &'static str,
}

#[cfg(any(target_os = "macos", test))]
const EVERMEET: EvermeetPin = EvermeetPin {
    version: "9.0.2",
    ffmpeg_sha256: "4acc0be580f9b2788029eb7bd4d645ff87968911b0a62aeeb3940d42d54558d5",
    ffprobe_sha256: "24a9c968cd4da72d99c7245e914b921815835eb6dff01d99868031aebaf1d439",
};

/// 平台限定加上 `test`：纯字符串拼接，这样在 Linux 上也能测到。
#[cfg(any(target_os = "macos", test))]
fn evermeet_source(name: &str, sha256: &'static str) -> Source {
    Source::sha256(
        format!("https://evermeet.cx/ffmpeg/{name}-{}.zip", EVERMEET.version),
        sha256,
    )
}

/// Windows 的 CUDA 版 whisper-cli（安装包只带 CPU 版，检测到显卡后再下这个）。
///
/// 为什么不内置：官方 CUDA 预编译包是 285 MB（cu11.8）到 685 MB（cu12.4），比整个应用还大。
/// 选 CUDA 11.8：包小一半以上，对显卡驱动版本要求更低，覆盖更多老机器。
///
/// 完整性：`b5454` 是固定标签，GitHub 为发布里的每个文件提供 sha256 摘要（2026-10-28 用
/// releases API 的 `digest` 字段核对）。以前这里的地址是 `whisper-bin-x64.zip`（8.9 MB 的 **CPU 版**），
/// 还被直接改名成 exe 使用，已一并修正：现在下载的是 zip，由调用方解压、自检后再整目录替换。
pub fn whisper_cuda() -> Asset {
    Asset::new(
        "whisper-cuda",
        vec![Source::sha256(
            "https://github.com/ggml-org/whisper.cpp/releases/download/b5454/whisper-bin-win-cuda-11.8.0-x64.zip",
            "e81c86c542be6e3d7b7245392d4e1c52ab6e5c61dbe38721754bc0ab59d4d027",
        )],
    )
}

/// Windows 一键配置的 Python 安装器：官方 CPython 3.11.9（amd64），下载后会被**静默执行**。
///
/// 哈希 2026-10-28 从 python.org 下载后计算，并与官方发布页的 MD5（`e8dcd502e34932eebcaf1be056d5cbcd`）
/// 和 `.sigstore` 包里的 SHA2_256 摘要交叉核对一致。地址以前多了一段 `amd64/`（该路径 404），已修正。
pub fn python_installer() -> Asset {
    Asset::new(
        "python",
        vec![Source::sha256(
            "https://www.python.org/ftp/python/3.11.9/python-3.11.9-amd64.exe",
            "5ee42c4eee1e6b4464bb23722f90b45303f79442df63083f05322f1785f5fdde",
        )],
    )
}

/// Windows 的 tesseract 安装器（UB-Mannheim 打包，Inno Setup），下载后会被**静默执行**。
///
/// 多级下载源：GitHub 直连在国内基本拉不动，`ghfast.top` 加速代理优先，官方源兜底。
/// 两个源共用**同一个哈希**（2026-10-28 从官方 GitHub 源下载后计算）：代理即使返回了被篡改的文件，
/// 也过不了校验，不会被执行。
pub fn tesseract_installer() -> Asset {
    const SHA256: &str = "c885fff6998e0608ba4bb8ab51436e1c6775c2bafc2559a19b423e18678b60c9";
    const PATH: &str =
        "UB-Mannheim/tesseract/releases/download/v5.4.0.20240606/tesseract-ocr-w64-setup-5.4.0.20240606.exe";
    Asset::new(
        "tesseract",
        vec![
            Source::sha256(
                format!("https://ghfast.top/https://github.com/{PATH}"),
                SHA256,
            ),
            Source::sha256(format!("https://github.com/{PATH}"), SHA256),
        ],
    )
}

/// tesseract 简体中文语言包（tessdata_fast）。钉到固定 commit，三个源共用同一个哈希。
///
/// 以前跟随 `main` 分支、不校验：内容会随上游变，下载中断留下的半截文件也会被当成「已安装」。
/// 哈希 2026-10-28 计算，内容与 GitHub 该 commit 下的 git blob `388bac27…7dab` 一致。
pub fn chi_sim_traineddata() -> Asset {
    const COMMIT: &str = "87416418657359cb625c412a48b6e1d6d41c29bd";
    const SHA256: &str = "a5fcb6f0db1e1d6d8522f39db4e848f05984669172e584e8d76b6b3141e1f730";
    Asset::new(
        "chi_sim",
        vec![
            Source::sha256(
                format!("https://raw.githubusercontent.com/tesseract-ocr/tessdata_fast/{COMMIT}/chi_sim.traineddata"),
                SHA256,
            ),
            Source::sha256(
                format!("https://cdn.jsdelivr.net/gh/tesseract-ocr/tessdata_fast@{COMMIT}/chi_sim.traineddata"),
                SHA256,
            ),
            Source::sha256(
                format!("https://ghfast.top/https://raw.githubusercontent.com/tesseract-ocr/tessdata_fast/{COMMIT}/chi_sim.traineddata"),
                SHA256,
            ),
        ],
    )
}

/// 文件头是不是 Windows 可执行文件（PE，以 `MZ` 开头）。
pub fn is_pe_header(bytes: &[u8]) -> bool {
    bytes.starts_with(b"MZ")
}

/// 文件头是不是 zip（`PK\x03\x04`）。用于发现「zip 被当成 exe」这类损坏。
pub fn is_zip_header(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04")
}

/// 读文件开头几个字节（读不到返回空）。
pub fn read_header(path: &Path) -> Vec<u8> {
    use std::io::Read;
    let mut buffer = [0_u8; 4];
    std::fs::File::open(path)
        .and_then(|mut file| file.read(&mut buffer).map(|n| buffer[..n].to_vec()))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    const GOOD: &[u8] = b"hello-ffmpeg";

    /// GOOD 内容的 sha256（运行时算出后泄漏成 'static，供 Integrity::Sha256 使用）
    fn good_hash() -> &'static str {
        let mut h = Sha256::new();
        h.update(GOOD);
        let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        Box::leak(hex.into_boxed_str())
    }

    fn dir(name: &str) -> std::path::PathBuf {
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../target/downloads-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// GOOD 内容的 sha256（运行时算出后泄漏成 'static，测试进程内一次性使用）

    #[test]
    fn verifies_hash() {
        let d = dir("verify");
        let f = d.join("a.bin");
        std::fs::write(&f, GOOD).unwrap();
        let hash = Integrity::Sha256(good_hash());
        assert!(verify_file(&f, &hash), "真实哈希必须能验证通过");
        assert!(
            verify_file(
                &f,
                &Integrity::Sha256(Box::leak(good_hash().to_uppercase().into_boxed_str()))
            ),
            "大写哈希也应接受"
        );
        assert!(!verify_file(
            &f,
            &Integrity::Sha256(Box::leak("0".repeat(64).into_boxed_str()))
        ));
        assert!(!verify_file(&d.join("missing.bin"), &hash));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn skips_download_when_existing_file_matches() {
        let d = dir("existing");
        let target = d.join("t.bin");
        std::fs::write(&target, GOOD).unwrap();
        let asset = Asset::new(
            "test",
            vec![Source::sha256("https://example.invalid/a", good_hash())],
        );
        let mut called = false;
        let out = fetch_with_fallback(&asset, &target, |_, _| {
            called = true;
            Ok(())
        })
        .unwrap();
        assert_eq!(out, Outcome::Existing);
        assert!(!called, "已就位时不应发起下载");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn falls_back_to_second_source() {
        let d = dir("fallback");
        let target = d.join("t.bin");
        let asset = Asset::new(
            "test",
            vec![
                Source::sha256("https://example.invalid/bad", good_hash()),
                Source::sha256("https://example.invalid/good", good_hash()),
            ],
        );
        let mut seen = Vec::new();
        let out = fetch_with_fallback(&asset, &target, |url, path| {
            seen.push(url.to_string());
            if url.ends_with("bad") {
                return Err(Failure::Network);
            }
            std::fs::write(path, GOOD).unwrap();
            Ok(())
        })
        .unwrap();
        assert_eq!(out, Outcome::Downloaded { index: 1 });
        assert_eq!(seen.len(), 2, "第一个失败后应尝试第二个源");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rejects_wrong_content_and_reports_checksum() {
        let d = dir("checksum");
        let target = d.join("t.bin");
        let asset = Asset::new(
            "test",
            vec![Source::sha256("https://example.invalid/a", good_hash())],
        );
        let err = fetch_with_fallback(&asset, &target, |_, path| {
            std::fs::write(path, b"tampered").unwrap();
            Ok(())
        })
        .unwrap_err();
        assert_eq!(
            err.0,
            Failure::Checksum,
            "内容不对必须报校验失败，而不是解压失败"
        );
        assert!(!target.exists(), "坏文件必须删掉");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn all_sources_failing_keeps_last_reason() {
        let d = dir("allfail");
        let target = d.join("t.bin");
        let bad: &'static str = Box::leak("0".repeat(64).into_boxed_str());
        let asset = Asset::new(
            "test",
            vec![
                Source::sha256("https://example.invalid/1", bad),
                Source::sha256("https://example.invalid/2", bad),
            ],
        );
        let err = fetch_with_fallback(&asset, &target, |_, _| Err(Failure::Network)).unwrap_err();
        assert_eq!(err.0, Failure::Network);
        assert!(err.1.contains("/2"), "应保留最后一个源的原因：{}", err.1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn empty_source_list_is_an_error() {
        let d = dir("empty");
        let asset = Asset::new("test", Vec::new());
        let err = fetch_with_fallback(&asset, &d.join("t.bin"), |_, _| Ok(())).unwrap_err();
        assert_eq!(err.0, Failure::Network);
        assert!(err.1.contains("没有配置可用的下载源"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// 下载后会被执行（或解压后执行）的资源，每个源都必须钉 sha256：
    /// 代理 / 镜像 / 链路被篡改时宁可装不上，也不能执行来历不明的程序。
    #[test]
    fn executable_assets_are_all_pinned_by_sha256() {
        for asset in [
            python_installer(),
            tesseract_installer(),
            whisper_cuda(),
            chi_sim_traineddata(),
        ] {
            assert!(!asset.sources.is_empty(), "{} 没有下载源", asset.name);
            let hashes: Vec<&str> = asset
                .sources
                .iter()
                .map(|source| match source.integrity {
                    Integrity::Sha256(hash) => hash,
                    ref other => {
                        panic!("{} 的源 {} 没有 sha256：{other:?}", asset.name, source.url)
                    }
                })
                .collect();
            for hash in &hashes {
                assert_eq!(hash.len(), 64, "{} 的哈希长度不对", asset.name);
                assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
            }
            // 镜像与官方源必须是同一个文件：共用同一个哈希
            assert!(
                hashes.windows(2).all(|pair| pair[0] == pair[1]),
                "{} 的多个源哈希不一致",
                asset.name
            );
            assert!(
                asset
                    .sources
                    .iter()
                    .all(|source| source.url.starts_with("https://")),
                "{} 有非 HTTPS 的源",
                asset.name
            );
        }
        // CUDA 包必须真的是 CUDA 版（以前误用了 8.9 MB 的 CPU 版 whisper-bin-x64.zip）
        assert!(whisper_cuda().sources[0].url.contains("cuda-11.8"));
    }

    #[test]
    fn header_sniffing() {
        assert!(is_pe_header(b"MZ\x90\x00"));
        assert!(!is_pe_header(b"PK\x03\x04"));
        assert!(is_zip_header(b"PK\x03\x04rest"));
        assert!(!is_zip_header(b"MZ"));
        assert!(read_header(std::path::Path::new("/不存在/的文件")).is_empty());
    }

    /// macOS 兜底源必须是「版本化地址 + 合法 sha256」，不能再退回只校验字节数。
    #[test]
    fn evermeet_sources_are_pinned_by_sha256() {
        for (name, digest) in [
            ("ffmpeg", EVERMEET.ffmpeg_sha256),
            ("ffprobe", EVERMEET.ffprobe_sha256),
        ] {
            let source = evermeet_source(name, digest);
            assert_eq!(
                source.url,
                format!("https://evermeet.cx/ffmpeg/{name}-9.0.2.zip")
            );
            match source.integrity {
                Integrity::Sha256(value) => {
                    assert_eq!(value.len(), 64);
                    assert!(value.bytes().all(|b| b.is_ascii_hexdigit()));
                }
                Integrity::None => panic!("{name} 必须有 sha256"),
            }
        }
        assert_ne!(EVERMEET.ffmpeg_sha256, EVERMEET.ffprobe_sha256);
    }
}
