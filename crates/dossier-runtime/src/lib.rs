//! dossier-runtime — 运行时能力管理（技术学习用途）
//!
//! 解决的问题：擦除/分离/识别这些能力依赖一堆**外部**东西（ffmpeg、python、torch、
//! 模型权重、字体、外部 CLI），而此前只有一句打印指引。这个 crate 把「运行时」
//! 当成可管理对象：
//!
//! - [`manifest`]：组件清单（角色 / 版本 / 依赖 / 逐文件 SHA-256）；
//! - [`command`]：可注入的命令执行器（真实进程 vs 测试回放）；
//! - [`probe`]：环境探测（外部工具、FFmpeg 编码器与滤镜、NVIDIA GPU、Python 框架）；
//! - [`caps`]：能力协商（GPU 门槛、硬件编码器与 CPU 回退、四档并行度）；
//! - [`store`]：组件安装 / 校验 / 回滚；
//! - [`update`]：更新通道清单 + Ed25519 签名校验；
//! - [`secret`]：本地凭据存储（文件权限 + 原子写，明确不是 OS 级密钥库）；
//! - [`device`]：设备指纹（仅稳定标识的哈希，不外发）；
//! - [`paths`]：平台数据目录解析（XDG / HOME / Windows `%APPDATA%`）。
//!
//! 合规边界：本 crate 只做本地能力管理与校验，不下载并执行未校验的可执行文件，
//! 也不向任何服务上报设备信息。

pub mod caps;
pub mod command;
pub mod device;
pub mod digest;
pub mod manifest;
pub mod paths;
pub mod probe;
pub mod secret;
pub mod store;
pub mod update;

pub use caps::{
    evaluate_gpu, negotiate_video, plan_encoder, plan_parallelism, probe_environment,
    CapabilityReport, EncoderPlan, GpuCapability, ParallelismCapability, ParallelismPreset,
    ProbeConfig, VideoProcessingCapabilities,
};
pub use command::{
    CommandError, CommandOutput, CommandRunner, CommandSpec, FakeRunner, SystemRunner,
};
pub use device::{compute_device_fingerprint, DeviceFingerprint};
pub use digest::{is_sha256_hex, sha256_bytes, sha256_file};
pub use manifest::{
    Component, ComponentFile, ComponentSource, ManifestError, ModelInfo, RuntimeManifest,
    RuntimeRoles, MANIFEST_SCHEMA_VERSION,
};
pub use paths::default_data_root;
pub use probe::{
    probe_ffmpeg, probe_ffmpeg_capabilities, probe_gpu, probe_python, EncoderList,
    FfmpegCapabilities, FilterList, GpuDevice, GpuReport, PythonReport, ToolReport,
};
pub use secret::{SecretError, SecretStore};
pub use store::{
    ComponentStatus, ComponentVerification, InstallReport, RollbackReport, RuntimeStore,
    StoreConfig, StoreError,
};
pub use update::{
    compare_versions, plan_update, verify_release_signature, Release, UpdateChannel, UpdateError,
    UpdatePlan,
};
