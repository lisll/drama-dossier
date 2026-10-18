use std::path::PathBuf;

use dossier_core::{
    load_settings, probe_episode, run_checks, save_settings, scan_drama_dir, validate_drama_dir,
    validate_drama_name, validate_library_root, AppSettings, ChecklistReport, ChecklistRules,
    Drama, EpisodeSpec, FfprobeConfig, LoadedSettings,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

mod account;
mod dossier;
mod encode;
mod erase;
mod erase_batch;
mod jianying;
mod jobs;
#[cfg(test)]
mod jobs_tests;
mod mount;
mod package;
mod paths;
mod toolbox;
mod version;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProbeOutcome {
    file_name: String,
    episode_number: Option<u32>,
    pub(crate) spec: Option<EpisodeSpec>,
    error: Option<String>,
}

/// 逐集探测进度事件（`probe-progress`）的负载：界面据此显示「已探测 n/N 集」。
///
/// `task` 区分调用来源（`analyze` 探测并检查 / `generate` 生成报审包），
/// 因为两者都会重新探测，界面需要知道进度条该挂在哪个模块上。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProbeProgress {
    task: String,
    done: usize,
    total: usize,
    file_name: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct DramaAnalysis {
    pub(crate) drama: Drama,
    pub(crate) probes: Vec<ProbeOutcome>,
    checklist: ChecklistReport,
}

/// 设置读取结果 + 文件位置，便于界面展示与手动编辑。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsPayload {
    #[serde(flatten)]
    loaded: LoadedSettings,
    path: String,
}

/// 设置文件位置：应用配置目录下的 `settings.json`。
pub(crate) fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|dir| dir.join("settings.json"))
        .map_err(|error| format!("无法定位应用配置目录：{error}。请检查系统权限。"))
}

#[tauri::command]
async fn pick_folder() -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        rfd::FileDialog::new()
            .set_title("选择剧集素材根目录")
            .pick_folder()
            .map(|path| path.display().to_string())
    })
    .await
    .map_err(|error| format!("文件夹选择任务失败：{error}"))
}

#[tauri::command]
fn scan_library(root: String) -> Result<Vec<Drama>, String> {
    let root = PathBuf::from(root);
    dossier_core::scan_library(&root).map_err(|error| error.to_string())
}

#[tauri::command]
fn load_app_settings(app: AppHandle) -> Result<SettingsPayload, String> {
    let path = settings_path(&app)?;
    let loaded = load_settings(&path);
    Ok(SettingsPayload {
        loaded,
        path: path.display().to_string(),
    })
}

#[tauri::command]
fn save_app_settings(app: AppHandle, settings: AppSettings) -> Result<String, String> {
    if let Some(rules) = &settings.rules {
        rules.validate()?;
    }
    let path = settings_path(&app)?;
    save_settings(&path, &settings)
        .map_err(|error| format!("保存设置失败（{}）：{error}", path.display()))?;
    Ok(path.display().to_string())
}

/// 探测并检查的实现体。
///
/// 与 IPC 命令分开，是为了让「生成报审包」这类内部调用也能走同一份探测逻辑，
/// 同时把进度回调留给调用方决定要不要发事件（发出去了界面才画进度条）。
pub(crate) fn analyze_drama_inner(
    root: String,
    name: String,
    ffprobe_path: Option<String>,
    rules: Option<ChecklistRules>,
    task: &str,
    mut on_progress: impl FnMut(ProbeProgress),
) -> Result<DramaAnalysis, String> {
    let root = PathBuf::from(root);
    validate_library_root(&root).map_err(|error| error.to_string())?;
    validate_drama_name(&name).map_err(|error| error.to_string())?;
    let drama_dir = root.join(&name);
    validate_drama_dir(&drama_dir).map_err(|error| error.to_string())?;
    let drama = scan_drama_dir(&name, &drama_dir);
    let cfg = FfprobeConfig {
        bin: ffprobe_path.map(PathBuf::from),
    };
    let mut probes = Vec::with_capacity(drama.episodes.len());
    let mut specs = Vec::with_capacity(drama.episodes.len());
    let total = drama.episodes.len();

    for (index, episode) in drama.episodes.iter().enumerate() {
        match probe_episode(episode, &cfg) {
            Ok(probed) => {
                specs.push(Some(probed.spec.clone()));
                probes.push(ProbeOutcome {
                    file_name: episode.file_name.clone(),
                    episode_number: episode.episode_number,
                    spec: Some(probed.spec),
                    error: None,
                });
            }
            Err(error) => {
                specs.push(None);
                probes.push(ProbeOutcome {
                    file_name: episode.file_name.clone(),
                    episode_number: episode.episode_number,
                    spec: None,
                    error: Some(error.to_string()),
                });
            }
        }
        // 每探完一集就报一次进度：集数多的素材可能要跑几十秒，界面不能干等着。
        on_progress(ProbeProgress {
            task: task.to_string(),
            done: index + 1,
            total,
            file_name: episode.file_name.clone(),
        });
    }

    let rules = rules.unwrap_or_default();
    rules.validate()?;
    let checklist = run_checks(&drama, &specs, &rules);
    Ok(DramaAnalysis {
        drama,
        probes,
        checklist,
    })
}

#[tauri::command]
fn analyze_drama(
    window: tauri::Window,
    root: String,
    name: String,
    ffprobe_path: Option<String>,
    rules: Option<ChecklistRules>,
) -> Result<DramaAnalysis, String> {
    analyze_drama_inner(root, name, ffprobe_path, rules, "analyze", |progress| {
        let _ = window.emit("probe-progress", progress);
    })
}

/// 在系统文件管理器里打开目录（或文件所在目录），供界面「打开目录」按钮调用。
#[tauri::command]
fn open_path(path: String) -> Result<(), String> {
    paths::open_in_file_manager(std::path::Path::new(&path))
}

pub fn run() {
    let app = tauri::Builder::default()
        .setup(|app| {
            account::init(app.handle());
            // 任务队列：读回上次没做完的任务（显示为已暂停），启动唯一的工作线程。
            let queue = version::start_queue(app.handle())?;
            app.manage(queue);
            Ok(())
        })
        // 闸门套在所有应用命令外面：本地功能要有使用权，生成报审包还要联网（见 account/gate.rs）。
        .invoke_handler(account::gate(tauri::generate_handler![
            pick_folder,
            scan_library,
            load_app_settings,
            save_app_settings,
            analyze_drama,
            package::generate_package,
            package::pick_scan_file,
            open_path,
            account::commands::account_status,
            account::commands::credits_overview,
            account::commands::credit_history,
            account::commands::recharge_tiers,
            account::commands::create_recharge_order,
            account::commands::recharge_order_status,
            account::commands::reconcile_recharge,
            account::commands::invite_status,
            // 服务端处理（M5b-8）：分片上传、建任务、查状态、取消、取产物
            account::server_jobs::server_upload_start,
            account::server_jobs::server_upload_status,
            account::server_jobs::server_upload_chunks,
            account::server_jobs::server_upload_complete,
            account::server_jobs::server_upload_abort,
            account::server_jobs::server_submit_task,
            account::server_jobs::server_task,
            account::server_jobs::server_cancel_task,
            account::server_jobs::server_artifact,
            account::commands::account_retry,
            account::commands::account_send_code,
            account::commands::account_register,
            account::commands::account_login,
            account::commands::account_logout,
            account::commands::account_reset_password,
            account::commands::account_change_password,
            account::commands::license_redeem,
            account::commands::license_refresh,
            account::commands::device_list,
            account::commands::device_unbind,
            account::commands::device_activate,
            account::commands::account_export,
            account::commands::account_delete,
            jianying::jianying_detect,
            mount::mount_plan_save,
            mount::mount_plan_load,
            mount::mount_csv_template,
            mount::mount_csv_save,
            mount::mount_csv_check,
            mount::ledger_list,
            erase::pick_video,
            erase::erase_status,
            erase::plan_erase,
            erase::run_erase,
            dossier::transcode_advice,
            dossier::diff_report_packages,
            dossier::platform_profiles,
            dossier::platform_review,
            dossier::export_draft,
            dossier::import_draft,
            toolbox::runtime_status,
            toolbox::list_rename_plan,
            toolbox::apply_rename,
            toolbox::probe_bitrate,
            toolbox::fix_bitrate,
            toolbox::transcode_execute,
            toolbox::install_runtime,
            dossier::generate_mount_copy_csv,
            dossier::load_mount_copy,
            dossier::save_mount_copy,
            dossier::transition_mount_copy_row,
            dossier::preview_mount_copy_publish,
            version::rights_status,
            version::rights_declare,
            version::pick_rights_file,
            version::pick_version_asset,
            version::version_template_load,
            version::version_template_save,
            version::version_plan,
            version::version_start,
            jobs::job_list,
            jobs::job_pause,
            jobs::job_resume,
            jobs::job_terminate,
            erase_batch::video_frame,
            erase_batch::detect_still_regions,
            erase_batch::erase_preview,
            erase_batch::erase_batch_start
        ]))
        .build(tauri::generate_context!())
        .expect("failed to build DramaDossier");
    app.run(|handle, event| {
        // 退出前把「见过的最晚时间」写回钥匙串(平时最多每分钟写一次)。
        if let tauri::RunEvent::Exit = event {
            handle.state::<account::AccountState>().flush();
        }
    });
}

#[cfg(test)]
mod probe_outcome_tests {
    use super::*;

    /// 界面的「分集」表格读的是 `fileName` / `episodeNumber`。
    ///
    /// 这里钉的是一个真实踩过的坑:`ProbeOutcome` 少写 `#[serde(rename_all = "camelCase")]`
    /// 时序列化成 `file_name` / `episode_number`,前端读不到,表格里「集号」和「文件」
    /// 两列会一直是空的(而同一份数据里的 `spec` 是好的,所以只看时长、分辨率看不出问题)。
    #[test]
    fn probe_outcome_serializes_to_camel_case_keys() {
        let outcome = ProbeOutcome {
            file_name: "第01集.mp4".into(),
            episode_number: Some(1),
            spec: None,
            error: Some("探测失败".into()),
        };
        let value = serde_json::to_value(&outcome).expect("能序列化");
        let object = value.as_object().expect("是个对象");
        assert_eq!(
            object.get("fileName").and_then(|v| v.as_str()),
            Some("第01集.mp4"),
            "前端读的是 fileName,实际键名:{:?}",
            object.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            object.get("episodeNumber").and_then(|v| v.as_u64()),
            Some(1)
        );
        assert!(
            !object.contains_key("file_name") && !object.contains_key("episode_number"),
            "不该同时出现下划线写法:{:?}",
            object.keys().collect::<Vec<_>>()
        );
    }
}
