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
    build_mount_copy_rows, builtin_profiles, diff_packages as diff_packages_core,
    export_capcut_draft, find_profile, import_capcut_draft, load_mount_copy_csv,
    make_publish_batch, plan_normalization, preview_publish_batch, probe_episode, review_drama,
    scan_drama_dir, transition_publish_row, validate_drama_dir, validate_drama_name,
    validate_library_root, validate_publish_update, write_mount_copy_csv, CapcutDraft,
    DraftExportOptions, DraftExportResult, Drama, FfprobeConfig, MountCopyRow,
    MountCopyWriteResult, NormalizationPlan, PackageDiff, PlatformProfile, ProbeFailure,
    ProbedEpisode, ProfileReview, PublishPreview, TranscodeTarget,
};
use serde::Serialize;

/// 逐集探测结果：成功的与失败的分开交给调用方。
///
/// 以前失败的分集被 `filter_map(.ok())` 静默丢掉，草稿、文案、转码产物里少了几集也照样报成功。
/// 现在每个调用方都必须决定：拒绝执行（[`ProbeSplit::require_all`]），或在结果里标明失败项。
#[derive(Debug, Default)]
pub(crate) struct ProbeSplit {
    pub ok: Vec<ProbedEpisode>,
    pub failed: Vec<ProbeFailure>,
}

impl ProbeSplit {
    /// 失败分集的可读清单：`a.mp4（原因）；b.mp4（原因）`。
    pub fn failure_list(&self) -> String {
        self.failed
            .iter()
            .map(|failure| format!("{}（{}）", failure.file.file_name, failure.reason))
            .collect::<Vec<_>>()
            .join("；")
    }

    /// 产物依赖完整集数的入口用：有任何一集探测失败就拒绝，并列出文件与原因。
    pub fn require_all(self, action: &str) -> Result<Vec<ProbedEpisode>, String> {
        if !self.failed.is_empty() {
            return Err(format!(
                "有 {} 集探测失败，未{action}（避免产物缺集）：{}。请修复或移走这些文件后重试。",
                self.failed.len(),
                self.failure_list()
            ));
        }
        if self.ok.is_empty() {
            return Err(format!("没有任何分集，无法{action}。"));
        }
        Ok(self.ok)
    }

    /// 只读分析的入口用：允许部分失败（结果里标明），但一集都没探测成功时报错并列出原因。
    pub fn require_some(&self, action: &str) -> Result<(), String> {
        if !self.ok.is_empty() {
            return Ok(());
        }
        if self.failed.is_empty() {
            return Err(format!("没有任何分集，无法{action}。"));
        }
        Err(format!(
            "所有分集都探测失败，无法{action}：{}。请先确认素材可读且 ffprobe 可用。",
            self.failure_list()
        ))
    }
}

/// 逐集探测（与 `analyze_drama` 走同一个探测函数，避免两套行为）。
pub(crate) fn probe_all(drama: &Drama, ffprobe_path: Option<String>) -> ProbeSplit {
    let config = FfprobeConfig {
        bin: ffprobe_path.map(PathBuf::from),
    };
    split_probes(&drama.episodes, |episode| {
        probe_episode(episode, &config).map_err(|error| error.to_string())
    })
}

/// 分拣逻辑本身（探测函数可注入，单测不依赖 ffprobe）。
fn split_probes(
    episodes: &[dossier_core::EpisodeFile],
    mut probe: impl FnMut(&dossier_core::EpisodeFile) -> Result<ProbedEpisode, String>,
) -> ProbeSplit {
    let mut split = ProbeSplit::default();
    for episode in episodes {
        match probe(episode) {
            Ok(probed) => split.ok.push(probed),
            Err(reason) => split.failed.push(ProbeFailure {
                file: episode.clone(),
                reason,
            }),
        }
    }
    split
}

pub(crate) fn load_drama(root: &str, name: &str) -> Result<Drama, String> {
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
    /// 探测失败、未纳入建议的分集（文件名 + 原因）。
    skipped: Vec<ProbeFailure>,
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
    // 只建议、不执行：探测失败的分集不拦，列在结果里（它们往往最需要处理）。
    let probes = probe_all(&drama, ffprobe_path);
    probes.require_some("生成转码建议")?;
    let plan = plan_normalization(
        &drama.name,
        &drama.dir,
        &probes.ok,
        &target.unwrap_or_default(),
    )?;
    let script = plan.script();
    Ok(TranscodeAdvicePayload {
        plan,
        script,
        skipped: probes.failed,
    })
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
    // 只读对照：探测失败的分集计入阻断项并写进说明，不静默跳过。
    let probes = probe_all(&drama, ffprobe_path);
    Ok(review_drama(
        &profile,
        &probes.ok,
        &probes.failed,
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
    crate::license::guard()?;
    let drama = load_drama(&root, &name)?;
    let episodes = probe_all(&drama, ffprobe_path).require_all("导出草稿")?;
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

/// 挂载文案 CSV 的输出路径校验：只允许 `.csv` 扩展名，防止填错路径（或前端被攻破）时
/// 覆盖 `.exe`、配置文件等任意文件。`overwrite` 为 false 时已存在的文件一律拒绝。
fn csv_output_path(path: &str, overwrite: bool) -> Result<PathBuf, String> {
    let path = PathBuf::from(path.trim());
    let is_csv = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("csv"));
    if !is_csv {
        return Err(format!("挂载文案只能保存为 .csv 文件：{}", path.display()));
    }
    // symlink_metadata 判断：悬空链接也算已存在，不顺着它写出文件。
    if !overwrite && dossier_core::fsio::path_occupied(&path) {
        return Err(format!(
            "文件已存在：{}。确认覆盖后再生成。",
            path.display()
        ));
    }
    Ok(path)
}

/// 生成可编辑的「挂载文案.csv」。这里只生成本地元数据，不连接第三方平台。
/// 目标文件已存在时需要前端显式传 `overwrite: true`（用户确认后）才覆盖。
#[tauri::command]
pub fn generate_mount_copy_csv(
    root: String,
    name: String,
    output_path: String,
    ffprobe_path: Option<String>,
    overwrite: Option<bool>,
) -> Result<MountCopyWriteResult, String> {
    crate::license::guard()?;
    let output_path = csv_output_path(&output_path, overwrite == Some(true))?;
    let drama = load_drama(&root, &name)?;
    let episodes = probe_all(&drama, ffprobe_path).require_all("生成挂载文案")?;
    let rows = build_mount_copy_rows(&drama.name, &episodes);
    write_mount_copy_csv(&output_path, &rows).map_err(|error| error.to_string())
}

/// 读回挂载文案 CSV，供人工修改后的结果复核。
#[tauri::command]
pub fn load_mount_copy(path: String) -> Result<Vec<MountCopyRow>, String> {
    load_mount_copy_csv(&PathBuf::from(path)).map_err(|error| error.to_string())
}

/// 保存人工编辑后的挂载文案 CSV。不会触发外部发布。
#[tauri::command]
pub fn save_mount_copy(
    path: String,
    rows: Vec<MountCopyRow>,
) -> Result<MountCopyWriteResult, String> {
    // 保存就是覆盖同一份文案；只拦扩展名，避免把文案写进任意文件。
    let path = csv_output_path(&path, true)?;
    if path.exists() {
        let previous = load_mount_copy_csv(&path).map_err(|error| error.to_string())?;
        if previous.len() != rows.len() {
            return Err("保存时不能增删行；请生成新的发布批次".into());
        }
        for (before, after) in previous.iter().zip(rows.iter()) {
            validate_publish_update(before, after).map_err(|error| error.to_string())?;
        }
    } else if rows.iter().any(|row| row.status != "draft") {
        return Err("新建发布 CSV 只能从 draft 状态开始".into());
    }
    write_mount_copy_csv(&path, &rows).map_err(|error| error.to_string())
}

/// 生成不联网的提交前预览，不上传、不创建平台单号，也不修改 CSV 状态。
#[tauri::command]
pub fn preview_mount_copy_publish(
    path: String,
    batch_id: String,
    drama_name: String,
) -> Result<PublishPreview, String> {
    let batch = make_publish_batch(&batch_id, &drama_name, &PathBuf::from(path))
        .map_err(|error| error.to_string())?;
    Ok(preview_publish_batch(&batch_id, &batch))
}

/// 按本地发布状态机更新一条记录。这里只记账，不会向平台提交内容。
#[tauri::command]
pub fn transition_mount_copy_row(
    row: MountCopyRow,
    next_status: String,
    result_note: String,
) -> Result<MountCopyRow, String> {
    let mut row = row;
    transition_publish_row(&mut row, &next_status, result_note)
        .map_err(|error| error.to_string())?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_output_path_requires_csv_extension_and_confirmation() {
        assert!(csv_output_path("/tmp/挂载文案.exe", true).is_err());
        assert!(csv_output_path("/tmp/没有扩展名", true).is_err());
        assert!(csv_output_path("/tmp/挂载文案.CSV", true).is_ok());

        let dir = std::env::temp_dir().join(format!("dossier-csvpath-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let existing = dir.join("已有.csv");
        std::fs::write(&existing, "x").unwrap();
        let path = existing.display().to_string();
        let error = csv_output_path(&path, false).unwrap_err();
        assert!(error.contains("已存在"), "{error}");
        assert!(csv_output_path(&path, true).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn episode_file(name: &str) -> dossier_core::EpisodeFile {
        dossier_core::EpisodeFile {
            path: PathBuf::from(format!("/素材/{name}")),
            file_name: name.into(),
            size_bytes: 1,
            episode_number: dossier_core::scanner::parse_episode_number(name),
        }
    }

    fn split_with_failures(failing: &[&str]) -> ProbeSplit {
        let files: Vec<_> = ["第1集.mp4", "第2集.mp4", "第3集.mp4"]
            .iter()
            .map(|name| episode_file(name))
            .collect();
        split_probes(&files, |file| {
            if failing.contains(&file.file_name.as_str()) {
                Err("moov atom not found".into())
            } else {
                Ok(ProbedEpisode {
                    file: file.clone(),
                    spec: Default::default(),
                })
            }
        })
    }

    #[test]
    fn probe_split_keeps_failures_instead_of_dropping_them() {
        let split = split_with_failures(&["第2集.mp4"]);
        assert_eq!(split.ok.len(), 2);
        assert_eq!(split.failed.len(), 1);
        assert_eq!(split.failed[0].file.file_name, "第2集.mp4");
        assert_eq!(split.failure_list(), "第2集.mp4（moov atom not found）");
    }

    #[test]
    fn require_all_rejects_any_failure_with_file_and_reason() {
        let error = split_with_failures(&["第2集.mp4"])
            .require_all("导出草稿")
            .unwrap_err();
        assert!(error.contains("有 1 集探测失败，未导出草稿"), "{error}");
        assert!(
            error.contains("第2集.mp4（moov atom not found）"),
            "{error}"
        );
        assert_eq!(
            split_with_failures(&[])
                .require_all("导出草稿")
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn require_some_allows_partial_but_lists_reasons_when_all_fail() {
        assert!(split_with_failures(&["第2集.mp4"])
            .require_some("检测码率")
            .is_ok());
        let error = split_with_failures(&["第1集.mp4", "第2集.mp4", "第3集.mp4"])
            .require_some("检测码率")
            .unwrap_err();
        assert!(error.contains("所有分集都探测失败"), "{error}");
        assert!(
            error.contains("第3集.mp4（moov atom not found）"),
            "{error}"
        );
    }

    /// 真实探测：ffprobe 指向不存在的路径时，每一集都进 failed，不会被静默丢掉。
    #[test]
    fn probe_all_reports_every_episode_when_ffprobe_is_missing() {
        let dir = std::env::temp_dir().join(format!("dossier-probe-all-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("第1集.mp4"), b"x").unwrap();
        std::fs::write(dir.join("第2集.mp4"), b"x").unwrap();
        let drama = scan_drama_dir("剧", &dir);
        let split = probe_all(&drama, Some("/不存在/ffprobe".into()));
        assert!(split.ok.is_empty());
        assert_eq!(split.failed.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
