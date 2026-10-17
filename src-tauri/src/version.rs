//! 处理版本（M4b-1，计划 2.1–2.5 节）：权属声明、模板、计划预览、开始生成，以及队列执行器。
//!
//! 数据流：
//! 1. 门槛：权属材料已声明且文件没变；这部剧还没有处理版本目录。
//! 2. 算任务哈希（分集文件 + 模板 + 权属材料 + 档位），向服务端申请配方，本机验签。
//! 3. 消费配方（服务端记账，只能一次），配方原文存进任务目录。
//! 4. 交给任务队列逐集处理：一次 ffmpeg 调用完成两层处理、编码、画质检测。
//! 5. 全部完成后写 `版本说明.json`、封面和台账 `processing.jsonl`。
//!
//! 先消费再处理：配方是一次性的，处理失败要重新申请（服务端有限流）。续跑不再找服务端，
//! 用本机保存的原文重新验签名、机器码和任务哈希（`verify_consumed`），参数和种子不变，
//! 反复续跑产出的文件和第一次完全相同。
//!
//! 安全边界：分集文件在申请配方时算过哈希；执行器每集开工前重算一次，文件被换过就拒绝，
//! 否则「申请时用 A、处理时换成 B」就能绕过配方绑定。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dossier_core::version::{
    build_cover_args, build_version_graph, fill_stats_paths, ClipInput, EpisodeInput,
    EpisodeReport, GraphExtras, OutputSpec, ProcessingRecord, QualityFloor, QualityResult,
    RightsDeclaration, SubtleParams, VersionReport, VersionTemplate,
};
use dossier_core::{append_jsonl, hash_file, now_timestamp, resolve_product_dir};
use dossier_license::recipe::Recipe;
use dossier_license::version_task::{
    template_hash, verify_consumed, version_task_hash, EpisodeInput as TaskEpisode, VersionTask,
};
use dossier_license::TrustedKey;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, State};

use crate::account::commands::{local_error, CommandError};
use crate::account::AccountState;
use crate::encode::{
    encode_with_fallback, prepare_encoders, CancelFlag, EncodeRequest, EncodeRunError, EncoderPlans,
};
use crate::jobs::{
    read_json, write_json_atomic, JobQueue, JobRecord, JobRunner, JobView, NewJob, PreparedJob,
    StepError, StepOutcome,
};

/// 配方里只有一套通用规格（计划 2.1a 节）。
const PLATFORM: &str = "universal";
/// 剧目目录下的模板文件。
const TEMPLATE_FILE: &str = "版本模板.json";
/// 剧目目录下的权属声明（隐藏目录，扫描分集时会跳过）。
const RIGHTS_FILE: &str = ".dossier/rights.json";
/// 产物目录名（`<产物根>/<剧名>/处理版本`）。
const OUTPUT_LEAF: &str = "处理版本";
/// 任务目录里的配方原文。
const RECIPE_FILE: &str = "recipe.token";
/// 任务目录里每集的结果（收尾时汇总进版本说明）。
const RESULTS_DIR: &str = "results";
pub(crate) const JOB_KIND: &str = "version";

/// 处理台账：和生成流水 `ledger.jsonl` 同目录。
fn processing_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|dir| dir.join("processing.jsonl"))
        .map_err(|error| format!("无法定位应用数据目录：{error}。请检查系统权限。"))
}

/// 任务队列根目录。
pub(crate) fn jobs_root(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|dir| dir.join("jobs"))
        .map_err(|error| format!("无法定位应用数据目录：{error}。请检查系统权限。"))
}

fn io_error(message: impl Into<String>) -> CommandError {
    local_error("io", message)
}

fn invalid(message: impl Into<String>) -> CommandError {
    local_error("version_invalid", message)
}

fn join_error(error: impl std::fmt::Display) -> CommandError {
    io_error(format!("后台任务异常结束：{error}"))
}

/// 剧目目录（复用报审包的校验：根目录、剧名、目录本身）。
fn drama_dir(root: &str, name: &str) -> Result<dossier_core::Drama, CommandError> {
    crate::dossier::load_drama(root, name).map_err(|message| local_error("invalid_drama", message))
}

// ---------------------------------------------------------------------------
// 权属材料声明
// ---------------------------------------------------------------------------

/// 权属声明的状态：界面据此决定显示声明区还是模板区。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RightsStatus {
    pub declared: Option<RightsDeclaration>,
    /// 声明过，但文件已经不在或内容变了：要重新声明。
    pub changed: bool,
    pub message: Option<String>,
}

fn read_rights(drama_dir: &Path) -> Option<RightsDeclaration> {
    read_json(&drama_dir.join(RIGHTS_FILE)).ok()
}

/// 核对声明：重算文件哈希，变了返回错误说明。
fn check_rights(declaration: &RightsDeclaration) -> Result<(), String> {
    let path = Path::new(&declaration.file);
    match hash_file(path) {
        Ok((_, sha256)) if sha256 == declaration.sha256 => Ok(()),
        Ok(_) => Err("权属材料文件的内容变了，请重新声明".into()),
        Err(error) => Err(format!(
            "权属材料文件读不到了（{}：{error}），请重新声明",
            path.display()
        )),
    }
}

#[tauri::command]
pub async fn rights_status(root: String, name: String) -> Result<RightsStatus, CommandError> {
    tauri::async_runtime::spawn_blocking(move || {
        let drama = drama_dir(&root, &name)?;
        Ok(match read_rights(&drama.dir) {
            None => RightsStatus {
                declared: None,
                changed: false,
                message: None,
            },
            Some(declaration) => {
                let check = check_rights(&declaration);
                RightsStatus {
                    changed: check.is_err(),
                    message: check.err(),
                    declared: Some(declaration),
                }
            }
        })
    })
    .await
    .map_err(join_error)?
}

/// 声明权属材料：选一个文件（版权登记证书或授权书的扫描件），记下路径和 SHA-256。
/// 程序不核验真伪：这道门槛只留记录、划清责任（总方案 2.2 节）。
#[tauri::command]
pub async fn rights_declare(
    app: AppHandle,
    root: String,
    name: String,
    file: String,
) -> Result<RightsStatus, CommandError> {
    tauri::async_runtime::spawn_blocking(move || {
        let drama = drama_dir(&root, &name)?;
        let path = PathBuf::from(&file);
        if !path.is_file() {
            return Err(invalid("请选择一个权属材料文件"));
        }
        let (_, sha256) =
            hash_file(&path).map_err(|error| io_error(format!("读取权属材料失败：{error}")))?;
        let declaration = RightsDeclaration {
            declared_at: now_timestamp(),
            file: path.display().to_string(),
            sha256: sha256.clone(),
        };
        let target = drama.dir.join(RIGHTS_FILE);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| io_error(format!("创建 .dossier 目录失败：{error}")))?;
        }
        write_json_atomic(&target, &declaration).map_err(io_error)?;
        record(
            &app,
            ProcessingRecord {
                ts: declaration.declared_at.clone(),
                event: "rights".into(),
                task: sha256,
                strength: None,
                seed: None,
                quality: None,
                output: None,
                sha256: None,
            },
        )?;
        Ok(RightsStatus {
            declared: Some(declaration),
            changed: false,
            message: None,
        })
    })
    .await
    .map_err(join_error)?
}

/// 选权属材料文件。
#[tauri::command]
pub async fn pick_rights_file() -> Result<Option<String>, String> {
    pick_file(
        "选择权属材料（版权登记证书或授权书扫描件）",
        &[("文件", &["pdf", "jpg", "jpeg", "png"])],
    )
    .await
}

/// 选片头片尾视频、背景音乐、字体文件（`kind`: video / audio / font）。
#[tauri::command]
pub async fn pick_version_asset(kind: String) -> Result<Option<String>, String> {
    match kind.as_str() {
        "video" => {
            pick_file(
                "选择片头或片尾视频",
                &[("视频", &["mp4", "mov", "mkv", "m4v", "webm"])],
            )
            .await
        }
        "audio" => {
            pick_file(
                "选择背景音乐（需要有使用授权）",
                &[("音频", &["mp3", "m4a", "aac", "wav", "flac"])],
            )
            .await
        }
        "font" => pick_file("选择中文字体文件", &[("字体", &["ttf", "otf", "ttc"])]).await,
        other => Err(format!("不认识的文件类型：{other}")),
    }
}

async fn pick_file(
    title: &'static str,
    filters: &'static [(&'static str, &'static [&'static str])],
) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut dialog = rfd::FileDialog::new().set_title(title);
        for (label, extensions) in filters {
            dialog = dialog.add_filter(*label, extensions);
        }
        dialog.pick_file().map(|path| path.display().to_string())
    })
    .await
    .map_err(|error| format!("文件选择任务失败：{error}"))
}

/// 追加一条处理台账。台账写不进去不能假装成功：权属声明、版本生成都依赖它划清责任。
fn record(app: &AppHandle, entry: ProcessingRecord) -> Result<(), CommandError> {
    let path = processing_path(app).map_err(io_error)?;
    append_jsonl(&path, &entry).map_err(|error| io_error(format!("写处理台账失败：{error}")))
}

// ---------------------------------------------------------------------------
// 模板
// ---------------------------------------------------------------------------

fn load_template(drama_dir: &Path) -> Result<VersionTemplate, CommandError> {
    let path = drama_dir.join(TEMPLATE_FILE);
    if !path.exists() {
        return Ok(VersionTemplate::default());
    }
    // 文件坏了要报出来：静默换成默认模板，生成出来的版本就和用户以为的不一样了。
    read_json(&path).map_err(|message| invalid(format!("{TEMPLATE_FILE} 无法读取：{message}")))
}

#[tauri::command]
pub async fn version_template_load(
    root: String,
    name: String,
) -> Result<VersionTemplate, CommandError> {
    tauri::async_runtime::spawn_blocking(move || load_template(&drama_dir(&root, &name)?.dir))
        .await
        .map_err(join_error)?
}

#[tauri::command]
pub async fn version_template_save(
    root: String,
    name: String,
    template: VersionTemplate,
) -> Result<VersionTemplate, CommandError> {
    tauri::async_runtime::spawn_blocking(move || {
        let drama = drama_dir(&root, &name)?;
        template
            .validate()
            .map_err(|error| invalid(error.to_string()))?;
        write_json_atomic(&drama.dir.join(TEMPLATE_FILE), &template).map_err(io_error)?;
        Ok(template)
    })
    .await
    .map_err(join_error)?
}

// ---------------------------------------------------------------------------
// 中文字体
// ---------------------------------------------------------------------------

/// 各系统自带的中文字体（按优先级）。macOS 苹方、Windows 微软雅黑，Linux 是常见发行版的 Noto CJK。
const SYSTEM_FONTS: &[&str] = &[
    "/System/Library/Fonts/PingFang.ttc",
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    "/System/Library/Fonts/STHeiti Medium.ttc",
    "C:\\Windows\\Fonts\\msyh.ttc",
    "C:\\Windows\\Fonts\\msyh.ttf",
    "C:\\Windows\\Fonts\\simhei.ttf",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
];

/// 找中文字体：设置里指定的优先（指定了却找不到就报错，不偷偷换别的字体）；
/// 没指定就找系统字体；都没有返回 `None`，由调用方决定要不要报错。
fn resolve_font(settings: &dossier_core::AppSettings) -> Result<Option<String>, String> {
    if let Some(path) = &settings.font_path {
        if path.is_file() {
            return Ok(Some(path.display().to_string()));
        }
        return Err(format!(
            "设置里指定的字体文件不存在：{}，请在设置里重新选择",
            path.display()
        ));
    }
    Ok(SYSTEM_FONTS
        .iter()
        .map(Path::new)
        .find(|path| path.is_file())
        .map(|path| path.display().to_string()))
}

// ---------------------------------------------------------------------------
// 计划：门槛检查 + 任务哈希（不找服务端、不扣次数）
// ---------------------------------------------------------------------------

/// 一集源文件：任务哈希的输入，也是执行器开工前复核的对象。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceEpisode {
    pub name: String,
    pub path: String,
    pub sha256: String,
}

/// 写进 `job.json` 的任务参数：执行器需要的全部输入。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionJobSpec {
    drama_name: String,
    task: String,
    strength: String,
    template: VersionTemplate,
    template_sha256: String,
    rights_sha256: String,
    music_sha256: Option<String>,
    episodes: Vec<SourceEpisode>,
    output_dir: String,
}

/// `version_plan` 的结果：界面上的「预览计划」。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionPlan {
    pub task: String,
    pub strength: String,
    pub output_dir: String,
    pub episodes: Vec<PlanEpisode>,
    /// 每集都会做的处理（中文，一条一句）。
    pub steps: Vec<String>,
    /// 生成前必须解决的问题；非空时「开始生成」不可用。
    pub problems: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanEpisode {
    pub name: String,
    pub duration_sec: f64,
    pub width: u32,
    pub height: u32,
    pub subtitle: bool,
}

/// 计划阶段算出来的东西（`version_plan` 展示，`version_start` 提交）。
struct Prepared {
    plan: VersionPlan,
    spec: VersionJobSpec,
    music_sha256: Option<String>,
}

fn strength_label(strength: &str) -> &'static str {
    match strength {
        "light" => "轻",
        "medium" => "中",
        "heavy" => "重",
        _ => "未知",
    }
}

fn subtitle_for(template: &VersionTemplate, episode_name: &str) -> Option<PathBuf> {
    let dir = template.subtitle_dir.as_deref()?;
    let stem = Path::new(episode_name)
        .file_stem()?
        .to_string_lossy()
        .into_owned();
    let path = Path::new(dir).join(format!("{stem}.srt"));
    path.is_file().then_some(path)
}

/// 门槛检查 + 逐集哈希。`on_hash` 报告哈希进度（大文件要几十秒）。
fn prepare_version(
    app: &AppHandle,
    root: &str,
    name: &str,
    strength: &str,
    output_root: Option<&str>,
    on_hash: &mut dyn FnMut(usize, usize, &str),
) -> Result<Prepared, CommandError> {
    if !dossier_license::recipe::STRENGTHS.contains(&strength) {
        return Err(invalid("强度档只能是轻、中、重"));
    }
    let drama = drama_dir(root, name)?;
    let template = load_template(&drama.dir)?;
    let settings = crate::toolbox::read_settings(app).map_err(io_error)?;
    let mut problems: Vec<String> = Vec::new();

    if let Err(error) = template.validate() {
        problems.push(error.to_string());
    }
    if template.recut && template.subtitle_dir.is_some() {
        problems.push("重新分集后原字幕的时间对不上，不能同时烧字幕；请二选一".into());
    }
    if template.needs_font() {
        match resolve_font(&settings) {
            Ok(Some(_)) => {}
            Ok(None) => problems.push(
                "画面上要写字（水印、AI 标识或封面文字），但本机没找到中文字体；请在设置里指定字体文件"
                    .into(),
            ),
            Err(error) => problems.push(error),
        }
    }
    for (label, path) in [
        ("片头", template.intro_path.as_deref()),
        ("片尾", template.outro_path.as_deref()),
        ("背景音乐", template.music_path.as_deref()),
    ] {
        if let Some(path) = path {
            if !Path::new(path).is_file() {
                problems.push(format!("{label}文件不存在：{path}"));
            }
        }
    }
    if let Some(dir) = template.subtitle_dir.as_deref() {
        if !Path::new(dir).is_dir() {
            problems.push(format!("字幕文件夹不存在：{dir}"));
        }
    }

    let rights = match read_rights(&drama.dir) {
        None => {
            problems.push("还没有声明权属材料".into());
            None
        }
        Some(declaration) => match check_rights(&declaration) {
            Ok(()) => Some(declaration.sha256),
            Err(error) => {
                problems.push(error);
                None
            }
        },
    };

    let output_dir = resolve_product_dir(
        None,
        output_root.map(Path::new),
        &drama.dir,
        &drama.name,
        OUTPUT_LEAF,
    );
    if output_dir.exists() {
        problems.push(format!(
            "这部剧已经有处理版本（{}）。一部剧只保留一套处理版本，想重做请先删掉旧的版本目录",
            output_dir.display()
        ));
    }

    // 只处理剧目目录下的分集；版本目录自己不算（扫描会进一层子目录）。
    let normalized_output = crate::paths::normalize(&output_dir);
    let mut files: Vec<_> = drama
        .episodes
        .iter()
        .filter(|episode| !crate::paths::normalize(&episode.path).starts_with(&normalized_output))
        .collect();
    files.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    if files.is_empty() {
        problems.push("剧目目录里没有分集视频".into());
    }
    // 输出统一成 `.mp4`：「第01集.mov」和「第01集.mp4」会写到同一个文件。
    let mut seen = std::collections::HashSet::new();
    for file in &files {
        let stem = Path::new(&file.file_name)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !seen.insert(stem.clone()) {
            problems.push(format!(
                "有两个分集文件主名相同（{stem}），输出会互相覆盖；请先改名"
            ));
        }
    }
    if let Some(count) = template.recut_count {
        if template.recut && count as usize > files.len().max(1) * 10 {
            problems.push(format!(
                "重新分集的集数（{count}）太多：最多是原集数的 10 倍"
            ));
        }
    }

    let ffprobe = crate::toolbox::resolve_ffprobe(app, None).map_err(io_error)?;
    let config = dossier_core::FfprobeConfig { bin: Some(ffprobe) };
    let total = files.len();
    let mut episodes = Vec::with_capacity(total);
    let mut plan_episodes = Vec::with_capacity(total);
    for (index, file) in files.iter().enumerate() {
        on_hash(index, total, &file.file_name);
        let probed = dossier_core::probe_episode(file, &config).map_err(|error| {
            local_error(
                "probe_failed",
                format!("{} 探测失败：{error}", file.file_name),
            )
        })?;
        let (_, sha256) = hash_file(&file.path)
            .map_err(|error| io_error(format!("读取 {} 失败：{error}", file.file_name)))?;
        plan_episodes.push(PlanEpisode {
            name: file.file_name.clone(),
            duration_sec: probed.spec.duration_sec,
            width: probed.spec.width,
            height: probed.spec.height,
            subtitle: subtitle_for(&template, &file.file_name).is_some(),
        });
        episodes.push(SourceEpisode {
            name: file.file_name.clone(),
            path: file.path.display().to_string(),
            sha256,
        });
    }
    on_hash(total, total, "");

    let template_value =
        serde_json::to_value(&template).map_err(|error| io_error(error.to_string()))?;
    let template_sha256 =
        template_hash(&template_value).map_err(|error| invalid(error.to_string()))?;
    let music_sha256 = match template.music_path.as_deref() {
        Some(path) if Path::new(path).is_file() => Some(
            hash_file(Path::new(path))
                .map_err(|error| io_error(format!("读取背景音乐失败：{error}")))?
                .1,
        ),
        _ => None,
    };
    // 权属没声明时任务哈希算不出来（它是哈希的一部分）；用空串占位，反正 problems 非空不能开始。
    let task = match &rights {
        Some(rights) => version_task_hash(&VersionTask {
            episodes: episodes
                .iter()
                .map(|episode| TaskEpisode {
                    name: episode.name.clone(),
                    sha256: episode.sha256.clone(),
                })
                .collect(),
            template: template_sha256.clone(),
            rights: rights.clone(),
            strength: strength.to_string(),
        })
        .map_err(|error| invalid(error.to_string()))?,
        None => String::new(),
    };

    let steps = describe_steps(&template, strength);
    let output_dir_text = output_dir.display().to_string();
    Ok(Prepared {
        plan: VersionPlan {
            task: task.clone(),
            strength: strength.to_string(),
            output_dir: output_dir_text.clone(),
            episodes: plan_episodes,
            steps,
            problems,
        },
        spec: VersionJobSpec {
            drama_name: drama.name.clone(),
            task,
            strength: strength.to_string(),
            template,
            template_sha256,
            rights_sha256: rights.unwrap_or_default(),
            music_sha256: music_sha256.clone(),
            episodes,
            output_dir: output_dir_text,
        },
        music_sha256,
    })
}

/// 计划预览里的「每集要做的处理」。
fn describe_steps(template: &VersionTemplate, strength: &str) -> Vec<String> {
    let mut steps = vec![
        "统一输出规格：1080×1920 竖屏、30 帧、H.264 + AAC，码率不超过 5000 kbps（比例不一致时等比缩放加黑边）"
            .to_string(),
        format!(
            "细微处理（{}档，参数由服务端配方给出）：颗粒、轻微放大裁边、亮度饱和度色相微调、变速变调、首尾裁几帧",
            strength_label(strength)
        ),
        "画质检测：对比只做几何处理的参考画面，低于配方下限的这一集判为失败、不交付".to_string(),
    ];
    if template.recut {
        steps.insert(0, "先按场景切点重新分集（统一重编码）".into());
    }
    if template.intro_path.is_some() || template.outro_path.is_some() {
        steps.push("拼接片头 / 片尾（统一到输出规格，没有声音的补静音）".into());
    }
    if template.subtitle_dir.is_some() {
        steps.push("按集名烧录字幕".into());
    }
    if !template.color_preset.filters().is_empty() {
        steps.push("套用调色预设".into());
    }
    if !template.watermark_text.is_empty() {
        steps.push(format!("剧名水印：{}", template.watermark_text));
    }
    if template.ai_marked {
        steps.push(
            "AI 标识：开头 3 秒右下角显示「人工智能生成合成」，之后常驻「AI生成」；文件元数据写入隐式标识"
                .into(),
        );
    }
    if template.music_path.is_some() {
        steps.push("混入背景音乐（循环铺满）".into());
    }
    steps.push("生成封面.jpg 和版本说明.json".into());
    steps
}

/// 预览计划：只算、不扣次数（不找服务端）。
#[tauri::command]
pub async fn version_plan(
    app: AppHandle,
    root: String,
    name: String,
    strength: String,
    output_root: Option<String>,
) -> Result<VersionPlan, CommandError> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut on_hash =
            |done: usize, total: usize, label: &str| hash_progress(&app, done, total, label);
        prepare_version(
            &app,
            &root,
            &name,
            &strength,
            output_root.as_deref(),
            &mut on_hash,
        )
        .map(|prepared| prepared.plan)
    })
    .await
    .map_err(join_error)?
}

fn hash_progress(app: &AppHandle, done: usize, total: usize, label: &str) {
    use tauri::Emitter;
    let label = if label.is_empty() {
        format!("已核对 {total} 集")
    } else {
        format!("正在核对第 {}/{} 集：{label}", done + 1, total)
    };
    let _ = app.emit(
        "task-progress",
        crate::toolbox::TaskProgress {
            task: "version".into(),
            done: done as u64,
            total: total as u64,
            label,
        },
    );
}

/// 开始生成：门槛 → 申请配方（本机验签）→ 消费 → 排队。
///
/// 消费成功之后才排队；消费之后、排队之前出错（极少见：磁盘写不进去），配方已用掉，
/// 错误信息里写明要重新申请，不假装没事。
#[tauri::command]
pub async fn version_start(
    app: AppHandle,
    account: State<'_, AccountState>,
    queue: State<'_, JobQueue>,
    root: String,
    name: String,
    strength: String,
    output_root: Option<String>,
) -> Result<JobView, CommandError> {
    let prepared = {
        let app = app.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let mut on_hash =
                |done: usize, total: usize, label: &str| hash_progress(&app, done, total, label);
            prepare_version(
                &app,
                &root,
                &name,
                &strength,
                output_root.as_deref(),
                &mut on_hash,
            )
        })
        .await
        .map_err(join_error)??
    };
    if !prepared.plan.problems.is_empty() {
        return Err(local_error(
            "version_blocked",
            prepared.plan.problems.join("；"),
        ));
    }
    let spec = prepared.spec;

    let (recipe, token, _credits) = account
        .issue_recipe(&spec.task, PLATFORM, &spec.strength, spec.episodes.len())
        .await?;
    // 配方参数先在本机解析一遍：格式不对就别浪费这次配方。
    let params = RecipeParams::parse(&recipe)
        .map_err(|message| local_error("recipe_bad_params", message))?;
    if params.spec.max_duration_sec == 0 {
        return Err(local_error("recipe_bad_params", "配方里的单集时长上限为 0"));
    }
    account.consume_recipe(&recipe).await?;

    let mut steps: Vec<String> = if spec.template.recut {
        let mut steps = vec![RECUT_STEP.to_string()];
        steps.extend(recut_episode_names(&spec));
        steps
    } else {
        spec.episodes
            .iter()
            .map(|episode| episode.name.clone())
            .collect()
    };
    steps.push(FINISH_STEP.to_string());

    let title = format!(
        "{} · 处理版本（{}档）",
        spec.drama_name,
        strength_label(&spec.strength)
    );
    let output_dir = spec.output_dir.clone();
    let task = spec.task.clone();
    let value = serde_json::to_value(&spec).map_err(|error| io_error(error.to_string()))?;
    let music_sha256 = prepared.music_sha256;
    let view = queue
        .submit(NewJob {
            kind: JOB_KIND.into(),
            title,
            output_dir,
            spec: value,
            fingerprint: task.clone(),
            steps,
            files: vec![(RECIPE_FILE.into(), token.into_bytes())],
        })
        .map_err(|error| {
            io_error(format!(
                "{error}。配方已经用掉，请稍后重新申请（这次不会生成任何文件）"
            ))
        })?;
    if let Some(sha256) = music_sha256 {
        record(
            &app,
            ProcessingRecord {
                ts: now_timestamp(),
                event: "music".into(),
                task: task.clone(),
                strength: None,
                seed: None,
                quality: None,
                output: spec.template.music_path.clone(),
                sha256: Some(sha256),
            },
        )?;
    }
    Ok(view)
}

/// 重新分集是任务的第一步（输出到任务目录下的临时目录），之后按新分集逐集处理。
const RECUT_STEP: &str = "重新分集";
/// 最后一步：封面、版本说明、台账。作为一个步骤记进检查点，收尾失败了也能单独重试。
const FINISH_STEP: &str = "封面与版本说明";

/// 重新分集后的集名（和 `toolbox::transcode_recut_blocking` 的输出命名一致）。
fn recut_episode_names(spec: &VersionJobSpec) -> Vec<String> {
    let count = spec
        .template
        .recut_count
        .map(|count| count as usize)
        .unwrap_or(spec.episodes.len())
        .clamp(1, spec.episodes.len().max(1) * 10);
    (1..=count)
        .map(|index| format!("第{index:02}集.mp4"))
        .collect()
}

/// 配方里的三组参数。
struct RecipeParams {
    spec: OutputSpec,
    subtle: SubtleParams,
    floor: QualityFloor,
}

impl RecipeParams {
    fn parse(recipe: &Recipe) -> Result<Self, String> {
        let get = |key: &str| {
            recipe
                .params
                .get(key)
                .ok_or_else(|| format!("配方里缺少 {key} 参数"))
        };
        Ok(Self {
            spec: OutputSpec::from_params(get("platform")?).map_err(|error| error.to_string())?,
            subtle: SubtleParams::from_params(get("subtle")?).map_err(|error| error.to_string())?,
            floor: QualityFloor::from_params(get("quality")?).map_err(|error| error.to_string())?,
        })
    }
}

// ---------------------------------------------------------------------------
// 队列执行器
// ---------------------------------------------------------------------------

/// 处理版本任务的执行器（注册到 `JobQueue`）。
pub struct VersionRunner {
    app: AppHandle,
}

impl VersionRunner {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

/// 每集处理结果（存在任务目录 `results/` 下，收尾时汇总）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StepResult {
    report: EpisodeReport,
}

struct VersionJob {
    app: AppHandle,
    dir: PathBuf,
    spec: VersionJobSpec,
    recipe: Recipe,
    params: RecipeParams,
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    font: Option<String>,
    prefer_hardware: bool,
    /// 第一次真正编码前才探测编码器（只有收尾一步要做时不必探测）。
    encoders: Option<EncoderPlans>,
    output_dir: PathBuf,
}

impl JobRunner for VersionRunner {
    fn prepare(&self, job: &JobRecord, dir: &Path) -> Result<Box<dyn PreparedJob>, String> {
        let spec: VersionJobSpec = serde_json::from_value(job.spec.clone())
            .map_err(|error| format!("任务描述无法读取：{error}"))?;
        let token = std::fs::read_to_string(dir.join(RECIPE_FILE))
            .map_err(|error| format!("本机保存的配方读不到了：{error}"))?;
        let (device_id, keys) = signing_context(&self.app)?;
        // 续跑凭本机保存的配方：重新验签名、机器码、任务哈希，不再验有效期（计划 2.6 节）。
        let recipe = verify_consumed(token.trim(), &keys, &device_id, &spec.task)
            .map_err(|error| format!("配方校验失败：{error}"))?;
        if recipe.strength != spec.strength || recipe.platform != PLATFORM {
            return Err("配方的档位或规格和任务不一致".into());
        }
        // 任务描述是本机文件：它说的任务哈希必须和描述本身算出来的一致，
        // 否则改 job.json 里的模板或分集就能套用别的任务的配方。
        let expected = version_task_hash(&VersionTask {
            episodes: spec
                .episodes
                .iter()
                .map(|episode| TaskEpisode {
                    name: episode.name.clone(),
                    sha256: episode.sha256.clone(),
                })
                .collect(),
            template: spec.template_sha256.clone(),
            rights: spec.rights_sha256.clone(),
            strength: spec.strength.clone(),
        })
        .map_err(|error| error.to_string())?;
        let template_value =
            serde_json::to_value(&spec.template).map_err(|error| error.to_string())?;
        let template_sha256 = template_hash(&template_value).map_err(|error| error.to_string())?;
        if expected != spec.task || template_sha256 != spec.template_sha256 {
            return Err("任务描述被改动过，和配方绑定的任务对不上".into());
        }
        if let Some(music) = spec.template.music_path.as_deref() {
            let (_, sha256) = hash_file(Path::new(music))
                .map_err(|error| format!("背景音乐读不到了：{error}"))?;
            if Some(&sha256) != spec.music_sha256.as_ref() {
                return Err("背景音乐文件和开始生成时不一样了".into());
            }
        }
        let params = RecipeParams::parse(&recipe)?;
        let settings = crate::toolbox::read_settings(&self.app)?;
        let font = resolve_font(&settings)?;
        if spec.template.needs_font() && font.is_none() {
            return Err("没有找到中文字体，请在设置里指定字体文件".into());
        }
        let ffmpeg = crate::toolbox::resolve_ffmpeg(&self.app)?;
        let ffprobe = crate::toolbox::resolve_ffprobe(&self.app, None)?;
        std::fs::create_dir_all(dir.join(RESULTS_DIR))
            .map_err(|error| format!("创建任务结果目录失败：{error}"))?;
        let output_dir = PathBuf::from(&spec.output_dir);
        Ok(Box::new(VersionJob {
            app: self.app.clone(),
            dir: dir.to_path_buf(),
            spec,
            recipe,
            params,
            ffmpeg,
            ffprobe,
            font,
            prefer_hardware: !settings.software_encode_only,
            encoders: None,
            output_dir,
        }))
    }
}

fn signing_context(app: &AppHandle) -> Result<(String, Vec<TrustedKey>), String> {
    app.state::<AccountState>()
        .signing_context()
        .map_err(|error| error.message())
}

impl VersionJob {
    /// 重新分集的输出目录（任务目录下的临时目录，完成后随任务一起保留，便于续跑）。
    fn recut_dir(&self) -> PathBuf {
        self.dir.join("recut")
    }

    fn result_path(&self, step: &str) -> PathBuf {
        self.dir
            .join(RESULTS_DIR)
            .join(format!("{}.json", crate::paths::safe_file_stem(step)))
    }

    fn encoders(&mut self) -> Result<&EncoderPlans, String> {
        if self.encoders.is_none() {
            self.encoders = Some(prepare_encoders(&self.ffmpeg, self.prefer_hardware)?);
        }
        Ok(self.encoders.as_ref().expect("刚刚填过"))
    }

    /// 开工前复核源文件：申请配方时算过哈希，被换过就拒绝。
    fn source_for(&self, step: &str) -> Result<PathBuf, StepError> {
        if self.spec.template.recut {
            let path = self.recut_dir().join(step);
            if !path.is_file() {
                return Err(StepError::Skipped("重新分集实际分出的集数比计划少".into()));
            }
            return Ok(path);
        }
        let episode = self
            .spec
            .episodes
            .iter()
            .find(|episode| episode.name == step)
            .ok_or_else(|| StepError::Failed("任务里没有这一集".into()))?;
        let path = PathBuf::from(&episode.path);
        let (_, sha256) = hash_file(&path)
            .map_err(|error| StepError::Failed(format!("源文件读不到了：{error}")))?;
        if sha256 != episode.sha256 {
            return Err(StepError::Failed(
                "源文件和开始生成时不一样了，配方只能用在原来那批文件上".into(),
            ));
        }
        Ok(path)
    }

    /// 重新分集：复核全部源文件后，按场景切点重排到任务目录下的临时目录。
    fn run_recut(&mut self) -> Result<StepOutcome, StepError> {
        let mut probed = Vec::with_capacity(self.spec.episodes.len());
        let config = dossier_core::FfprobeConfig {
            bin: Some(self.ffprobe.clone()),
        };
        for episode in &self.spec.episodes {
            let path = PathBuf::from(&episode.path);
            let (size, sha256) = hash_file(&path).map_err(|error| {
                StepError::Failed(format!("{} 读不到了：{error}", episode.name))
            })?;
            if sha256 != episode.sha256 {
                return Err(StepError::Failed(format!(
                    "{} 和开始生成时不一样了，配方只能用在原来那批文件上",
                    episode.name
                )));
            }
            let file = dossier_core::EpisodeFile {
                path,
                file_name: episode.name.clone(),
                size_bytes: size,
                episode_number: None,
            };
            probed.push(
                dossier_core::probe_episode(&file, &config).map_err(|error| {
                    StepError::Failed(format!("{} 探测失败：{error}", episode.name))
                })?,
            );
        }
        let dir = self.recut_dir();
        // 上次中断留下的半成品不可信，整目录重来。
        let _ = std::fs::remove_dir_all(&dir);
        let target = recut_episode_names(&self.spec).len();
        let mut on_step = |_: u64, _: u64, _: &str| {};
        let report = crate::toolbox::transcode_recut_blocking(
            &self.ffmpeg,
            &probed,
            target,
            &dir,
            &mut on_step,
        )
        .map_err(StepError::Failed)?;
        let artifacts: Vec<PathBuf> = report
            .items
            .iter()
            .map(|item| PathBuf::from(&item.output))
            .collect();
        Ok(StepOutcome {
            detail: format!("分成 {} 集", artifacts.len()),
            artifacts,
        })
    }

    fn run_episode(&mut self, step: &str, cancel: &CancelFlag) -> Result<StepOutcome, StepError> {
        let source = self.source_for(step)?;
        let source_text = source.display().to_string();
        let config = dossier_core::FfprobeConfig {
            bin: Some(self.ffprobe.clone()),
        };
        let file = dossier_core::EpisodeFile {
            path: source.clone(),
            file_name: step.to_string(),
            size_bytes: 0,
            episode_number: None,
        };
        let probed = dossier_core::probe_episode(&file, &config)
            .map_err(|error| StepError::Failed(format!("探测失败：{error}")))?;
        let template = &self.spec.template;
        let subtitle = if template.recut {
            None
        } else {
            subtitle_for(template, step)
        };
        let subtitle_text = subtitle.as_ref().map(|path| path.display().to_string());
        let intro = self.clip(template.intro_path.as_deref())?;
        let outro = self.clip(template.outro_path.as_deref())?;
        let episode = EpisodeInput {
            path: &source_text,
            width: probed.spec.width,
            height: probed.spec.height,
            fps: probed.spec.fps.filter(|fps| *fps > 0.0).unwrap_or(30.0),
            duration_sec: probed.spec.duration_sec,
            has_audio: probed.spec.audio_codec.is_some(),
            subtitle_path: subtitle_text.as_deref(),
        };
        let extras = GraphExtras {
            intro: intro.as_ref().map(|(path, duration, audio)| ClipInput {
                path,
                duration_sec: *duration,
                has_audio: *audio,
            }),
            outro: outro.as_ref().map(|(path, duration, audio)| ClipInput {
                path,
                duration_sec: *duration,
                has_audio: *audio,
            }),
            music: template.music_path.as_deref(),
            font: self.font.as_deref(),
        };
        let graph = build_version_graph(
            template,
            &self.params.spec,
            &self.params.subtle,
            &episode,
            &extras,
            self.recipe.seed,
        )
        .map_err(|error| StepError::Failed(error.to_string()))?;

        std::fs::create_dir_all(&self.output_dir)
            .map_err(|error| StepError::Failed(format!("创建版本目录失败：{error}")))?;
        let output = self.output_dir.join(&graph.output_name);
        let stats_dir = self.dir.join("stats");
        std::fs::create_dir_all(&stats_dir)
            .map_err(|error| StepError::Failed(format!("创建画质统计目录失败：{error}")))?;
        let stem = crate::paths::safe_file_stem(step);
        let ssim_path = stats_dir.join(format!("{stem}.ssim.log"));
        let psnr_path = stats_dir.join(format!("{stem}.psnr.log"));
        let _ = std::fs::remove_file(&ssim_path);
        let _ = std::fs::remove_file(&psnr_path);

        let mut args = graph.input_args.clone();
        args.push("-filter_complex".into());
        args.push(fill_stats_paths(
            &graph.filter_complex,
            &ssim_path.display().to_string(),
            &psnr_path.display().to_string(),
        ));
        args.extend(graph.map_args.iter().cloned());
        args.push("-r".into());
        args.push(self.params.spec.fps.to_string());
        for (key, value) in &graph.metadata {
            args.push("-metadata".into());
            args.push(format!("{key}={value}"));
        }
        let request = EncodeRequest {
            output: &output,
            max_video_kbps: Some(self.params.spec.video_bitrate_kbps),
            audio_kbps: self.params.spec.audio_bitrate_kbps,
            metadata_tags: !graph.metadata.is_empty(),
        };
        let ffmpeg = self.ffmpeg.clone();
        let plans = self.encoders().map_err(StepError::Failed)?.clone();
        let choice =
            encode_with_fallback(&ffmpeg, &args, &request, &plans, cancel).map_err(|error| {
                match error {
                    EncodeRunError::Cancelled => StepError::Cancelled,
                    other => StepError::Failed(other.to_string()),
                }
            })?;

        // 画质：低于配方下限就判失败、删掉这一集，不交付（计划 2.4 节）。
        let read = |path: &Path| std::fs::read_to_string(path).unwrap_or_default();
        let quality =
            QualityResult::from_stats(&read(&ssim_path), &read(&psnr_path)).map_err(|error| {
                let _ = std::fs::remove_file(&output);
                StepError::Failed(error.to_string())
            })?;
        if !quality.passes(&self.params.floor) {
            let _ = std::fs::remove_file(&output);
            return Err(StepError::Failed(format!(
                "画质低于下限（SSIM {:.4} / 下限 {:.2}，PSNR {:.2} dB / 下限 {:.0} dB），这一集不交付",
                quality.ssim, self.params.floor.min_ssim, quality.psnr, self.params.floor.min_psnr
            )));
        }

        let (_, sha256) = hash_file(&output)
            .map_err(|error| StepError::Failed(format!("读取输出失败：{error}")))?;
        let report = EpisodeReport {
            name: step.to_string(),
            quality: Some(quality.clone()),
            encoder: choice.encoder.clone(),
            software_encode: choice.fell_back_to_software,
            output_name: graph.output_name.clone(),
            sha256,
        };
        write_json_atomic(&self.result_path(step), &StepResult { report })
            .map_err(StepError::Failed)?;
        let mut detail = format!(
            "SSIM {:.4} · PSNR {:.2} dB · {}",
            quality.ssim, quality.psnr, choice.encoder
        );
        if choice.fell_back_to_software {
            detail.push_str(" · 硬件编码失败，已改用软件编码");
        }
        Ok(StepOutcome {
            artifacts: vec![output, self.result_path(step)],
            detail,
        })
    }

    /// 片头片尾：探测时长和有没有音轨。
    fn clip(&self, path: Option<&str>) -> Result<Option<(String, f64, bool)>, StepError> {
        let Some(path) = path else {
            return Ok(None);
        };
        let file = dossier_core::EpisodeFile {
            path: PathBuf::from(path),
            file_name: path.to_string(),
            size_bytes: 0,
            episode_number: None,
        };
        let config = dossier_core::FfprobeConfig {
            bin: Some(self.ffprobe.clone()),
        };
        let probed = dossier_core::probe_episode(&file, &config).map_err(|error| {
            StepError::Failed(format!("片头 / 片尾探测失败（{path}）：{error}"))
        })?;
        if probed.spec.duration_sec <= 0.0 {
            return Err(StepError::Failed(format!("片头 / 片尾时长为 0：{path}")));
        }
        Ok(Some((
            path.to_string(),
            probed.spec.duration_sec,
            probed.spec.audio_codec.is_some(),
        )))
    }

    /// 计划里的分集步骤（不含重新分集和收尾）。
    fn episode_steps(&self) -> Vec<String> {
        if self.spec.template.recut {
            recut_episode_names(&self.spec)
        } else {
            self.spec
                .episodes
                .iter()
                .map(|episode| episode.name.clone())
                .collect()
        }
    }

    /// 收尾：封面、版本说明、台账。每集结果从任务目录 `results/` 读（只有通过画质检测的集才有）。
    fn run_finish(&mut self) -> Result<StepOutcome, StepError> {
        let mut episodes = Vec::new();
        for step in self.episode_steps() {
            let path = self.result_path(&step);
            if !path.is_file() {
                continue;
            }
            let result: StepResult = read_json(&path).map_err(StepError::Failed)?;
            // 再核对一遍输出：版本说明里写的哈希必须是交付的那个文件。
            let output = self.output_dir.join(&result.report.output_name);
            let (_, sha256) = hash_file(&output).map_err(|error| {
                StepError::Failed(format!("{} 读不到了：{error}", result.report.output_name))
            })?;
            if sha256 != result.report.sha256 {
                return Err(StepError::Failed(format!(
                    "{} 在生成后被改动过",
                    result.report.output_name
                )));
            }
            episodes.push(result.report);
        }
        if episodes.is_empty() {
            return Err(StepError::Failed("没有任何一集生成成功".into()));
        }

        // 封面：取第 1 集成品的画面（已经是输出规格，水印和标识也在上面）。
        let first = self.output_dir.join(&episodes[0].output_name);
        let cover = self.output_dir.join("封面.jpg");
        let args = build_cover_args(
            &self.spec.template,
            &self.params.spec,
            &first.display().to_string(),
            self.font.as_deref(),
            &cover.display().to_string(),
        )
        .map_err(|error| StepError::Failed(error.to_string()))?;
        crate::encode::run_ffmpeg_cancellable(&self.ffmpeg, &args, &cover, &CancelFlag::new())
            .map_err(|error| StepError::Failed(format!("生成封面失败：{error}")))?;
        if !cover.is_file() {
            return Err(StepError::Failed(
                "生成封面失败：ffmpeg 没有输出画面（封面取帧秒数可能超过了第 1 集的时长）".into(),
            ));
        }

        let report = VersionReport {
            generated_at: now_timestamp(),
            task: self.spec.task.clone(),
            strength: self.spec.strength.clone(),
            recipe_id: self.recipe.recipe_id.clone(),
            seed: self.recipe.seed,
            spec: self.params.spec.clone(),
            subtle: self.params.subtle.clone(),
            quality_floor: self.params.floor.clone(),
            rights_sha256: self.spec.rights_sha256.clone(),
            template_sha256: self.spec.template_sha256.clone(),
            music_sha256: self.spec.music_sha256.clone(),
            ai_marked: self.spec.template.ai_marked,
            episodes,
        };
        let report_path = self.output_dir.join("版本说明.json");
        write_json_atomic(&report_path, &report).map_err(StepError::Failed)?;

        record(
            &self.app,
            ProcessingRecord {
                ts: report.generated_at.clone(),
                event: "version".into(),
                task: report.task.clone(),
                strength: Some(report.strength.clone()),
                seed: Some(report.seed),
                quality: Some(
                    report
                        .episodes
                        .iter()
                        .filter_map(|episode| episode.quality.clone())
                        .collect(),
                ),
                output: Some(self.output_dir.display().to_string()),
                sha256: None,
            },
        )
        .map_err(|error| StepError::Failed(error.message))?;

        Ok(StepOutcome {
            detail: format!("{} 集已交付", report.episodes.len()),
            artifacts: vec![cover, report_path],
        })
    }
}

impl PreparedJob for VersionJob {
    fn run_step(&mut self, step: &str, cancel: &CancelFlag) -> Result<StepOutcome, StepError> {
        match step {
            RECUT_STEP => {
                // 重新分集后旧的每集结果都作废。
                let _ = std::fs::remove_dir_all(self.dir.join(RESULTS_DIR));
                std::fs::create_dir_all(self.dir.join(RESULTS_DIR))
                    .map_err(|error| StepError::Failed(format!("创建任务结果目录失败：{error}")))?;
                self.run_recut()
            }
            FINISH_STEP => self.run_finish(),
            _ => self.run_episode(step, cancel),
        }
    }
}

/// 应用启动时创建队列并注册执行器（处理版本 + 区域擦除）。
pub fn start_queue(app: &AppHandle) -> Result<JobQueue, String> {
    use tauri::Emitter;
    let root = jobs_root(app)?;
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("创建任务目录失败（{}）：{error}", root.display()))?;
    let runners: Vec<(String, Arc<dyn JobRunner>)> = vec![
        (
            JOB_KIND.to_string(),
            Arc::new(VersionRunner::new(app.clone())),
        ),
        (
            crate::erase_batch::JOB_KIND.to_string(),
            Arc::new(crate::erase_batch::EraseRunner::new(app.clone())),
        ),
    ];
    let handle = app.clone();
    Ok(JobQueue::start(root, runners, move |view| {
        let _ = handle.emit("job-progress", view);
    }))
}
