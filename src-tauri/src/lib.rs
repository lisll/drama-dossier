use std::path::{Path, PathBuf};

use dossier_core::{
    generate_report_package, load_settings, probe_episode, resolve_product_dir, run_checks,
    save_settings, scan_drama_dir, validate_drama_dir, validate_drama_name, validate_library_root,
    AppSettings, ChecklistReport, ChecklistRules, CostSheetForm, Drama, EpisodeSpec, FfprobeConfig,
    LoadedSettings, PackageOptions, PackageResult, ProbedEpisode,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

mod account;
mod dossier;
mod erase;
mod jianying;
mod mount;
mod paths;
mod toolbox;

#[derive(Debug, Serialize)]
struct ProbeOutcome {
    file_name: String,
    episode_number: Option<u32>,
    spec: Option<EpisodeSpec>,
    error: Option<String>,
}

/// 逐集探测进度事件（`probe-progress`）的负载：界面据此显示「已探测 n/N 集」。
///
/// `task` 区分调用来源（`analyze` 探测并检查 / `generate` 生成报审包），
/// 因为两者都会重新探测，界面需要知道进度条该挂在哪个模块上。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProbeProgress {
    task: String,
    done: usize,
    total: usize,
    file_name: String,
}

#[derive(Debug, Serialize)]
struct DramaAnalysis {
    drama: Drama,
    probes: Vec<ProbeOutcome>,
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
fn analyze_drama_inner(
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

#[tauri::command]
// 参数名即前端 IPC 协议字段（camelCase 映射），合并成结构体会破坏现有调用方；
// 与 dossier-inpaint/src/pipeline.rs 的既有做法一致，这里显式豁免参数数量检查。
#[allow(clippy::too_many_arguments)]
fn generate_package(
    window: tauri::Window,
    app: AppHandle,
    root: String,
    name: String,
    output_dir: Option<String>,
    output_root: Option<String>,
    copy_episodes: bool,
    ai_generated: bool,
    cost_sheet: Option<CostSheetForm>,
    ffprobe_path: Option<String>,
    rules: Option<ChecklistRules>,
) -> Result<PackageResult, String> {
    let root = PathBuf::from(root);
    // 生成前会重新探测（避免用过期规格），这一段同样要有进度，否则界面像卡死。
    let analysis = analyze_drama_inner(
        root.display().to_string(),
        name,
        ffprobe_path,
        rules,
        "generate",
        |progress| {
            let _ = window.emit("probe-progress", progress);
        },
    )?;
    let drama = analysis.drama;
    let episodes: Vec<ProbedEpisode> = drama
        .episodes
        .iter()
        .cloned()
        .zip(analysis.probes)
        .filter_map(|(file, probe)| probe.spec.map(|spec| ProbedEpisode { file, spec }))
        .collect();

    // 落点三层：本次覆盖 → 设置里的「默认输出目录」（产物根目录）→ 剧目目录内。
    // 规则与单测都在 `dossier_core::resolve_product_dir`，这一层只做字符串到路径的转换。
    let package_dir = resolve_product_dir(
        output_dir.as_deref().map(Path::new),
        output_root.as_deref().map(Path::new),
        &drama.dir,
        &drama.name,
        "报审包",
    );

    let opts = PackageOptions {
        output_dir: Some(package_dir),
        copy_episodes,
        cost_sheet,
        ai_generated,
    };

    let result = generate_report_package(
        &drama.name,
        &drama.dir,
        drama.cover.as_deref(),
        drama.synopsis.as_deref(),
        &episodes,
        &opts,
    )
    .map_err(|error| error.to_string())?;

    // 成功后记入本机生成流水（设置-账单页展示；失败不影响生成结果本身）。
    mount::record_generation(
        &app,
        &drama.name,
        episodes.len(),
        Path::new(&result.package_dir),
    )
    .unwrap_or_else(|error| eprintln!("{error}"));
    Ok(result)
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
            Ok(())
        })
        // 闸门套在所有应用命令外面：本地功能要有使用权，生成报审包还要联网（见 account/gate.rs）。
        .invoke_handler(account::gate(tauri::generate_handler![
            pick_folder,
            scan_library,
            load_app_settings,
            save_app_settings,
            analyze_drama,
            generate_package,
            open_path,
            account::commands::account_status,
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
            dossier::preview_mount_copy_publish
        ]))
        .build(tauri::generate_context!())
        .expect("failed to build DramaDossier");
    app.run(|handle, event| {
        // 退出前把「见过的最晚时间」写回钥匙串（平时最多每分钟写一次）。
        if let tauri::RunEvent::Exit = event {
            handle.state::<account::AccountState>().flush();
        }
    });
}
