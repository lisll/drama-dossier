//! 能力协商：把「这台机器有什么」翻译成「这台机器该怎么做」。
//!
//! [`crate::probe`] 只报事实（有没有 nvidia-smi、ffmpeg 支持哪些编码器、几个核），
//! 本模块负责决策，并且**每条决策都带理由**：
//!
//! - GPU 是否**够格**跑模型（显存下限 + 计算能力下限），不够格时明确写出卡在哪一条；
//! - 编码器分两级判断：先看「编译进来了吗 + 硬件设备在吗」，再用**一帧真编码**实测。
//!   本仓库的构建机上 `h264_vaapi` 编译进来了、`/dev/dri/renderD128` 也在，
//!   但 `libva` 初始化失败——只看前两级就会选中一个跑不起来的编码器，
//!   所以默认开启实测，失败就带原因回退到下一档；
//! - 并行度按四档呈现：`automatic`（自动推断）/ `configured`（人工指定）/
//!   `effective`（实际生效）/ `maximum`（上限）。
//!
//! 除「实测」这一处外，决策都是纯函数（输入探测结果，输出结论），
//! 因此无 GPU 机器也能把 GPU 分支测穿。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::command::{CommandRunner, CommandSpec};
use crate::probe::{compare_compute_capability, EncoderList, GpuDevice, GpuReport, ToolReport};

/// 单帧实测编码的超时（合成画面只有 128×128，正常在秒级完成）。
const VERIFY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CapsError {
    #[error("不认识的编码目标 `{codec}`；可用：h264 / hevc / vp9 / av1 / prores / mjpeg")]
    UnknownCodec { codec: String },
    #[error("没有任何可用的 {codec} 编码器；候选链：{}", .candidates.join(" → "))]
    NoEncoder {
        codec: String,
        candidates: Vec<String>,
    },
}

/// 硬件加速的**设备侧**信号：设备节点在不在。
///
/// 这一层只是「值得一试」的门槛，不是结论——能不能用由 [`verify_hardware_encoder`] 说了算。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HardwareSignal {
    /// NVIDIA 设备节点（NVENC 需要）。
    pub cuda: bool,
    /// `/dev/dri/renderD*` 或 `/dev/dri/card*`（VAAPI 需要）。
    pub vaapi: bool,
    /// `/dev/video*`（V4L2 M2M 需要）。
    pub v4l2m2m: bool,
}

impl HardwareSignal {
    /// 什么都不具备（测试与「强制软件」场景用）。
    pub fn none() -> Self {
        Self {
            cuda: false,
            vaapi: false,
            v4l2m2m: false,
        }
    }

    /// 全部具备（测试「硬件优先路径」用）。
    pub fn all() -> Self {
        Self {
            cuda: true,
            vaapi: true,
            v4l2m2m: true,
        }
    }

    /// 读设备节点得到信号。
    pub fn detect() -> Self {
        Self {
            cuda: Path::new("/dev/nvidia0").exists() || Path::new("/dev/nvidiactl").exists(),
            vaapi: first_vaapi_device().is_some(),
            v4l2m2m: first_v4l2_device().is_some(),
        }
    }

    /// 用 GPU 探测结果补强 CUDA 信号（有些容器里没有 `/dev/nvidia*` 但 nvidia-smi 可用）。
    #[must_use]
    pub fn with_cuda(mut self, cuda: bool) -> Self {
        self.cuda = self.cuda || cuda;
        self
    }

    /// 某个编码器的硬件前置条件是否满足；软件编码器恒为真。
    pub fn allows(&self, encoder: &str) -> bool {
        if encoder.ends_with("_nvenc") {
            self.cuda
        } else if encoder.ends_with("_videotoolbox") {
            // VideoToolbox 是 macOS 系统框架，不依赖设备节点，随编译进 ffmpeg 即可用。
            cfg!(target_os = "macos")
        } else if encoder.ends_with("_vaapi") {
            self.vaapi
        } else if encoder.ends_with("_v4l2m2m") {
            self.v4l2m2m
        } else {
            true
        }
    }

    /// 可读描述（写进决策理由里）。
    pub fn describe(&self) -> String {
        let base = format!(
            "cuda={} vaapi={} v4l2m2m={}",
            if self.cuda { "有" } else { "无" },
            if self.vaapi { "有" } else { "无" },
            if self.v4l2m2m { "有" } else { "无" }
        );
        if cfg!(target_os = "macos") {
            format!("{base} videotoolbox=系统框架")
        } else {
            base
        }
    }
}

/// 第一个可用的 VAAPI 渲染节点。
pub fn first_vaapi_device() -> Option<PathBuf> {
    for index in 128..136 {
        let candidate = PathBuf::from(format!("/dev/dri/renderD{index}"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    let card = PathBuf::from("/dev/dri/card0");
    if card.exists() {
        return Some(card);
    }
    None
}

/// 第一个可用的 V4L2 视频节点。
pub fn first_v4l2_device() -> Option<PathBuf> {
    for index in 0..16 {
        let candidate = PathBuf::from(format!("/dev/video{index}"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// 一个编码器候选。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncoderCandidate {
    pub name: String,
    pub hardware: bool,
    /// ffmpeg 是否编译进了这个编码器。
    pub compiled: bool,
    /// 当前是否可用（编译 + 硬件信号 + 实测）。
    pub available: bool,
    /// 硬件候选的实测记录；软件候选为 `None`。
    #[serde(default)]
    pub verification: Option<String>,
}

/// 针对某个编码目标的编码器选择结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EncoderPlan {
    pub codec: String,
    pub prefer_hardware: bool,
    /// 有序候选链（含不可用项，方便解释「为什么没用 NVENC」）。
    pub candidates: Vec<EncoderCandidate>,
    pub selected: String,
    pub hardware: bool,
    pub reason: String,
}

impl EncoderPlan {
    pub fn candidate(&self, name: &str) -> Option<&EncoderCandidate> {
        self.candidates.iter().find(|item| item.name == name)
    }
}

/// 编码目标 → 候选编码器（硬件在前）。
///
/// 顺序即优先级：NVENC（NVIDIA）→ VideoToolbox（macOS 系统框架）→ VAAPI（Linux 通用硬件）
/// → V4L2 M2M（嵌入式/ARM）→ 软件编码器。软件编码器永远在链尾，
/// 保证「硬件不可用就自动兜底」——任何机器都能跑。
pub fn encoder_chain(codec: &str) -> Option<Vec<(&'static str, bool)>> {
    let chain = match codec {
        "h264" | "avc" => vec![
            ("h264_nvenc", true),
            ("h264_videotoolbox", true),
            ("h264_vaapi", true),
            ("h264_v4l2m2m", true),
            ("libx264", false),
            ("libx264rgb", false),
        ],
        "hevc" | "h265" => vec![
            ("hevc_nvenc", true),
            ("hevc_videotoolbox", true),
            ("hevc_vaapi", true),
            ("hevc_v4l2m2m", true),
            ("libx265", false),
        ],
        "vp9" => vec![("vp9_vaapi", true), ("libvpx-vp9", false)],
        "av1" => vec![
            ("av1_nvenc", true),
            ("av1_vaapi", true),
            ("libaom-av1", false),
            ("libsvtav1", false),
        ],
        "prores" => vec![("prores_ks", false), ("prores", false)],
        "mjpeg" => vec![("mjpeg_vaapi", true), ("mjpeg", false), ("ljpeg", false)],
        _ => return None,
    };
    Some(chain)
}

/// 按候选链挑编码器。
///
/// `prefer_hardware = false` 时把软件编码器提到链首（用户显式要求可复现的软件编码），
/// 但硬件项仍保留在后面——软件编码器在这台机器上不存在时不会直接失败。
pub fn plan_encoder(
    encoders: &EncoderList,
    codec: &str,
    prefer_hardware: bool,
    signal: &HardwareSignal,
) -> Result<EncoderPlan, CapsError> {
    let chain = encoder_chain(codec).ok_or_else(|| CapsError::UnknownCodec {
        codec: codec.to_string(),
    })?;

    let mut ordered: Vec<(&'static str, bool)> = chain;
    if !prefer_hardware {
        ordered.sort_by_key(|(_, hardware)| *hardware);
    }
    let candidates: Vec<EncoderCandidate> = ordered
        .iter()
        .map(|(name, hardware)| {
            let compiled = encoders.supports(name);
            EncoderCandidate {
                name: (*name).to_string(),
                hardware: *hardware,
                compiled,
                available: compiled && (!*hardware || signal.allows(name)),
                verification: None,
            }
        })
        .collect();

    let selected_index = match candidates.iter().position(|candidate| candidate.available) {
        Some(index) => index,
        None => {
            return Err(CapsError::NoEncoder {
                codec: codec.to_string(),
                candidates: candidates
                    .iter()
                    .map(|candidate| candidate.name.clone())
                    .collect(),
            })
        }
    };
    let selected = candidates[selected_index].clone();
    let reason = build_reason(&candidates, &selected, prefer_hardware, signal);

    Ok(EncoderPlan {
        codec: codec.to_string(),
        prefer_hardware,
        candidates,
        selected: selected.name,
        hardware: selected.hardware,
        reason,
    })
}

fn build_reason(
    candidates: &[EncoderCandidate],
    selected: &EncoderCandidate,
    _prefer_hardware: bool,
    signal: &HardwareSignal,
) -> String {
    let hardware_usable = candidates
        .iter()
        .any(|candidate| candidate.hardware && candidate.available);
    let gated: Vec<String> = candidates
        .iter()
        .filter(|candidate| candidate.hardware && candidate.compiled && !candidate.available)
        .map(|candidate| candidate.name.clone())
        .collect();
    let gated_note = if gated.is_empty() {
        String::new()
    } else {
        format!(
            "；{} 编译进来了但缺少硬件设备（{}）",
            gated.join(" / "),
            signal.describe()
        )
    };

    if selected.hardware {
        format!(
            "选择硬件编码器 {}（设备信号：{}）",
            selected.name,
            signal.describe()
        )
    } else if hardware_usable {
        format!(
            "按用户要求优先软件编码 {}；候选链里也有可用硬件编码器（{}）",
            selected.name,
            describe_availability(candidates, true)
        )
    } else {
        format!(
            "没有可用的硬件编码器，回退软件编码 {}{gated_note}",
            selected.name
        )
    }
}

fn describe_availability(candidates: &[EncoderCandidate], hardware: bool) -> String {
    let names: Vec<String> = candidates
        .iter()
        .filter(|candidate| candidate.hardware == hardware)
        .map(|candidate| {
            if candidate.available {
                format!("{}✓", candidate.name)
            } else if candidate.compiled {
                format!("{}△", candidate.name)
            } else {
                format!("{}✗", candidate.name)
            }
        })
        .collect();
    if names.is_empty() {
        "无".to_string()
    } else {
        names.join(" / ")
    }
}

/// 硬件编码器实测的配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderVerificationConfig {
    /// `ffmpeg` 可执行文件（用探测到的那一个，避免 PATH 上有多个版本）。
    pub ffmpeg: String,
    /// VAAPI 要显式指定设备节点。
    pub vaapi_device: Option<PathBuf>,
    pub timeout: Duration,
}

impl EncoderVerificationConfig {
    pub fn new(ffmpeg: impl Into<String>) -> Self {
        Self {
            ffmpeg: ffmpeg.into(),
            vaapi_device: first_vaapi_device(),
            timeout: VERIFY_TIMEOUT,
        }
    }
}

/// 用一帧合成画面实测一个编码器是否真的能跑。
///
/// 只对硬件编码器调用：软件编码器（libx264 等）在 ffmpeg 里稳定得多，
/// 每次都实测会让探测变慢，收益不成比例。
pub fn verify_hardware_encoder(
    runner: &dyn CommandRunner,
    config: &EncoderVerificationConfig,
    encoder: &str,
) -> Result<(), String> {
    let mut args = vec![
        "-hide_banner".to_string(),
        "-loglevel".to_string(),
        "error".to_string(),
    ];
    let upload: Vec<String> = if encoder.ends_with("_vaapi") {
        let device = config
            .vaapi_device
            .clone()
            .ok_or_else(|| "没有可用的 VAAPI 设备节点".to_string())?;
        args.push("-vaapi_device".to_string());
        args.push(device.display().to_string());
        vec!["-vf".to_string(), "format=nv12,hwupload".to_string()]
    } else {
        Vec::new()
    };
    args.extend([
        "-f".to_string(),
        "lavfi".to_string(),
        "-i".to_string(),
        "testsrc2=size=128x128:rate=1".to_string(),
        "-frames:v".to_string(),
        "1".to_string(),
    ]);
    args.extend(upload);
    args.extend([
        "-c:v".to_string(),
        encoder.to_string(),
        "-f".to_string(),
        "null".to_string(),
        "-".to_string(),
    ]);

    let output = runner
        .run(
            &CommandSpec::new(config.ffmpeg.clone())
                .args(args)
                .timeout(config.timeout),
        )
        .map_err(|error| error.to_string())?;
    if output.success() {
        Ok(())
    } else {
        Err(first_line(&output.combined()))
    }
}

/// 对已选中的硬件编码器做实测；不通过就带原因回退到下一档。
///
/// 返回的 plan 一定反映「当前真正会被用到的编码器」。
pub fn verify_encoder_plan(
    runner: &dyn CommandRunner,
    config: &EncoderVerificationConfig,
    plan: &EncoderPlan,
) -> Result<EncoderPlan, CapsError> {
    let mut plan = plan.clone();
    let Some(index) = plan
        .candidates
        .iter()
        .position(|candidate| candidate.name == plan.selected)
    else {
        return Err(CapsError::NoEncoder {
            codec: plan.codec.clone(),
            candidates: plan
                .candidates
                .iter()
                .map(|candidate| candidate.name.clone())
                .collect(),
        });
    };
    // 选中的是软件编码器：不需要实测。
    if !plan.candidates[index].hardware {
        return Ok(plan);
    }

    let encoder = plan.candidates[index].name.clone();
    match verify_hardware_encoder(runner, config, &encoder) {
        Ok(()) => {
            plan.candidates[index].verification = Some("单帧实测通过".to_string());
            plan.reason = format!("{encoder} 单帧实测通过；{}", plan.reason);
            Ok(plan)
        }
        Err(detail) => {
            plan.candidates[index].available = false;
            plan.candidates[index].verification = Some(format!("单帧实测失败：{detail}"));
            match plan
                .candidates
                .iter()
                .position(|candidate| candidate.available)
            {
                Some(next) => {
                    let fallback = plan.candidates[next].clone();
                    plan.selected = fallback.name.clone();
                    plan.hardware = fallback.hardware;
                    plan.reason = format!(
                        "硬件编码器 {encoder} 单帧实测失败（{detail}），回退到 {}",
                        fallback.name
                    );
                    if fallback.hardware {
                        // 回退目标仍是硬件（例如 VAAPI → V4L2 M2M）：继续实测。
                        return verify_encoder_plan(runner, config, &plan);
                    }
                    Ok(plan)
                }
                None => Err(CapsError::NoEncoder {
                    codec: plan.codec.clone(),
                    candidates: plan
                        .candidates
                        .iter()
                        .map(|candidate| candidate.name.clone())
                        .collect(),
                }),
            }
        }
    }
}

/// GPU 是否可用于模型推理。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuCapability {
    pub available: bool,
    pub backend: String,
    pub devices: Vec<GpuDevice>,
    pub reason: String,
    /// 是否满足推理门槛（显存 + 计算能力）。
    pub eligible_for_inpaint: bool,
    /// 不满足门槛时的具体原因。
    #[serde(default)]
    pub ineligible_reason: Option<String>,
    pub min_memory_mb: u64,
    pub min_compute_capability: String,
}

impl GpuCapability {
    /// 推荐使用的设备（显存最大的那块）。
    pub fn recommended_device(&self) -> Option<&GpuDevice> {
        self.devices
            .iter()
            .max_by_key(|device| device.memory_total_mb)
    }
}

/// 判断 GPU 是否够格跑时序修复类模型。
///
/// 门槛是显式的：显存低于下限（时序模型的多帧缓存）或计算能力过低（老卡缺算子）
/// 时判不合格，并说明是哪一条没过，而不是含糊地「不支持」。
pub fn evaluate_gpu(
    report: &GpuReport,
    min_memory_mb: u64,
    min_compute_capability: &str,
) -> GpuCapability {
    let base = |eligible_for_inpaint: bool, ineligible_reason: Option<String>| GpuCapability {
        available: report.available,
        backend: report.backend.clone(),
        devices: report.devices.clone(),
        reason: report.reason.clone(),
        eligible_for_inpaint,
        ineligible_reason,
        min_memory_mb,
        min_compute_capability: min_compute_capability.to_string(),
    };

    if !report.available {
        return base(false, Some(format!("没有可用 GPU：{}", report.reason)));
    }

    let memory_ok = report
        .devices
        .iter()
        .any(|device| device.memory_total_mb >= min_memory_mb);
    if !memory_ok {
        let best = report
            .devices
            .iter()
            .map(|device| device.memory_total_mb)
            .max()
            .unwrap_or(0);
        return base(
            false,
            Some(format!("显存不足：最大 {best} MB，门槛 {min_memory_mb} MB")),
        );
    }

    // 计算能力查不到（老驱动）时不拦——只按显存判定。
    let best_capability = report.max_compute_capability();
    match best_capability {
        Some(capability)
            if compare_compute_capability(capability, min_compute_capability)
                == std::cmp::Ordering::Less =>
        {
            base(
                false,
                Some(format!(
                    "计算能力不足：最高 {capability}，门槛 {min_compute_capability}"
                )),
            )
        }
        _ => base(true, None),
    }
}

/// 并行度档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParallelismPreset {
    /// 按硬件自动取舍（有可用 GPU 时偏保守，避免显存争抢）。
    Automatic,
    Conservative,
    Balanced,
    Maximum,
    Single,
}

impl ParallelismPreset {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::Conservative => "conservative",
            Self::Balanced => "balanced",
            Self::Maximum => "maximum",
            Self::Single => "single",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "automatic" | "auto" => Some(Self::Automatic),
            "conservative" | "low" => Some(Self::Conservative),
            "balanced" | "medium" => Some(Self::Balanced),
            "maximum" | "max" | "all" => Some(Self::Maximum),
            "single" | "one" => Some(Self::Single),
            _ => None,
        }
    }

    pub const ALL: &'static [ParallelismPreset] = &[
        ParallelismPreset::Automatic,
        ParallelismPreset::Conservative,
        ParallelismPreset::Balanced,
        ParallelismPreset::Maximum,
        ParallelismPreset::Single,
    ];
}

/// 四档并行度。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParallelismCapability {
    pub preset: String,
    /// 自动推断值（只看硬件与核数）。
    pub automatic: usize,
    /// 人工指定值（`None` 表示没指定）。
    #[serde(default)]
    pub configured: Option<usize>,
    /// 实际生效值。
    pub effective: usize,
    /// 上限（CPU 逻辑核数）。
    pub maximum: usize,
    /// 为什么是这个值。
    pub basis: String,
}

/// 推断并行度。
///
/// 决策依据：ffmpeg 的解码/滤镜/编码是 CPU 并行受益的，所以以核数为基准；
/// 但一旦走 GPU 模型推理，多进程会争抢显存和 CUDA 上下文，因此 `Automatic`
/// 在有可用 GPU 时自动降到保守档。
pub fn plan_parallelism(
    preset: ParallelismPreset,
    configured: Option<usize>,
    cpu_count: usize,
    gpu: &GpuCapability,
) -> ParallelismCapability {
    let maximum = cpu_count.max(1);
    let conservative = (maximum / 4).clamp(1, 4);
    let balanced = (maximum / 2).clamp(1, 8);

    let (resolved, reason) = match preset {
        ParallelismPreset::Single => (1, "单路串行（显式指定）".to_string()),
        ParallelismPreset::Conservative => {
            (conservative, format!("保守档：{maximum} 核的 1/4，上限 4"))
        }
        ParallelismPreset::Balanced => (balanced, format!("均衡档：{maximum} 核的 1/2，上限 8")),
        ParallelismPreset::Maximum => (maximum, format!("最大档：吃满 {maximum} 个逻辑核")),
        ParallelismPreset::Automatic => {
            if gpu.eligible_for_inpaint {
                (
                    conservative,
                    format!(
                        "自动：检测到可用 GPU（{}），转保守档以避免显存与 CUDA 上下文争抢",
                        gpu.recommended_device()
                            .map(|device| device.name.as_str())
                            .unwrap_or("未知型号")
                    ),
                )
            } else {
                (
                    balanced,
                    format!("自动：无可用 GPU，按 {maximum} 核的一半并行"),
                )
            }
        }
    };

    let effective = configured.unwrap_or(resolved).clamp(1, maximum).max(1);
    let basis = match configured {
        Some(value) if value != resolved => {
            format!("{reason}；用户指定 {value}，按上限 {maximum} 收敛为 {effective}")
        }
        Some(_) => format!("{reason}；用户指定值与推断一致"),
        None => reason,
    };

    ParallelismCapability {
        preset: preset.as_str().to_string(),
        automatic: resolved,
        configured,
        effective,
        maximum,
        basis,
    }
}

/// 视频处理能力总览（对应界面上要展示的那一块）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoProcessingCapabilities {
    pub gpu: GpuCapability,
    /// 各编码目标的选型结果。
    #[serde(default)]
    pub encoders: BTreeMap<String, EncoderPlan>,
    /// 编码器候选链全部不可用的目标（应当提示用户而不是静默失败）。
    #[serde(default)]
    pub encoder_errors: BTreeMap<String, String>,
    pub parallelism: ParallelismCapability,
    pub cpu_count: usize,
    /// 本轮用到的硬件设备信号（便于解释「为什么没走硬件」）。
    pub hardware_signal: HardwareSignal,
}

/// 需要预置选型的编码目标。
pub const NEGOTIATED_CODECS: &[&str] = &["h264", "hevc"];

/// 组装视频处理能力（不做实测；实测见 [`verify_encoder_plan`]）。
pub fn negotiate_video(
    encoders: &EncoderList,
    gpu: &GpuCapability,
    preset: ParallelismPreset,
    configured: Option<usize>,
    cpu_count: usize,
    prefer_hardware: bool,
    signal: &HardwareSignal,
) -> VideoProcessingCapabilities {
    let mut plans = BTreeMap::new();
    let mut errors = BTreeMap::new();
    for codec in NEGOTIATED_CODECS {
        match plan_encoder(encoders, codec, prefer_hardware, signal) {
            Ok(plan) => {
                plans.insert((*codec).to_string(), plan);
            }
            Err(error) => {
                errors.insert((*codec).to_string(), error.to_string());
            }
        }
    }
    VideoProcessingCapabilities {
        gpu: gpu.clone(),
        encoders: plans,
        encoder_errors: errors,
        parallelism: plan_parallelism(preset, configured, cpu_count, gpu),
        cpu_count,
        hardware_signal: signal.clone(),
    }
}

/// 对已选出的硬件编码器逐个实测，并把失败原因与回退结果写回能力表。
pub fn verify_video_encoders(
    runner: &dyn CommandRunner,
    config: &EncoderVerificationConfig,
    video: &mut VideoProcessingCapabilities,
) {
    let snapshot = video.encoders.clone();
    for (codec, plan) in snapshot {
        match verify_encoder_plan(runner, config, &plan) {
            Ok(updated) => {
                video.encoders.insert(codec, updated);
            }
            Err(error) => {
                video.encoders.remove(&codec);
                video.encoder_errors.insert(codec, error.to_string());
            }
        }
    }
}

/// 环境探测的输入参数。
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeConfig {
    pub ffmpeg: Option<PathBuf>,
    pub ffprobe: Option<PathBuf>,
    /// Python 解释器（本仓库实测路径是 `/usr/bin/python3.9`，所以不要硬编码 `python3`）。
    pub python: String,
    pub gpu_min_memory_mb: u64,
    pub gpu_min_compute_capability: String,
    pub parallelism: ParallelismPreset,
    pub parallelism_configured: Option<usize>,
    pub prefer_hardware_encoder: bool,
    /// 是否用一帧真编码实测硬件编码器（默认开；关掉只影响速度，不影响软件路径）。
    pub verify_hardware_encoders: bool,
    /// 额外的外部工具探测项。
    pub extra_tools: Vec<ExternalToolSpec>,
}

/// 一个需要探测的通用外部工具。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalToolSpec {
    pub name: String,
    pub program: String,
    pub version_args: Vec<String>,
}

impl ExternalToolSpec {
    pub fn new(name: &str, program: &str, version_args: &[&str]) -> Self {
        Self {
            name: name.to_string(),
            program: program.to_string(),
            version_args: version_args.iter().map(|arg| (*arg).to_string()).collect(),
        }
    }
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            ffmpeg: None,
            ffprobe: None,
            python: "python3".to_string(),
            // 时序修复的实测门槛：低于 4 GB 会频繁 OOM。
            gpu_min_memory_mb: 4096,
            gpu_min_compute_capability: "6.1".to_string(),
            parallelism: ParallelismPreset::Automatic,
            parallelism_configured: None,
            prefer_hardware_encoder: true,
            verify_hardware_encoders: true,
            extra_tools: default_external_tools(),
        }
    }
}

/// 本仓库关心的外部工具默认清单。
///
/// 版本参数是「尽力而为」：工具缺失或参数不被支持时只影响该条报告的 `ok`，
/// 不会让整体探测失败。
pub fn default_external_tools() -> Vec<ExternalToolSpec> {
    vec![
        ExternalToolSpec::new("rubberband", "rubberband", &["--version"]),
        ExternalToolSpec::new("demucs", "demucs", &["--version"]),
        ExternalToolSpec::new("tesseract", "tesseract", &["--version"]),
        ExternalToolSpec::new("openssl", "openssl", &["version"]),
        ExternalToolSpec::new("7z", "7z", &["i"]),
        ExternalToolSpec::new("curl", "curl", &["--version"]),
        ExternalToolSpec::new("wget", "wget", &["--version"]),
    ]
}

/// 顶层探测结果：事实（tools/ffmpeg/gpu/python）+ 决策（video）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityReport {
    #[serde(default)]
    pub tools: Vec<ToolReport>,
    pub ffmpeg: crate::probe::FfmpegCapabilities,
    pub gpu: GpuReport,
    pub python: crate::probe::PythonReport,
    pub video: VideoProcessingCapabilities,
}

impl CapabilityReport {
    /// 生成给云端 `ModuleSpec::capability_probe` 用的能力标签集合。
    ///
    /// 标签是「部署侧是否具备某能力」的唯一事实来源：不满足的模块不会出现在
    /// worker 的能力清单里，不兼容的任务在计划阶段就会被路由走。
    pub fn capability_tags(&self) -> Vec<String> {
        let mut tags = Vec::new();
        if self.ffmpeg.usable() {
            tags.push("media.ffmpeg".to_string());
        }
        if self.ffmpeg.ffmpeg.found {
            tags.push("media.ffprobe".to_string());
        }
        for (codec, plan) in &self.video.encoders {
            tags.push(format!(
                "encode.{codec}.{}",
                if plan.hardware {
                    "hardware"
                } else {
                    "software"
                }
            ));
            tags.push(format!("encode.{codec}.{}", plan.selected));
            if plan.hardware {
                if let Some(verification) = plan
                    .candidate(&plan.selected)
                    .and_then(|candidate| candidate.verification.as_deref())
                {
                    if verification.contains("实测通过") {
                        tags.push(format!("encode.{codec}.verified"));
                    }
                }
            }
        }
        if self.video.gpu.eligible_for_inpaint {
            tags.push(format!("inpaint.gpu.{}", self.video.gpu.backend));
        } else {
            tags.push("inpaint.cpu".to_string());
        }
        if self.python.usable() {
            tags.push("model.python".to_string());
            if self.python.torch_version.is_some() {
                tags.push("model.torch".to_string());
            }
            if self.python.onnxruntime_version.is_some() {
                tags.push("model.onnxruntime".to_string());
            }
        }
        // 外部工具标签：`tool.<name>`，缺失的工具不产生标签。
        for tool in &self.tools {
            if tool.found {
                tags.push(format!("tool.{}", tool.name));
            }
        }
        tags.sort();
        tags.dedup();
        tags
    }

    /// 探测是否整体可用（能跑最基础的转码/擦除链路）。
    pub fn usable(&self) -> bool {
        self.ffmpeg.usable()
    }
}

/// 跑一轮完整探测。
pub fn probe_environment(runner: &dyn CommandRunner, config: &ProbeConfig) -> CapabilityReport {
    use crate::probe::{
        probe_ffmpeg_capabilities, probe_ffprobe, probe_gpu, probe_optional_tool, probe_python,
    };

    let ffmpeg = probe_ffmpeg_capabilities(runner, config.ffmpeg.as_deref());
    let ffprobe = probe_ffprobe(runner, config.ffprobe.as_deref());
    let gpu = probe_gpu(runner);
    let python = probe_python(runner, &config.python);

    let mut tools = vec![ffmpeg.ffmpeg.clone(), ffprobe, python_tool_report(&python)];
    for spec in &config.extra_tools {
        let args: Vec<&str> = spec.version_args.iter().map(String::as_str).collect();
        tools.push(probe_optional_tool(
            runner,
            &spec.name,
            &spec.program,
            &args,
        ));
    }

    let gpu_capability = evaluate_gpu(
        &gpu,
        config.gpu_min_memory_mb,
        &config.gpu_min_compute_capability,
    );
    let cpu_count = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1);
    let signal = HardwareSignal::detect().with_cuda(gpu.available);

    let mut video = negotiate_video(
        &ffmpeg.encoders,
        &gpu_capability,
        config.parallelism,
        config.parallelism_configured,
        cpu_count,
        config.prefer_hardware_encoder,
        &signal,
    );
    if config.verify_hardware_encoders {
        let verification = EncoderVerificationConfig {
            ffmpeg: ffmpeg.ffmpeg.program.clone(),
            vaapi_device: first_vaapi_device(),
            timeout: VERIFY_TIMEOUT,
        };
        verify_video_encoders(runner, &verification, &mut video);
    }

    CapabilityReport {
        tools,
        ffmpeg,
        gpu,
        python,
        video,
    }
}

fn python_tool_report(report: &crate::probe::PythonReport) -> ToolReport {
    ToolReport {
        name: "python".to_string(),
        program: report.program.clone(),
        found: report.found,
        version: report.version.clone(),
        ok: report.found,
        detail: match (
            &report.version,
            &report.torch_version,
            &report.onnxruntime_version,
        ) {
            (Some(version), torch, onnx) => format!(
                "Python {version}；torch={}；onnxruntime={}",
                torch.as_deref().unwrap_or("未安装"),
                onnx.as_deref().unwrap_or("未安装")
            ),
            (None, _, _) => "未找到 Python 解释器".to_string(),
        },
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{CommandOutput, FakeRunner};
    use crate::probe::{probe_ffmpeg_capabilities, probe_gpu, GpuDevice};

    fn encoders(names: &[&str]) -> EncoderList {
        let mut list = EncoderList::default();
        for name in names {
            list.video.insert((*name).to_string());
        }
        list
    }

    fn gpu_report(devices: Vec<GpuDevice>) -> GpuReport {
        GpuReport {
            available: !devices.is_empty(),
            backend: "cuda".into(),
            devices,
            queried: true,
            reason: "test".into(),
        }
    }

    fn device(memory_mb: u64, capability: Option<&str>) -> GpuDevice {
        GpuDevice {
            index: 0,
            name: "Test GPU".into(),
            memory_total_mb: memory_mb,
            driver_version: "550.0".into(),
            compute_capability: capability.map(str::to_string),
        }
    }

    fn no_gpu() -> GpuCapability {
        evaluate_gpu(
            &GpuReport {
                available: false,
                backend: "cuda".into(),
                devices: Vec::new(),
                queried: true,
                reason: "无".into(),
            },
            4096,
            "6.1",
        )
    }

    #[test]
    fn prefers_hardware_encoder_when_available() {
        let list = encoders(&["libx264", "h264_vaapi"]);
        let plan = plan_encoder(&list, "h264", true, &HardwareSignal::all()).unwrap();
        assert_eq!(plan.selected, "h264_vaapi");
        assert!(plan.hardware);
        assert!(plan.reason.contains("硬件编码器"));
        // 候选链完整保留，界面可以解释「为什么不是 NVENC」。
        assert_eq!(plan.candidates[0].name, "h264_nvenc");
        assert!(!plan.candidates[0].compiled);
        assert!(!plan.candidates[0].available);
    }

    #[test]
    fn hardware_gating_requires_device_signal() {
        // 编码器编译进来了，但没有硬件设备：不能选。
        let list = encoders(&["libx264", "h264_vaapi", "h264_v4l2m2m"]);
        let plan = plan_encoder(&list, "h264", true, &HardwareSignal::none()).unwrap();
        assert_eq!(plan.selected, "libx264");
        assert!(!plan.hardware);
        assert!(plan.reason.contains("回退软件编码"));
        assert!(plan.reason.contains("缺少硬件设备"));
        let vaapi = plan.candidate("h264_vaapi").unwrap();
        assert!(vaapi.compiled);
        assert!(!vaapi.available);
        // △ 表示「编译进来了但设备不在」。
        assert!(plan.reason.contains("h264_vaapi△") || plan.reason.contains("h264_vaapi"));

        // 只有 VAAPI 设备时选 VAAPI，不选 V4L2。
        let only_vaapi = HardwareSignal {
            cuda: false,
            vaapi: true,
            v4l2m2m: false,
        };
        let plan = plan_encoder(&list, "h264", true, &only_vaapi).unwrap();
        assert_eq!(plan.selected, "h264_vaapi");
    }

    #[test]
    fn falls_back_to_software_encoder() {
        let list = encoders(&["libx264"]);
        let plan = plan_encoder(&list, "h264", true, &HardwareSignal::all()).unwrap();
        assert_eq!(plan.selected, "libx264");
        assert!(!plan.hardware);
        assert!(plan.reason.contains("回退软件编码"));

        let hevc = plan_encoder(
            &encoders(&["libx265"]),
            "hevc",
            true,
            &HardwareSignal::all(),
        )
        .unwrap();
        assert_eq!(hevc.selected, "libx265");
        // 软件编码器不参与硬件门槛判定。
        assert!(plan_encoder(
            &encoders(&["libx264"]),
            "h264",
            true,
            &HardwareSignal::none()
        )
        .is_ok());
    }

    #[test]
    fn software_preferred_still_keeps_hardware_fallback() {
        let list = encoders(&["libx264", "h264_nvenc"]);
        let plan = plan_encoder(&list, "h264", false, &HardwareSignal::all()).unwrap();
        assert_eq!(plan.selected, "libx264");
        assert!(!plan.hardware);
        assert!(plan.reason.contains("用户要求优先软件编码"));
        // 软件编码器缺失时仍然能兜到硬件。
        let only_hw = plan_encoder(
            &encoders(&["h264_nvenc"]),
            "h264",
            false,
            &HardwareSignal::all(),
        )
        .unwrap();
        assert_eq!(only_hw.selected, "h264_nvenc");
    }

    #[test]
    fn videotoolbox_selected_on_macos_and_gated_elsewhere() {
        // macOS 上 VideoToolbox 是系统框架，无需设备信号即可入选；
        // 其他平台没有该框架，必须回退软件编码。
        let list = encoders(&["h264_videotoolbox", "libx264"]);
        let plan = plan_encoder(&list, "h264", true, &HardwareSignal::none()).unwrap();
        if cfg!(target_os = "macos") {
            assert_eq!(plan.selected, "h264_videotoolbox");
            assert!(plan.hardware);
            assert!(plan.reason.contains("硬件编码器"));
        } else {
            assert_eq!(plan.selected, "libx264");
            assert!(!plan.hardware);
        }
    }

    #[test]
    fn reports_unknown_codec_and_empty_chain() {
        let list = encoders(&["libx264"]);
        assert_eq!(
            plan_encoder(&list, "theora", true, &HardwareSignal::all()).unwrap_err(),
            CapsError::UnknownCodec {
                codec: "theora".into()
            }
        );
        let error = plan_encoder(&encoders(&[]), "h264", true, &HardwareSignal::all()).unwrap_err();
        match error {
            CapsError::NoEncoder { candidates, .. } => {
                assert!(candidates.contains(&"libx264".to_string()));
                assert_eq!(candidates.len(), 6);
            }
            other => panic!("期望 NoEncoder，得到 {other:?}"),
        }
    }

    #[test]
    fn hardware_encoder_verification_downgrades_on_real_failure() {
        let list = encoders(&["h264_vaapi", "libx264"]);
        let plan = plan_encoder(&list, "h264", true, &HardwareSignal::all()).unwrap();
        assert_eq!(plan.selected, "h264_vaapi");

        // 实测失败（正是构建机上的真实情况：libva 初始化失败）→ 回退到 libx264。
        let runner = FakeRunner::new().on(
            "ffmpeg",
            CommandOutput::new(
                1,
                "",
                "libva: vaGetDriverNameByIndex() failed\nFailed to initialise VAAPI connection",
            ),
        );
        let config = EncoderVerificationConfig {
            ffmpeg: "ffmpeg".into(),
            vaapi_device: Some(PathBuf::from("/dev/dri/renderD128")),
            timeout: Duration::from_secs(5),
        };
        let verified = verify_encoder_plan(&runner, &config, &plan).unwrap();
        assert_eq!(verified.selected, "libx264");
        assert!(!verified.hardware);
        assert!(verified.reason.contains("单帧实测失败"));
        assert!(verified.reason.contains("回退到 libx264"));
        let failed = verified.candidate("h264_vaapi").unwrap();
        assert!(!failed.available);
        assert!(failed.verification.as_deref().unwrap().contains("libva"));
        // 实测命令必须真的指定了设备节点与 hwupload。
        let call = runner.calls()[0].display();
        assert!(call.contains("-vaapi_device /dev/dri/renderD128"));
        assert!(call.contains("format=nv12,hwupload"));
        assert!(call.contains("-c:v h264_vaapi"));
    }

    #[test]
    fn hardware_encoder_verification_accepts_success() {
        let list = encoders(&["h264_nvenc", "libx264"]);
        let plan = plan_encoder(&list, "h264", true, &HardwareSignal::all()).unwrap();
        let runner = FakeRunner::new().on("ffmpeg", CommandOutput::ok(""));
        let config = EncoderVerificationConfig {
            ffmpeg: "ffmpeg".into(),
            vaapi_device: None,
            timeout: Duration::from_secs(5),
        };
        let verified = verify_encoder_plan(&runner, &config, &plan).unwrap();
        assert_eq!(verified.selected, "h264_nvenc");
        assert!(verified.hardware);
        assert!(verified.reason.contains("单帧实测通过"));
        assert_eq!(
            verified
                .candidate("h264_nvenc")
                .unwrap()
                .verification
                .as_deref(),
            Some("单帧实测通过")
        );
        // NVENC 不需要 vaapi 参数。
        assert!(!runner.calls()[0].display().contains("-vaapi_device"));
    }

    #[test]
    fn software_selection_skips_verification() {
        let list = encoders(&["libx264"]);
        let plan = plan_encoder(&list, "h264", true, &HardwareSignal::all()).unwrap();
        let runner = FakeRunner::new();
        let config = EncoderVerificationConfig {
            ffmpeg: "ffmpeg".into(),
            vaapi_device: None,
            timeout: Duration::from_secs(5),
        };
        let verified = verify_encoder_plan(&runner, &config, &plan).unwrap();
        assert_eq!(verified.selected, "libx264");
        // 软件编码器不该触发任何实测调用。
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn verification_reports_when_no_candidate_survives() {
        let list = encoders(&["h264_vaapi"]);
        let plan = plan_encoder(&list, "h264", true, &HardwareSignal::all()).unwrap();
        let runner =
            FakeRunner::new().on("ffmpeg", CommandOutput::new(1, "", "device init failed"));
        let config = EncoderVerificationConfig {
            ffmpeg: "ffmpeg".into(),
            vaapi_device: Some(PathBuf::from("/dev/dri/renderD128")),
            timeout: Duration::from_secs(5),
        };
        let error = verify_encoder_plan(&runner, &config, &plan).unwrap_err();
        assert!(matches!(error, CapsError::NoEncoder { .. }));

        // VAAPI 设备缺失时不发起实测，直接报错。
        let no_device = EncoderVerificationConfig {
            ffmpeg: "ffmpeg".into(),
            vaapi_device: None,
            timeout: Duration::from_secs(5),
        };
        let error = verify_encoder_plan(&FakeRunner::new(), &no_device, &plan).unwrap_err();
        match error {
            CapsError::NoEncoder { .. } => {}
            other => panic!("期望 NoEncoder，得到 {other:?}"),
        }
    }

    #[test]
    fn verification_falls_through_hardware_to_hardware_then_software() {
        // VAAPI 失败 → V4L2 也失败 → 软件兜底。
        let list = encoders(&["h264_vaapi", "h264_v4l2m2m", "libx264"]);
        let plan = plan_encoder(&list, "h264", true, &HardwareSignal::all()).unwrap();
        let runner = FakeRunner::new().on("ffmpeg", CommandOutput::new(1, "", "unsupported"));
        let config = EncoderVerificationConfig {
            ffmpeg: "ffmpeg".into(),
            vaapi_device: Some(PathBuf::from("/dev/dri/renderD128")),
            timeout: Duration::from_secs(5),
        };
        let verified = verify_encoder_plan(&runner, &config, &plan).unwrap();
        assert_eq!(verified.selected, "libx264");
        assert_eq!(runner.calls().len(), 2);
    }

    #[test]
    fn gpu_eligibility_checks_memory_and_compute_capability() {
        let weak = evaluate_gpu(&gpu_report(vec![device(2048, Some("8.6"))]), 4096, "6.1");
        assert!(!weak.eligible_for_inpaint);
        assert!(weak.ineligible_reason.unwrap().contains("显存不足"));

        let old = evaluate_gpu(&gpu_report(vec![device(8192, Some("5.2"))]), 4096, "6.1");
        assert!(!old.eligible_for_inpaint);
        assert!(old.ineligible_reason.unwrap().contains("计算能力不足"));

        let good = evaluate_gpu(&gpu_report(vec![device(8192, Some("8.9"))]), 4096, "6.1");
        assert!(good.eligible_for_inpaint);
        assert!(good.ineligible_reason.is_none());
        assert_eq!(good.recommended_device().unwrap().memory_total_mb, 8192);

        // 老驱动查不到计算能力时不拦，只按显存判定。
        let unknown = evaluate_gpu(&gpu_report(vec![device(8192, None)]), 4096, "6.1");
        assert!(unknown.eligible_for_inpaint);

        let none = no_gpu();
        assert!(!none.eligible_for_inpaint);
        assert!(none.ineligible_reason.unwrap().contains("无"));
    }

    #[test]
    fn parallelism_four_tiers_behave_as_documented() {
        let no_gpu = no_gpu();
        let with_gpu = evaluate_gpu(&gpu_report(vec![device(24564, Some("8.9"))]), 4096, "6.1");

        let auto_cpu = plan_parallelism(ParallelismPreset::Automatic, None, 16, &no_gpu);
        assert_eq!(auto_cpu.automatic, 8);
        assert_eq!(auto_cpu.effective, 8);
        assert_eq!(auto_cpu.maximum, 16);
        assert_eq!(auto_cpu.configured, None);

        let auto_gpu = plan_parallelism(ParallelismPreset::Automatic, None, 16, &with_gpu);
        assert_eq!(auto_gpu.automatic, 4);
        assert!(auto_gpu.basis.contains("GPU"));

        let configured = plan_parallelism(ParallelismPreset::Conservative, Some(3), 16, &no_gpu);
        assert_eq!(configured.automatic, 4);
        assert_eq!(configured.configured, Some(3));
        assert_eq!(configured.effective, 3);

        // 用户给的值超过核数时收敛到上限，并把收敛过程写进 basis。
        let clamped = plan_parallelism(ParallelismPreset::Maximum, Some(999), 4, &no_gpu);
        assert_eq!(clamped.effective, 4);
        assert!(clamped.basis.contains("收敛"));

        let single = plan_parallelism(ParallelismPreset::Single, Some(0), 32, &no_gpu);
        assert_eq!(single.effective, 1);
        assert_eq!(single.automatic, 1);

        // 极端核数不会产生 0 通道。
        let tiny = plan_parallelism(ParallelismPreset::Conservative, None, 1, &no_gpu);
        assert_eq!(tiny.effective, 1);
        assert_eq!(
            plan_parallelism(ParallelismPreset::Balanced, None, 3, &no_gpu).automatic,
            1
        );
        assert_eq!(
            plan_parallelism(ParallelismPreset::Balanced, None, 40, &no_gpu).automatic,
            8
        );
        assert_eq!(
            plan_parallelism(ParallelismPreset::Maximum, None, 40, &no_gpu).automatic,
            40
        );
    }

    #[test]
    fn preset_parsing_round_trips() {
        for preset in ParallelismPreset::ALL {
            assert_eq!(ParallelismPreset::parse(preset.as_str()), Some(*preset));
        }
        assert_eq!(
            ParallelismPreset::parse("AUTO"),
            Some(ParallelismPreset::Automatic)
        );
        assert_eq!(ParallelismPreset::parse("nonsense"), None);
    }

    #[test]
    fn negotiate_builds_plans_and_errors() {
        let video = negotiate_video(
            &encoders(&["libx264"]),
            &no_gpu(),
            ParallelismPreset::Automatic,
            None,
            8,
            true,
            &HardwareSignal::none(),
        );
        assert_eq!(video.encoders["h264"].selected, "libx264");
        // hevc 在此环境没有编码器 → 记录为错误而不是 panic。
        assert_eq!(video.encoder_errors.len(), 1);
        assert!(video.encoder_errors["hevc"].contains("libx265"));
        assert_eq!(video.parallelism.effective, 4);
        assert!(!video.hardware_signal.vaapi);
    }

    #[test]
    fn hardware_signal_describes_itself_and_gates_encoders() {
        let signal = HardwareSignal {
            cuda: true,
            vaapi: false,
            v4l2m2m: true,
        };
        assert!(signal.allows("h264_nvenc"));
        assert!(!signal.allows("h264_vaapi"));
        assert!(signal.allows("h264_v4l2m2m"));
        assert!(signal.allows("libx264"));
        let described = signal.describe();
        assert!(described.contains("cuda=有"));
        assert!(described.contains("vaapi=无"));
        assert!(HardwareSignal::detect().with_cuda(true).cuda);
    }

    #[test]
    fn capability_tags_reflect_environment() {
        let runner = FakeRunner::new()
            .on_match(
                "ffmpeg",
                "-version",
                CommandOutput::ok("ffmpeg version 4.2.4\n"),
            )
            .on_match(
                "ffmpeg",
                "-encoders",
                CommandOutput::ok(
                    "Encoders:\n V..... = Video\n ------\n V....D libx264  x\n A....D aac  x\n",
                ),
            )
            .on_match(
                "ffmpeg",
                "-filters",
                CommandOutput::ok("Filters:\n  T.. = Timeline support\n ... drawtext  V->V  x\n"),
            )
            .on("ffprobe", CommandOutput::ok("ffprobe version 4.2.4\n"))
            .on_match("python3.9", "import torch", CommandOutput::ok("2.1.0\n"))
            .on_match(
                "python3.9",
                "import onnxruntime",
                CommandOutput::new(1, "", "missing"),
            )
            .on("python3.9", CommandOutput::new(0, "Python 3.9.9\n", ""))
            .on(
                "openssl",
                CommandOutput::ok("OpenSSL 3.0.7 7 Feb 2023 (Library: OpenSSL 3.0.7)\n"),
            );

        let config = ProbeConfig {
            python: "python3.9".into(),
            extra_tools: vec![ExternalToolSpec::new("openssl", "openssl", &["version"])],
            parallelism_configured: Some(2),
            ..ProbeConfig::default()
        };
        let report = probe_environment(&runner, &config);
        assert!(report.usable());
        assert_eq!(report.python.torch_version.as_deref(), Some("2.1.0"));

        let tags = report.capability_tags();
        assert!(tags.contains(&"media.ffmpeg".to_string()));
        assert!(tags.contains(&"encode.h264.libx264".to_string()));
        assert!(tags.contains(&"encode.h264.software".to_string()));
        assert!(tags.contains(&"inpaint.cpu".to_string()));
        assert!(tags.contains(&"model.torch".to_string()));
        assert!(!tags.contains(&"model.onnxruntime".to_string()));
        assert!(tags.contains(&"tool.openssl".to_string()));
        assert!(!tags.contains(&"tool.demucs".to_string()));
        assert_eq!(report.video.parallelism.effective, 2);

        // 版本号出现在工具报告里（供界面展示）。
        let ffmpeg = report
            .tools
            .iter()
            .find(|tool| tool.name == "ffmpeg")
            .unwrap();
        assert_eq!(ffmpeg.version.as_deref(), Some("4.2.4"));
        let python = report
            .tools
            .iter()
            .find(|tool| tool.name == "python")
            .unwrap();
        assert!(python.detail.contains("torch=2.1.0"));
    }

    /// 在真实机器上跑完整探测（含硬件编码器实测）。
    ///
    /// 只断言「结论自洽」，不假设机器上装了什么；但会检查一条关键不变式：
    /// **凡是选中硬件编码器，就必须有实测通过记录**——这正是构建机上
    /// 「VAAPI 编译进来了、设备节点也在，但 libva 起不来」要拦住的情况。
    #[test]
    fn real_environment_probe_smoke_test() {
        let runner = crate::command::SystemRunner::new();
        let report = probe_environment(
            &runner,
            &ProbeConfig {
                extra_tools: Vec::new(),
                ..ProbeConfig::default()
            },
        );
        assert!(report.video.parallelism.effective >= 1);
        assert!(report.video.parallelism.effective <= report.video.parallelism.maximum);

        // 下面的断言都要读真实解析结果：没有 ffmpeg 的机器（CI runner 就是）直接跳过，
        // 与本仓库其余真机用例（`stamp_cli.rs` / `render.rs`）的跳过约定一致。
        if !report.ffmpeg.usable() {
            eprintln!("跳过：本机没有可用的 ffmpeg");
            return;
        }
        assert!(report
            .capability_tags()
            .contains(&"media.ffmpeg".to_string()));

        // 解析确实跑通了：ffmpeg 可用就必须给出非空的编码器/滤镜清单。
        // 只断言「有清单」，不断言清单里是 libx264 / drawtext 这些具体条目——
        // 那是某一份 ffmpeg 构建的内容，不是探测链路的不变式。
        assert!(
            !report.ffmpeg.encoders.video.is_empty(),
            "ffmpeg 可用却没解析出编码器：{:?}",
            report.ffmpeg.list_error
        );
        assert!(
            !report.ffmpeg.filters.video.is_empty(),
            "ffmpeg 可用却没解析出滤镜：{:?}",
            report.ffmpeg.list_error
        );

        let h264 = report.video.encoders.get("h264").unwrap_or_else(|| {
            panic!(
                "有可用的 ffmpeg 却没有 h264 选型结论：{:?}",
                report.video.encoder_errors
            )
        });
        // 选型结果必须来自这台机器真实的编码器清单。
        assert!(
            report.ffmpeg.encoders.supports(&h264.selected),
            "选中的编码器 {} 不在本机清单里",
            h264.selected
        );
        if h264.hardware {
            let verification = h264
                .candidate(&h264.selected)
                .and_then(|candidate| candidate.verification.as_deref())
                .unwrap_or("");
            assert!(
                verification.contains("实测通过"),
                "选了硬件编码器 {} 却没有实测通过记录：{verification}",
                h264.selected
            );
        } else {
            // 软件回退必须给出理由。
            assert!(!h264.reason.is_empty());
        }
    }

    #[test]
    fn probe_gpu_and_caps_interoperate() {
        // 无 GPU 机器上的真实探测路径。
        let report = probe_gpu(&crate::command::SystemRunner::new());
        let capability = evaluate_gpu(&report, 4096, "6.1");
        assert!(!capability.eligible_for_inpaint);
        assert_eq!(capability.min_memory_mb, 4096);
    }

    #[test]
    fn ffmpeg_probe_helper_is_used() {
        let runner = FakeRunner::new();
        let caps = probe_ffmpeg_capabilities(&runner, None);
        assert!(!caps.usable());
    }
}
