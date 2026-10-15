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
//! - [`progress`]：长任务进度流（JSONL 追加写 + 乱序拦截）
//! - [`checkpoint`]：检查点与断点续跑（产物校验 + 输入指纹）
//! - [`pipeline_version`]：管线版本化与跨产物一致性校验
//!
//! 合规边界：本 crate 不提供、也不应加入任何内容指纹规避、
//! 版权保护绕过或材料伪造能力。报审包内置于权属声明模板。

pub mod account;
pub mod checklist;
pub mod checkpoint;
pub mod diff;
pub mod draft;
pub mod ledger;
pub mod package;
pub mod pipeline_version;
pub mod platforms;
pub mod probe;
pub mod progress;
pub mod scanner;
pub mod settings;
pub mod tools;
pub mod transcode;
pub mod types;

pub use account::{
    change_password, generate_salt, hash_password, login, logout, register, status,
    validate_email, validate_password, AccountError, AccountRecord, AccountStatus, AccountStore,
};
pub use checklist::{run_checks, ChecklistReport, ChecklistRules};
pub use checkpoint::{
    load_checkpoint, save_checkpoint, ArtifactRef, Checkpoint, CheckpointError, StepState,
    StepStatus,
};
pub use diff::{
    diff_manifests, diff_packages, load_manifest, parse_manifest, PackageDiff, PackageManifest,
};
pub use draft::{
    export_capcut_draft, import_capcut_draft, CapcutDraft, DraftExportOptions, DraftExportResult,
};
pub use ledger::{append_record, list_records, now_timestamp, LedgerRecord};
pub use package::{generate_report_package, CostSheetForm, PackageOptions, PackageResult};
pub use pipeline_version::{
    check_consistency, load_and_check, pipeline_version, ArtifactStamp, ConsistencyIssue,
    ConsistencyReport, SkewKind, ToolStamp, ERASE_PIPELINE, REPORT_PIPELINE, STAMP_SUFFIX,
    SUBTITLE_PIPELINE,
};
pub use platforms::{
    builtin_profiles, check_profile, find_profile, review_drama, PlatformProfile, ProfileReview,
};
pub use probe::{probe_episode, probe_episodes, FfprobeConfig};
pub use progress::{
    load_progress_log, parse_progress_log, ProgressError, ProgressEvent, ProgressEventKind,
    ProgressLog, ProgressRecorder, TaskProgress,
};
pub use scanner::{
    scan_drama_dir, scan_library, validate_drama_dir, validate_drama_name, validate_library_root,
};
pub use settings::{load_settings, save_settings, AppSettings, LoadedSettings};
pub use tools::{
    bitrate_report, episode_target_name, nudge_frames, output_dir_hits_sources,
    plan_recut_segments, plan_rename, same_path, validate_mount_csv, BitrateItem, MountCsvCheck,
    MountCsvIssue, RenameItem, RenamePlan,
};
pub use transcode::{advise_episode, plan_normalization, NormalizationPlan, TranscodeTarget};
pub use types::{CheckStatus, Drama, EpisodeFile, EpisodeSpec, ProbedEpisode};
