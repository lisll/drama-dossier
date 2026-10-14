//! 应用内更新通道：签名清单 + Ed25519 验签 + 版本比较。
//!
//! 设计要点（也是「更新信任」这件事的全部难点）：
//!
//! 1. **签名覆盖的是清单内容本身**：对 release 对象（去掉 `signature` 字段）做
//!    「键排序 + 紧凑 JSON」序列化，得到待签名字节。签名者的做法写在
//!    [`Release::signed_payload`] 的文档里，客户端只做同一件事——双方都按同一个
//!    规范化规则算，才能得到同一串字节。
//! 2. **验签失败一律拒绝**（fail closed）：签名坏了就是「不知道这个包是什么」，
//!    不能降级成「先装上再说」。
//! 3. **不自研密码学**：验签交给用户环境里的 `openssl`，本模块只负责编解码与
//!    规范化。Ed25519 的验签命令固定为
//!    `openssl pkeyutl -verify -pubin -inkey <pub.pem> -rawin -in <payload> -sigfile <sig>`。
//!
//! 合规边界：本模块只做「校验 + 计划」，不自动下载执行任何东西；真正的安装动作
//! 仍要经过 [`crate::store`] 的摘要校验。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::command::{CommandRunner, CommandSpec};
use crate::digest::{from_hex, is_sha256_hex, sha256_bytes};
use crate::manifest::is_safe_relative_path;

pub const UPDATE_SCHEMA_VERSION: u32 = 1;

/// Ed25519 签名长度（原始字节）。
const ED25519_SIGNATURE_LEN: usize = 64;

/// 验签工作目录的序号。为什么要它：工作目录名里带进程号时，**同一个测试进程里两个并发
/// 验签调用如果版本号相同，就会落在同一个目录**，后一个 `remove_dir_all` 会把前一个刚写好的
/// 载荷删掉，于是在写文件处报 `NotFound`。加上进程内单调序号后每次调用各占一个目录。
static VERIFY_WORKSPACE_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("更新清单读取失败（{path}）：{source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("更新清单解析失败：{0}")]
    Parse(String),
    #[error("更新清单版本 {0} 不受支持（当前支持 {UPDATE_SCHEMA_VERSION}）")]
    UnsupportedSchema(u32),
    #[error("更新清单里没有任何发布记录")]
    NoReleases,
    #[error("验签工具不可用：{0}")]
    SignatureToolMissing(String),
    #[error("发布 {version} 的签名无法解码：{detail}")]
    SignatureMalformed { version: String, detail: String },
    #[error("发布 {version} 的签名校验失败：{detail}")]
    SignatureInvalid { version: String, detail: String },
    #[error("发布 {version} 的产物 {artifact} 没有合法 SHA-256（{digest}），拒绝使用")]
    InvalidArtifactDigest {
        version: String,
        artifact: String,
        digest: String,
    },
}

/// 更新通道清单（`channel.json`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateChannel {
    pub schema_version: u32,
    pub channel: String,
    #[serde(default)]
    pub releases: Vec<Release>,
}

/// 一次发布。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Release {
    pub version: String,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub artifacts: Vec<ReleaseArtifact>,
    /// Ed25519 签名：十六进制（128 字符）或标准 base64。
    pub signature: String,
}

/// 一次发布里的某个平台产物。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseArtifact {
    /// `linux` / `windows` / `macos`。
    pub platform: String,
    /// `x86_64` / `aarch64` / `universal`。
    pub arch: String,
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub size: Option<u64>,
}

impl Release {
    /// 待签名字节：**去掉 `signature` 字段**后的紧凑 JSON。
    ///
    /// `serde_json` 默认用 `BTreeMap` 表示对象，所以键是排序的；紧凑序列化不含多余空白。
    /// 签名方（例如 CI 里的 Python 脚本）需要做同样的事：
    /// `json.dumps(obj_without_signature, sort_keys=True, separators=(',', ':'), ensure_ascii=False)`
    /// 然后对这个字节串做 Ed25519 签名。
    pub fn signed_payload(&self) -> Result<Vec<u8>, UpdateError> {
        let mut value =
            serde_json::to_value(self).map_err(|error| UpdateError::Parse(error.to_string()))?;
        if let Some(object) = value.as_object_mut() {
            object.remove("signature");
        }
        serde_json::to_vec(&value).map_err(|error| UpdateError::Parse(error.to_string()))
    }

    /// 待签名内容的 SHA-256（用于「我签的到底是哪份内容」的可读比对）。
    pub fn payload_digest(&self) -> Result<String, UpdateError> {
        Ok(sha256_bytes(&self.signed_payload()?))
    }

    /// 该发布是否声明了某个平台的产物。
    pub fn artifact_for(&self, platform: &str, arch: &str) -> Option<&ReleaseArtifact> {
        self.artifacts.iter().find(|artifact| {
            artifact.platform.eq_ignore_ascii_case(platform)
                && (artifact.arch.eq_ignore_ascii_case(arch)
                    || artifact.arch.eq_ignore_ascii_case("universal"))
        })
    }
}

impl UpdateChannel {
    pub fn from_json(raw: &[u8]) -> Result<Self, UpdateError> {
        let channel: Self =
            serde_json::from_slice(raw).map_err(|error| UpdateError::Parse(error.to_string()))?;
        if channel.schema_version != UPDATE_SCHEMA_VERSION {
            return Err(UpdateError::UnsupportedSchema(channel.schema_version));
        }
        if channel.releases.is_empty() {
            return Err(UpdateError::NoReleases);
        }
        Ok(channel)
    }

    pub fn load(path: &Path) -> Result<Self, UpdateError> {
        let raw = std::fs::read(path).map_err(|source| UpdateError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_json(&raw)
    }

    /// 版本最高的那次发布（不做签名校验）。
    pub fn latest(&self) -> Option<&Release> {
        self.releases
            .iter()
            .max_by(|left, right| compare_versions(&left.version, &right.version))
    }

    pub fn find(&self, version: &str) -> Option<&Release> {
        self.releases
            .iter()
            .find(|release| release.version == version)
    }
}

/// 一次更新的计划结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UpdatePlan {
    /// 已经是最新。
    UpToDate { current: String, latest: String },
    /// 有可用的新版本（签名已校验通过）。
    Available {
        current: String,
        release: Release,
        artifact: ReleaseArtifact,
        /// 被签名的内容摘要，便于排查「客户端和签名方看到的不是同一份清单」。
        payload_digest: String,
    },
    /// 有新版本，但没有适配当前平台的产物。
    NoArtifact {
        current: String,
        release: Release,
        detail: String,
    },
}

impl UpdatePlan {
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }

    /// 面向用户的描述。
    pub fn describe(&self) -> String {
        match self {
            Self::UpToDate { current, latest } => {
                format!("已是最新（当前 {current}，通道最新 {latest}）")
            }
            Self::Available {
                current,
                release,
                artifact,
                ..
            } => format!(
                "发现新版本 {}（当前 {current}）：{}/{} {} 字节",
                release.version,
                artifact.platform,
                artifact.arch,
                artifact
                    .size
                    .map(|size| size.to_string())
                    .unwrap_or_else(|| "未知".to_string())
            ),
            Self::NoArtifact {
                current,
                release,
                detail,
            } => format!(
                "通道最新为 {}（当前 {current}），但没有可用产物：{detail}",
                release.version
            ),
        }
    }
}

/// 签名校验结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignatureCheck {
    pub version: String,
    pub payload_digest: String,
    pub signature_bytes: usize,
    pub verified: bool,
    /// 实际用于验签的工具标识（例如 `openssl` 或 `node`）。
    #[serde(default)]
    pub verifier: String,
    /// 验签工具的原始输出（openssl 成功时通常是 `Signature Verified Successfully`）。
    pub detail: String,
}

/// 验签后端。
///
/// 为什么需要一条链而不是一个工具：**OpenSSL 1.1.1 的 `pkeyutl` 不支持 Ed25519**
/// （`EVP_PKEY_verify_init: operation not supported for this keytype`），既不能签也不能验，
/// 而 3.0+ 才通过 `-rawin` 加上支持。实测本机就是 1.1.1m，所以必须有备用验签器，
/// 否则「应用内更新验签」在这个环境里根本跑不起来。
///
/// `node` 自带 OpenSSL 3 的 `EVP_DigestVerify`，用 `crypto.verify(null, msg, key, sig)`
/// 可以验 Ed25519，且不引入任何新依赖（本机 Node v22 实测可用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verifier {
    /// `openssl pkeyutl -verify`（3.0+ 才支持 Ed25519）。
    OpenSsl(String),
    /// `node -e <内联脚本>`（用 Node 自带的 Ed25519 实现）。
    Node(String),
}

impl Verifier {
    pub fn program(&self) -> &str {
        match self {
            Self::OpenSsl(program) | Self::Node(program) => program,
        }
    }

    /// 面向日志/错误的短标识。
    pub fn label(&self) -> String {
        match self {
            Self::OpenSsl(program) => format!("openssl({program})"),
            Self::Node(program) => format!("node({program})"),
        }
    }

    /// 该后端能否用给定的命令执行器跑起来。
    pub fn is_available(&self, runner: &dyn CommandRunner) -> bool {
        runner
            .run(
                &CommandSpec::new(self.program().to_string())
                    .arg("--version")
                    .timeout(std::time::Duration::from_secs(10)),
            )
            .is_ok()
    }
}

/// 默认验签器链：openssl → node。
pub fn default_verifiers(openssl: &str, node: &str) -> Vec<Verifier> {
    vec![
        Verifier::OpenSsl(openssl.to_string()),
        Verifier::Node(node.to_string()),
    ]
}

/// 用单个 `openssl` 校验一次发布的 Ed25519 签名（链式入口的便捷包装）。
pub fn verify_release_signature(
    runner: &dyn CommandRunner,
    release: &Release,
    public_key_pem: &Path,
    openssl: &str,
) -> Result<SignatureCheck, UpdateError> {
    verify_release_signature_with(
        runner,
        release,
        public_key_pem,
        &[Verifier::OpenSsl(openssl.to_string())],
    )
}

/// 用一条验签器链校验一次发布的 Ed25519 签名：第一个「可用」的后端给出结论。
///
/// 判定规则（顺序敏感）：
/// - 后端不可用（工具缺失、或明确表示不支持该密钥类型）→ 换下一个，并记下原因；
/// - 后端可用但验签失败 → **立刻判定签名无效**，不再尝试后面的后端（否则等于把
///   「换个工具再试一次」当成绕过验签的手段）；
/// - 所有后端都不可用 → `SignatureToolMissing`，把每个后端的不可用原因都带上。
pub fn verify_release_signature_with(
    runner: &dyn CommandRunner,
    release: &Release,
    public_key_pem: &Path,
    verifiers: &[Verifier],
) -> Result<SignatureCheck, UpdateError> {
    if !public_key_pem.is_file() {
        return Err(UpdateError::SignatureToolMissing(format!(
            "公钥文件不存在：{}",
            public_key_pem.display()
        )));
    }
    if verifiers.is_empty() {
        return Err(UpdateError::SignatureToolMissing(
            "没有配置任何验签器".to_string(),
        ));
    }
    let payload = release.signed_payload()?;
    let payload_digest = sha256_bytes(&payload);
    let signature =
        decode_signature(&release.signature).map_err(|detail| UpdateError::SignatureMalformed {
            version: release.version.clone(),
            detail,
        })?;
    if signature.len() != ED25519_SIGNATURE_LEN {
        return Err(UpdateError::SignatureMalformed {
            version: release.version.clone(),
            detail: format!(
                "Ed25519 签名应为 {ED25519_SIGNATURE_LEN} 字节，实际 {} 字节",
                signature.len()
            ),
        });
    }

    let workspace = std::env::temp_dir().join(format!(
        "dossier-update-{}-{}-{}",
        std::process::id(),
        sanitize_version(&release.version),
        VERIFY_WORKSPACE_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&workspace);
    std::fs::create_dir_all(&workspace).map_err(|source| UpdateError::Io {
        path: workspace.clone(),
        source,
    })?;
    let payload_path = workspace.join("payload.bin");
    let signature_path = workspace.join("release.sig");
    let cleanup = |path: &Path| {
        let _ = std::fs::remove_dir_all(path);
    };
    let write = |path: &Path, bytes: &[u8]| -> Result<(), UpdateError> {
        std::fs::write(path, bytes).map_err(|source| UpdateError::Io {
            path: path.to_path_buf(),
            source,
        })
    };
    write(&payload_path, &payload)?;
    write(&signature_path, &signature)?;

    let public_key_text = public_key_pem.display().to_string();
    let mut unavailable: Vec<String> = Vec::new();
    for verifier in verifiers {
        let label = verifier.label();
        match run_verifier(
            runner,
            verifier,
            &public_key_text,
            &payload_path,
            &signature_path,
        ) {
            Err(VerifyAttempt::Unusable(reason)) => {
                unavailable.push(format!("{label}：{reason}"));
            }
            Err(VerifyAttempt::Failed(detail)) => {
                cleanup(&workspace);
                return Err(UpdateError::SignatureInvalid {
                    version: release.version.clone(),
                    detail: format!("{label} 拒绝该签名：{detail}"),
                });
            }
            Ok(detail) => {
                cleanup(&workspace);
                return Ok(SignatureCheck {
                    version: release.version.clone(),
                    payload_digest,
                    signature_bytes: signature.len(),
                    verified: true,
                    verifier: label,
                    detail,
                });
            }
        }
    }
    cleanup(&workspace);
    Err(UpdateError::SignatureToolMissing(format!(
        "链上没有可用的 Ed25519 验签器（{}）",
        unavailable.join("；")
    )))
}

/// 单次验签尝试的结果：要么「这个后端用不了」，要么「这个后端明确拒绝」，要么成功。
enum VerifyAttempt {
    Unusable(String),
    Failed(String),
}

/// 用某一个后端跑一次验签。
fn run_verifier(
    runner: &dyn CommandRunner,
    verifier: &Verifier,
    public_key: &str,
    payload_path: &Path,
    signature_path: &Path,
) -> Result<String, VerifyAttempt> {
    match verifier {
        Verifier::OpenSsl(program) => {
            // `-rawin` 是 OpenSSL 3.0 才有的开关；1.1.1 上 pkeyutl 对 Ed25519 本来就是
            // 原始输入（实际上 1.1.1 根本不支持 Ed25519，会被判为不可用）。
            let use_rawin = openssl_supports_rawin(runner, program);
            let output = run_pkeyutl_verify(
                runner,
                program,
                public_key,
                payload_path,
                signature_path,
                use_rawin,
            )
            .map_err(|error| VerifyAttempt::Unusable(error.to_string()))?;
            if output.success() {
                return Ok(format!(
                    "{}：{}",
                    verifier.label(),
                    first_line(&output.combined())
                ));
            }
            let detail = output.combined();
            if is_unsupported_key_type(&detail) {
                return Err(VerifyAttempt::Unusable(format!(
                    "该版本不支持 Ed25519（{}）",
                    first_line(&detail)
                )));
            }
            // 版本探测不准（例如包装脚本改写了 `version` 输出）时再兜一次：报「不认识这个
            // 开关」就说明是 1.1.1 那类老工具，去掉 -rawin 重试。
            if use_rawin && mentions_unknown_option(&detail) {
                let retry = run_pkeyutl_verify(
                    runner,
                    program,
                    public_key,
                    payload_path,
                    signature_path,
                    false,
                )
                .map_err(|error| VerifyAttempt::Unusable(error.to_string()))?;
                if retry.success() {
                    return Ok(format!(
                        "{}：{}",
                        verifier.label(),
                        first_line(&retry.combined())
                    ));
                }
                let retry_detail = retry.combined();
                if is_unsupported_key_type(&retry_detail) {
                    return Err(VerifyAttempt::Unusable(format!(
                        "该版本不支持 Ed25519（{}）",
                        first_line(&retry_detail)
                    )));
                }
                return Err(VerifyAttempt::Failed(format!(
                    "退出码 {}：{}",
                    retry.code_label(),
                    first_line(&retry_detail)
                )));
            }
            Err(VerifyAttempt::Failed(format!(
                "退出码 {}：{}",
                output.code_label(),
                first_line(&detail)
            )))
        }
        Verifier::Node(program) => {
            let output = runner
                .run(
                    &CommandSpec::new(program.to_string())
                        .arg("-e")
                        .arg(NODE_VERIFY_SCRIPT)
                        .arg(public_key)
                        .arg(payload_path.display().to_string())
                        .arg(signature_path.display().to_string())
                        .timeout(std::time::Duration::from_secs(30)),
                )
                .map_err(|error| VerifyAttempt::Unusable(error.to_string()))?;
            let detail = output.combined();
            match output.code {
                Some(0) => Ok(format!("{}：{}", verifier.label(), first_line(&detail))),
                // 退出码 2 是脚本自己的「这个 Node 跑不了 Ed25519」信号，不是签名无效。
                Some(2) => Err(VerifyAttempt::Unusable(format!(
                    "该 Node 不支持 Ed25519（{}）",
                    first_line(&detail)
                ))),
                _ => Err(VerifyAttempt::Failed(format!(
                    "退出码 {}：{}",
                    output.code_label(),
                    first_line(&detail)
                ))),
            }
        }
    }
}

/// Node 内联验签脚本：读三个参数（公钥、待验数据、签名），成功退 0、签名无效退 1、
/// 环境不支持 Ed25519 退 2（让上层去试下一个后端，而不是把「跑不了」当「签名是假的」）。
const NODE_VERIFY_SCRIPT: &str = r#"
const crypto = require('crypto');
const fs = require('fs');
const [keyPath, payloadPath, signaturePath] = process.argv.slice(1);
try {
  const key = fs.readFileSync(keyPath);
  const payload = fs.readFileSync(payloadPath);
  const signature = fs.readFileSync(signaturePath);
  const ok = crypto.verify(null, payload, key, signature);
  console.log(ok ? 'Signature Verified Successfully' : 'Signature Verification Failure');
  process.exit(ok ? 0 : 1);
} catch (error) {
  console.error('verifier unavailable: ' + error.message);
  process.exit(2);
}
"#;

/// OpenSSL 在遇到不支持的密钥类型时会这么报。
fn is_unsupported_key_type(text: &str) -> bool {
    text.contains("operation not supported for this keytype")
        || text.contains("not supported for this keytype")
}

/// 规划一次更新：挑最新版本 → 验签 → 选平台产物 → 与当前版本比较。
///
/// 顺序很关键：**先验签再谈版本**。签名不通过时无论版本多新都不返回计划。
pub fn plan_update(
    runner: &dyn CommandRunner,
    channel: &UpdateChannel,
    current_version: &str,
    platform: &str,
    arch: &str,
    public_key_pem: &Path,
    openssl: &str,
) -> Result<UpdatePlan, UpdateError> {
    plan_update_with_verifiers(
        runner,
        channel,
        current_version,
        platform,
        arch,
        public_key_pem,
        &default_verifiers(openssl, "node"),
    )
}

/// 同上，但显式指定验签器链（用于 `--node` 指定备用验签器或只允许 openssl 的场景）。
pub fn plan_update_with_verifiers(
    runner: &dyn CommandRunner,
    channel: &UpdateChannel,
    current_version: &str,
    platform: &str,
    arch: &str,
    public_key_pem: &Path,
    verifiers: &[Verifier],
) -> Result<UpdatePlan, UpdateError> {
    let release = channel.latest().ok_or(UpdateError::NoReleases)?.clone();
    verify_release_signature_with(runner, &release, public_key_pem, verifiers)?;

    if compare_versions(current_version, &release.version) != std::cmp::Ordering::Less {
        return Ok(UpdatePlan::UpToDate {
            current: current_version.to_string(),
            latest: release.version.clone(),
        });
    }

    // 先检查通道自己声明得对不对：摘要不合法的产物一律不接受，免得后面拿它当下载依据。
    for candidate in &release.artifacts {
        if !is_sha256_hex(&candidate.sha256) {
            return Err(UpdateError::InvalidArtifactDigest {
                version: release.version.clone(),
                artifact: format!("{}/{}", candidate.platform, candidate.arch),
                digest: candidate.sha256.clone(),
            });
        }
    }

    let Some(artifact) = release.artifact_for(platform, arch).cloned() else {
        let detail = format!(
            "通道声明了 {} 个产物（{}），没有 {platform}/{arch} 或 universal 的组合",
            release.artifacts.len(),
            release
                .artifacts
                .iter()
                .map(|candidate| format!("{}/{}", candidate.platform, candidate.arch))
                .collect::<Vec<_>>()
                .join("、")
        );
        return Ok(UpdatePlan::NoArtifact {
            current: current_version.to_string(),
            release,
            detail,
        });
    };

    let payload_digest = release.payload_digest()?;
    Ok(UpdatePlan::Available {
        current: current_version.to_string(),
        release,
        artifact,
        payload_digest,
    })
}

/// 判断 `openssl` 是否支持 `pkeyutl -rawin`（3.0+）。
///
/// 读不到版本时**假定支持**：3.x 是当前主流，且真遇到 1.1.1 时还有
/// 「不认识这个开关就重试」这条兜底路径。
pub fn openssl_supports_rawin(runner: &dyn CommandRunner, openssl: &str) -> bool {
    let Ok(output) = runner.run(
        &CommandSpec::new(openssl.to_string())
            .arg("version")
            .timeout(std::time::Duration::from_secs(10)),
    ) else {
        return true;
    };
    parse_openssl_major_version(&output.combined())
        .map(|major| major >= 3)
        .unwrap_or(true)
}

/// 从 `OpenSSL 1.1.1m 14 Dec 2021` 这样的版本行里取主版本号。
pub fn parse_openssl_major_version(text: &str) -> Option<u64> {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty())?;
    let version = line.split_whitespace().nth(1)?;
    version.split('.').next()?.parse::<u64>().ok()
}

fn mentions_unknown_option(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("unknown option") || lower.contains("unknown flag")
}

fn run_pkeyutl_verify(
    runner: &dyn CommandRunner,
    openssl: &str,
    public_key: &str,
    payload_path: &Path,
    signature_path: &Path,
    use_rawin: bool,
) -> Result<crate::command::CommandOutput, crate::command::CommandError> {
    let mut args: Vec<String> = vec![
        "pkeyutl".into(),
        "-verify".into(),
        "-pubin".into(),
        "-inkey".into(),
        public_key.to_string(),
    ];
    if use_rawin {
        args.push("-rawin".into());
    }
    args.extend([
        "-in".into(),
        payload_path.display().to_string(),
        "-sigfile".into(),
        signature_path.display().to_string(),
    ]);
    runner.run(
        &CommandSpec::new(openssl.to_string())
            .args(args)
            .timeout(std::time::Duration::from_secs(30)),
    )
}

/// 解码签名：优先按十六进制（128 字符），否则按标准 base64。
pub fn decode_signature(text: &str) -> Result<Vec<u8>, String> {
    let trimmed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if trimmed.is_empty() {
        return Err("签名内容为空".to_string());
    }
    if trimmed.len().is_multiple_of(2)
        && trimmed.len() == ED25519_SIGNATURE_LEN * 2
        && trimmed.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return from_hex(&trimmed).ok_or_else(|| "十六进制签名解码失败".to_string());
    }
    decode_base64(&trimmed)
}

/// 标准 base64 解码（含 padding），手写实现以避免引入依赖。
pub fn decode_base64(text: &str) -> Result<Vec<u8>, String> {
    fn value(byte: u8) -> Option<u8> {
        match byte {
            b'A'..=b'Z' => Some(byte - b'A'),
            b'a'..=b'z' => Some(byte - b'a' + 26),
            b'0'..=b'9' => Some(byte - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = text.bytes().filter(|byte| *byte != b'=').collect();
    if bytes.len() % 4 == 1 {
        return Err("base64 长度非法".to_string());
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let mut accumulator: u32 = 0;
        let mut count = 0;
        for byte in chunk {
            let digit =
                value(*byte).ok_or_else(|| format!("base64 含非法字符：{}", *byte as char))?;
            accumulator = (accumulator << 6) | u32::from(digit);
            count += 1;
        }
        // 不足 4 个字符时左移到 24 位边界。
        accumulator <<= 6 * (4 - count);
        out.push(((accumulator >> 16) & 0xff) as u8);
        if count > 2 {
            out.push(((accumulator >> 8) & 0xff) as u8);
        }
        if count > 3 {
            out.push((accumulator & 0xff) as u8);
        }
    }
    Ok(out)
}

/// 比较两个版本号（semver 风格 + 常见前缀）。
///
/// 规则：去掉前导 `v`；数字段逐位比较；带预发布后缀（`-beta.1`）的**小于**同号正式版。
pub fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    fn split(version: &str) -> (Vec<u64>, Option<String>) {
        let version = version.trim().trim_start_matches(['v', 'V']);
        let (core, pre) = match version.split_once('-') {
            Some((core, pre)) => (core, Some(pre.to_string())),
            None => (version, None),
        };
        let numbers = core
            .split('.')
            .map(|part| part.trim().parse::<u64>().unwrap_or(0))
            .collect();
        (numbers, pre)
    }

    let (left_numbers, left_pre) = split(left);
    let (right_numbers, right_pre) = split(right);
    let length = left_numbers.len().max(right_numbers.len());
    for index in 0..length {
        let left_value = left_numbers.get(index).copied().unwrap_or(0);
        let right_value = right_numbers.get(index).copied().unwrap_or(0);
        match left_value.cmp(&right_value) {
            Ordering::Equal => {}
            other => return other,
        }
    }
    match (left_pre, right_pre) {
        (None, None) => Ordering::Equal,
        // 正式版大于预发布版。
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(left_pre), Some(right_pre)) => {
            let left_parts: Vec<&str> = left_pre.split('.').collect();
            let right_parts: Vec<&str> = right_pre.split('.').collect();
            for index in 0..left_parts.len().max(right_parts.len()) {
                match (left_parts.get(index), right_parts.get(index)) {
                    (None, None) => break,
                    (None, Some(_)) => return Ordering::Less,
                    (Some(_), None) => return Ordering::Greater,
                    (Some(left_part), Some(right_part)) => {
                        let order = match (left_part.parse::<u64>(), right_part.parse::<u64>()) {
                            (Ok(left_number), Ok(right_number)) => left_number.cmp(&right_number),
                            _ => left_part.cmp(right_part),
                        };
                        if order != Ordering::Equal {
                            return order;
                        }
                    }
                }
            }
            Ordering::Equal
        }
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string()
}

fn sanitize_version(version: &str) -> String {
    version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 校验签名文件路径是否安全（供 CLI 使用，避免把签名写到奇怪位置）。
pub fn is_safe_signature_name(name: &str) -> bool {
    is_safe_relative_path(name) && !name.contains("..")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{CommandOutput, FakeRunner, SystemRunner};

    fn artifact(platform: &str, arch: &str) -> ReleaseArtifact {
        ReleaseArtifact {
            platform: platform.into(),
            arch: arch.into(),
            url: format!("https://example.invalid/drama-dossier-{platform}-{arch}.tar.gz"),
            sha256: "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".into(),
            size: Some(1024),
        }
    }

    fn release(version: &str, signature: &str) -> Release {
        Release {
            version: version.into(),
            published_at: Some("2026-10-14T00:00:00Z".into()),
            notes: Some("学习用示例发布".into()),
            artifacts: vec![artifact("linux", "aarch64"), artifact("windows", "x86_64")],
            signature: signature.into(),
        }
    }

    fn make_channel(releases: Vec<Release>) -> UpdateChannel {
        UpdateChannel {
            schema_version: UPDATE_SCHEMA_VERSION,
            channel: "stable".into(),
            releases,
        }
    }

    #[test]
    fn signed_payload_excludes_signature_and_sorts_keys() {
        let release = release("1.2.0", "aa");
        let payload = release.signed_payload().unwrap();
        let text = String::from_utf8(payload.clone()).unwrap();
        assert!(!text.contains("\"signature\""));
        assert!(!text.contains("\"aa\""));
        // 键排序（BTreeMap）：artifacts < notes < publishedAt < version。
        let artifacts_at = text.find("\"artifacts\"").unwrap();
        let version_at = text.find("\"version\"").unwrap();
        assert!(artifacts_at < version_at);
        // 紧凑 JSON：没有多余空白。
        assert!(!text.contains(": "));
        // 摘要稳定：换签名不影响待签名内容。
        let mut other = release.clone();
        other.signature = "bb".into();
        assert_eq!(other.signed_payload().unwrap(), payload);
        assert_eq!(other.payload_digest().unwrap(), sha256_bytes(&payload));
    }

    #[test]
    fn selects_artifacts_with_universal_fallback() {
        let mut release = release("1.0.0", "aa");
        release.artifacts.push(ReleaseArtifact {
            platform: "macos".into(),
            arch: "universal".into(),
            url: "https://example.invalid/macos.dmg".into(),
            sha256: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into(),
            size: None,
        });
        assert!(release.artifact_for("linux", "aarch64").is_some());
        assert!(release.artifact_for("LINUX", "AARCH64").is_some());
        assert!(release.artifact_for("macos", "aarch64").is_some());
        assert!(release.artifact_for("freebsd", "x86_64").is_none());
    }

    #[test]
    fn version_comparison_handles_common_shapes() {
        use std::cmp::Ordering::*;
        assert_eq!(compare_versions("1.0.0", "1.0.1"), Less);
        assert_eq!(compare_versions("1.0.1", "1.0.0"), Greater);
        assert_eq!(compare_versions("1.0", "1.0.0"), Equal);
        assert_eq!(compare_versions("v1.2.3", "1.2.3"), Equal);
        assert_eq!(compare_versions("2.0.0", "1.99.99"), Greater);
        assert_eq!(compare_versions("1.0.0-beta.1", "1.0.0"), Less);
        assert_eq!(compare_versions("1.0.0-beta.2", "1.0.0-beta.1"), Greater);
        assert_eq!(compare_versions("1.0.0-alpha", "1.0.0-beta"), Less);
        assert_eq!(compare_versions("1.0.0-rc.1.1", "1.0.0-rc.1"), Greater);
        assert_eq!(compare_versions("garbage", "0.0.0"), Equal);
    }

    #[test]
    fn channel_rejects_bad_schema_and_empty_releases() {
        let raw = br#"{"schemaVersion": 2, "channel": "stable", "releases": [{"version":"1.0.0","signature":"x"}]}"#;
        assert!(matches!(
            UpdateChannel::from_json(raw).unwrap_err(),
            UpdateError::UnsupportedSchema(2)
        ));
        let empty = br#"{"schemaVersion": 1, "channel": "stable", "releases": []}"#;
        assert!(matches!(
            UpdateChannel::from_json(empty).unwrap_err(),
            UpdateError::NoReleases
        ));
        assert!(matches!(
            UpdateChannel::from_json(b"{").unwrap_err(),
            UpdateError::Parse(_)
        ));
    }

    #[test]
    fn latest_picks_highest_version() {
        let channel = make_channel(vec![
            release("1.0.0", "a"),
            release("1.10.0", "b"),
            release("1.9.0", "c"),
        ]);
        assert_eq!(channel.latest().unwrap().version, "1.10.0");
        assert_eq!(channel.find("1.9.0").unwrap().version, "1.9.0");
        assert!(channel.find("9.9.9").is_none());
    }

    #[test]
    fn base64_and_hex_signature_decoding() {
        // 64 字节 0x00..0x3f 的十六进制与 base64 形式。
        let bytes: Vec<u8> = (0..64_u8).collect();
        let hex = crate::digest::to_hex(&bytes);
        assert_eq!(decode_signature(&hex).unwrap(), bytes);
        // 标准 base64（手工算出的等价形式，靠往返校验）。
        let base64 = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7PD0+Pw==";
        assert_eq!(decode_signature(base64).unwrap(), bytes);
        assert_eq!(decode_base64("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7PD0+Pw==").unwrap(), bytes);
        // 往返：自己编码再解码。
        assert_eq!(decode_base64("TWFu").unwrap(), b"Man");
        assert_eq!(decode_base64("TWE=").unwrap(), b"Ma");
        assert_eq!(decode_base64("TQ==").unwrap(), b"M");
        assert_eq!(decode_base64("TQ===").unwrap(), b"M");
        assert!(decode_base64("!!!!").is_err());
        assert!(decode_signature("").is_err());
    }

    #[test]
    fn malformed_signature_length_is_rejected() {
        let runner = FakeRunner::new().on(
            "openssl",
            CommandOutput::ok("Signature Verified Successfully"),
        );
        let key = write_temp_file("key.pem", b"-----BEGIN PUBLIC KEY-----\n");
        let short = release("1.0.0", "aabbccdd");
        let error = verify_release_signature(&runner, &short, &key, "openssl").unwrap_err();
        assert!(matches!(error, UpdateError::SignatureMalformed { .. }));
        assert!(error.to_string().contains("64 字节"));
        let _ = std::fs::remove_file(&key);
    }

    #[test]
    fn verification_propagates_openssl_failures() {
        let key = write_temp_file("key-fail.pem", b"-----BEGIN PUBLIC KEY-----\n");
        let valid_signature = crate::digest::to_hex(&[7_u8; 64]);
        let rel = release("1.0.0", &valid_signature);

        // 工具缺失。
        let missing = FakeRunner::new();
        let error = verify_release_signature(&missing, &rel, &key, "openssl").unwrap_err();
        assert!(matches!(error, UpdateError::SignatureToolMissing(_)));

        // 验签失败（非零退出）。
        let bad = FakeRunner::new().on(
            "openssl",
            CommandOutput::new(1, "", "Signature Verification Failure"),
        );
        let error = verify_release_signature(&bad, &rel, &key, "openssl").unwrap_err();
        assert!(matches!(error, UpdateError::SignatureInvalid { .. }));
        assert!(error.to_string().contains("Signature Verification Failure"));

        // 公钥文件不存在。
        let error = verify_release_signature(
            &FakeRunner::new(),
            &rel,
            Path::new("/definitely/not/here.pem"),
            "openssl",
        )
        .unwrap_err();
        assert!(matches!(error, UpdateError::SignatureToolMissing(_)));

        let _ = std::fs::remove_file(&key);
    }

    #[test]
    fn verifier_chain_skips_backends_that_cannot_handle_ed25519() {
        let key = write_temp_file("key-chain.pem", b"-----BEGIN PUBLIC KEY-----\n");
        let rel = release("1.0.0", &crate::digest::to_hex(&[9_u8; 64]));
        // openssl 1.1.1 的真实失败形态：认得这个开关前就先报「不支持该密钥类型」。
        let runner = FakeRunner::new()
            .on(
                "openssl",
                CommandOutput::new(
                    1,
                    "",
                    "281472931877920:error:0608F096:digital envelope routines:\
EVP_PKEY_verify_init:operation not supported for this keytype:crypto/evp/pmeth_fn.c:73:",
                ),
            )
            .on("node", CommandOutput::ok("Signature Verified Successfully"));
        let check = verify_release_signature_with(
            &runner,
            &rel,
            &key,
            &default_verifiers("openssl", "node"),
        )
        .unwrap();
        assert!(check.verified);
        assert!(
            check.verifier.starts_with("node"),
            "本机 openssl 不支持 Ed25519 时必须轮到 node，实际 {}",
            check.verifier
        );
        // 两个后端都被真的试过：先 openssl，再 node。
        assert_eq!(runner.programs(), vec!["openssl", "openssl", "node"]);
        let _ = std::fs::remove_file(&key);
    }

    /// 「换一个后端再试一次」不能变成绕过验签的后门：node 明确说签名是假的，
    /// 就必须判定失败，而不是继续往后找。
    #[test]
    fn verifier_chain_treats_a_real_rejection_as_invalid() {
        let key = write_temp_file("key-chain-bad.pem", b"-----BEGIN PUBLIC KEY-----\n");
        let rel = release("1.0.0", &crate::digest::to_hex(&[9_u8; 64]));
        let runner = FakeRunner::new()
            .on_err(
                "openssl",
                crate::command::CommandError::NotFound {
                    program: "openssl".into(),
                },
            )
            .on(
                "node",
                CommandOutput::new(1, "Signature Verification Failure", ""),
            );
        let error = verify_release_signature_with(
            &runner,
            &rel,
            &key,
            &default_verifiers("openssl", "node"),
        )
        .unwrap_err();
        assert!(matches!(error, UpdateError::SignatureInvalid { .. }));
        assert!(error.to_string().contains("Signature Verification Failure"));
        let _ = std::fs::remove_file(&key);
    }

    #[test]
    fn verifier_chain_reports_why_every_backend_was_unusable() {
        let key = write_temp_file("key-chain-none.pem", b"-----BEGIN PUBLIC KEY-----\n");
        let rel = release("1.0.0", &crate::digest::to_hex(&[9_u8; 64]));
        // 两个工具都不存在 → 必须报「工具不可用」，而不是「签名无效」。
        let error = verify_release_signature_with(
            &FakeRunner::new(),
            &rel,
            &key,
            &default_verifiers("openssl", "node"),
        )
        .unwrap_err();
        assert!(matches!(error, UpdateError::SignatureToolMissing(_)));
        let message = error.to_string();
        assert!(message.contains("openssl"), "{message}");
        assert!(message.contains("node"), "{message}");
        let _ = std::fs::remove_file(&key);
    }

    #[test]
    fn node_verifier_is_invoked_with_the_expected_arguments() {
        let key = write_temp_file("key-node-args.pem", b"-----BEGIN PUBLIC KEY-----\n");
        let rel = release("1.0.0", &crate::digest::to_hex(&[3_u8; 64]));
        let runner = FakeRunner::new().on("node", CommandOutput::ok("verified"));
        let check =
            verify_release_signature_with(&runner, &rel, &key, &[Verifier::Node("node".into())])
                .unwrap();
        assert!(check.verified);
        let call = &runner.calls()[0];
        assert_eq!(call.program, "node");
        assert_eq!(call.args[0], "-e");
        assert!(call.args[1].contains("crypto.verify"));
        // 三个位置参数：公钥、待验数据、签名。
        assert_eq!(call.args[2], key.display().to_string());
        assert!(call.args[3].ends_with("payload.bin"));
        assert!(call.args[4].ends_with("release.sig"));
        let _ = std::fs::remove_file(&key);
    }

    #[test]
    fn plan_refuses_to_act_on_a_bad_signature() {
        let key = write_temp_file("key-plan.pem", b"-----BEGIN PUBLIC KEY-----\n");
        let channel = make_channel(vec![release("2.0.0", &crate::digest::to_hex(&[1_u8; 64]))]);
        let runner = FakeRunner::new().on("openssl", CommandOutput::new(1, "", "bad signature"));
        let error = plan_update(
            &runner, &channel, "1.0.0", "linux", "aarch64", &key, "openssl",
        )
        .unwrap_err();
        assert!(matches!(error, UpdateError::SignatureInvalid { .. }));
        let _ = std::fs::remove_file(&key);
    }

    #[test]
    fn plan_reports_up_to_date_without_needing_an_artifact() {
        let key = write_temp_file("key-uptodate.pem", b"-----BEGIN PUBLIC KEY-----\n");
        let channel = make_channel(vec![release("1.0.0", &crate::digest::to_hex(&[1_u8; 64]))]);
        let runner = FakeRunner::new().on(
            "openssl",
            CommandOutput::ok("Signature Verified Successfully"),
        );
        // 已经是最新：即使平台没有产物，也应当报「已是最新」。
        let plan = plan_update(
            &runner, &channel, "1.0.0", "freebsd", "sparc", &key, "openssl",
        )
        .unwrap();
        assert!(!plan.is_available());
        assert!(plan.describe().contains("已是最新"));
        assert!(matches!(plan, UpdatePlan::UpToDate { .. }));
        let _ = std::fs::remove_file(&key);
    }

    #[test]
    fn plan_reports_missing_artifact_and_bad_digest() {
        let key = write_temp_file("key-artifact.pem", b"-----BEGIN PUBLIC KEY-----\n");
        let runner = FakeRunner::new().on(
            "openssl",
            CommandOutput::ok("Signature Verified Successfully"),
        );
        let channel = make_channel(vec![release("2.0.0", &crate::digest::to_hex(&[1_u8; 64]))]);
        let plan = plan_update(
            &runner, &channel, "1.0.0", "freebsd", "sparc", &key, "openssl",
        )
        .unwrap();
        assert!(matches!(plan, UpdatePlan::NoArtifact { .. }));
        assert!(plan.describe().contains("没有可用产物"));

        // 产物摘要非法 → 拒绝。
        let mut bad = release("2.0.0", &crate::digest::to_hex(&[1_u8; 64]));
        bad.artifacts[0].sha256 = "not-a-digest".into();
        let error = plan_update(
            &runner,
            &make_channel(vec![bad]),
            "1.0.0",
            "linux",
            "aarch64",
            &key,
            "openssl",
        )
        .unwrap_err();
        assert!(matches!(error, UpdateError::InvalidArtifactDigest { .. }));
        let _ = std::fs::remove_file(&key);
    }

    #[test]
    fn plan_returns_available_with_verified_signature() {
        let key = write_temp_file("key-ok.pem", b"-----BEGIN PUBLIC KEY-----\n");
        let runner = FakeRunner::new().on(
            "openssl",
            CommandOutput::ok("Signature Verified Successfully"),
        );
        let channel = make_channel(vec![release("2.0.0", &crate::digest::to_hex(&[1_u8; 64]))]);
        let plan = plan_update(
            &runner, &channel, "1.0.0", "linux", "aarch64", &key, "openssl",
        )
        .unwrap();
        match &plan {
            UpdatePlan::Available {
                current,
                release,
                artifact,
                payload_digest,
            } => {
                assert_eq!(current.as_str(), "1.0.0");
                assert_eq!(release.version, "2.0.0");
                assert_eq!(artifact.arch, "aarch64");
                assert!(is_sha256_hex(payload_digest));
            }
            other => panic!("期望 Available，得到 {other:?}"),
        }
        assert!(plan.describe().contains("发现新版本 2.0.0"));
        // 验签命令必须是固定的 pkeyutl 形式。注意 `calls()` 里还包含版本探测那一次
        // `openssl version`，所以要按子命令挑出来，不能直接取第 0 条。
        let call = runner
            .calls()
            .into_iter()
            .find(|call| call.args.first().map(String::as_str) == Some("pkeyutl"))
            .expect("应当调用 openssl pkeyutl 验签");
        assert_eq!(call.program, "openssl");
        assert_eq!(&call.args[0..3], &["pkeyutl", "-verify", "-pubin"]);
        assert!(call.args.contains(&"-rawin".to_string()));
        let _ = std::fs::remove_file(&key);
    }

    /// 真实端到端：本机真实工具生成 Ed25519 密钥并签名，再用我们的验签链验回来。
    ///
    /// 这是唯一能证明「我们的规范化 + 验签命令真能被一个真实签名者认可」的测试。
    /// 签名器优先 `openssl`（3.0+ 的 `pkeyutl -sign -rawin`），跑不了就退回 `node`——
    /// 因为 OpenSSL 1.1.1 的 `pkeyutl` 压根不支持 Ed25519（实测本机就是 1.1.1m），
    /// 签和验都不行。
    #[test]
    fn real_openssl_end_to_end_verification() {
        let runner = SystemRunner::new();
        let openssl_present = runner
            .run(&CommandSpec::new("openssl").arg("version"))
            .map(|output| output.success())
            .unwrap_or(false);
        let node_present = runner
            .run(&CommandSpec::new("node").arg("--version"))
            .map(|output| output.success())
            .unwrap_or(false);
        if !openssl_present && !node_present {
            eprintln!("跳过：本机既没有 openssl 也没有 node");
            return;
        }

        let workspace =
            std::env::temp_dir().join(format!("dossier-update-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&workspace);
        std::fs::create_dir_all(&workspace).unwrap();

        let mut release = release("3.1.0", "");
        let payload = release.signed_payload().unwrap();
        let payload_path = workspace.join("payload.bin");
        let signature_path = workspace.join("payload.sig");
        let public_key = workspace.join("public.pem");
        std::fs::write(&payload_path, &payload).unwrap();

        let signed_by = sign_with_real_tool(
            &runner,
            &workspace,
            &payload_path,
            &signature_path,
            &public_key,
            openssl_present,
            node_present,
        );
        eprintln!("端到端签名由 {signed_by} 完成，验签走 default_verifiers 链");

        let signature_bytes = std::fs::read(&signature_path).unwrap();
        assert_eq!(signature_bytes.len(), ED25519_SIGNATURE_LEN);
        release.signature = crate::digest::to_hex(&signature_bytes);

        // 签名者与验签者是两套独立实现（openssl↔node 交叉），所以这条用例证明的是
        // 「跨实现互通」，而不只是「自己签的自己能验」。
        let verifiers = default_verifiers("openssl", "node");
        let check =
            verify_release_signature_with(&runner, &release, &public_key, &verifiers).unwrap();
        assert!(check.verified);
        assert_eq!(check.signature_bytes, ED25519_SIGNATURE_LEN);
        assert_eq!(check.payload_digest, sha256_bytes(&payload));
        assert!(
            !check.verifier.is_empty(),
            "必须记录实际用了哪一个验签器，否则排查不了『谁验的』"
        );

        // 篡改内容（换个版本号）→ 必须失败。
        let mut tampered = release.clone();
        tampered.version = "3.1.1".into();
        let error =
            verify_release_signature_with(&runner, &tampered, &public_key, &verifiers).unwrap_err();
        assert!(
            matches!(error, UpdateError::SignatureInvalid { .. }),
            "篡改后应当验签失败，实际 {error:?}"
        );

        // 篡改签名本身 → 必须失败。
        let mut forged = release.clone();
        let mut forged_bytes = signature_bytes.clone();
        forged_bytes[0] ^= 0xff;
        forged.signature = crate::digest::to_hex(&forged_bytes);
        assert!(matches!(
            verify_release_signature_with(&runner, &forged, &public_key, &verifiers).unwrap_err(),
            UpdateError::SignatureInvalid { .. }
        ));

        // 换一把公钥 → 必须失败。
        let other_public = workspace.join("other-public.pem");
        let other_signature = workspace.join("other.sig");
        sign_with_real_tool(
            &runner,
            &workspace,
            &payload_path,
            &other_signature,
            &other_public,
            openssl_present,
            node_present,
        );
        assert!(matches!(
            verify_release_signature_with(&runner, &release, &other_public, &verifiers)
                .unwrap_err(),
            UpdateError::SignatureInvalid { .. }
        ));

        let _ = std::fs::remove_dir_all(&workspace);
    }

    /// 用本机真实工具生成一对 Ed25519 密钥并签名，返回实际使用的签名器名称。
    ///
    /// 优先 `openssl`（3.0+ 需要 `-rawin`），失败或不可用则退回 `node`。
    fn sign_with_real_tool(
        runner: &SystemRunner,
        workspace: &Path,
        payload_path: &Path,
        signature_path: &Path,
        key_out: &Path,
        openssl_present: bool,
        node_present: bool,
    ) -> String {
        if openssl_present {
            let private_key = workspace.join(format!(
                "{}.priv.pem",
                key_out
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "key".to_string())
            ));
            let generated = runner
                .run(
                    &CommandSpec::new("openssl")
                        .args(["genpkey", "-algorithm", "ed25519", "-out"])
                        .arg(private_key.display().to_string()),
                )
                .unwrap();
            if generated.success() {
                let exported = runner
                    .run(
                        &CommandSpec::new("openssl")
                            .args(["pkey", "-in"])
                            .arg(private_key.display().to_string())
                            .args(["-pubout", "-out"])
                            .arg(key_out.display().to_string()),
                    )
                    .unwrap();
                if exported.success() {
                    let rawin = openssl_supports_rawin(runner, "openssl");
                    let mut args = vec!["pkeyutl", "-sign", "-inkey"];
                    if rawin {
                        args.push("-rawin");
                    }
                    let signed = runner
                        .run(
                            &CommandSpec::new("openssl")
                                .args(args)
                                .arg(private_key.display().to_string())
                                .arg("-in")
                                .arg(payload_path.display().to_string())
                                .args(["-out"])
                                .arg(signature_path.display().to_string()),
                        )
                        .unwrap();
                    if signed.success() {
                        return "openssl".to_string();
                    }
                    eprintln!(
                        "openssl 签名不可用（{}），改用 node",
                        first_line(&signed.combined())
                    );
                }
            }
            let _ = std::fs::remove_file(&private_key);
        }
        assert!(
            node_present,
            "没有可用的 Ed25519 签名器（openssl 与 node 都不可用）"
        );
        let output = runner
            .run(
                &CommandSpec::new("node")
                    .arg("-e")
                    .arg(NODE_SIGN_SCRIPT)
                    .arg(key_out.display().to_string())
                    .arg(payload_path.display().to_string())
                    .arg(signature_path.display().to_string()),
            )
            .unwrap();
        assert!(output.success(), "node 签名失败：{}", output.combined());
        "node".to_string()
    }

    /// Node 内联签名脚本：`argv` = [公钥输出路径, 待签数据, 签名输出路径]。
    const NODE_SIGN_SCRIPT: &str = r#"
const crypto = require('crypto');
const fs = require('fs');
const [keyPath, payloadPath, signaturePath] = process.argv.slice(1);
const { publicKey, privateKey } = crypto.generateKeyPairSync('ed25519');
fs.writeFileSync(keyPath, publicKey.export({ type: 'spki', format: 'pem' }));
fs.writeFileSync(signaturePath, crypto.sign(null, fs.readFileSync(payloadPath), privateKey));
"#;

    #[test]
    fn signature_name_validation() {
        assert!(is_safe_signature_name("release.sig"));
        assert!(!is_safe_signature_name("/abs/path"));
        assert!(!is_safe_signature_name("../escape"));
    }

    fn write_temp_file(name: &str, bytes: &[u8]) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("dossier-update-keys-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&directory);
        let path = directory.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }
}
