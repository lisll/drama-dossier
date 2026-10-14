//! dossier-core — 剧备（drama-dossier）领域核心
//!
//! 纯 Rust 实现，不依赖任何 GUI 框架：
//! - [`scanner`]：剧集库扫描（一部剧一个子目录）
//! - [`probe`]：ffprobe 探测分集规格
//! - [`checklist`]：报审材料完整性检查
//! - [`package`]：报审包目录生成与清单导出
//! - [`transcode`]：转码建议与批量规范化脚本（只建议，不执行）
//! - [`diff`]：两次报审包的清单差异对比
//! - [`platforms`]：平台材料要求知识库（内置示例数据）
//! - [`draft`]：剪映草稿的导入与导出
//!
//! 合规边界：本 crate 不提供、也不应加入任何内容指纹规避、
//! 版权保护绕过或材料伪造能力。报审包内置于权属声明模板。

pub mod checklist;
pub mod diff;
pub mod draft;
pub mod package;
pub mod platforms;
pub mod probe;
pub mod scanner;
pub mod settings;
pub mod transcode;
pub mod types;

pub use checklist::{run_checks, ChecklistReport, ChecklistRules};
pub use diff::{
    diff_manifests, diff_packages, load_manifest, parse_manifest, PackageDiff, PackageManifest,
};
pub use draft::{
    export_capcut_draft, import_capcut_draft, CapcutDraft, DraftExportOptions, DraftExportResult,
};
pub use package::{generate_report_package, CostSheetForm, PackageOptions, PackageResult};
pub use platforms::{
    builtin_profiles, check_profile, find_profile, review_drama, PlatformProfile, ProfileReview,
};
pub use probe::{probe_episode, probe_episodes, FfprobeConfig};
pub use scanner::{
    scan_drama_dir, scan_library, validate_drama_dir, validate_drama_name, validate_library_root,
};
pub use settings::{load_settings, save_settings, AppSettings, LoadedSettings};
pub use transcode::{advise_episode, plan_normalization, NormalizationPlan, TranscodeTarget};
pub use types::{CheckStatus, Drama, EpisodeFile, EpisodeSpec, ProbedEpisode};
