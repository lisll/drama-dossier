use std::path::PathBuf;

use dossier_core::{
    generate_report_package, load_settings, probe_episode, run_checks, save_settings,
    scan_drama_dir, validate_drama_dir, validate_drama_name, validate_library_root, AppSettings,
    ChecklistReport, ChecklistRules, CostSheetForm, Drama, EpisodeSpec, FfprobeConfig,
    LoadedSettings, PackageOptions, PackageResult, ProbedEpisode,
};
use serde::Serialize;
use tauri::{AppHandle, Manager};

#[derive(Debug, Serialize)]
struct ProbeOutcome {
    file_name: String,
    episode_number: Option<u32>,
    spec: Option<EpisodeSpec>,
    error: Option<String>,
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
fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
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

#[tauri::command]
fn analyze_drama(
    root: String,
    name: String,
    ffprobe_path: Option<String>,
    rules: Option<ChecklistRules>,
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

    for episode in &drama.episodes {
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
fn generate_package(
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
    let analysis = analyze_drama(root.display().to_string(), name, ffprobe_path, rules)?;
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

pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            pick_folder,
            scan_library,
            load_app_settings,
            save_app_settings,
            analyze_drama,
            generate_package
        ])
        .run(tauri::generate_context!())
        .expect("failed to run DramaDossier");
}
