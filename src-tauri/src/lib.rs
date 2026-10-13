use std::path::PathBuf;

use dossier_core::{
    generate_report_package, probe_episode, run_checks, scan_drama_dir, ChecklistReport,
    ChecklistRules, CostSheetForm, Drama, EpisodeSpec, FfprobeConfig, PackageOptions,
    PackageResult, ProbedEpisode,
};
use serde::Serialize;

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
fn analyze_drama(
    root: String,
    name: String,
    ffprobe_path: Option<String>,
) -> Result<DramaAnalysis, String> {
    let root = PathBuf::from(root);
    let drama = scan_drama_dir(&name, &root.join(&name));
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

    let checklist = run_checks(&drama, &specs, &ChecklistRules::default());
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
) -> Result<PackageResult, String> {
    let root = PathBuf::from(root);
    let analysis = analyze_drama(root.display().to_string(), name, ffprobe_path)?;
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
            analyze_drama,
            generate_package
        ])
        .run(tauri::generate_context!())
        .expect("failed to run DramaDossier");
}
