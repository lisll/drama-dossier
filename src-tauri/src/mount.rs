//! 挂载桌面入口：挂载计划持久化与「挂载文案.csv」的生成 / 校验 / 保存。
//!
//! 与竞品一致的交互：计划保存时在剧目目录创建（或复用）「挂载文案.csv」；
//! 校验规则在 `dossier_core::tools::validate_mount_csv`（有单测），这里只做文件编排。
//! 视频号登录与上传为后续云端能力，命令层暂不提供。

use std::path::{Path, PathBuf};

use dossier_core::{append_record, list_records, now_timestamp, validate_mount_csv, LedgerRecord};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::dossier::load_drama;

/// 挂载文案固定文件名（与竞品一致）。
pub const MOUNT_CSV_NAME: &str = "挂载文案.csv";

/// 挂载计划：当前绑定哪部剧、CSV 在哪、上次保存时间。
/// 四步（素材→文案→账号→执行）的临时勾选状态留在前端，不落盘。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MountPlan {
    pub root: String,
    pub drama_name: String,
    pub csv_path: String,
    pub saved_at: String,
}

/// 校验结果回包（在 core 结果上补充 CSV 路径与当前内容，前端可直接回填编辑区）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MountCsvCheckPayload {
    #[serde(flatten)]
    pub check: dossier_core::MountCsvCheck,
    pub csv_path: String,
    pub content: String,
}

fn plan_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|dir| dir.join("mount-plan.json"))
        .map_err(|error| format!("无法定位应用数据目录：{error}。请检查系统权限。"))
}

/// 剧目目录下的挂载文案 CSV 路径。
fn drama_csv_path(root: &str, name: &str) -> Result<PathBuf, String> {
    let drama = load_drama(root, name)?;
    Ok(drama.dir.join(MOUNT_CSV_NAME))
}

/// 保存挂载计划（前端四步流程点「保存计划」时调用）。
#[tauri::command]
pub fn mount_plan_save(
    app: AppHandle,
    root: String,
    drama_name: String,
) -> Result<MountPlan, String> {
    crate::logging::log_event("挂载", format!("保存计划：{drama_name}（{root}）"));
    let result = mount_plan_save_inner(&app, root, drama_name);
    crate::logging::log_event(
        "挂载",
        crate::logging::result_text(&result, |plan| {
            format!("计划已保存：{}", plan.csv_path)
        }),
    );
    result
}

fn mount_plan_save_inner(
    app: &AppHandle,
    root: String,
    drama_name: String,
) -> Result<MountPlan, String> {
    // 剧目必须真实存在，避免计划指向不存在的目录。
    let csv = drama_csv_path(&root, &drama_name)?;
    let plan = MountPlan {
        root,
        drama_name,
        csv_path: csv.display().to_string(),
        saved_at: now_timestamp(),
    };
    let path = plan_path(&app)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("创建数据目录失败：{error}"))?;
    }
    let data =
        serde_json::to_vec_pretty(&plan).map_err(|error| format!("序列化计划失败：{error}"))?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, data).map_err(|error| format!("写入计划失败：{error}"))?;
    std::fs::rename(&temp, &path).map_err(|error| format!("替换计划失败：{error}"))?;
    Ok(plan)
}

/// 读取挂载计划；没有时返回 `None`。
#[tauri::command]
pub fn mount_plan_load(app: AppHandle) -> Result<Option<MountPlan>, String> {
    let path = plan_path(&app)?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).map_err(|error| format!("读取计划失败：{error}"))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("计划文件损坏：{error}"))
}

/// 生成「挂载文案.csv」模板到剧目目录：已存在则直接复用并返回现有内容路径。
/// 模板为每集一行「集号,文案」，文案留空待填。
#[tauri::command]
pub fn mount_csv_template(
    root: String,
    drama_name: String,
) -> Result<MountCsvCheckPayload, String> {
    crate::logging::log_event("挂载", format!("生成模板：{drama_name}"));
    let result = mount_csv_template_inner(&root, &drama_name);
    crate::logging::log_event(
        "挂载",
        crate::logging::result_text(&result, |payload| csv_check_text(payload, "模板就绪")),
    );
    result
}

/// 挂载校验结果摘要（成功 / 问题数复用给模板 / 保存 / 校验三处日志）。
fn csv_check_text(payload: &MountCsvCheckPayload, prefix: &str) -> String {
    if payload.check.ok {
        format!("{prefix}：{}（校验通过，{} 行）", payload.csv_path, payload.check.rows)
    } else {
        format!(
            "{prefix}：{}（{} 处问题）",
            payload.csv_path,
            payload.check.issues.len()
        )
    }
}

fn mount_csv_template_inner(root: &str, drama_name: &str) -> Result<MountCsvCheckPayload, String> {
    let drama = load_drama(root, drama_name)?;
    let csv_path = PathBuf::from(&drama.dir).join(MOUNT_CSV_NAME);

    if !csv_path.exists() {
        let mut body = String::from("集号,文案\n");
        for episode in &drama.episodes {
            let number = episode
                .episode_number
                .map(|n| n.to_string())
                .unwrap_or_else(|| "0".to_string());
            body.push_str(&format!("{number},\n"));
        }
        std::fs::write(&csv_path, body)
            .map_err(|error| format!("写入挂载文案模板失败（{}）：{error}", csv_path.display()))?;
    }

    let content =
        std::fs::read_to_string(&csv_path).map_err(|error| format!("读取挂载文案失败：{error}"))?;
    let numbers: Vec<u32> = drama
        .episodes
        .iter()
        .filter_map(|e| e.episode_number)
        .collect();
    Ok(MountCsvCheckPayload {
        check: validate_mount_csv(&content, &numbers),
        csv_path: csv_path.display().to_string(),
        content,
    })
}

/// 保存前端编辑后的挂载文案内容并校验。
#[tauri::command]
pub fn mount_csv_save(
    root: String,
    drama_name: String,
    content: String,
) -> Result<MountCsvCheckPayload, String> {
    crate::logging::log_event("挂载", format!("保存文案：{drama_name}（{} 字符）", content.chars().count()));
    let result = mount_csv_save_inner(&root, &drama_name, &content);
    crate::logging::log_event(
        "挂载",
        crate::logging::result_text(&result, |payload| csv_check_text(payload, "文案已保存")),
    );
    result
}

fn mount_csv_save_inner(
    root: &str,
    drama_name: &str,
    content: &str,
) -> Result<MountCsvCheckPayload, String> {
    let drama = load_drama(root, drama_name)?;
    let csv_path = PathBuf::from(&drama.dir).join(MOUNT_CSV_NAME);
    std::fs::write(&csv_path, content)
        .map_err(|error| format!("保存挂载文案失败（{}）：{error}", csv_path.display()))?;
    let numbers: Vec<u32> = drama
        .episodes
        .iter()
        .filter_map(|e| e.episode_number)
        .collect();
    Ok(MountCsvCheckPayload {
        check: validate_mount_csv(content, &numbers),
        csv_path: csv_path.display().to_string(),
        content: content.to_string(),
    })
}

/// 只校验当前剧目目录下的「挂载文案.csv」（文案可能被外部编辑过）。
#[tauri::command]
pub fn mount_csv_check(root: String, drama_name: String) -> Result<MountCsvCheckPayload, String> {
    crate::logging::log_event("挂载", format!("校验文案：{drama_name}"));
    let result = mount_csv_check_inner(&root, &drama_name);
    crate::logging::log_event(
        "挂载",
        crate::logging::result_text(&result, |payload| csv_check_text(payload, "校验完成")),
    );
    result
}

fn mount_csv_check_inner(root: &str, drama_name: &str) -> Result<MountCsvCheckPayload, String> {
    let drama = load_drama(root, drama_name)?;
    let csv_path = PathBuf::from(&drama.dir).join(MOUNT_CSV_NAME);
    if !csv_path.exists() {
        return Err(format!(
            "{} 尚未生成，请先点击「生成模板」。",
            csv_path.display()
        ));
    }
    let content =
        std::fs::read_to_string(&csv_path).map_err(|error| format!("读取挂载文案失败：{error}"))?;
    let numbers: Vec<u32> = drama
        .episodes
        .iter()
        .filter_map(|e| e.episode_number)
        .collect();
    Ok(MountCsvCheckPayload {
        check: validate_mount_csv(&content, &numbers),
        csv_path: csv_path.display().to_string(),
        content,
    })
}

/// 生成流水文件位置（供账单页读取；与 generate_package 写入共用）。
pub(crate) fn ledger_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|dir| dir.join("ledger.jsonl"))
        .map_err(|error| format!("无法定位应用数据目录：{error}。请检查系统权限。"))
}

/// 账单页数据：流水（时间倒序）+ 汇总。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerPayload {
    pub records: Vec<LedgerRecord>,
    /// 本月已生成次数（演示口径：自然月）。
    pub month_count: usize,
    /// 流水文件位置。
    pub path: String,
}

/// 读取生成流水（时间倒序）。本地演示版「还能生成 N 次」无云端配额，界面按无限展示。
#[tauri::command]
pub fn ledger_list(app: AppHandle) -> Result<LedgerPayload, String> {
    let path = ledger_path(&app)?;
    let records = list_records(&path).map_err(|error| format!("读取生成流水失败：{error}"))?;
    let prefix = &now_timestamp()[..7]; // "2026-10"
    let month_count = records.iter().filter(|r| r.ts.starts_with(prefix)).count();
    Ok(LedgerPayload {
        records,
        month_count,
        path: path.display().to_string(),
    })
}

/// 供 `generate_package` 成功后追加流水（时间 / 剧名 / 集数 / 包路径 / 大小）。
pub(crate) fn record_generation(
    app: &AppHandle,
    drama_name: &str,
    episode_count: usize,
    package_dir: &Path,
) -> Result<(), String> {
    let size = dir_size(package_dir);
    let record = LedgerRecord {
        ts: now_timestamp(),
        drama_name: drama_name.to_string(),
        episode_count,
        package_path: package_dir.display().to_string(),
        size_bytes: size,
    };
    append_record(&ledger_path(app)?, &record).map_err(|error| format!("记录生成流水失败：{error}"))
}

/// 目录总大小（忽略无法读取的条目）。
fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(path) else {
        return total;
    };
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_file() {
            total += metadata.len();
        } else if metadata.is_dir() {
            total += dir_size(&entry.path());
        }
    }
    total
}
