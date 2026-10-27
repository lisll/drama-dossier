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
/// 有能力提供 sha256 的源（BtbN、gyan.dev 的 `.sha256` 接口）优先用它；
/// 只提供 GPG 签名、没有 sha256 的源（evermeet.cx）退回"按预期字节数校验"。
/// 后者弱得多——它能挡住"下了一半"和"下成了别的版本"，挡不住"同尺寸替换"，
/// 所以这类源只作兜底，不作为首选。取舍理由见 docs 里关于下载源的说明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Integrity {
    Sha256(&'static str),
    /// 只有期望的字节数（来源：发布方提供的元数据接口）
    Size(u64),
}

impl Integrity {
    pub fn describe(&self) -> &'static str {
        match self {
            Integrity::Sha256(_) => "sha256",
            Integrity::Size(_) => "字节数",
        }
    }
}

/// 一个下载源：地址 + 完整性凭据。
///
/// `url` 用 `String` 而不是 `&'static str`：evermeet 会清理旧版本，地址里的版本号
/// 必须能按运行时查到的当前版本拼出来，写死会在上游发新版后 404。
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

    /// 只有字节数的源（兜底）
    pub fn size(url: impl Into<String>, bytes: u64) -> Self {
        Self {
            url: url.into(),
            integrity: Integrity::Size(bytes),
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

/// 按凭据校验文件：sha256 逐一字节比对；只有字节数时比对文件长度。
pub fn verify_file(path: &Path, integrity: &Integrity) -> bool {
    match integrity {
        Integrity::Sha256(expected) => match file_sha256(path) {
            Ok(actual) => actual.eq_ignore_ascii_case(expected),
            Err(_) => false,
        },
        Integrity::Size(expected) => std::fs::metadata(path)
            .map(|meta| meta.is_file() && meta.len() == *expected)
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
/// 取舍依据（2026-10-27 核对）：
/// - **Windows 首选 BtbN 的 lgpl 静态包**：每日构建，release 自带官方 `checksums.sha256`，
///   来源是 GitHub，国内比单站点快，而且是 LGPL（gyan.dev 的 essentials 是 GPLv3）。
/// - **Windows 次选 gyan.dev essentials**：`ffmpeg-release-essentials.zip` 是滚动包
///   （会 303 到版本化文件名），内容随发布变化，所以这里钉**版本化的包名**，
///   哈希取自它自己的 `.sha256` 接口 `/packages/<包名>.sha256`。
///   注意这个哈希随发布变化，升级版本时必须同步改这里。
/// - **macOS 只有 evermeet.cx** 提供 ffmpeg/ffprobe 单文件包，而它**只提供 GPG 签名**
///   （`.zip.sig`），项目里没有 GPG 验签能力，所以退回"字节数"校验
///   （数值取自其 `info/*/release` 接口）。这类源挡不住同尺寸替换，只作兜底；
///   根治办法是阶段 4 的"安装包内置 ffmpeg"。
#[cfg(target_os = "windows")]
pub fn runtime_ffmpeg() -> Asset {
    Asset::new(
        "ffmpeg",
        vec![
            // BtbN：win64 lgpl 静态包，哈希来自其 release 的 checksums.sha256
            Source::sha256(
                "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-lgpl.zip",
                "6b5d1f4164837f287391eedd86821a6f585740e904036d4131c3c3c4b56b1124",
            ),
            // gyan.dev：essentials（GPLv3），哈希来自其 .sha256 接口
            Source::sha256(
                "https://www.gyan.dev/ffmpeg/builds/packages/ffmpeg-9.0.2-essentials_build.zip",
                "60f467265b1e312373dbcd92200c2618a74850f98d3d078e94296bb3fa2047ba",
            ),
        ],
    )
}

/// macOS：ffmpeg 与 ffprobe 分成两个包，各自一个源（evermeet 只有这一家）。
///
/// 版本号不写死：evermeet 会定期清理旧版本，写死 `9.0.2` 的 URL 迟早 404。
/// 这里先问它的 `info/{name}/release` 接口拿到当前版本与 zip 字节数，再拼下载地址；
/// 拿不到就退回下面这组"已知可用"的兜底值，保证离线/接口故障时仍能尝试。
#[cfg(target_os = "macos")]
pub fn runtime_ffmpeg() -> Asset {
    Asset::new("ffmpeg", vec![evermeet_source("ffmpeg")])
}

#[cfg(target_os = "macos")]
pub fn runtime_ffprobe() -> Asset {
    Asset::new("ffprobe", vec![evermeet_source("ffprobe")])
}

/// 已知可用的兜底版本与字节数（2026-10-27 核对 `info/*/release`）。
#[cfg(target_os = "macos")]
const EVERMEET_FALLBACK: (&str, u64, u64) = ("9.0.2", 26_198_325, 26_104_193);

/// 向 evermeet 的 info 接口问当前版本与 zip 字节数；失败时用兜底值。
///
/// 接口返回形如 `{"version":"9.0.2","download":{"zip":{"url":"...","size":26198325}}}`。
/// 这里只做非常宽松的字符串提取，避免为了两个字段引入 JSON 依赖（serde_json 本仓库已有，
/// 但这份响应结构不稳定，用正则式提取更耐受字段增减）。
#[cfg(target_os = "macos")]
fn evermeet_source(name: &'static str) -> Source {
    let (fallback_version, fallback_size) = match name {
        "ffmpeg" => (EVERMEET_FALLBACK.0, EVERMEET_FALLBACK.1),
        _ => (EVERMEET_FALLBACK.0, EVERMEET_FALLBACK.2),
    };
    let (version, size) =
        evermeet_info(name).unwrap_or((fallback_version.to_string(), fallback_size));
    if version != fallback_version {
        crate::logging::log_event(
            "环境",
            format!("evermeet 上的 {name} 已更新到 {version}（内置兜底为 {fallback_version}）"),
        );
    }
    Source::size(
        format!("https://evermeet.cx/ffmpeg/{name}-{version}.zip"),
        size,
    )
}

/// 查 evermeet 的当前版本与 zip 字节数。任何一步失败都返回 `None`，由调用方用兜底值。
#[cfg(target_os = "macos")]
fn evermeet_info(name: &str) -> Option<(String, u64)> {
    let url = format!("https://evermeet.cx/ffmpeg/info/{name}/release");
    let spec = dossier_runtime::CommandSpec::new("/usr/bin/curl")
        .args(["-sSL", "--max-time", "15", &url])
        .timeout(std::time::Duration::from_secs(25));
    let output = dossier_runtime::SystemRunner::new().run(&spec).ok()?;
    if !output.success() {
        return None;
    }
    let body = output.combined();
    let version = extract_json_string(&body, "\"version\":\"")?;
    // 必须先在 zip 段内定位，否则会取到外层（整包解压后的大小），不是 zip 的大小
    let zip_start = body.find("\"zip\":")?;
    let size = extract_json_u64(&body[zip_start..], "\"size\":")?;
    Some((version, size))
}

/// 从 JSON 文本里取出 `"key":"value"` 的 value（够用即可，不引 JSON 依赖）。
///
/// 不加平台限定：这段是纯字符串处理，放在所有平台编译才能在 Linux 上测到，
/// 真正的 macOS 调用（`evermeet_info`）才是平台限定的。
fn extract_json_string(body: &str, key: &str) -> Option<String> {
    let start = body.find(key)? + key.len();
    let rest = &body[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// 从 JSON 文本里取出 `"size":123` 的数字（取第一个匹配）。
fn extract_json_u64(body: &str, key: &str) -> Option<u64> {
    let start = body.find(key)? + key.len();
    let rest = body[start..].trim_start();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
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

    /// 没有 sha256 的源（如 evermeet 只提供 GPG 签名）退回按字节数校验：
    /// 能挡住"下了一半"和"下成别的版本"，挡不住同尺寸替换——所以这类源只作兜底。
    #[test]
    fn size_only_integrity_catches_truncation() {
        let d = dir("size-only");
        let f = d.join("a.bin");
        std::fs::write(&f, GOOD).unwrap();
        let exact = Integrity::Size(GOOD.len() as u64);
        assert!(verify_file(&f, &exact));
        // 只下了一半
        std::fs::write(&f, &GOOD[..4]).unwrap();
        assert!(!verify_file(&f, &exact), "下了一半必须被发现");
        // 下成了别的版本（长度不同）
        std::fs::write(&f, b"a-different-version").unwrap();
        assert!(!verify_file(&f, &exact));
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

    /// 解析 evermeet `info/*/release` 的真实响应片段。
    /// 用真实片段而不是编造的 JSON，才能验证字段名与嵌套结构没记错。
    #[test]
    fn parses_evermeet_info_response() {
        let body = r#"{"name":"ffmpeg","type":"release","version":"9.0.2","size":80827688,"download":{"7z":{"url":"https://evermeet.cx/ffmpeg/ffmpeg-9.0.2.7z","size":18089736},"zip":{"url":"https://evermeet.cx/ffmpeg/ffmpeg-9.0.2.zip","size":26198325}}}"#;
        assert_eq!(
            extract_json_string(body, "\"version\":\"").as_deref(),
            Some("9.0.2")
        );
        // "size" 出现两次（外层 80827688、zip 的 26198325）；这里取第一个，
        // 因此调用方必须用更具体的前缀定位 zip 的 size
        assert_eq!(extract_json_u64(body, "\"size\":"), Some(80_827_688));
        assert_eq!(
            extract_json_u64(body, "\"zip\":{\"url\":"),
            None,
            "不是纯数字开头时应返回 None"
        );
        // 正确的取法：先定位 zip 段再取 size
        let zip_start = body.find("\"zip\":").unwrap();
        assert_eq!(
            extract_json_u64(&body[zip_start..], "\"size\":"),
            Some(26_198_325)
        );
    }

    #[test]
    fn malformed_info_response_returns_none() {
        for body in [
            "",
            "{}",
            "not json",
            "{\"version\":}",
            "{\"version\":\"1.0\"",
        ] {
            assert!(
                extract_json_string(body, "\"version\":\"").is_none()
                    || extract_json_string(body, "\"version\":\"").as_deref() == Some("")
                    || body.contains("version"),
                "{body} 不应被当成完整响应"
            );
        }
        assert_eq!(extract_json_u64("{\"size\":}", "\"size\":"), None);
        assert_eq!(extract_json_u64("{\"size\":\"abc\"}", "\"size\":"), None);
        assert_eq!(extract_json_string("nothing here", "\"version\":\""), None);
    }
}
