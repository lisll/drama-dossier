//! 报审配套能力的桌面入口：转码建议、改稿对比、平台档案、剪映草稿。
//!
//! 与 `erase.rs` 一样，这一层只做参数适配与中文错误转换；
//! 规则与计算全部在 `dossier-core` 里（并有单元测试覆盖）。
//!
//! 注意：本模块依赖 Tauri 系统库，在装不了桌面壳的机器上无法编译验证，
//! 改动后请用 `rustfmt --edition 2021 --check` 做语法/风格校验，并在能装桌面壳的
//! 机器上跑一次 `cargo check -p drama-dossier`。

use std::path::PathBuf;

use dossier_core::{
    builtin_profiles, diff_packages as diff_packages_core, export_capcut_draft, find_profile,
    import_capcut_draft, plan_normalization, probe_episode, review_drama, scan_drama_dir,
    validate_drama_dir, validate_drama_name, validate_library_root, CapcutDraft,
    DraftExportOptions, DraftExportResult, Drama, FfprobeConfig, NormalizationPlan, PackageDiff,
    PlatformProfile, ProbedEpisode, ProfileReview, TranscodeTarget,
};
use serde::Serialize;

/// 逐集探测（与 `analyze_drama` 走同一个探测函数，避免两套行为）。
fn probe_all(drama: &Drama, ffprobe_path: Option<String>) -> Vec<ProbedEpisode> {
    let config = FfprobeConfig {
        bin: ffprobe_path.map(PathBuf::from),
    };
    drama
        .episodes
        .iter()
        .filter_map(|episode| probe_episode(episode, &config).ok())
        .collect()
}

fn load_drama(root: &str, name: &str) -> Result<Drama, String> {
    let root = PathBuf::from(root);
    validate_library_root(&root).map_err(|error| error.to_string())?;
    validate_drama_name(name).map_err(|error| error.to_string())?;
    let drama_dir = root.join(name);
    validate_drama_dir(&drama_dir).map_err(|error| error.to_string())?;
    Ok(scan_drama_dir(name, &drama_dir))
}

/// 转码建议结果：计划 + 可直接保存执行的脚本。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscodeAdvicePayload {
    plan: NormalizationPlan,
    script: String,
}

/// 生成转码建议（只建议，不执行任何转码）。
#[tauri::command]
pub fn transcode_advice(
    root: String,
    name: String,
    target: Option<TranscodeTarget>,
    ffprobe_path: Option<String>,
) -> Result<TranscodeAdvicePayload, String> {
    let drama = load_drama(&root, &name)?;
    let episodes = probe_all(&drama, ffprobe_path);
    if episodes.is_empty() {
        return Err("没有任何成功探测规格的分集；请先确认素材可读且 ffprobe 可用。".into());
    }
    let plan = plan_normalization(
        &drama.name,
        &drama.dir,
        &episodes,
        &target.unwrap_or_default(),
    )?;
    let script = plan.script();
    Ok(TranscodeAdvicePayload { plan, script })
}

/// 两份报审包的清单差异。
#[tauri::command]
pub fn diff_report_packages(left_dir: String, right_dir: String) -> Result<PackageDiff, String> {
    diff_packages_core(&PathBuf::from(left_dir), &PathBuf::from(right_dir))
        .map_err(|error| error.to_string())
}

/// 内置平台档案列表（学习示例数据）。
#[tauri::command]
pub fn platform_profiles() -> Vec<PlatformProfile> {
    builtin_profiles()
}

/// 按平台档案逐集对照一部剧。
#[tauri::command]
pub fn platform_review(
    root: String,
    name: String,
    profile_id: String,
    ffprobe_path: Option<String>,
) -> Result<ProfileReview, String> {
    let profile =
        find_profile(&profile_id).ok_or_else(|| format!("未知的平台档案：{profile_id}"))?;
    let drama = load_drama(&root, &name)?;
    let episodes = probe_all(&drama, ffprobe_path);
    Ok(review_drama(
        &profile,
        &episodes,
        drama.cover.is_some(),
        drama.synopsis.is_some(),
    ))
}

/// 导出剪映草稿（分集顺序与时长）。
#[tauri::command]
pub fn export_draft(
    root: String,
    name: String,
    options: Option<DraftExportOptions>,
    ffprobe_path: Option<String>,
) -> Result<DraftExportResult, String> {
    let drama = load_drama(&root, &name)?;
    let episodes = probe_all(&drama, ffprobe_path);
    if episodes.is_empty() {
        return Err("没有任何成功探测规格的分集，无法导出草稿。".into());
    }
    export_capcut_draft(
        &drama.name,
        &drama.dir,
        &episodes,
        &options.unwrap_or_default(),
    )
    .map_err(|error| error.to_string())
}

/// 读回剪映草稿的时间线。
#[tauri::command]
pub fn import_draft(draft_dir: String) -> Result<CapcutDraft, String> {
    import_capcut_draft(&PathBuf::from(draft_dir)).map_err(|error| error.to_string())
}
