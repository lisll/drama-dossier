//! dossier-runtime 命令行：把运行时能力管理接到脚本与桌面上。
//!
//! ```text
//! dossier-runtime probe                      # 这台机器能干什么
//! dossier-runtime check --manifest m.json    # 清单里的组件装齐了没
//! dossier-runtime install --manifest m.json --id e2fgvi --from ./models/e2fgvi
//! dossier-runtime rollback --manifest m.json --id e2fgvi
//! dossier-runtime manifest-gen --dir ./models/e2fgvi --id e2fgvi --role inpaint_model --version 1.0
//! dossier-runtime update --channel channel.json --public-key cosign.pub --current 1.0.0
//! dossier-runtime device
//! dossier-runtime secret put cloud.token        # 从 stdin 读，避免进 shell 历史
//! dossier-runtime selfcheck
//! ```
//!
//! 参数解析与输出在这里；规则全部在库里（与本仓库其它 crate 的分工一致）。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use dossier_runtime::caps::{probe_environment, ParallelismPreset, ProbeConfig};
use dossier_runtime::command::SystemRunner;
use dossier_runtime::device::compute_device_fingerprint;
use dossier_runtime::manifest::{RuntimeManifest, RuntimeRoles};
use dossier_runtime::secret::{mask, SecretStore};
use dossier_runtime::store::{
    generate_component, Archiver, Downloader, GenerateOptions, RuntimeStore, StoreConfig,
};
use dossier_runtime::update::{default_verifiers, plan_update_with_verifiers, UpdateChannel};

#[derive(Parser, Debug)]
#[command(
    name = "dossier-runtime",
    about = "运行时能力管理：探测、校验、安装、回滚、更新验签（技术学习用途）",
    long_about = None
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 探测本机运行时能力（工具 / FFmpeg / GPU / Python / 并行度）
    Probe {
        #[arg(long)]
        ffmpeg: Option<PathBuf>,
        #[arg(long)]
        ffprobe: Option<PathBuf>,
        #[arg(long)]
        python: Option<String>,
        #[arg(long, value_parser = parse_preset)]
        parallelism: Option<ParallelismPreset>,
        /// 人工指定并行度（会被收敛到 [1, 核数]）
        #[arg(long)]
        jobs: Option<usize>,
        /// 关掉「硬件编码优先」
        #[arg(long)]
        no_prefer_hardware: bool,
        #[arg(long)]
        gpu_min_memory_mb: Option<u64>,
        #[arg(long)]
        gpu_min_compute: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 按清单校验已安装组件
    Check {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// 安装（或重装）一个组件
    Install {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        id: String,
        #[arg(long)]
        root: Option<PathBuf>,
        /// 从本地目录安装
        #[arg(long)]
        from: Option<PathBuf>,
        /// 从清单里声明的 URL 在线安装（需清单给出产物 SHA-256）
        #[arg(long)]
        url: bool,
        /// 下载工具：`curl` / `wget` 或可执行文件路径
        #[arg(long)]
        download_tool: Option<String>,
        /// 解压工具：`tar` / `7z` / `7zz` 或可执行文件路径
        #[arg(long)]
        archive_tool: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 回滚到上一个已安装版本
    Rollback {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        id: String,
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// 扫描本地目录生成带真实摘要的清单项
    ManifestGen {
        #[arg(long)]
        dir: PathBuf,
        #[arg(long)]
        id: String,
        #[arg(long)]
        role: String,
        #[arg(long)]
        version: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        optional: bool,
        /// 依赖的组件 id，用逗号分隔
        #[arg(long)]
        requires: Option<String>,
        #[arg(long)]
        url: Option<String>,
        /// 下载产物（归档本身）的 SHA-256；给了 --url 就必须给
        #[arg(long)]
        artifact_sha256: Option<String>,
        /// 归档格式：zip / tar.gz / tar / 7z
        #[arg(long)]
        archive: Option<String>,
        /// 归档内需要上提的子目录
        #[arg(long)]
        layout: Option<String>,
        #[arg(long)]
        license: Option<String>,
        /// 模型后端标识（模型类角色必填）
        #[arg(long)]
        backend: Option<String>,
        /// 运行框架：pytorch / onnx / native
        #[arg(long)]
        framework: Option<String>,
        #[arg(long)]
        entrypoint: Option<String>,
        /// 写到文件；缺省打印到 stdout
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// 检查更新（会对通道清单验签）
    Update {
        #[arg(long)]
        channel: PathBuf,
        #[arg(long)]
        public_key: PathBuf,
        #[arg(long)]
        current: String,
        #[arg(long)]
        platform: Option<String>,
        #[arg(long)]
        arch: Option<String>,
        #[arg(long)]
        openssl: Option<String>,
        /// 备用验签器（OpenSSL 1.1.1 的 pkeyutl 不支持 Ed25519，会回退到 node）
        #[arg(long)]
        node: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 打印设备指纹（本地用途；不外发任何字段）
    Device {
        #[arg(long)]
        salt: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 本地凭据存储
    Secret {
        #[command(subcommand)]
        action: SecretAction,
    },
    /// 一次性跑完所有检查，给出总览与退出码
    Selfcheck {
        #[arg(long)]
        manifest: Option<PathBuf>,
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum SecretAction {
    /// 列出凭据名与元信息（不含值）
    List {
        #[arg(long)]
        root: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// 写入凭据（不带 --value 时从 stdin 读取）
    Put {
        name: String,
        #[arg(long)]
        value: Option<String>,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 读取凭据值（打印到 stdout）
    Get {
        name: String,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 删除凭据
    Delete {
        name: String,
        #[arg(long)]
        root: Option<PathBuf>,
    },
}

fn parse_preset(value: &str) -> Result<ParallelismPreset, String> {
    ParallelismPreset::parse(value).ok_or_else(|| {
        "并行度档位只能是 automatic / conservative / balanced / maximum / single".to_string()
    })
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(args) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("错误：{message}");
            ExitCode::from(2)
        }
    }
}

fn run(args: Args) -> Result<ExitCode, String> {
    let runner = SystemRunner::new();
    match args.command {
        Command::Probe {
            ffmpeg,
            ffprobe,
            python,
            parallelism,
            jobs,
            no_prefer_hardware,
            gpu_min_memory_mb,
            gpu_min_compute,
            json,
        } => {
            let config = ProbeConfig {
                ffmpeg,
                ffprobe,
                python: python.unwrap_or_else(default_python),
                parallelism: parallelism.unwrap_or(ParallelismPreset::Automatic),
                parallelism_configured: jobs,
                prefer_hardware_encoder: !no_prefer_hardware,
                gpu_min_memory_mb: gpu_min_memory_mb.unwrap_or(4096),
                gpu_min_compute_capability: gpu_min_compute.unwrap_or_else(|| "6.1".to_string()),
                ..ProbeConfig::default()
            };
            let report = probe_environment(&runner, &config);
            if json {
                print_json(&report)?;
            } else {
                print_probe(&report);
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Check {
            manifest,
            root,
            json,
        } => {
            let manifest = load_manifest(&manifest)?;
            let store = RuntimeStore::new(store_config(root)?);
            let status = store.status_all(&manifest);
            if json {
                print_json(&status)?;
            } else {
                println!("组件状态（根目录 {}）：", store.root().display());
                for item in &status {
                    println!(
                        "  [{}] {} ({}) {}",
                        if item.satisfied() { "✓" } else { "✗" },
                        item.id,
                        item.role,
                        item.detail
                    );
                }
            }
            let unsatisfied = status.iter().filter(|item| !item.satisfied()).count();
            if unsatisfied > 0 {
                eprintln!("有 {unsatisfied} 个必装组件未就绪。");
                Ok(ExitCode::from(1))
            } else {
                Ok(ExitCode::SUCCESS)
            }
        }
        Command::Install {
            manifest,
            id,
            root,
            from,
            url,
            download_tool,
            archive_tool,
            json,
        } => {
            let manifest = load_manifest(&manifest)?;
            let component = manifest
                .find(&id)
                .ok_or_else(|| format!("清单里没有组件 `{id}`"))?
                .clone();
            let mut config = store_config(root)?;
            if let Some(tool) = download_tool {
                config.downloader = parse_downloader(&tool);
            }
            if let Some(tool) = archive_tool {
                config.archiver = parse_archiver(&tool);
            }
            let store = RuntimeStore::new(config);
            let report = match (from, url) {
                (Some(directory), false) => store
                    .install_local(&component, &directory)
                    .map_err(|error| error.to_string())?,
                (None, true) => store
                    .install_from_url(&component, &runner)
                    .map_err(|error| error.to_string())?,
                (Some(_), true) => return Err("--from 与 --url 只能给一个".into()),
                (None, false) => {
                    return Err(
                        "请用 --from <目录> 或 --url（按清单声明的地址下载）指定来源".into(),
                    )
                }
            };
            if json {
                print_json(&report)?;
            } else {
                println!(
                    "已安装 {} {} → {}（{} 个文件，{} 字节，校验{}）",
                    report.id,
                    report.version,
                    report.dir.display(),
                    report.file_count,
                    report.total_bytes,
                    if report.verified {
                        "通过"
                    } else {
                        "未通过"
                    }
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Rollback {
            manifest,
            id,
            root,
            json,
        } => {
            let manifest = load_manifest(&manifest)?;
            let component = manifest
                .find(&id)
                .ok_or_else(|| format!("清单里没有组件 `{id}`"))?
                .clone();
            let store = RuntimeStore::new(store_config(root)?);
            let report = store
                .rollback(&component)
                .map_err(|error| error.to_string())?;
            if json {
                print_json(&report)?;
            } else {
                println!(
                    "已回滚 {}：{} → {}（{}）",
                    report.id,
                    report.from,
                    report.to,
                    report.dir.display()
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::ManifestGen {
            dir,
            id,
            role,
            version,
            title,
            optional,
            requires,
            url,
            artifact_sha256,
            archive,
            layout,
            license,
            backend,
            framework,
            entrypoint,
            out,
            json,
        } => {
            if !RuntimeRoles::is_known(&role) {
                return Err(format!(
                    "未知角色 `{role}`；可用角色：{}",
                    RuntimeRoles::ALL.join(" / ")
                ));
            }
            let options = GenerateOptions {
                id,
                role,
                version,
                title,
                optional,
                requires: requires
                    .map(|value| {
                        value
                            .split(',')
                            .map(str::trim)
                            .filter(|item| !item.is_empty())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                url,
                artifact_sha256,
                archive,
                layout,
                license,
                backend,
                framework,
                entrypoint,
            };
            let component =
                generate_component(&dir, &options).map_err(|error| error.to_string())?;
            let manifest = RuntimeManifest {
                schema_version: dossier_runtime::MANIFEST_SCHEMA_VERSION,
                generated_at: Some(chrono::Utc::now().to_rfc3339()),
                components: vec![component],
            };
            manifest.validate().map_err(|error| error.to_string())?;
            match out {
                Some(path) => {
                    manifest.save(&path).map_err(|error| error.to_string())?;
                    println!("已写入 {}", path.display());
                }
                None => {
                    let text = String::from_utf8(manifest.to_json().map_err(|e| e.to_string())?)
                        .map_err(|error| error.to_string())?;
                    if json {
                        print!("{text}");
                    } else {
                        println!("{text}");
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Update {
            channel,
            public_key,
            current,
            platform,
            arch,
            openssl,
            node,
            json,
        } => {
            let channel = UpdateChannel::load(&channel).map_err(|error| error.to_string())?;
            let platform = platform.unwrap_or_else(default_platform);
            let arch = arch.unwrap_or_else(default_arch);
            let openssl = openssl.unwrap_or_else(|| "openssl".to_string());
            let node = node.unwrap_or_else(|| "node".to_string());
            let verifiers = default_verifiers(&openssl, &node);
            let plan = plan_update_with_verifiers(
                &runner,
                &channel,
                &current,
                &platform,
                &arch,
                &public_key,
                &verifiers,
            )
            .map_err(|error| error.to_string())?;
            if json {
                print_json(&plan)?;
            } else {
                println!("{}", plan.describe());
                if let dossier_runtime::update::UpdatePlan::Available {
                    release,
                    payload_digest,
                    ..
                } = &plan
                {
                    println!("  被签名内容摘要：{payload_digest}");
                    if let Some(notes) = &release.notes {
                        println!("  说明：{notes}");
                    }
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Device { salt, json } => {
            let fingerprint = compute_device_fingerprint(&runner, salt.as_deref());
            if json {
                print_json(&fingerprint)?;
            } else {
                println!("设备标识：{}", fingerprint.short_id());
                println!("{}", fingerprint.describe());
                for component in &fingerprint.components {
                    println!(
                        "  {:<12} {:<8} {}",
                        component.kind,
                        if component.available {
                            component.digest.clone()
                        } else {
                            "不可用".into()
                        },
                        component.source
                    );
                }
                if let Some(limitation) = &fingerprint.limitation {
                    println!("局限：{limitation}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Secret { action } => {
            let store = secret_store(None)?;
            match action {
                SecretAction::List { root, json } => {
                    let store = root.map_or(store, SecretStore::new);
                    let list = store.list().map_err(|error| error.to_string())?;
                    if json {
                        print_json(&list)?;
                    } else if list.is_empty() {
                        println!("（没有已保存的凭据）");
                    } else {
                        for meta in &list {
                            println!(
                                "  {}（{} 字节，更新于 {}）",
                                meta.name,
                                meta.bytes,
                                meta.updated_at.as_deref().unwrap_or("未知")
                            );
                        }
                    }
                    for warning in store
                        .permission_warnings()
                        .map_err(|error| error.to_string())?
                    {
                        eprintln!("警告：{warning}");
                    }
                }
                SecretAction::Put { name, value, root } => {
                    let store = root.map_or(store, SecretStore::new);
                    let value = match value {
                        Some(value) => value,
                        None => read_stdin()?,
                    };
                    let meta = store
                        .put(&name, &value)
                        .map_err(|error| error.to_string())?;
                    println!("已保存 {}（{}）", meta.name, mask(&value));
                }
                SecretAction::Get { name, root } => {
                    let store = root.map_or(store, SecretStore::new);
                    let value = store.get(&name).map_err(|error| error.to_string())?;
                    print!("{value}");
                }
                SecretAction::Delete { name, root } => {
                    let store = root.map_or(store, SecretStore::new);
                    let removed = store.delete(&name).map_err(|error| error.to_string())?;
                    println!(
                        "{}",
                        if removed {
                            format!("已删除 {name}")
                        } else {
                            format!("{name} 本来就不存在")
                        }
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Selfcheck {
            manifest,
            root,
            json,
        } => {
            let config = ProbeConfig {
                python: default_python(),
                ..ProbeConfig::default()
            };
            let report = probe_environment(&runner, &config);
            let fingerprint = compute_device_fingerprint(&runner, Some("dossier-selfcheck"));
            let secrets = secret_store(None)?;
            let warnings = secrets
                .permission_warnings()
                .map_err(|error| error.to_string())?;
            let component_status = match &manifest {
                Some(path) => {
                    let manifest = load_manifest(path)?;
                    let store = RuntimeStore::new(store_config(root)?);
                    Some(store.status_all(&manifest))
                }
                None => None,
            };

            let mut failures = Vec::new();
            if !report.usable() {
                failures.push("ffmpeg 不可用（转码/擦除链路无法运行）".to_string());
            }
            if report.video.encoders.is_empty() {
                failures.push("没有任何可用的视频编码器".to_string());
            }
            if !report.python.found {
                failures.push("没有找到 Python 解释器（模型 worker 无法运行）".to_string());
            }
            if let Some(status) = &component_status {
                for item in status.iter().filter(|item| !item.satisfied()) {
                    failures.push(format!("组件 {}：{}", item.id, item.detail));
                }
            }

            if json {
                let payload = serde_json::json!({
                    "capabilities": report,
                    "device": fingerprint,
                    "secretWarnings": warnings,
                    "componentStatus": component_status,
                    "failures": failures,
                    "ok": failures.is_empty(),
                });
                print_json(&payload)?;
            } else {
                print_probe(&report);
                println!();
                println!(
                    "设备：{}（{}）",
                    fingerprint.short_id(),
                    fingerprint.describe()
                );
                if warnings.is_empty() {
                    println!("凭据目录：权限正常");
                } else {
                    for warning in &warnings {
                        println!("凭据目录：{warning}");
                    }
                }
                if let Some(status) = &component_status {
                    println!();
                    println!("组件清单：");
                    for item in status {
                        println!(
                            "  [{}] {} — {}",
                            if item.satisfied() { "✓" } else { "✗" },
                            item.id,
                            item.detail
                        );
                    }
                }
                println!();
                if failures.is_empty() {
                    println!("自检通过。");
                } else {
                    println!("自检发现 {} 个问题：", failures.len());
                    for failure in &failures {
                        println!("  - {failure}");
                    }
                }
            }
            if failures.is_empty() {
                Ok(ExitCode::SUCCESS)
            } else {
                Ok(ExitCode::from(1))
            }
        }
    }
}

fn print_probe(report: &dossier_runtime::CapabilityReport) {
    println!("外部工具：");
    for tool in &report.tools {
        println!(
            "  {:<12} {:<10} {}",
            tool.name,
            if tool.found {
                tool.display_version()
            } else {
                "缺失"
            },
            tool.detail
        );
    }
    println!();
    println!(
        "FFmpeg：{}",
        if report.ffmpeg.usable() {
            format!(
                "{}（视频编码器 {} 个，滤镜 {} 个）",
                report.ffmpeg.ffmpeg.display_version(),
                report.ffmpeg.encoders.video.len(),
                report.ffmpeg.filters.total()
            )
        } else {
            "不可用".to_string()
        }
    );
    for codec in ["h264", "hevc"] {
        match report.video.encoders.get(codec) {
            Some(plan) => println!(
                "  {codec}：{}（{}）",
                plan.selected,
                if plan.hardware { "硬件" } else { "软件" }
            ),
            None => println!(
                "  {codec}：无可用编码器 —— {}",
                report
                    .video
                    .encoder_errors
                    .get(codec)
                    .cloned()
                    .unwrap_or_default()
            ),
        }
    }
    if let Some(error) = &report.ffmpeg.list_error {
        println!("  提示：{error}");
    }
    println!();
    println!("GPU：{}", report.video.gpu.reason);
    if report.video.gpu.available {
        for device in &report.video.gpu.devices {
            println!(
                "  #{} {}（{} MB，驱动 {}，计算能力 {}）",
                device.index,
                device.name,
                device.memory_total_mb,
                device.driver_version,
                device.compute_capability.as_deref().unwrap_or("未知")
            );
        }
    }
    println!(
        "  推理可用：{}",
        if report.video.gpu.eligible_for_inpaint {
            "是".to_string()
        } else {
            format!(
                "否（{}）",
                report
                    .video
                    .gpu
                    .ineligible_reason
                    .clone()
                    .unwrap_or_else(|| "原因未知".to_string())
            )
        }
    );
    println!();
    println!(
        "Python：{}（torch {}，onnxruntime {}）",
        report
            .python
            .version
            .clone()
            .unwrap_or_else(|| "未找到".to_string()),
        report.python.torch_version.as_deref().unwrap_or("未安装"),
        report
            .python
            .onnxruntime_version
            .as_deref()
            .unwrap_or("未安装")
    );
    let parallel = &report.video.parallelism;
    println!();
    println!(
        "并行度：生效 {}（自动 {}，指定 {}，上限 {}）",
        parallel.effective,
        parallel.automatic,
        parallel
            .configured
            .map(|value| value.to_string())
            .unwrap_or_else(|| "未指定".to_string()),
        parallel.maximum
    );
    println!("  依据：{}", parallel.basis);
    println!();
    println!("能力标签：{}", report.capability_tags().join(", "));
}

fn load_manifest(path: &Path) -> Result<RuntimeManifest, String> {
    let manifest = RuntimeManifest::load(path).map_err(|error| error.to_string())?;
    manifest.validate().map_err(|error| error.to_string())?;
    Ok(manifest)
}

fn store_config(root: Option<PathBuf>) -> Result<StoreConfig, String> {
    let root = match root {
        Some(root) => root,
        None => default_runtime_root()?,
    };
    Ok(StoreConfig::new(root))
}

fn default_runtime_root() -> Result<PathBuf, String> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(xdg).join("dossier").join("runtime"));
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or("无法确定运行时目录：既没有 XDG_DATA_HOME 也没有 HOME；请用 --root 指定")?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("dossier")
        .join("runtime"))
}

fn secret_store(root: Option<PathBuf>) -> Result<SecretStore, String> {
    match root {
        Some(root) => Ok(SecretStore::new(root)),
        None => SecretStore::with_default_root().map_err(|error| error.to_string()),
    }
}

/// 默认 Python：优先 `python3.9`（本仓库实测可用的版本），退回 `python3`。
fn default_python() -> String {
    for candidate in ["python3.9", "python3"] {
        if let Ok(output) = std::process::Command::new(candidate)
            .arg("--version")
            .output()
        {
            if output.status.success() {
                return candidate.to_string();
            }
        }
    }
    "python3".to_string()
}

fn default_platform() -> String {
    std::env::consts::OS.to_string()
}

fn default_arch() -> String {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64".to_string(),
        "aarch64" => "aarch64".to_string(),
        other => other.to_string(),
    }
}

fn parse_downloader(value: &str) -> Downloader {
    let lower = value.to_ascii_lowercase();
    if lower.contains("wget") {
        Downloader::Wget(value.to_string())
    } else {
        Downloader::Curl(value.to_string())
    }
}

fn parse_archiver(value: &str) -> Archiver {
    let lower = value.to_ascii_lowercase();
    if lower.contains("7z") {
        Archiver::SevenZip(value.to_string())
    } else {
        Archiver::Tar(value.to_string())
    }
}

fn read_stdin() -> Result<String, String> {
    let mut buffer = String::new();
    std::io::stdin()
        .read_to_string(&mut buffer)
        .map_err(|error| format!("读取 stdin 失败：{error}"))?;
    Ok(buffer.trim_end_matches(['\n', '\r']).to_string())
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    println!("{text}");
    Ok(())
}
