//! dossier-core — 剧备（drama-dossier）领域核心
//!
//! 纯 Rust 实现，不依赖任何 GUI 框架：
//! - [`scanner`]：剧集库扫描（一部剧一个子目录）
//! - [`probe`]：ffprobe 探测分集规格
//! - [`checklist`]：报审材料完整性检查
//! - [`package`]：报审包目录生成与清单导出
//!
//! 合规边界：本 crate 不提供、也不应加入任何内容指纹规避、
//! 版权保护绕过或材料伪造能力。报审包内置于权属声明模板。

pub mod checklist;
pub mod package;
pub mod probe;
pub mod scanner;
pub mod types;

pub use checklist::{run_checks, ChecklistReport, ChecklistRules};
pub use package::{generate_report_package, CostSheetForm, PackageOptions, PackageResult};
pub use probe::{probe_episode, probe_episodes, FfprobeConfig};
pub use scanner::{scan_drama_dir, scan_library};
pub use types::{CheckStatus, Drama, EpisodeFile, EpisodeSpec, ProbedEpisode};
