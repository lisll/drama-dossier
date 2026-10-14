use std::path::PathBuf;

use dossier_core::{
    generate_report_package, load_settings, probe_episode, run_checks, save_settings,
    scan_drama_dir, validate_drama_dir, validate_drama_name, validate_library_root, AppSettings,
    ChecklistReport, ChecklistRules, CostSheetForm, Drama, EpisodeSpec, FfprobeConfig,
    LoadedSettings, PackageOptions, PackageResult, ProbedEpisode,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

mod dossier;
mod erase;
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
    root: String,
    name: String,
    output_dir: Option<String>,
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

    let opts = PackageOptions {
        output_dir: output_dir.map(PathBuf::from),
        copy_episodes,
        cost_sheet,
        ai_generated,
    };

    generate_report_package(
        &drama.name,
        &drama.dir,
        drama.cover.as_deref(),
        drama.synopsis.as_deref(),
        &episodes,
        &opts,
    )
    .map_err(|error| error.to_string())
}

/// 在系统文件管理器里打开目录（或文件所在目录），供界面「打开目录」按钮调用。
#[tauri::command]
fn open_path(path: String) -> Result<(), String> {
    paths::open_in_file_manager(std::path::Path::new(&path))
}

pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            pick_folder,
            scan_library,
            load_app_settings,
            save_app_settings,
            analyze_drama,
            generate_package,
            open_path,
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
            toolbox::install_runtime
        ])
        .run(tauri::generate_context!())
        .expect("failed to run DramaDossier");
}
