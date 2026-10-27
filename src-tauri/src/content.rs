//! 深度处理前置材料流水线。
//!
//! 职责分成三层，避免把业务流程塞进单个函数：
//! 1. **运行时模型管理**：探测 ASR/OCR 可执行文件与权重，不自动下载大模型；
//! 2. **视频内容提取**：按前 N 集抽音轨转写、抽底部字幕帧识别，落盘可复核证据；
//! 3. **材料早期编排**：在深度处理编码线程旁启动工作线程，先提取内容，
//!    再生成剧名/简介/海报/过程图，最后由转码主流程汇合结果。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dossier_core::{save_settings, AppSettings, ProbedEpisode};
use dossier_media::asr::{AsrPlan, WhisperFlavor};
use dossier_media::ocr::{Region, RegionFramePlan};
use dossier_media::render::TimeRange;
use dossier_runtime::probe::probe_tool;
use dossier_runtime::{CommandRunner, CommandSpec, SystemRunner, ToolReport};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::{AppHandle, Emitter};

use crate::ai;

/// ASR 单集采样上限：足够理解主线，也避免首屏材料被整部长片拖住。
pub const ASR_SAMPLE_SECONDS: f64 = 240.0;
/// OCR 抽帧窗口：默认只看每集前 3 分钟。
pub const OCR_SAMPLE_SECONDS: f64 = 180.0;
/// OCR 抽帧间隔：硬字幕通常持续数秒，5 秒粒度够用且不至于放大模型耗时。
pub const OCR_INTERVAL_SECONDS: f64 = 5.0;
/// ASR 权重加载和长对白转写都可能超过普通命令超时。
const ASR_TIMEOUT_SECS: u64 = 30 * 60;

/// ASR 引擎类型，序列化口径与设置一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AsrEngine {
    #[serde(rename = "openai")]
    OpenAi,
    WhisperCpp,
}

impl AsrEngine {
    fn parse(value: Option<&str>) -> Self {
        match value
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "whisper_cpp" | "whisper-cpp" | "cpp" => Self::WhisperCpp,
            _ => Self::OpenAi,
        }
    }

    fn flavor(self) -> WhisperFlavor {
        match self {
            Self::OpenAi => WhisperFlavor::OpenAi,
            Self::WhisperCpp => WhisperFlavor::WhisperCpp,
        }
    }

    fn fallback_program(self) -> &'static str {
        if cfg!(windows) {
            match self {
                Self::OpenAi => "whisper.exe",
                Self::WhisperCpp => "whisper-cli.exe",
            }
        } else {
            match self {
                Self::OpenAi => "whisper",
                Self::WhisperCpp => "whisper-cli",
            }
        }
    }
}

/// 材料模型运行时状态。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRuntimeStatus {
    pub asr: ToolReport,
    pub asr_engine: AsrEngine,
    pub asr_model: String,
    pub ocr: ToolReport,
    /// OCR 自检的完整结果：选了哪一级、跳过了哪些、各自原因。
    /// 提取阶段据此建立识别管线，不必再探测一遍（探测要跑子进程，重复做很浪费）。
    #[serde(skip)]
    pub ocr_env: dossier_media::ocr_backend::Environment,
    pub ready: bool,
    pub guidance: String,
}

/// 一集提取出的可复核证据。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EpisodeEvidence {
    pub file_name: String,
    pub audio: PathBuf,
    pub subtitle: PathBuf,
    pub transcript_lines: usize,
    pub ocr_lines: usize,
    pub transcript_excerpt: String,
    pub ocr_excerpt: String,
}

/// 一部剧抽样提取后的证据包。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentEvidence {
    pub drama_name: String,
    pub work_dir: PathBuf,
    pub manifest: PathBuf,
    pub episodes: Vec<EpisodeEvidence>,
    /// 逐集软失败信息；例如某一集没有音轨或 OCR 失败时仍保留其他集的证据。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<String>,
}

/// 自动材料产物与诊断。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaterialPipelineReport {
    pub status: String,
    pub title: Option<String>,
    pub synopsis: Option<String>,
    pub poster: Option<PathBuf>,
    pub process_images: Vec<PathBuf>,
    pub proof: Option<PathBuf>,
    pub evidence: Option<ContentEvidence>,
    pub messages: Vec<String>,
    pub title_source: String,
    pub synopsis_source: String,
    pub poster_source: String,
    pub storyboard_source: String,
    /// 海报的 data URL，供转码页直接预览；落盘清单里不包含它。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster_data_url: Option<String>,
}

impl MaterialPipelineReport {
    pub(crate) fn failed(error: String) -> Self {
        Self {
            status: "failed".into(),
            title: None,
            synopsis: None,
            poster: None,
            process_images: Vec::new(),
            proof: None,
            evidence: None,
            messages: vec![error],
            title_source: "未生成".into(),
            synopsis_source: "未生成".into(),
            poster_source: "未生成".into(),
            storyboard_source: "未生成".into(),
            poster_data_url: None,
        }
    }
}

/// 编码与证明上下文；材料线程与编码并行，但证明仍记录本次编码策略。
#[derive(Debug, Clone)]
pub(crate) struct MaterialProofContext {
    pub drama_name: String,
    pub original_episodes: usize,
    pub encoder: String,
}

/// IPC：探测材料模型运行时（不加载权重，不访问网络）。
#[tauri::command]
pub fn model_runtime_status(app: AppHandle) -> Result<ModelRuntimeStatus, String> {
    let settings = crate::toolbox::read_settings(&app)?;
    Ok(model_runtime(&settings, &SystemRunner::new()))
}

/// 探测 ASR/OCR；只执行 `--help` / `--version`，秒级返回。
pub(crate) fn model_runtime(
    settings: &AppSettings,
    runner: &dyn CommandRunner,
) -> ModelRuntimeStatus {
    let engine = AsrEngine::parse(settings.asr_engine.as_deref());
    let executable = settings
        .asr_executable
        .clone()
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from(engine.fallback_program()));
    let asr = probe_tool(runner, "ASR", &executable, &["--help"]);
    let ocr_program = settings
        .ocr_executable
        .clone()
        .unwrap_or_else(|| PathBuf::from("tesseract"));
    let mut ocr = probe_tool(runner, "OCR", &ocr_program, &["--version"]);
    if ocr.ok {
        let languages = CommandSpec::new(ocr.program.clone())
            .arg("--list-langs")
            .timeout(Duration::from_secs(20));
        let has_chinese = runner
            .run(&languages)
            .map(|output| {
                output.success()
                    && output
                        .combined()
                        .lines()
                        .any(|line| line.trim().eq_ignore_ascii_case("chi_sim"))
            })
            .unwrap_or(false);
        if !has_chinese {
            ocr.ok = false;
            ocr.detail = "缺少 chi_sim 中文语言包".into();
        }
    }
    // 按降级链决定 OCR 最终用哪个后端（PaddleOCR → 系统 OCR → tesseract）。
    // 每一级都要先确认「本机现在真的有它」，再参与选择；探测失败不报错，只跳过。
    let paddle_program = dossier_media::ocr::paddle::find_program();
    let backend_env = dossier_media::ocr_backend::Environment {
        paddle_program,
        // RapidOCR 本轮不做（引擎选型未定），自检恒为「没有」，降级链会跳过这一级。
        rapid_program: None,
        // 与实际识别用同一个系统接口判断，不需要管理员权限（见 ocr::system::available）。
        system_available: dossier_media::ocr::system::available(),
        tesseract: if ocr.ok {
            Some((PathBuf::from(ocr.program.clone()), true))
        } else {
            None
        },
    };
    let (backend, skipped) = dossier_media::ocr_backend::select(&backend_env);
    if let Some(selection) = &backend {
        if selection.backend != dossier_media::ocr_backend::Backend::Tesseract {
            // 选中的是非 tesseract 后端：把 OCR 视为可用，并注明用的是哪一个
            ocr.ok = true;
            ocr.detail = format!("使用 {}", selection.backend.label());
        }
    } else {
        ocr.ok = false;
        ocr.detail = dossier_media::ocr_backend::unavailable_hint(&skipped);
    }

    let asr_model = settings.asr_model.clone().unwrap_or_else(|| {
        if engine == AsrEngine::WhisperCpp {
            String::new()
        } else {
            // 默认从 small 升到 medium：抽样音频只有 240 秒，GPU 上 medium 的耗时仍在
            // 数秒级（显存实测 3.3~4.4 GB，8 GB 卡足够），中文准确率比 small 明显更好。
            // 没有 GPU 时由 whisper 自己回落 CPU，只是慢一些，不影响结果。
            "medium".to_string()
        }
    });
    let mut problems: Vec<String> = Vec::new();
    if !asr.ok {
        problems.push("ASR 可执行文件不可用".into());
    }
    if asr_model.trim().is_empty()
        || (engine == AsrEngine::WhisperCpp && !Path::new(&asr_model).is_file())
    {
        problems.push("ASR 模型缺失".into());
    }
    // OCR 是**可选增强**：缺了只是少一路证据，不应把整个内容模型判为未就绪。
    // 就绪只看 ASR（对白是主证据）；OCR 的情况单独写进 guidance 说明后果。
    let ready = problems.is_empty();
    let ocr_note = if ocr.ok {
        String::new()
    } else {
        format!("（OCR 未启用，材料只依据对白：{}）", ocr.detail)
    };
    let mut guidance = if ready {
        String::new()
    } else {
        format!(
            "请在「设置 → 目录与引擎 → 内容模型」配置：{}。ASR 支持 openai-whisper 或 whisper.cpp；\
             OCR 可选（PaddleOCR / RapidOCR / 系统 OCR / tesseract 任一可用即可）。",
            problems.join("；")
        )
    };
    guidance.push_str(&ocr_note);
    ModelRuntimeStatus {
        asr,
        asr_engine: engine,
        asr_model,
        ocr,
        ocr_env: backend_env,
        ready,
        guidance,
    }
}

/// 把一集视频转成“对白 + 画面文字”证据。
#[allow(clippy::too_many_lines)]
fn extract_episode(
    app: &AppHandle,
    settings: &AppSettings,
    runtime: &ModelRuntimeStatus,
    ffmpeg: &Path,
    episode: &ProbedEpisode,
    work_dir: &Path,
    index: usize,
) -> Result<EpisodeEvidence, String> {
    if crate::toolbox::transcode_cancelled() {
        return Err("已停止".into());
    }
    crate::logging::log_event(
        "材料",
        format!("开始提取第 {} 集：{}", index + 1, episode.file.file_name),
    );
    let _ = app.emit(
        "material-progress",
        json!({
            "stage": "extract",
            "label": format!("材料线程：提取第 {} 集对白与画面文字", index + 1),
        }),
    );
    let stem = format!("{:02}", index + 1);
    let episode_dir = work_dir.join(&stem);
    std::fs::create_dir_all(&episode_dir)
        .map_err(|error| format!("创建内容提取目录失败：{error}"))?;

    // 1) 抽取 16 kHz 单声道音轨；这是本地 ASR 的标准输入。
    let audio = episode_dir.join("audio.wav");
    let audio_str = audio.display().to_string();
    let source_str = episode.file.path.display().to_string();
    crate::toolbox::run_ffmpeg(
        ffmpeg,
        &[
            "-y",
            "-t",
            &format!("{ASR_SAMPLE_SECONDS:.3}"),
            "-i",
            &source_str,
            "-vn",
            "-map",
            "a:0?",
            "-ac",
            "1",
            "-ar",
            "16000",
            &audio_str,
        ],
    )?;

    // 2) 调用既有 ASR 适配层；这里显式设置超时并接入统一停止开关。
    let engine = AsrEngine::parse(settings.asr_engine.as_deref());
    let asr_program = runtime.asr.program.clone();
    let mut plan = AsrPlan::new(
        &audio,
        &episode_dir,
        engine.flavor(),
        runtime.asr_model.clone(),
    );
    plan.language = Some("zh".into());
    // 显卡意图：有 NVIDIA 显卡就明确要求 GPU，没有则明确要求 CPU。
    // 前者让 openai-whisper 传 --device cuda/mps、whisper.cpp 用默认 GPU；
    // 后者让 whisper.cpp 传 -ng，避免它在没显卡的机器上白试一次。
    let has_gpu = crate::toolbox::nvidia_gpu_present() || cfg!(target_os = "macos");
    plan.gpu = Some(has_gpu);
    let runner = SystemRunner::new();
    let mut spec = plan
        .build_command(&asr_program, runtime.asr.ok)
        .map_err(|error| error.to_string())?;
    spec.timeout = Some(Duration::from_secs(ASR_TIMEOUT_SECS));
    // whisper CLI 解码音频统一走 PATH 上的 ffmpeg；GUI 进程 PATH 很干净，
    // 把当前解析出的 ffmpeg 所在目录前置注入，避免 Windows 打包版报找不到 ffmpeg。
    if let Some(ffmpeg_dir) = ffmpeg.parent() {
        let chained = std::env::join_paths(std::iter::once(ffmpeg_dir.to_path_buf()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .map(|value| value.display().to_string())
        .unwrap_or_default();
        spec.env.push(("PATH".into(), chained));
    }
    let output = runner
        .run_cancellable(&spec, &crate::toolbox::transcode_cancelled)
        .map_err(|error| format!("ASR 执行失败：{error}"))?;
    if !output.success() {
        let combined = output.combined();
        let detail = combined
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("（无输出）");
        return Err(format!("ASR 退出码 {}：{detail}", output.code_label()));
    }
    let subtitle = plan.expected_subtitle_path();
    let cues = dossier_media::asr::load_transcript(&subtitle).map_err(|error| error.to_string())?;

    // 3) 抽画面底部字幕区域并 OCR。缺尺寸时按竖屏 720x1280 兜底，避免因 ffprobe 元数据缺失中断。
    let width = if episode.spec.width > 0 {
        episode.spec.width
    } else {
        720
    };
    let height = if episode.spec.height > 0 {
        episode.spec.height
    } else {
        1280
    };
    let region_height = ((height as f64) * 0.26).round().max(48.0) as u32;
    let region_y = ((height as f64) * 0.68).round() as u32;
    let region_y = region_y.min(height.saturating_sub(region_height));
    let region = Region::new(0, region_y, width, region_height);
    let frame_plan =
        RegionFramePlan::new(&episode.file.path, episode_dir.join("ocr-frames"), region);
    let mut frame_plan = frame_plan;
    frame_plan.interval_sec = OCR_INTERVAL_SECONDS;
    frame_plan.range = Some(TimeRange::new(
        0.0,
        Some(OCR_SAMPLE_SECONDS.min(episode.spec.duration_sec.max(1.0))),
    ));
    frame_plan.prefix = "caption".into();
    frame_plan.language = "chi_sim+eng".into();
    let frames = frame_plan
        .execute(&runner, &ffmpeg.display().to_string())
        .map_err(|error| error.to_string())?;

    // 建立识别管线：按自检结果排好候选链，运行期若某一级跑不通会自动降级。
    // 没有可用后端时 pipeline 为 None，OCR 证据整段跳过，但整集提取仍然成功。
    let (selection, _) = dossier_media::ocr_backend::select(&runtime.ocr_env);
    let mut pipeline = dossier_media::ocr::engine::OcrPipeline::new(selection, &runtime.ocr_env);
    let frame_count = frames.len();
    // 整批识别：PaddleOCR 一次加载模型处理全部帧，其他后端在内部逐帧跑。
    // 每帧中间产物用帧名区分（<帧名>.tsv），不再互相覆盖同一个文件。
    let results = match pipeline.as_mut() {
        Some(active) => active.recognize_frames(&frames, &episode_dir.join("ocr"), &runner),
        None => Vec::new(),
    };
    let mut ocr_lines = Vec::new();
    let mut ocr_failures = 0usize;
    for (frame, result) in frames.iter().zip(results) {
        match result {
            Ok(lines) => ocr_lines.extend(lines.into_iter().filter(|line| line.passes(60.0))),
            Err(error) => {
                ocr_failures += 1;
                crate::logging::log_event(
                    "材料",
                    format!(
                        "第 {} 集画面文字识别失败（{}）：{error}",
                        index + 1,
                        frame
                            .file_name()
                            .map(|name| name.to_string_lossy())
                            .unwrap_or_default()
                    ),
                );
            }
        }
    }
    // 降级一旦发生就要留痕：用户看到的「用的是哪一级」可能与自检结论不同。
    if let Some(active) = &pipeline {
        for downgrade in active.downgrades() {
            crate::logging::log_event(
                "材料",
                format!(
                    "第 {} 集 OCR 从 {} 降级到 {}：{}",
                    index + 1,
                    downgrade.from.label(),
                    downgrade.to.label(),
                    downgrade.reason
                ),
            );
        }
    }
    if ocr_failures > 0 {
        crate::logging::log_event(
            "材料",
            format!(
                "第 {} 集 OCR {}/{} 帧失败，已保留成功帧",
                index + 1,
                ocr_failures,
                frame_count
            ),
        );
    }

    let transcript_lines: Vec<&str> = cues
        .iter()
        .map(|cue| cue.text.trim())
        .filter(|text| !text.is_empty())
        .take(120)
        .collect();
    let transcript_excerpt = transcript_lines.join("\n");
    let ocr_excerpt = dedup_lines(ocr_lines.iter().map(|line| line.text.as_str()))
        .into_iter()
        .take(60)
        .collect::<Vec<_>>()
        .join("\n");
    let evidence = EpisodeEvidence {
        file_name: episode.file.file_name.clone(),
        audio,
        subtitle,
        transcript_lines: transcript_lines.len(),
        ocr_lines: ocr_lines.len(),
        transcript_excerpt,
        ocr_excerpt,
    };
    Ok(evidence)
}

/// 提取抽样分集并落盘内容清单。
pub(crate) fn extract_content(
    app: &AppHandle,
    settings: &AppSettings,
    runtime: &ModelRuntimeStatus,
    ffmpeg: &Path,
    episodes: &[ProbedEpisode],
    work_dir: &Path,
) -> Result<ContentEvidence, String> {
    if episodes.is_empty() {
        return Err("没有可分析的分集".into());
    }
    std::fs::create_dir_all(work_dir).map_err(|error| format!("创建内容提取目录失败：{error}"))?;
    let sample_count = settings
        .effective_content_sample_count()
        .min(episodes.len() as u32) as usize;
    let mut extracted = Vec::new();
    let mut messages = Vec::new();
    for (index, episode) in episodes.iter().take(sample_count).enumerate() {
        if crate::toolbox::transcode_cancelled() {
            return Err("已停止".into());
        }
        match extract_episode(app, settings, runtime, ffmpeg, episode, work_dir, index) {
            Ok(item) => extracted.push(item),
            Err(error) => {
                let message = format!(
                    "第 {} 集内容提取失败（{}）：{error}",
                    index + 1,
                    episode.file.file_name
                );
                crate::logging::log_event("材料", &message);
                messages.push(message);
            }
        }
    }
    if extracted.is_empty() {
        return Err(format!(
            "抽样的 {sample_count} 集都没有提取到内容，材料线程已停止。{}",
            messages
                .first()
                .map(|message| format!("首个错误：{message}"))
                .unwrap_or_default()
        ));
    }
    let recognized_lines: usize = extracted
        .iter()
        .map(|episode| episode.transcript_lines + episode.ocr_lines)
        .sum();
    if recognized_lines == 0 {
        return Err(
            "抽样分集没有识别到任何对白或画面文字，请检查 ASR/OCR 语言包与画面区域。".into(),
        );
    }
    let evidence = ContentEvidence {
        drama_name: episodes[0]
            .file
            .path
            .parent()
            .and_then(Path::file_name)
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("未命名剧目")
            .to_string(),
        work_dir: work_dir.to_path_buf(),
        manifest: work_dir.join("content.json"),
        episodes: extracted,
        messages,
    };
    let raw = serde_json::to_vec_pretty(&evidence)
        .map_err(|error| format!("序列化内容清单失败：{error}"))?;
    std::fs::write(&evidence.manifest, raw)
        .map_err(|error| format!("写入内容清单失败：{error}"))?;
    Ok(evidence)
}

/// 给 LLM 的紧凑证据：分别限制长度，避免 16 集转写撑爆上下文。
pub(crate) fn evidence_prompt(evidence: &ContentEvidence) -> String {
    let mut out = format!("剧目目录名：{}\n", evidence.drama_name);
    if !evidence.messages.is_empty() {
        out.push_str("注意：部分抽样分集提取失败，以下只包含成功分集的证据；不要补写缺失情节。\n");
    }
    for episode in &evidence.episodes {
        out.push_str(&format!(
            "\n【{}】\n对白：\n{}\n画面文字：\n{}\n",
            episode.file_name,
            cut_text(&episode.transcript_excerpt, 6000),
            cut_text(&episode.ocr_excerpt, 1500),
        ));
    }
    out
}

fn cut_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let result = text.chars().take(max_chars).collect::<String>();
    format!("{result}\n…（已截断）")
}

fn dedup_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for line in lines {
        let text = line.trim();
        if text.len() < 2 || !seen.insert(text.to_string()) {
            continue;
        }
        result.push(text.to_string());
    }
    result
}

fn non_empty(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn ai_source_label(model: &Option<String>) -> String {
    match non_empty(model) {
        Some(name) => format!("AI 生成（模型 {name}）"),
        None => "AI 生成".into(),
    }
}

fn existing_poster(settings: &AppSettings, output_dir: &Path) -> Option<PathBuf> {
    settings
        .poster_path
        .as_ref()
        .filter(|path| path.is_file())
        .cloned()
        .or_else(|| {
            ["png", "jpg", "jpeg", "webp"]
                .iter()
                .map(|ext| output_dir.join(format!("剧目海报.{ext}")))
                .find(|path| path.is_file())
        })
}

/// 保存材料流水线元数据，同时不覆盖用户在材料线程运行期间保存的其他设置。
fn persist_metadata(
    app: &AppHandle,
    title: Option<&str>,
    synopsis: Option<&str>,
    poster: Option<&Path>,
) -> Result<(), String> {
    let path = crate::settings_path(app)?;
    let mut settings = dossier_core::load_settings(&path).settings;
    if let Some(title) = title {
        settings.drama_title = Some(title.to_string());
    }
    if let Some(synopsis) = synopsis {
        settings.drama_synopsis = Some(synopsis.to_string());
    }
    // 剧名/简介已更新时必须同步海报：生成失败就清空，避免旧海报配新文案。
    settings.poster_path = poster.map(Path::to_path_buf);
    save_settings(&path, &settings).map_err(|error| format!("保存材料元数据失败：{error}"))
}

fn local_date_string() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
        + 8 * 3600;
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

#[allow(clippy::too_many_arguments)]
fn write_ai_proof(
    app: &AppHandle,
    context: &MaterialProofContext,
    output_dir: &Path,
    evidence: Option<&ContentEvidence>,
    title: Option<&str>,
    title_source: &str,
    synopsis: Option<&str>,
    synopsis_source: &str,
    poster: Option<&Path>,
    poster_source: &str,
    storyboard_source: &str,
    process_count: usize,
) -> Result<PathBuf, String> {
    let version = app.package_info().version.to_string();
    let payload = json!({
        "date": local_date_string(),
        "version": version,
        "dramaName": context.drama_name,
        "originalEpisodes": context.original_episodes,
        "encoder": context.encoder,
        "processing": format!(
            "按场景切点重组分集并逐集微帧，编码 {}。",
            context.encoder
        ),
        "evidence": evidence.map_or_else(
            || "未提取（ASR/OCR 未就绪或未启用）".to_string(),
            |value| format!("本地 ASR/OCR，共 {} 集", value.episodes.len())
        ),
        "title": title.unwrap_or_default(),
        "titleSource": title_source,
        "synopsis": synopsis.unwrap_or_default(),
        "synopsisSource": synopsis_source,
        "posterName": poster
            .and_then(|path| path.file_name())
            .and_then(|name| name.to_str())
            .unwrap_or_default(),
        "posterSource": poster_source,
        "posterPath": poster
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        "processCount": process_count,
        "storyboardSource": storyboard_source,
    });
    // 优先出图片证明；排版 Python 不可用时退回文本版，保证证明文件不缺位。
    if let Ok(path) = write_ai_proof_image(app, output_dir, &payload) {
        return Ok(path);
    }
    write_ai_proof_text(output_dir, &payload)
}

/// 调 Pillow 脚本把证明字段渲染成 `AI制作证明.png`（toolbox 兜底证明复用）。
pub(crate) fn write_ai_proof_image(
    app: &AppHandle,
    output_dir: &Path,
    payload: &serde_json::Value,
) -> Result<PathBuf, String> {
    let data_path = output_dir.join(".proof-data.json");
    std::fs::write(&data_path, payload.to_string())
        .map_err(|error| format!("写入证明数据失败（{}）：{error}", data_path.display()))?;
    let cleanup = |error: String| {
        let _ = std::fs::remove_file(&data_path);
        error
    };
    let python = crate::toolbox::locate_materials_python(app).map_err(cleanup)?;
    let script = crate::toolbox::materials_script(app, "make_proof_image.py").map_err(cleanup)?;
    let output_path = output_dir.join("AI制作证明.png");
    let spec = CommandSpec::new(python.display().to_string())
        .arg(script.display().to_string())
        .args(["--data", &data_path.display().to_string()])
        .args(["--output", &output_path.display().to_string()])
        .timeout(Duration::from_secs(120));
    let output = SystemRunner::new()
        .run(&spec)
        .map_err(|error| cleanup(format!("证明图片进程启动失败：{error}")))?;
    let _ = std::fs::remove_file(&data_path);
    if !output.success() {
        let mut tail: Vec<&str> = output.stderr.lines().rev().take(3).collect();
        tail.reverse();
        let tail = tail.join("\n");
        return Err(format!(
            "证明图片脚本退出码 {}：{}",
            output.code_label(),
            if tail.is_empty() {
                "（无输出）"
            } else {
                &tail
            }
        ));
    }
    if !output_path.is_file() {
        return Err("证明图片脚本未产出文件".into());
    }
    Ok(output_path)
}

/// 兜底文本版证明：字段拼成 `AI制作证明.txt`（排版 Python 不可用时）。
pub(crate) fn write_ai_proof_text(
    output_dir: &Path,
    payload: &serde_json::Value,
) -> Result<PathBuf, String> {
    crate::logging::log_event(
        "材料",
        "AI 制作证明图片生成失败，已退回文本版（材料排版 Python 环境不可用）",
    );
    let get = |key: &str| payload[key].as_str().unwrap_or_default();
    let line = |label: &str, value: &str, source: &str| {
        format!(
            "{label}：{}\n来源：{source}\n",
            if value.is_empty() { "未填写" } else { value }
        )
    };
    let mut body = String::new();
    body.push_str("AI 制作证明\n\n");
    body.push_str(&format!("生成日期：{}（本地时间）\n", get("date")));
    body.push_str(&format!("制作工具：DramaDossier v{}\n", get("version")));
    body.push_str(&format!(
        "原剧目：{}（原始素材 {} 集）\n",
        get("dramaName"),
        payload["originalEpisodes"].as_u64().unwrap_or(0)
    ));
    body.push_str(&format!("处理方式：{}\n\n", get("processing")));
    body.push_str(&format!("内容证据：{}\n\n", get("evidence")));
    body.push_str(&line("剧名", get("title"), get("titleSource")));
    body.push_str(&line("简介", get("synopsis"), get("synopsisSource")));
    body.push_str(&line("海报", get("posterName"), get("posterSource")));
    body.push_str(&format!(
        "过程图：已生成 {} 张（画面抽自原始素材代表集）\n来源：{}\n\n",
        payload["processCount"].as_u64().unwrap_or(0),
        get("storyboardSource"),
    ));
    body.push_str(
        "AI 声明：本目录中的剧名、简介、海报与过程图由人工智能辅助生成或整理，\
         分集重组与微帧由 DramaDossier 自动化完成，原始视频素材未作改动。\
         报审前请人工复核上述内容。\n",
    );
    let path = output_dir.join("AI制作证明.txt");
    std::fs::write(&path, body)
        .map_err(|error| format!("写入 AI 制作证明失败（{}）：{error}", path.display()))?;
    Ok(path)
}

/// 给界面回传海报预览；超过 8 MiB 的图片不塞进 IPC，界面仍可用路径打开。
fn poster_preview_data_url(path: &Path) -> Option<String> {
    const MAX_PREVIEW_BYTES: usize = 8 * 1024 * 1024;
    let bytes = std::fs::read(path).ok()?;
    if bytes.is_empty() || bytes.len() > MAX_PREVIEW_BYTES {
        return None;
    }
    let mime = if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else {
        return None;
    };
    use base64::Engine as _;
    Some(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

/// 生成 4 张过程图：直接从原始抽样分集抽帧，不等待深度处理成片。
fn generate_process_images(
    app: &AppHandle,
    ffmpeg: &Path,
    episodes: &[ProbedEpisode],
    output_dir: &Path,
) -> Result<Vec<PathBuf>, String> {
    let frames_dir = output_dir.join(".material-frames");
    let _ = std::fs::remove_dir_all(&frames_dir);
    // 上一次运行的过程图不能被本次失败继承；先删掉，成功后再返回实际文件。
    for index in 1..=4 {
        let _ = std::fs::remove_file(output_dir.join(format!("过程图_{index:02}.png")));
    }
    std::fs::create_dir_all(&frames_dir)
        .map_err(|error| format!("创建过程图帧目录失败：{error}"))?;
    let mut frames = Vec::new();
    let mut labels = Vec::new();
    for slot in 0..4usize {
        if crate::toolbox::transcode_cancelled() {
            let _ = std::fs::remove_dir_all(&frames_dir);
            return Err("已停止".into());
        }
        let _ = app.emit(
            "material-progress",
            json!({
                "stage": "process-images",
                "label": format!("材料线程：抽取过程图素材 {}/4", slot + 1),
            }),
        );
        let episode = &episodes[slot % episodes.len()];
        let duration = episode.spec.duration_sec.max(1.0);
        for (step, percent) in [0.10f64, 0.40, 0.70].into_iter().enumerate() {
            let path = frames_dir.join(format!("frame-{slot:02}-{step}.jpg"));
            if let Err(error) = crate::toolbox::run_ffmpeg(
                ffmpeg,
                &[
                    "-y",
                    "-ss",
                    &format!("{:.3}", duration * percent),
                    "-i",
                    &episode.file.path.display().to_string(),
                    "-frames:v",
                    "1",
                    "-q:v",
                    "3",
                    &path.display().to_string(),
                ],
            ) {
                let _ = std::fs::remove_dir_all(&frames_dir);
                return Err(format!("抽取过程图帧失败：{error}"));
            }
            // 帧的真实出处（集号 + 集内时间码），脚本渲染成右上角角标。
            let seconds = (duration * percent) as u64;
            labels.push(format!(
                "第{}集 {:02}:{:02}",
                slot + 1,
                seconds / 60,
                seconds % 60
            ));
            frames.push(path);
        }
    }
    let python = crate::toolbox::locate_materials_python(app)?;
    let script =
        crate::toolbox::materials_script(app, "make_process_images.py").inspect_err(|_| {
            let _ = std::fs::remove_dir_all(&frames_dir);
        })?;
    let spec = CommandSpec::new(python.display().to_string())
        .arg(script.display().to_string())
        .args(["--output-dir", &output_dir.display().to_string()])
        .args([
            "--frames",
            &frames
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(","),
        ])
        .args(["--labels", &labels.join("|")])
        .timeout(Duration::from_secs(120));
    let output = SystemRunner::new().run(&spec).map_err(|error| {
        let _ = std::fs::remove_dir_all(&frames_dir);
        format!("过程图进程启动失败：{error}")
    })?;
    let _ = std::fs::remove_dir_all(&frames_dir);
    if !output.success() {
        let mut tail: Vec<&str> = output.stderr.lines().rev().take(4).collect();
        tail.reverse();
        let detail = tail.join("\n");
        return Err(format!(
            "过程图脚本退出码 {}：{detail}",
            output.code_label()
        ));
    }
    Ok((1..=4)
        .map(|index| output_dir.join(format!("过程图_{index:02}.png")))
        .filter(|path| path.is_file())
        .collect())
}

/// 后台材料任务句柄：编码线程可以继续跑，最后再汇合。
pub(crate) struct MaterialJob {
    handle: Option<JoinHandle<()>>,
    result: Arc<Mutex<Option<Result<MaterialPipelineReport, String>>>>,
}

impl MaterialJob {
    pub(crate) fn join(mut self) -> Option<Result<MaterialPipelineReport, String>> {
        let panicked = self
            .handle
            .take()
            .map(|handle| handle.join().is_err())
            .unwrap_or(false);
        if panicked {
            return Some(Err("材料线程意外退出（panic），自动材料未完成".into()));
        }
        match self.result.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => Some(Err("读取材料线程结果失败（锁已损坏）".into())),
        }
    }
}

/// 在深度处理编码前启动材料线程。
#[allow(clippy::too_many_arguments)]
pub(crate) fn start_material_pipeline(
    app: &AppHandle,
    settings: &AppSettings,
    runtime: &ModelRuntimeStatus,
    ffmpeg: &Path,
    episodes: &[ProbedEpisode],
    output_dir: &Path,
    drama_dir: &Path,
    proof: MaterialProofContext,
) -> MaterialJob {
    let result = Arc::new(Mutex::new(None));
    let thread_result = Arc::clone(&result);
    let app = app.clone();
    let settings = settings.clone();
    let runtime = runtime.clone();
    let ffmpeg = ffmpeg.to_path_buf();
    let episodes = episodes.to_vec();
    let output_dir = output_dir.to_path_buf();
    let drama_dir = drama_dir.to_path_buf();
    let handle = std::thread::spawn(move || {
        let value = material_pipeline_blocking(
            &app,
            &settings,
            &runtime,
            &ffmpeg,
            &episodes,
            &output_dir,
            &drama_dir,
            &proof,
        );
        if let Ok(mut guard) = thread_result.lock() {
            *guard = Some(value);
        }
    });
    MaterialJob {
        handle: Some(handle),
        result,
    }
}

/// 自动材料主流程：内容提取 → LLM → 元数据/海报 → 过程图。
#[allow(clippy::too_many_arguments)]
fn material_pipeline_blocking(
    app: &AppHandle,
    settings: &AppSettings,
    runtime: &ModelRuntimeStatus,
    ffmpeg: &Path,
    episodes: &[ProbedEpisode],
    output_dir: &Path,
    drama_dir: &Path,
    proof: &MaterialProofContext,
) -> Result<MaterialPipelineReport, String> {
    let mut messages = Vec::new();
    if !settings.effective_material_pipeline_enabled() {
        return Ok(MaterialPipelineReport {
            status: "disabled".into(),
            title: None,
            synopsis: None,
            poster: None,
            process_images: Vec::new(),
            proof: None,
            evidence: None,
            messages: vec!["自动材料流水线已在设置中关闭".into()],
            title_source: "未启用".into(),
            synopsis_source: "未启用".into(),
            poster_source: "未启用".into(),
            storyboard_source: "未启用".into(),
            poster_data_url: None,
        });
    }

    std::fs::create_dir_all(output_dir)
        .map_err(|error| format!("创建输出目录失败（{}）：{error}", output_dir.display()))?;
    // 清单与证据每次重建；旧海报保留到替换成功或新海报明确失败，避免失效元数据。
    let _ = std::fs::remove_file(output_dir.join("material-manifest.json"));
    let _ = std::fs::remove_dir_all(output_dir.join(".material-content"));

    let evidence = if runtime.ready {
        let _ = app.emit(
            "material-progress",
            json!({
                "stage": "extract",
                "label": "材料线程：正在提取视频内容",
            }),
        );
        match extract_content(
            app,
            settings,
            runtime,
            ffmpeg,
            episodes,
            &output_dir.join(".material-content"),
        ) {
            Ok(evidence) => {
                messages.extend(evidence.messages.iter().cloned());
                crate::logging::log_event(
                    "材料",
                    format!(
                        "内容提取完成：{} 集，清单 {}",
                        evidence.episodes.len(),
                        evidence.manifest.display()
                    ),
                );
                Some(evidence)
            }
            Err(error) if error.contains("已停止") => return Err(error),
            Err(error) => {
                messages.push(format!("视频内容提取失败，改用已有文案：{error}"));
                None
            }
        }
    } else {
        messages.push(format!(
            "ASR/OCR 未就绪，跳过视频内容提取：{}",
            runtime.guidance
        ));
        None
    };
    if crate::toolbox::transcode_cancelled() {
        return Err("已停止".into());
    }

    let llm_ready = ai::storyboard_llm_ready(settings);
    let _ = app.emit(
        "material-progress",
        json!({
            "stage": "ai",
            "label": if llm_ready { "材料线程：正在生成剧名与简介" } else { "材料线程：LLM 未配置，整理已有文案" },
        }),
    );
    let identity = if llm_ready {
        let context = evidence.as_ref().map(evidence_prompt).unwrap_or_else(|| {
            // ASR/OCR 不可用时至少给出剧目目录名、集数与已有文案，
            // 否则模型拿不到任何剧情线索，只能输出空洞的模板话。
            let mut context = format!(
                "剧目目录名：{}；原始素材 {} 集。",
                proof.drama_name, proof.original_episodes
            );
            if let Some(title) = non_empty(&settings.drama_title) {
                context.push_str(&format!("已有剧名：{title}。"));
            }
            if let Some(synopsis) = non_empty(&settings.drama_synopsis) {
                context.push_str(&format!("已有简介：{synopsis}"));
            }
            context
        });
        match ai::material_identity_blocking(settings, &context) {
            Ok(identity) => Some(identity),
            Err(error) => {
                messages.push(format!("AI 文案生成失败，沿用已有内容：{error}"));
                None
            }
        }
    } else {
        messages.push("LLM 未配置，剧名/简介沿用已有内容。".into());
        None
    };

    let existing_title = non_empty(&settings.drama_title);
    let existing_synopsis = non_empty(&settings.drama_synopsis);
    let title_ai_on = settings.ai_title_enabled != Some(false);
    let synopsis_ai_on = settings.ai_synopsis_enabled != Some(false);
    let llm_label = ai_source_label(&settings.llm_model);

    let (title, title_source) = if let Some(title) = existing_title.clone() {
        let source = if title_ai_on {
            llm_label.clone()
        } else {
            "本地提供".into()
        };
        (Some(title), source)
    } else if title_ai_on {
        match identity.as_ref().map(|identity| identity.title.clone()) {
            Some(title) => (Some(title), llm_label.clone()),
            None => (None, "AI 生成失败，未填写".into()),
        }
    } else {
        (None, "未填写（本地模式）".into())
    };

    let (synopsis, synopsis_source) = if let Some(synopsis) = existing_synopsis.clone() {
        let source = if synopsis_ai_on {
            llm_label.clone()
        } else {
            "本地提供".into()
        };
        (Some(synopsis), source)
    } else if synopsis_ai_on {
        match identity.as_ref().map(|identity| identity.synopsis.clone()) {
            Some(synopsis) => (Some(synopsis), llm_label.clone()),
            None => (None, "AI 生成失败，未填写".into()),
        }
    } else {
        (None, "未填写（本地模式）".into())
    };

    // 过程图不再携带分镜概要文字（纯原片帧 + 角标），来源固定为原始素材抽帧。
    let storyboard_source = "原始素材抽帧".to_string();

    let old_poster = existing_poster(settings, output_dir);
    let text_changed = title != existing_title || synopsis != existing_synopsis;
    let poster_ai_on = settings.ai_poster_enabled != Some(false);
    let should_generate_poster = poster_ai_on && (old_poster.is_none() || text_changed);
    let _ = app.emit(
        "material-progress",
        json!({
            "stage": "poster",
            "label": if should_generate_poster { "材料线程：正在生成海报" } else { "材料线程：检查已有海报" },
        }),
    );
    let (poster, poster_source) = if should_generate_poster {
        match ai::poster_blocking(
            settings,
            output_dir,
            title.as_deref().unwrap_or_default(),
            synopsis.as_deref().unwrap_or_default(),
        ) {
            Ok(path) => (Some(path), ai_source_label(&settings.img_model)),
            Err(error) => {
                if let Some(path) = old_poster.as_ref() {
                    let _ = std::fs::remove_file(path);
                }
                messages.push(format!("海报生成失败（其余材料继续）：{error}"));
                // 兜底：复制素材目录里的封面（报审素材按规范都带封面）。
                match crate::toolbox::copy_local_cover(drama_dir, output_dir) {
                    Some(path) => (Some(path), "本地提供（素材封面）".into()),
                    None => (None, "AI 生成失败，未生成".into()),
                }
            }
        }
    } else if let Some(path) = old_poster {
        let source = if poster_ai_on {
            "已生成（本次复用）".into()
        } else {
            "本地提供".into()
        };
        (Some(path), source)
    } else {
        let source = if poster_ai_on {
            "AI 生成失败，未生成".into()
        } else {
            "未提供（本地模式）".into()
        };
        (None, source)
    };

    if crate::toolbox::transcode_cancelled() {
        return Err("已停止".into());
    }
    let _ = app.emit(
        "material-progress",
        json!({
            "stage": "process-images",
            "label": "材料线程：正在生成过程图",
        }),
    );
    let process_images = match generate_process_images(app, ffmpeg, episodes, output_dir) {
        Ok(paths) => paths,
        Err(error) => {
            messages.push(format!("过程图生成失败：{error}"));
            Vec::new()
        }
    };

    if crate::toolbox::transcode_cancelled() {
        return Err("已停止".into());
    }
    // 简介落盘：输出目录带一份 简介.txt（AI 生成或素材目录原有，随成片一起交付）。
    if let Some(text) = synopsis.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        if let Err(error) = std::fs::write(output_dir.join("简介.txt"), text) {
            messages.push(format!("简介.txt 写入失败：{error}"));
        }
    }
    let proof_path = match write_ai_proof(
        app,
        proof,
        output_dir,
        evidence.as_ref(),
        title.as_deref(),
        &title_source,
        synopsis.as_deref(),
        &synopsis_source,
        poster.as_deref(),
        &poster_source,
        &storyboard_source,
        process_images.len(),
    ) {
        Ok(path) => Some(path),
        Err(error) => {
            messages.push(error);
            None
        }
    };
    if let Err(error) = persist_metadata(
        app,
        title.as_deref(),
        synopsis.as_deref(),
        poster.as_deref(),
    ) {
        messages.push(error);
    }

    let status = if messages.is_empty() {
        "completed"
    } else {
        "partial"
    };
    let poster_data_url = poster.as_deref().and_then(poster_preview_data_url);
    let report = MaterialPipelineReport {
        status: status.into(),
        title,
        synopsis,
        poster,
        process_images,
        proof: proof_path,
        evidence,
        messages,
        title_source,
        synopsis_source,
        poster_source,
        storyboard_source,
        poster_data_url,
    };
    let manifest = output_dir.join("material-manifest.json");
    // data URL 只用于本次界面的预览回包，落盘清单保留路径即可，避免文件膨胀。
    let mut manifest_report = report.clone();
    manifest_report.poster_data_url = None;
    if let Ok(raw) = serde_json::to_vec_pretty(&manifest_report) {
        let _ = std::fs::write(manifest, raw);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dossier_runtime::{CommandOutput, FakeRunner};

    #[test]
    fn parses_asr_engine_aliases() {
        assert_eq!(AsrEngine::parse(None), AsrEngine::OpenAi);
        assert_eq!(AsrEngine::parse(Some("WHISPER-CPP")), AsrEngine::WhisperCpp);
        assert_eq!(AsrEngine::parse(Some("cpp")), AsrEngine::WhisperCpp);
        assert_eq!(
            serde_json::to_string(&AsrEngine::OpenAi).unwrap(),
            "\"openai\""
        );
        assert_eq!(
            serde_json::to_string(&AsrEngine::WhisperCpp).unwrap(),
            "\"whisper_cpp\""
        );
    }

    #[test]
    fn truncates_long_evidence_with_marker() {
        assert_eq!(cut_text("abc", 2), "ab\n…（已截断）");
        assert_eq!(cut_text("中文", 10), "中文");
    }

    #[test]
    fn dedups_confident_caption_lines() {
        let lines = vec!["龙", "龙", "继续说", " "];
        assert_eq!(dedup_lines(lines.into_iter()), vec!["龙", "继续说"]);
    }

    #[test]
    fn poster_preview_only_accepts_small_png_or_jpeg() {
        let root = std::env::temp_dir().join(format!(
            "dossier-poster-preview-{}-{}",
            std::process::id(),
            std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let png = root.join("poster.png");
        std::fs::write(&png, [0x89, b'P', b'N', b'G', 0, 0, 0]).unwrap();
        assert!(poster_preview_data_url(&png)
            .unwrap()
            .starts_with("data:image/png;base64,"));

        let text = root.join("poster.txt");
        std::fs::write(&text, b"not an image").unwrap();
        assert!(poster_preview_data_url(&text).is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn model_runtime_requires_configured_files_and_chinese_ocr() {
        let root = std::env::temp_dir().join(format!(
            "dossier-model-runtime-{}-{}",
            std::process::id(),
            std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let whisper = root.join("whisper");
        std::fs::write(&whisper, b"fake").unwrap();
        let mut settings = AppSettings::default();
        let whisper_program = whisper.display().to_string();
        settings.asr_executable = Some(whisper);

        // 注意规则顺序：FakeRunner 按插入顺序取第一个命中，
        // 通用 on("tesseract") 必须放在 on_match 之后，否则 --list-langs 会拿到版本输出。
        let runner = FakeRunner::new()
            .on(&whisper_program, CommandOutput::ok("whisper 1.0"))
            .on_match(
                "tesseract",
                "--list-langs",
                CommandOutput::ok("eng\nchi_sim"),
            )
            .on("tesseract", CommandOutput::ok("tesseract 5"));
        let ready = model_runtime(&settings, &runner);
        assert!(ready.ready, "{}", ready.guidance);

        let no_chinese = FakeRunner::new()
            .on(&whisper_program, CommandOutput::ok("whisper 1.0"))
            .on_match("tesseract", "--list-langs", CommandOutput::ok("eng"))
            .on("tesseract", CommandOutput::ok("tesseract 5"));
        let missing = model_runtime(&settings, &no_chinese);
        // OCR 是可选增强：缺中文语言包不影响"内容模型就绪"，只降级为仅用对白
        assert!(missing.ready, "OCR 缺失不应阻塞就绪：{}", missing.guidance);
        assert!(
            missing.guidance.contains("只依据对白"),
            "要说明后果：{}",
            missing.guidance
        );
        // 降级链的说明里要点名 tesseract 缺的是什么
        assert!(
            missing.guidance.contains("tesseract"),
            "要说明是哪一级不可用：{}",
            missing.guidance
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
