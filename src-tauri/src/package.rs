//! 生成报审包（M4a）：本机探测并算哈希 → 服务端生成文档、盖章、签清单 → 本机验签并组装。
//!
//! 视频不上传，只把文件名、大小、SHA-256 和探测规格发给服务端。服务端发回的清单里登记了
//! 这些哈希，组装时逐个重算比对：提交后被改动或调换的分集会被认出来。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use base64::Engine;
use dossier_core::{
    assemble_package, hash_file, resolve_product_dir, AssembleRequest, ChecklistRules,
    CostSheetForm, PackageResult,
};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, State};

use crate::account::commands::{local_error, CommandError};
use crate::account::AccountState;
use crate::toolbox::TaskProgress;
use crate::{analyze_drama_inner, mount};

/// 本机准备好的东西：发给服务端的请求体，和组装时要用的本机文件位置。
struct Prepared {
    drama_name: String,
    drama_dir: PathBuf,
    episode_count: usize,
    body: Value,
    local_sources: HashMap<String, PathBuf>,
}

fn emit_progress(window: &tauri::Window, done: usize, total: usize, label: String) {
    let _ = window.emit(
        "task-progress",
        TaskProgress {
            task: "package".into(),
            done: done as u64,
            total: total as u64,
            label,
        },
    );
}

fn hash_asset(path: &Path) -> Result<(String, u64, String), CommandError> {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| local_error("io", format!("文件名无效：{}", path.display())))?;
    let (size, sha256) = hash_file(path)
        .map_err(|error| local_error("io", format!("读取 {file_name} 失败：{error}")))?;
    Ok((file_name, size, sha256))
}

/// 探测、逐个算哈希、拼请求体。全是同步的磁盘活，在阻塞线程里跑。
#[allow(clippy::too_many_arguments)]
fn prepare(
    window: &tauri::Window,
    root: String,
    name: String,
    copy_episodes: bool,
    ai_generated: bool,
    cost_sheet: Option<CostSheetForm>,
    ffprobe_path: Option<String>,
    rules: Option<ChecklistRules>,
    device_id: &str,
    cost_sheet_scan: Option<String>,
    ownership_scan: Option<String>,
) -> Result<Prepared, CommandError> {
    // 生成前重新探测（避免用过期规格），这一段同样要有进度，否则界面像卡死。
    let analysis = analyze_drama_inner(root, name, ffprobe_path, rules, "generate", |progress| {
        let _ = window.emit("probe-progress", progress);
    })
    .map_err(|message| local_error("invalid_drama", message))?;
    let drama = analysis.drama;

    let mut local_sources = HashMap::new();
    let mut asset = |path: &Path| -> Result<Value, CommandError> {
        let (file_name, size, sha256) = hash_asset(path)?;
        local_sources.insert(file_name.clone(), path.to_path_buf());
        Ok(json!({ "file_name": file_name, "size_bytes": size, "sha256": sha256 }))
    };
    let cover = drama.cover.as_deref().map(&mut asset).transpose()?;
    let synopsis = drama.synopsis.as_deref().map(&mut asset).transpose()?;

    // 探测失败的分集和以前一样不进包（检查结果里已经标出来了）。
    let probed: Vec<_> = drama
        .episodes
        .iter()
        .zip(analysis.probes)
        .filter_map(|(file, probe)| probe.spec.map(|spec| (file, spec)))
        .collect();
    let total = probed.len();
    let mut episodes = Vec::with_capacity(total);
    for (index, (file, spec)) in probed.into_iter().enumerate() {
        emit_progress(
            window,
            index,
            total,
            format!(
                "正在计算分集校验值 {}/{total}：{}",
                index + 1,
                file.file_name
            ),
        );
        let (size, sha256) = hash_file(&file.path)
            .map_err(|error| local_error("io", format!("读取 {} 失败：{error}", file.file_name)))?;
        local_sources.insert(file.file_name.clone(), file.path.clone());
        episodes.push(json!({
            "file_name": file.file_name,
            "episode_number": file.episode_number,
            "size_bytes": size,
            "sha256": sha256,
            "spec": {
                "duration_sec": spec.duration_sec,
                "width": spec.width,
                "height": spec.height,
                "video_codec": spec.video_codec,
                "audio_codec": spec.audio_codec,
                "fps": spec.fps,
                "bitrate_bps": spec.bitrate_bps,
            },
        }));
    }
    emit_progress(
        window,
        total,
        total,
        "校验值计算完成，正在请服务端生成文档".into(),
    );

    // 扫描件签章：给了哪几份就用哪几份，其余仍走占位章（混合签章，用户已确认要支持）。
    // 本机先把格式与大小查清楚，免得传上去才被服务端拒（那会白占一次生成次数）。
    let scans = read_scans(cost_sheet_scan.as_deref(), ownership_scan.as_deref())?;
    let signer = if scans.is_empty() { "mock" } else { "scan" };
    let mut body = json!({
        "device_id": device_id,
        "drama_name": drama.name,
        "ai_generated": ai_generated,
        "copy_episodes": copy_episodes,
        "cost_sheet": cost_sheet.map(|form| json!({
            "company_name": form.company_name,
            "total_investment_wan": form.total_investment_wan,
            "actor_ratio_percent": form.actor_ratio_percent,
            "contact": form.contact,
        })),
        "synopsis": synopsis,
        "cover": cover,
        "episodes": episodes,
        "signer": signer,
    });
    if !scans.is_empty() {
        body["scans"] = serde_json::Value::Object(scans);
    }
    Ok(Prepared {
        drama_name: drama.name,
        drama_dir: drama.dir,
        episode_count: total,
        body,
        local_sources,
    })
}

/// 选一份「已盖好章的扫描件」（成本配置表 / 内容权属声明用）。
///
/// 只允许 PNG / JPEG / PDF——和服务端的 `scan_to_pdf` 支持的格式一致，
/// 也是本机 `read_scans` 会先查一遍的那三种。在文件选择器这一层就挡掉别的格式，
/// 比让用户选完再报错省事。
#[tauri::command]
pub async fn pick_scan_file() -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        rfd::FileDialog::new()
            .set_title("选择已盖好章的扫描件（PNG / JPEG / PDF，不超过 5 MB）")
            .add_filter("扫描件", &["png", "jpg", "jpeg", "pdf"])
            .pick_file()
            .map(|path| path.display().to_string())
    })
    .await
    .map_err(|error| format!("文件选择任务失败：{error}"))
}

/// 要盖章的两份文档在报审包里的路径（必须和服务端 `render_documents` 给出的 `sealed` 一致，
/// 否则服务端会以「不需要盖章的文档」拒绝扫描件）。
const COST_SHEET_PDF: &str = "04_成本配置表/成本配置表.pdf";
const OWNERSHIP_PDF: &str = "05_权属声明/内容权属声明.pdf";
/// 单份扫描件的本机上限：服务端 base64 字段上限约 5.24 MB 原文，留一点余量。
const MAX_SCAN_BYTES: u64 = 5 * 1024 * 1024;

/// 读扫描件并编码成 base64（按服务端要求的「文档路径 → base64」）。
///
/// 本机先查格式与大小：只认 PNG / JPEG / PDF，超过 5 MB 直接拒绝并说清原因。
/// 一份都没给时返回空表（`signer` 保持 `mock`）。
fn read_scans(
    cost_sheet: Option<&str>,
    ownership: Option<&str>,
) -> Result<serde_json::Map<String, serde_json::Value>, CommandError> {
    let mut scans = serde_json::Map::new();
    for (label, path, document) in [
        ("成本配置表", cost_sheet, COST_SHEET_PDF),
        ("内容权属声明", ownership, OWNERSHIP_PDF),
    ] {
        let Some(path) = path else { continue };
        let path = Path::new(path);
        if !path.is_file() {
            return Err(local_error(
                "scan_invalid",
                format!("{label}的扫描件不存在：{}", path.display()),
            ));
        }
        let extension = path
            .extension()
            .map(|value| value.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if !matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "pdf") {
            return Err(local_error(
                "scan_invalid",
                format!("{label}的扫描件只能是 PNG、JPEG 或 PDF，实际是 .{extension}"),
            ));
        }
        let metadata = std::fs::metadata(path).map_err(|error| {
            local_error(
                "io",
                format!("读取{label}的扫描件失败（{}）：{error}", path.display()),
            )
        })?;
        if metadata.len() > MAX_SCAN_BYTES {
            return Err(local_error(
                "scan_invalid",
                format!(
                    "{label}的扫描件太大（{:.1} MB），上限是 5 MB；请压缩后再选",
                    metadata.len() as f64 / (1024.0 * 1024.0)
                ),
            ));
        }
        let bytes = std::fs::read(path).map_err(|error| {
            local_error(
                "io",
                format!("读取{label}的扫描件失败（{}）：{error}", path.display()),
            )
        })?;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        scans.insert(document.to_string(), json!(encoded));
    }
    Ok(scans)
}

fn decode_files(
    files: Vec<crate::account::api::PackageFileView>,
) -> Result<HashMap<String, Vec<u8>>, CommandError> {
    let mut decoded = HashMap::with_capacity(files.len());
    for file in files {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(file.content.as_bytes())
            .map_err(|_| {
                local_error(
                    "protocol",
                    format!("服务端发回的 {} 不是合法的 base64", file.path),
                )
            })?;
        if decoded.insert(file.path.clone(), bytes).is_some() {
            return Err(local_error(
                "protocol",
                format!("服务端发回了两个 {}", file.path),
            ));
        }
    }
    Ok(decoded)
}

fn join_error(error: tauri::Error) -> CommandError {
    local_error("internal", format!("后台任务失败：{error}"))
}

#[tauri::command]
// 参数名即前端 IPC 协议字段（camelCase 映射），合并成结构体会破坏现有调用方。
#[allow(clippy::too_many_arguments)]
pub async fn generate_package(
    window: tauri::Window,
    app: AppHandle,
    account: State<'_, AccountState>,
    root: String,
    name: String,
    output_dir: Option<String>,
    output_root: Option<String>,
    copy_episodes: bool,
    ai_generated: bool,
    cost_sheet: Option<CostSheetForm>,
    ffprobe_path: Option<String>,
    rules: Option<ChecklistRules>,
    cost_sheet_scan: Option<String>,
    ownership_scan: Option<String>,
) -> Result<PackageResult, CommandError> {
    let (device_id, keys) = account.signing_context()?;

    let prepared = {
        let window = window.clone();
        let device_id = device_id.clone();
        tauri::async_runtime::spawn_blocking(move || {
            prepare(
                &window,
                root,
                name,
                copy_episodes,
                ai_generated,
                cost_sheet,
                ffprobe_path,
                rules,
                &device_id,
                cost_sheet_scan,
                ownership_scan,
            )
        })
        .await
        .map_err(join_error)??
    };

    // 落点三层：本次覆盖 → 设置里的「默认输出目录」（产物根目录）→ 剧目目录内。
    let package_dir = resolve_product_dir(
        output_dir.as_deref().map(Path::new),
        output_root.as_deref().map(Path::new),
        &prepared.drama_dir,
        &prepared.drama_name,
        "报审包",
    );
    // 先在本机挡掉「目录已存在」：免得白白占用一次服务端的生成次数。组装时还会再查。
    if package_dir.exists() {
        return Err(local_error(
            "package_already_exists",
            format!(
                "报审包目录已存在：{}。请选择新的输出目录或先移除旧目录。",
                package_dir.display()
            ),
        ));
    }

    let response = account.create_package(prepared.body).await?;
    let server_files = decode_files(response.files)?;

    let result = {
        let window = window.clone();
        let package_dir = package_dir.clone();
        let local_sources = prepared.local_sources;
        tauri::async_runtime::spawn_blocking(move || {
            let request = AssembleRequest {
                manifest: response.manifest.as_bytes(),
                signature: response.signature.as_bytes(),
                server_files: &server_files,
                local_sources: &local_sources,
                output_dir: &package_dir,
                trusted: &keys,
                device_id: &device_id,
            };
            assemble_package(&request, |done, total, label| {
                emit_progress(&window, done, total, label.to_owned());
            })
            .map_err(|error| local_error(&format!("package_{}", error.code()), error.to_string()))
        })
        .await
        .map_err(join_error)??
    };

    // 成功后记入本机生成流水（设置-账单页展示；失败不影响生成结果本身）。
    mount::record_generation(
        &app,
        &prepared.drama_name,
        prepared.episode_count,
        &result.package_dir,
    )
    .unwrap_or_else(|error| eprintln!("{error}"));
    Ok(result)
}
