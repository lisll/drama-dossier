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

use dossier_core::{AppSettings, ProbedEpisode};
use dossier_media::asr::{AsrPlan, WhisperFlavor};
use dossier_media::ocr::{Region, RegionFramePlan};
use dossier_media::render::TimeRange;
use dossier_runtime::probe::probe_tool;
use dossier_runtime::{CommandRunner, CommandSpec, SystemRunner, ToolReport};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::{AppHandle, Emitter};

use crate::ai;
use crate::tasks::CancelToken;
use crate::toolbox::FfmpegCtx;

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
    /// 沿用的海报配的是旧剧名/简介（处理中不重做，费用由用户在结果区确认后才产生）。
    pub poster_stale: bool,
    /// 海报过期且可以按新文案重做（AI 海报开着且生图接口已配置）。
    pub poster_regenerable: bool,
    /// 当前这张海报是按哪个剧名 / 简介生成的（AI 生成或重做时写入，沿用时原样带过来）。
    /// 过期判断拿它和本次文案比，提示会一直持续到重做为止。
    /// `None` = 依据未知（旧目录没有清单可查），清单里不写这一项，下次退回旧的判断方式。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster_basis: Option<PosterBasis>,
    /// 海报的 data URL，供转码页直接预览；落盘清单里不包含它。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster_data_url: Option<String>,
    /// AI 制作证明字段：材料线程组装，深度处理收尾时补上成片清单与处理方式后出证明
    /// （见 [`finalize_deep_proof`]）。不发给界面，落盘时单独写进清单的 `proofPayload`。
    #[serde(skip)]
    pub proof_payload: Option<serde_json::Value>,
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
            poster_stale: false,
            poster_regenerable: false,
            poster_basis: None,
            poster_data_url: None,
            proof_payload: None,
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
    model_runtime_with_probe(settings, runner, OcrBackendProbe::detect())
}

/// OCR 降级链前两级（PaddleOCR / 系统 OCR）的本机探测结果。
///
/// 这两级无法用 FakeRunner 桩掉（是纯本机探测，不走命令行），生产路径来自
/// 真实探测；单元测试注入固定值，避免结果随测试机环境漂移——中文 Windows
/// 自带系统 OCR 中文语言包，会把「tesseract 缺 chi_sim」的用例误判成可用。
#[derive(Debug, Clone)]
pub(crate) struct OcrBackendProbe {
    pub paddle_program: Option<PathBuf>,
    pub system_available: bool,
}

impl OcrBackendProbe {
    /// 真实探测本机；生产调用统一走这里。
    pub(crate) fn detect() -> Self {
        Self {
            paddle_program: dossier_media::ocr::paddle::find_program(),
            system_available: dossier_media::ocr::system::available(),
        }
    }
}

/// 同 [`model_runtime`]，但 OCR 前两级后端的探测结果由调用方注入（测试用）。
pub(crate) fn model_runtime_with_probe(
    settings: &AppSettings,
    runner: &dyn CommandRunner,
    probe: OcrBackendProbe,
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
    let backend_env = dossier_media::ocr_backend::Environment {
        paddle_program: probe.paddle_program,
        // RapidOCR 本轮不做（引擎选型未定），自检恒为「没有」，降级链会跳过这一级。
        rapid_program: None,
        // 与实际识别用同一个系统接口判断，不需要管理员权限（见 OcrBackendProbe::detect）。
        system_available: probe.system_available,
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

/// 取命令输出中最后一条非空行：ASR 失败时 stderr 的最后一行最有诊断价值。
fn last_line(output: &dossier_runtime::CommandOutput) -> String {
    output
        .combined()
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("（无输出）")
        .to_string()
}

/// GPU 转写失败、但错误指向「CUDA 不可用」（典型：asr-env 里的 torch 装成了
/// CPU 版，whisper 在加载模型那一步就崩）。此时用 `--device cpu` 重跑是值得的；
/// 其他错误（音频解码、模型缺失等）重跑也不会好，返回 false。
fn asr_gpu_failure_worth_cpu_retry(detail: &str) -> bool {
    detail.contains("on a CUDA device") || detail.contains("torch.cuda.is_available()")
}

/// 把一集视频转成“对白 + 画面文字”证据。
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn extract_episode(
    app: &AppHandle,
    settings: &AppSettings,
    runtime: &ModelRuntimeStatus,
    ffmpeg: &Path,
    episode: &ProbedEpisode,
    work_dir: &Path,
    index: usize,
    cancel: &CancelToken,
) -> Result<EpisodeEvidence, String> {
    if cancel.is_cancelled() {
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
        FfmpegCtx::light(cancel),
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
    // GPU 转写失败要能降级 CPU 重试，所以按当前 plan 现算命令（含 PATH 注入）。
    let mut build_spec = |gpu: bool| -> Result<CommandSpec, String> {
        plan.gpu = Some(gpu);
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
        Ok(spec)
    };
    let mut output = runner
        .run_cancellable(&build_spec(has_gpu)?, &|| cancel.is_cancelled())
        .map_err(|error| format!("ASR 执行失败：{error}"))?;
    if !output.success() {
        let mut detail = last_line(&output);
        // 「机器有 NVIDIA」不代表 asr-env 里的 torch 真带 CUDA（旧版安装/升级失败
        // 都会装成 CPU 版），此时 whisper 在加载模型那一步就崩。自动降级 CPU 重跑
        // 一次，别让整轮材料报废。
        if has_gpu && asr_gpu_failure_worth_cpu_retry(&detail) {
            crate::logging::log_event(
                "材料",
                "GPU 转写不可用（torch 不带 CUDA），自动改用 CPU 重新转写",
            );
            output = runner
                .run_cancellable(&build_spec(false)?, &|| cancel.is_cancelled())
                .map_err(|error| format!("ASR 执行失败：{error}"))?;
            detail = last_line(&output);
        }
        if !output.success() {
            return Err(format!("ASR 退出码 {}：{detail}", output.code_label()));
        }
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
    cancel: &CancelToken,
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
        if cancel.is_cancelled() {
            return Err("已停止".into());
        }
        match extract_episode(
            app, settings, runtime, ffmpeg, episode, work_dir, index, cancel,
        ) {
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

/// 本次输出目录里已有的海报（上次深度处理的产物）。只认输出目录内的文件：
/// 输出目录由归属标记（`.dossier-owner.json`，转码开始前检查）保证只属于本剧，
/// 所以这里不会拿到别的剧的海报；材料线程只沿用它，只有用户确认的重做海报才会替换它。
fn existing_poster(output_dir: &Path) -> Option<PathBuf> {
    ["png", "jpg", "jpeg", "webp"]
        .iter()
        .map(|ext| output_dir.join(format!("剧目海报.{ext}")))
        .find(|path| path.is_file())
}

/// AI 文案这一步的结局，决定「没有现成值时」怎么如实标注来源。
#[derive(Debug, Clone, PartialEq, Eq)]
enum AiText {
    /// 来源开关关闭（本地模式）。
    Disabled,
    /// 开关开着，但 LLM 未配置。
    NotConfigured,
    /// 调用了但失败。
    Failed,
    /// 本次真的生成了。
    Generated(String),
}

/// 一项文字材料的最终取值与来源说明（来源会写进 AI 制作证明，必须如实）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Resolved {
    value: Option<String>,
    source: String,
}

const SOURCE_SAVED: &str = "沿用（本剧已保存）";
const SOURCE_LOCAL_DIR: &str = "本地提供（素材目录）";

/// 没有现成值时，按 AI 结局给出取值与来源。
fn from_ai(ai: &AiText, llm_label: &str) -> Resolved {
    let (value, source) = match ai {
        AiText::Generated(text) => (Some(text.clone()), llm_label.to_string()),
        AiText::Disabled => (None, "未填写（本地模式）".to_string()),
        AiText::NotConfigured => (None, "未填写（未配置 LLM）".to_string()),
        AiText::Failed => (None, "AI 生成失败，未填写".to_string()),
    };
    Resolved { value, source }
}

/// 剧名：本剧已保存 → AI → 不填。
fn resolve_title(saved: Option<String>, ai: &AiText, llm_label: &str) -> Resolved {
    match saved {
        Some(value) => Resolved {
            value: Some(value),
            source: SOURCE_SAVED.into(),
        },
        None => from_ai(ai, llm_label),
    }
}

/// 简介：素材目录 `简介*.txt` → 本剧已保存 → AI → 不填。
///
/// 素材目录的文件排最前：用户放了（或改了）简介文件就一定生效，不会被以前保存的值挡住。
fn resolve_synopsis(
    saved: Option<String>,
    local: Option<String>,
    ai: &AiText,
    llm_label: &str,
) -> Resolved {
    if let Some(value) = local {
        return Resolved {
            value: Some(value),
            source: SOURCE_LOCAL_DIR.into(),
        };
    }
    if let Some(value) = saved {
        return Resolved {
            value: Some(value),
            source: SOURCE_SAVED.into(),
        };
    }
    from_ai(ai, llm_label)
}

/// 海报的处理方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PosterAction {
    /// 调生图接口（失败再退回素材封面）。
    Generate,
    /// 沿用本次输出目录里已有的海报。
    ReuseExisting,
    /// 复制素材目录里的封面。
    CopyLocalCover,
    /// 没有任何来源。
    Missing,
}

/// 海报：本次输出目录已有 → 一律沿用（处理中不重做：生图要花钱，文案变了由用户在结果区确认后再重做）；
/// 没有时 AI（开且已配置）→ 素材封面 → 无。
fn decide_poster(
    ai_on: bool,
    api_ready: bool,
    has_existing: bool,
    has_local_cover: bool,
) -> PosterAction {
    if has_existing {
        PosterAction::ReuseExisting
    } else if ai_on && api_ready {
        PosterAction::Generate
    } else if has_local_cover {
        PosterAction::CopyLocalCover
    } else {
        PosterAction::Missing
    }
}

const MANIFEST_FILE: &str = "material-manifest.json";

/// 一张海报所依据的文案（清单里的 `posterBasis`）。
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PosterBasis {
    pub title: Option<String>,
    pub synopsis: Option<String>,
}

impl PosterBasis {
    fn of(title: Option<&str>, synopsis: Option<&str>) -> Self {
        Self {
            title: title.map(str::to_string),
            synopsis: synopsis.map(str::to_string),
        }
    }
}

/// 上一次深度处理的清单里，判断海报是否过期所需的字段。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct PreviousManifest {
    title: Option<String>,
    synopsis: Option<String>,
    poster_source: Option<String>,
    /// 当时那张海报所依据的文案；旧版本清单没有这一项。
    poster_basis: Option<PosterBasis>,
}

impl PreviousManifest {
    /// 上一张海报所依据的文案：清单记了就用记下的；旧版本清单没记，退回清单里的剧名与简介。
    fn effective_poster_basis(&self) -> PosterBasis {
        self.poster_basis
            .clone()
            .unwrap_or_else(|| PosterBasis::of(self.title.as_deref(), self.synopsis.as_deref()))
    }
}

/// 原子写入清单：写到一半失败时上一份清单原样保留（过期判断与重做海报都依赖它）。
fn write_manifest(output_dir: &Path, value: &serde_json::Value) -> Result<(), String> {
    let path = output_dir.join(MANIFEST_FILE);
    let raw = serde_json::to_vec_pretty(value).map_err(|error| format!("生成清单失败：{error}"))?;
    crate::paths::write_atomic(&path, &raw)
        .map_err(|error| format!("写入清单失败（{}）：{error}", path.display()))
}

/// 读上一次的清单；文件不存在或损坏都按「没有上次清单」处理（只影响是否提示海报过期）。
fn read_previous_manifest(output_dir: &Path) -> Option<PreviousManifest> {
    let raw = std::fs::read(output_dir.join(MANIFEST_FILE)).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    let text = |key: &str| value[key].as_str().map(str::to_string);
    Some(PreviousManifest {
        title: text("title"),
        synopsis: text("synopsis"),
        poster_source: text("posterSource"),
        poster_basis: value
            .get("posterBasis")
            .and_then(|basis| serde_json::from_value(basis.clone()).ok()),
    })
}

/// 沿用海报时来源写成「沿用（…；原为 X）」；连续多次沿用时取出最初的来源 X，避免越套越长。
const REUSE_ORIGIN_MARK: &str = "原为 ";

fn original_poster_source(label: &str) -> &str {
    match label.rfind(REUSE_ORIGIN_MARK) {
        // 只去掉「沿用（…）」自己的那一个收尾括号：原来源本身可能也以「）」结尾，如「AI 生成（模型 m）」。
        Some(at) if label.starts_with("沿用（") => {
            let rest = &label[at + REUSE_ORIGIN_MARK.len()..];
            rest.strip_suffix('）').unwrap_or(rest)
        }
        _ => label,
    }
}

/// 沿用输出目录已有海报时的来源说明：带上它最初的来源，`stale` 时写明配的是旧文案。
fn reused_poster_source(previous: Option<&str>, stale: bool) -> String {
    let reason = if stale {
        "文案已变，海报配的是旧剧名/简介"
    } else {
        "本次输出目录已有"
    };
    match previous
        .map(original_poster_source)
        .filter(|s| !s.is_empty())
    {
        Some(origin) => format!("沿用（{reason}；{REUSE_ORIGIN_MARK}{origin}）"),
        None => format!("沿用（{reason}）"),
    }
}

/// 沿用的海报是否配的是旧文案：
/// - 上次海报是 AI 生成的，且本次最终剧名或简介与海报所依据的文案不同 → 过期；
/// - 没有上次清单，但本次 AI 新生成了剧名或简介 → 过期（海报一定早于这份文案）；
/// - 上次海报是素材封面等非 AI 来源 → 不过期（它本来就不随文案变化）。
fn poster_is_stale(
    previous: Option<&PreviousManifest>,
    title: Option<&str>,
    synopsis: Option<&str>,
    generated_any: bool,
) -> bool {
    match previous {
        Some(previous) => {
            let ai_poster = previous
                .poster_source
                .as_deref()
                .is_some_and(|source| original_poster_source(source).starts_with("AI 生成"));
            // 拿「海报当初所依据的文案」比，而不是上一份清单的文案：后者每跑一次就被改写成新文案，
            // 过期提示只会出现一次；前者在重做之前一直不变，提示会持续到重做（或文案改回原样）为止。
            let basis = previous.effective_poster_basis();
            ai_poster && (basis.title.as_deref() != title || basis.synopsis.as_deref() != synopsis)
        }
        None => generated_any,
    }
}

/// 把本次 **AI 新生成** 的剧名/简介写回本剧材料；沿用或读自素材目录的值不回写
/// （素材目录里的简介以文件为准，回写后用户改文件就不再生效）。只动这一部剧。
fn persist_generated(
    app: &AppHandle,
    material_key: &str,
    title: Option<&str>,
    synopsis: Option<&str>,
) -> Result<(), String> {
    if title.is_none() && synopsis.is_none() {
        return Ok(());
    }
    let path = crate::settings_path(app)?;
    dossier_core::update_settings(&path, |settings| {
        let mut materials = settings.materials_for(material_key);
        if let Some(title) = title {
            materials.title = Some(title.to_string());
        }
        if let Some(synopsis) = synopsis {
            materials.synopsis = Some(synopsis.to_string());
        }
        settings.set_materials_for(material_key, materials);
        Ok(())
    })
    .map(|_| ())
    .map_err(|error| format!("保存材料元数据失败：{error}"))
}

/// 当前日期（**北京时间** UTC+8，与系统时区无关）。证明里按此标注为「北京时间」。
fn beijing_date_string() -> String {
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

/// 证明里的一条文件指纹，与报审包清单的 `{fileName, size, sha256}` 同口径。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProofFile {
    pub file_name: String,
    pub size: u64,
    pub sha256: String,
}

/// 逐个计算文件指纹（流式 SHA-256）。每个文件之间检查停止令牌；任何一个读不到都整体报错，
/// 证明里不会出现缺项的清单。
pub(crate) fn proof_files(
    paths: &[PathBuf],
    cancel: &CancelToken,
) -> Result<Vec<ProofFile>, String> {
    paths
        .iter()
        .map(|path| {
            if cancel.is_cancelled() {
                return Err("已停止".to_string());
            }
            let size = std::fs::metadata(path)
                .map_err(|error| format!("读取 {} 失败：{error}", path.display()))?
                .len();
            let sha256 = dossier_core::sha256_file(path)
                .map_err(|error| format!("计算 {} 的 SHA-256 失败：{error}", path.display()))?;
            Ok(ProofFile {
                file_name: path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                size,
                sha256,
            })
        })
        .collect()
}

/// 本次深度处理实际执行了哪些处理（编码结束才完整可知）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProofProcessing {
    /// 编码器与并行路数，如「libx264（2 路并行）」
    pub encoder: String,
    /// 去重配方种子；`None` = 没有启用去重配方（也就没有改写元数据）
    pub dedup_seed: Option<u64>,
    /// 烧录的 AI 角标文案；`None` = 没有烧录
    pub badge_text: Option<String>,
}

/// 证明「处理方式」的逐行文字（纯函数，便于单测）。与深度处理的实际开关一一对应，
/// 去重配方写入的元数据也如实列出（见 `dedup.rs` 的 `metadata_tags`）。
pub(crate) fn processing_lines(processing: &ProofProcessing) -> Vec<String> {
    let mut lines = vec![
        "源文件原样保留，成片为重新编码的新文件".to_string(),
        "按场景切点重组分集".to_string(),
        "逐集微帧".to_string(),
        format!("编码：{}", processing.encoder),
    ];
    match processing.dedup_seed {
        Some(seed) => {
            lines.push(format!(
                "去重配方：种子 {seed}，逐集参数存证于 {}",
                crate::dedup::RECIPE_FILE_NAME
            ));
            let fields: Vec<String> = crate::dedup::METADATA_FIELDS
                .iter()
                .map(|(key, meaning)| format!("{key}（{meaning}）"))
                .collect();
            lines.push(format!("成片元数据改写：{}", fields.join("、")));
        }
        None => lines.push("去重配方：未启用".to_string()),
    }
    lines.push(match &processing.badge_text {
        Some(text) => format!("AI 合规角标：已烧录「{text}」"),
        None => "AI 合规角标：未烧录".to_string(),
    });
    lines
}

/// 一项材料的 AI 参与情况（按来源说明判断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AiOrigin {
    /// 由 AI 生成（含沿用的、原来就是 AI 生成的海报）
    Ai,
    /// 非 AI（本地提供、素材封面、原始素材抽帧）
    NotAi,
    /// 沿用的值，当初是否由 AI 生成没有记录
    Unrecorded,
}

/// 按来源说明判断 AI 参与情况；值为空（未生成 / 未填写 / 失败）时返回 None，声明里不列。
fn ai_origin(value_present: bool, source: &str) -> Option<AiOrigin> {
    if !value_present {
        return None;
    }
    // 「沿用（…；原为 X）」取出 X 再判断；没有「原为」的沿用就是来源未记录。
    let origin = original_poster_source(source);
    Some(
        if origin.starts_with("AI 生成") && !origin.contains("失败") {
            AiOrigin::Ai
        } else if source.starts_with("沿用") && origin == source {
            AiOrigin::Unrecorded
        } else {
            AiOrigin::NotAi
        },
    )
}

/// 由证明字段算出 AI 声明：逐项按实际来源列出（纯函数，便于单测）。
/// 返回 `(是否含 AI 生成内容, 声明正文)`；含 AI 项时页眉才显示「由人工智能辅助制作」。
pub(crate) fn proof_declaration(payload: &serde_json::Value) -> (bool, String) {
    let text = |key: &str| payload[key].as_str().unwrap_or_default();
    let items = [
        (
            "剧名",
            !text("title").trim().is_empty(),
            text("titleSource"),
        ),
        (
            "简介",
            !text("synopsis").trim().is_empty(),
            text("synopsisSource"),
        ),
        ("海报", !text("posterName").is_empty(), text("posterSource")),
        (
            "过程图",
            payload["processCount"].as_u64().unwrap_or(0) > 0,
            text("storyboardSource"),
        ),
    ];
    let mut ai = Vec::new();
    let mut not_ai = Vec::new();
    let mut unrecorded = Vec::new();
    for (label, present, source) in items {
        match ai_origin(present, source) {
            Some(AiOrigin::Ai) => ai.push(format!("{label}（{}）", original_poster_source(source))),
            Some(AiOrigin::NotAi) => not_ai.push(format!("{label}（{source}）")),
            Some(AiOrigin::Unrecorded) => unrecorded.push(label.to_string()),
            None => {}
        }
    }
    let mut parts = Vec::new();
    if !ai.is_empty() {
        parts.push(format!("由人工智能生成：{}。", ai.join("、")));
    }
    if !not_ai.is_empty() {
        parts.push(format!("非人工智能生成：{}。", not_ai.join("、")));
    }
    if !unrecorded.is_empty() {
        parts.push(format!(
            "{}沿用本剧已保存的内容，当初是否由人工智能生成未记录，请制作方确认。",
            unrecorded.join("、")
        ));
    }
    if parts.is_empty() {
        parts.push("本目录未生成剧名、简介、海报或过程图。".into());
    }
    parts.push("成片的处理方式见上文，报审前请人工复核上述内容。".into());
    (!ai.is_empty(), parts.join(""))
}

/// AI 制作证明的字段（材料线程组装；成片清单与处理方式在编码结束后由
/// [`attach_proof_outcome`] 补上）。另存进清单（`proofPayload`），「按新文案重做海报」时据此重渲证明。
#[allow(clippy::too_many_arguments)]
fn build_proof_payload(
    app: &AppHandle,
    context: &MaterialProofContext,
    evidence: Option<&ContentEvidence>,
    title: Option<&str>,
    title_source: &str,
    synopsis: Option<&str>,
    synopsis_source: &str,
    poster: Option<&Path>,
    poster_source: &str,
    storyboard_source: &str,
    process_count: usize,
    sources: Option<&[ProofFile]>,
) -> serde_json::Value {
    let version = app.package_info().version.to_string();
    json!({
        "date": beijing_date_string(),
        "version": version,
        "dramaName": context.drama_name,
        "originalEpisodes": context.original_episodes,
        "encoder": context.encoder,
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
        // None = 素材指纹没算出来（读取失败或被停止），证明里写「未记录」
        "sources": sources,
    })
}

/// 编码完成后补上处理方式与成片清单（`outputs` 为 None = 成片指纹没算出来）。
pub(crate) fn attach_proof_outcome(
    payload: &mut serde_json::Value,
    processing: &ProofProcessing,
    outputs: Option<&[ProofFile]>,
) {
    payload["processingLines"] = json!(processing_lines(processing));
    payload["outputs"] = json!(outputs);
}

/// 渲染 AI 制作证明：先按当前字段重算 AI 声明，再优先出图片；图片失败退回文本版，
/// 并把图片失败的真实原因作为提示返回。只保留一份当前证明：写成哪种就删掉另一种。
/// 两种都失败时两份旧证明都删掉——留着会被当成本次的证明，而它对应的是上一轮的成片或海报。
fn render_ai_proof(
    app: &AppHandle,
    output_dir: &Path,
    payload: &mut serde_json::Value,
) -> Result<(PathBuf, Option<String>), String> {
    let (ai_assisted, declaration) = proof_declaration(payload);
    payload["aiAssisted"] = json!(ai_assisted);
    payload["declaration"] = json!(declaration);
    let png = output_dir.join(PROOF_IMAGE_FILE);
    let txt = output_dir.join(PROOF_TEXT_FILE);
    match write_ai_proof_image(app, output_dir, payload) {
        Ok(path) => {
            let _ = std::fs::remove_file(&txt);
            Ok((path, None))
        }
        Err(image_error) => {
            crate::logging::log_event(
                "材料",
                format!("AI 制作证明图片生成失败，改出文本版：{image_error}"),
            );
            match write_ai_proof_text(output_dir, payload) {
                Ok(path) => {
                    let _ = std::fs::remove_file(&png);
                    Ok((
                        path,
                        Some(format!(
                            "AI 制作证明图片生成失败，已改出文本版：{image_error}"
                        )),
                    ))
                }
                Err(text_error) => {
                    let _ = std::fs::remove_file(&png);
                    let _ = std::fs::remove_file(&txt);
                    Err(format!(
                        "AI 制作证明生成失败（图片：{image_error}；文本：{text_error}）"
                    ))
                }
            }
        }
    }
}

const PROOF_IMAGE_FILE: &str = "AI制作证明.png";
const PROOF_TEXT_FILE: &str = "AI制作证明.txt";

/// 深度处理开跑前作废上一轮的 AI 制作证明。返回 `true` 表示确实作废了旧证明（文件或清单里的记录）。
///
/// 一旦开跑，材料线程会陆续删过程图、换海报和简介，编码成功前随时可能停止或失败；
/// 旧证明写的是旧内容，留着就会与目录里的实际文件对不上。只有本轮完整成功时，
/// `finalize_deep_proof` 才生成新证明。宁可暂时没有证明，也不留一张写错的。
///
/// 顺序：**先改清单、再删证明文件**。清单写失败时什么都没动，调用方报「未开始处理」是实情；
/// 清单已改、删文件失败时，清单已不再引用旧证明，报错让用户处理残留文件。
///
/// - 清单存在且能解析：`proof` 置空；`proofPayload` 去掉 `outputs`、`processingLines`；
///   剧名、简介、海报来源与 `posterBasis` 保留，海报过期判断与「按新文案重做海报」照常可用；
/// - `clear_process_images`（材料流水线开着，本轮会重做过程图）：同时删除旧过程图并把计数清零，
///   磁盘与清单一致。流水线关着时过程图不会被动，计数原样保留；
/// - 清单损坏或不是对象：不动它（读清单的逻辑本来就把它当成没有），只删证明并记日志，
///   不能因此让这部剧再也做不了深度处理；
/// - 删除 `AI制作证明.png` / `.txt`（不存在不算错）。
pub(crate) fn invalidate_previous_proof(
    output_dir: &Path,
    clear_process_images: bool,
) -> Result<bool, String> {
    let mut invalidated = false;
    let manifest_path = output_dir.join(MANIFEST_FILE);
    match std::fs::read(&manifest_path) {
        Ok(raw) => match serde_json::from_slice::<serde_json::Value>(&raw) {
            Ok(mut manifest) if manifest.is_object() => {
                if strip_proof_record(&mut manifest, clear_process_images) {
                    write_manifest(output_dir, &manifest)?;
                    invalidated = true;
                }
            }
            _ => crate::logging::log_event(
                "材料",
                format!(
                    "材料清单无法解析，保持原样（本轮会重写）：{}",
                    manifest_path.display()
                ),
            ),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "无法读取材料清单（{}：{error}）",
                manifest_path.display()
            ))
        }
    }
    let mut doomed: Vec<(PathBuf, bool)> = [PROOF_IMAGE_FILE, PROOF_TEXT_FILE]
        .iter()
        .map(|name| (output_dir.join(name), true))
        .collect();
    if clear_process_images {
        doomed.extend(
            process_image_paths(output_dir)
                .into_iter()
                .map(|p| (p, false)),
        );
    }
    for (path, is_proof) in doomed {
        match std::fs::remove_file(&path) {
            Ok(()) => invalidated |= is_proof,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "无法作废上一轮 AI 制作证明（{}：{error}）",
                    path.display()
                ))
            }
        }
    }
    Ok(invalidated)
}

/// 输出目录里的过程图文件（固定 4 张）。生成与作废共用这一份命名。
fn process_image_paths(output_dir: &Path) -> Vec<PathBuf> {
    (1..=4)
        .map(|index| output_dir.join(format!("过程图_{index:02}.png")))
        .collect()
}

/// 从清单里去掉与成片（以及将被重做的过程图）对应的证明记录；有改动时返回 true。
fn strip_proof_record(manifest: &mut serde_json::Value, clear_process_images: bool) -> bool {
    let Some(object) = manifest.as_object_mut() else {
        return false;
    };
    let mut changed = object.get("proof").is_some_and(|proof| !proof.is_null());
    object.insert("proof".into(), serde_json::Value::Null);
    if let Some(payload) = object
        .get_mut("proofPayload")
        .and_then(serde_json::Value::as_object_mut)
    {
        changed |= payload.remove("outputs").is_some();
        changed |= payload.remove("processingLines").is_some();
        if clear_process_images
            && payload
                .get("processCount")
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|count| count > 0)
        {
            payload.insert("processCount".into(), json!(0));
            payload.insert(
                "storyboardSource".into(),
                json!("未生成（上次深度处理未完成）"),
            );
            changed = true;
        }
    }
    changed
}

/// 调 Pillow 脚本把证明字段渲染成 `AI制作证明.png`。
fn write_ai_proof_image(
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
    let output_path = output_dir.join(PROOF_IMAGE_FILE);
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

/// 文件清单的文字版：每行「文件名　大小　SHA-256」；没有记录时如实说明（纯函数，便于单测）。
fn proof_file_section(title: &str, files: &serde_json::Value, missing: &str) -> String {
    let Some(list) = files.as_array() else {
        return format!("{title}：{missing}\n");
    };
    let mut text = format!("{title}（{} 个）：\n", list.len());
    for file in list {
        text.push_str(&format!(
            "  {}　{} 字节　SHA-256 {}\n",
            file["fileName"].as_str().unwrap_or_default(),
            file["size"].as_u64().unwrap_or(0),
            file["sha256"].as_str().unwrap_or_default(),
        ));
    }
    text
}

/// 文本版证明的正文（纯函数，便于单测）：与图片版内容一致，文件清单列全。
/// 调用前 `payload` 里须已有 `declaration`（见 [`render_ai_proof`]）。
fn proof_text_body(payload: &serde_json::Value) -> String {
    let get = |key: &str| payload[key].as_str().unwrap_or_default();
    let line = |label: &str, value: &str, source: &str| {
        format!(
            "{label}：{}\n来源：{source}\n",
            if value.is_empty() { "未填写" } else { value }
        )
    };
    let mut body = String::new();
    body.push_str("AI 制作证明\n\n");
    body.push_str(&format!("生成日期：{}（北京时间）\n", get("date")));
    body.push_str(&format!("制作工具：DramaDossier v{}\n", get("version")));
    body.push_str(&format!(
        "原剧目：{}（原始素材 {} 集）\n\n",
        get("dramaName"),
        payload["originalEpisodes"].as_u64().unwrap_or(0)
    ));
    body.push_str("处理方式：\n");
    match payload["processingLines"].as_array() {
        Some(lines) => {
            for item in lines {
                body.push_str(&format!("  · {}\n", item.as_str().unwrap_or_default()));
            }
        }
        None => body.push_str("  未记录（上次深度处理未完成）\n"),
    }
    body.push_str(&format!("\n内容证据：{}\n\n", get("evidence")));
    body.push_str(&line("剧名", get("title"), get("titleSource")));
    if get("synopsis").trim().is_empty() {
        body.push_str(&format!("简介：未填写\n来源：{}\n", get("synopsisSource")));
    } else {
        body.push_str(&line("简介", get("synopsis"), get("synopsisSource")));
    }
    body.push_str(&line("海报", get("posterName"), get("posterSource")));
    let count = payload["processCount"].as_u64().unwrap_or(0);
    if count == 0 {
        body.push_str("过程图：未生成\n\n");
    } else {
        body.push_str(&format!(
            "过程图：已生成 {count} 张（画面抽自原始素材代表集）\n来源：{}\n\n",
            get("storyboardSource"),
        ));
    }
    body.push_str(&proof_file_section(
        "成片清单",
        &payload["outputs"],
        "未记录（上次深度处理未完成）",
    ));
    body.push_str(&proof_file_section(
        "原始素材",
        &payload["sources"],
        "未记录（素材指纹未能计算）",
    ));
    body.push_str(&format!("\nAI 声明：{}\n\n", get("declaration")));
    body.push_str("制作方：________________　签署：________________　日期：________________\n");
    body
}

/// 文本版证明 `AI制作证明.txt`（图片版生成失败时）。
fn write_ai_proof_text(output_dir: &Path, payload: &serde_json::Value) -> Result<PathBuf, String> {
    let path = output_dir.join(PROOF_TEXT_FILE);
    std::fs::write(&path, proof_text_body(payload))
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

/// 过程图画布上的文字与海报：本次最终的剧名、简介与海报（脚本据此排「故事梗概」等区块）。
struct ProcessImageContext<'a> {
    title: &'a str,
    synopsis: Option<&'a str>,
    poster: Option<&'a Path>,
}

/// 生成 4 张过程图：直接从原始抽样分集抽帧，不等待深度处理成片；
/// 由 Pillow 脚本排成「AI 分镜创作画布」整屏截图（剧名 / 简介 / 海报进画布）。
fn generate_process_images(
    app: &AppHandle,
    ffmpeg: &Path,
    episodes: &[ProbedEpisode],
    output_dir: &Path,
    canvas: &ProcessImageContext,
    cancel: &CancelToken,
) -> Result<Vec<PathBuf>, String> {
    let frames_dir = output_dir.join(".material-frames");
    let _ = std::fs::remove_dir_all(&frames_dir);
    // 上一次运行的过程图不能被本次失败继承；先删掉，成功后再返回实际文件。
    for path in process_image_paths(output_dir) {
        let _ = std::fs::remove_file(path);
    }
    std::fs::create_dir_all(&frames_dir)
        .map_err(|error| format!("创建过程图帧目录失败：{error}"))?;
    let mut frames = Vec::new();
    let mut labels = Vec::new();
    for slot in 0..4usize {
        if cancel.is_cancelled() {
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
                FfmpegCtx::light(cancel),
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
        .args(["--title", canvas.title])
        .args(["--synopsis", canvas.synopsis.unwrap_or_default()])
        .args([
            "--poster",
            &canvas
                .poster
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
        ])
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
    Ok(process_image_paths(output_dir)
        .into_iter()
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
    cancel: CancelToken,
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
            &cancel,
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
    cancel: &CancelToken,
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
            poster_stale: false,
            poster_regenerable: false,
            poster_basis: None,
            poster_data_url: None,
            proof_payload: None,
        });
    }

    std::fs::create_dir_all(output_dir)
        .map_err(|error| format!("创建输出目录失败（{}）：{error}", output_dir.display()))?;
    // 证据每次重建。清单**不预先删除**：只在本轮结束时原子覆盖，中途停止或失败时上一份清单原样保留，
    // 下次的过期判断与「按新文案重做海报」都还能用。先记下上一份清单里与海报有关的字段。
    let previous_manifest = read_previous_manifest(output_dir);
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
            cancel,
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
    if cancel.is_cancelled() {
        return Err("已停止".into());
    }

    // ── 文字材料：按剧读取已保存值。剧名：本剧已保存 → AI → 不填；
    //    简介：素材目录 → 本剧已保存 → AI → 不填 ──
    let material_key = crate::paths::material_key(drama_dir);
    let saved = settings.materials_for(&material_key);
    let saved_title = non_empty(&saved.title);
    let saved_synopsis = non_empty(&saved.synopsis);
    let local_synopsis = crate::toolbox::read_local_synopsis(drama_dir);
    let title_ai_on = settings.ai_title_enabled != Some(false);
    let synopsis_ai_on = settings.ai_synopsis_enabled != Some(false);
    let llm_label = ai_source_label(&settings.llm_model);
    // 只有真的缺、且开关开着时才调 LLM：已有剧名和简介就不白花一次调用。
    let need_title = title_ai_on && saved_title.is_none();
    let need_synopsis = synopsis_ai_on && saved_synopsis.is_none() && local_synopsis.is_none();
    let llm_ready = ai::storyboard_llm_ready(settings);
    let identity = if (need_title || need_synopsis) && llm_ready {
        let _ = app.emit(
            "material-progress",
            json!({ "stage": "ai", "label": "材料线程：正在生成剧名与简介" }),
        );
        let context = evidence.as_ref().map(evidence_prompt).unwrap_or_else(|| {
            // ASR/OCR 不可用时至少给出剧目目录名、集数与已有文案，
            // 否则模型拿不到任何剧情线索，只能输出空洞的模板话。
            let mut context = format!(
                "剧目目录名：{}；原始素材 {} 集。",
                proof.drama_name, proof.original_episodes
            );
            if let Some(title) = &saved_title {
                context.push_str(&format!("已有剧名：{title}。"));
            }
            // 与简介的来源顺序一致：素材目录的简介优先于本剧已保存的。
            if let Some(synopsis) = local_synopsis.as_ref().or(saved_synopsis.as_ref()) {
                context.push_str(&format!("已有简介：{synopsis}"));
            }
            context
        });
        // 计费调用之前再看一次令牌：编码失败或用户停止后不再发起新的 LLM 请求。
        if cancel.is_cancelled() {
            return Err("已停止".into());
        }
        match ai::material_identity_blocking(settings, &context) {
            Ok(identity) => {
                // 超长被作废的那一项按「生成失败」处理（下面 ai_text 走 Failed），原因如实告诉用户。
                for reason in &identity.rejected {
                    messages.push(reason.clone());
                    crate::logging::log_event("材料", reason);
                }
                Some(identity)
            }
            Err(error) => {
                messages.push(format!("AI 文案生成失败，沿用已有内容：{error}"));
                crate::logging::log_event(
                    "材料",
                    format!("LLM 剧名/简介生成失败（跳过，沿用已有内容）：{error}"),
                );
                None
            }
        }
    } else {
        if (need_title || need_synopsis) && !llm_ready {
            messages.push("LLM 未配置，缺失的剧名/简介未生成。".into());
            crate::logging::log_event("材料", "LLM 未配置，缺失的剧名/简介跳过 AI 生成");
        }
        None
    };
    let ai_text = |on: bool, needed: bool, generated: Option<&String>| {
        if !on {
            AiText::Disabled
        } else if let Some(text) = generated {
            AiText::Generated(text.clone())
        } else if needed && !llm_ready {
            AiText::NotConfigured
        } else {
            AiText::Failed
        }
    };
    let title_ai = ai_text(
        title_ai_on,
        need_title,
        identity
            .as_ref()
            .and_then(|identity| identity.title.as_ref()),
    );
    let synopsis_ai = ai_text(
        synopsis_ai_on,
        need_synopsis,
        identity
            .as_ref()
            .and_then(|identity| identity.synopsis.as_ref()),
    );
    let title_resolved = resolve_title(saved_title.clone(), &title_ai, &llm_label);
    let synopsis_resolved = resolve_synopsis(
        saved_synopsis.clone(),
        local_synopsis.clone(),
        &synopsis_ai,
        &llm_label,
    );
    // 本次新生成的值（只有它们需要写回本剧材料，也只有它们会让旧海报过期）。
    let generated_title = (saved_title.is_none())
        .then(|| match &title_ai {
            AiText::Generated(text) => Some(text.clone()),
            _ => None,
        })
        .flatten();
    let generated_synopsis = (saved_synopsis.is_none() && local_synopsis.is_none())
        .then(|| match &synopsis_ai {
            AiText::Generated(text) => Some(text.clone()),
            _ => None,
        })
        .flatten();
    let (title, title_source) = (title_resolved.value, title_resolved.source);
    let (synopsis, synopsis_source) = (synopsis_resolved.value, synopsis_resolved.source);
    // 已付费拿到的剧名/简介立即写回本剧材料：后面海报、过程图阶段若因编码失败或用户停止
    // 提前返回，下一轮直接复用，不再重复计费。
    if let Err(error) = persist_generated(
        app,
        &material_key,
        generated_title.as_deref(),
        generated_synopsis.as_deref(),
    ) {
        crate::logging::log_event("材料", error.clone());
        messages.push(error);
    }

    // ── 海报：本次输出目录已有就沿用（处理中不重做）→ AI → 素材封面 → 无 ──
    let old_poster = existing_poster(output_dir);
    let poster_ai_on = settings.ai_poster_enabled != Some(false);
    let poster_api_ready = ai::poster_api_ready(settings);
    let action = decide_poster(
        poster_ai_on,
        poster_api_ready,
        old_poster.is_some(),
        crate::toolbox::find_local_cover(drama_dir).is_some(),
    );
    let poster_stale = action == PosterAction::ReuseExisting
        && poster_is_stale(
            previous_manifest.as_ref(),
            title.as_deref(),
            synopsis.as_deref(),
            generated_title.is_some() || generated_synopsis.is_some(),
        );
    let poster_regenerable = poster_stale && poster_ai_on && poster_api_ready;
    if poster_stale {
        messages.push(if poster_regenerable {
            "剧名或简介已变化，海报仍是旧文案。可在结果区点「按新文案重做海报」（会调用生图接口，产生费用）。"
                .to_string()
        } else {
            "剧名或简介已变化，海报仍是旧文案。打开 AI 海报并配置生图接口后可按新文案重做。".to_string()
        });
    }
    let _ = app.emit(
        "material-progress",
        json!({
            "stage": "poster",
            "label": if action == PosterAction::Generate { "材料线程：正在生成海报" } else { "材料线程：检查已有海报" },
        }),
    );
    let copy_cover = |reason: &str| match crate::toolbox::copy_local_cover(drama_dir, output_dir) {
        Some(path) => (Some(path), "本地提供（素材封面）".to_string()),
        None => (None, reason.to_string()),
    };
    // 生图同样计费：发起前确认任务没有被停止 / 编码没有失败。
    if action == PosterAction::Generate && cancel.is_cancelled() {
        return Err("已停止".into());
    }
    let (poster, poster_source) = match action {
        PosterAction::Generate => match ai::poster_blocking(
            settings,
            output_dir,
            title.as_deref().unwrap_or_default(),
            synopsis.as_deref().unwrap_or_default(),
        ) {
            Ok(path) => (Some(path), ai_source_label(&settings.img_model)),
            Err(error) => {
                // 走到这里说明输出目录原本没有海报（有就沿用、不会生图），没有旧文件要处理。
                messages.push(format!("海报生成失败（其余材料继续）：{error}"));
                crate::logging::log_event(
                    "材料",
                    format!("海报生成失败（跳过，兜底素材封面）：{error}"),
                );
                copy_cover("AI 生成失败，未生成")
            }
        },
        PosterAction::ReuseExisting => (
            old_poster,
            reused_poster_source(
                previous_manifest
                    .as_ref()
                    .and_then(|previous| previous.poster_source.as_deref()),
                poster_stale,
            ),
        ),
        PosterAction::CopyLocalCover => copy_cover("未提供"),
        PosterAction::Missing => {
            let source = if !poster_ai_on {
                "未提供（本地模式）"
            } else {
                "未提供（未配置生图接口，素材目录也没有封面）"
            };
            crate::logging::log_event("材料", format!("海报缺失（{source}），材料流水线继续"));
            (None, source.to_string())
        }
    };
    // 这张海报所依据的文案：本次 AI 生成的就是本次文案；沿用的原样带上一张的依据
    // （旧目录没有清单时依据未知，不写，下次退回旧的判断方式）；素材封面 / 缺失与文案无关。
    let poster_basis = match action {
        PosterAction::Generate if poster_source.starts_with("AI 生成") => {
            Some(PosterBasis::of(title.as_deref(), synopsis.as_deref()))
        }
        PosterAction::ReuseExisting => previous_manifest
            .as_ref()
            .map(PreviousManifest::effective_poster_basis),
        _ => Some(PosterBasis::default()),
    };

    if cancel.is_cancelled() {
        return Err("已停止".into());
    }
    let _ = app.emit(
        "material-progress",
        json!({
            "stage": "process-images",
            "label": "材料线程：正在生成过程图",
        }),
    );
    // 剧名为空时退回剧目名（与远程 0257ac1 在旧兜底路径里的做法一致），画布标题不留空。
    let canvas = ProcessImageContext {
        title: title
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .unwrap_or(&proof.drama_name),
        synopsis: synopsis.as_deref(),
        poster: poster.as_deref(),
    };
    let process_images =
        match generate_process_images(app, ffmpeg, episodes, output_dir, &canvas, cancel) {
            Ok(paths) => paths,
            Err(error) => {
                messages.push(format!("过程图生成失败：{error}"));
                Vec::new()
            }
        };

    if cancel.is_cancelled() {
        return Err("已停止".into());
    }
    // 过程图是纯原片帧；一张都没生成时如实写「未生成」。
    let storyboard_source = if process_images.is_empty() {
        "未生成".to_string()
    } else {
        "原始素材抽帧".to_string()
    };
    // 简介落盘：输出目录带一份 简介.txt（AI 生成或素材目录原有，随成片一起交付）。
    if let Some(text) = synopsis.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        if let Err(error) = std::fs::write(output_dir.join("简介.txt"), text) {
            messages.push(format!("简介.txt 写入失败：{error}"));
        }
    }
    // 原始素材指纹：在材料线程里算，和编码并行，不额外拖慢收尾。
    let _ = app.emit(
        "material-progress",
        json!({ "stage": "proof", "label": "材料线程：正在计算原始素材指纹" }),
    );
    let source_paths: Vec<PathBuf> = episodes
        .iter()
        .map(|episode| episode.file.path.clone())
        .collect();
    let sources = match proof_files(&source_paths, cancel) {
        Ok(files) => Some(files),
        Err(error) if cancel.is_cancelled() => return Err(error),
        Err(error) => {
            crate::logging::log_event("材料", format!("原始素材指纹未算出：{error}"));
            messages.push(format!("原始素材指纹未算出（证明里写「未记录」）：{error}"));
            None
        }
    };
    let proof_payload = build_proof_payload(
        app,
        proof,
        evidence.as_ref(),
        title.as_deref(),
        &title_source,
        synopsis.as_deref(),
        &synopsis_source,
        poster.as_deref(),
        &poster_source,
        &storyboard_source,
        process_images.len(),
        sources.as_deref(),
    );
    let status = if messages.is_empty() {
        "completed"
    } else {
        "partial"
    };
    let poster_data_url = poster.as_deref().and_then(poster_preview_data_url);
    let mut report = MaterialPipelineReport {
        status: status.into(),
        title,
        synopsis,
        poster,
        process_images,
        // 证明要等成片哈希与实际处理方式，深度处理收尾时才生成（finalize_deep_proof）。
        proof: None,
        evidence,
        messages,
        title_source,
        synopsis_source,
        poster_source,
        storyboard_source,
        poster_stale,
        poster_regenerable,
        poster_basis,
        poster_data_url,
        proof_payload: Some(proof_payload),
    };
    record_manifest(output_dir, &mut report);
    Ok(report)
}

/// 写材料清单：报告字段 + `proofPayload`（data URL 只用于本次界面预览，不写入）。
fn write_material_manifest(
    output_dir: &Path,
    report: &MaterialPipelineReport,
) -> Result<(), String> {
    let mut manifest_report = report.clone();
    manifest_report.poster_data_url = None;
    let mut value =
        serde_json::to_value(&manifest_report).map_err(|error| format!("生成清单失败：{error}"))?;
    if let Some(payload) = &report.proof_payload {
        value["proofPayload"] = payload.clone();
    }
    write_manifest(output_dir, &value)
}

/// 写清单；写不进去时上一份清单原样保留（原子写），把后果写进报告让用户看到——
/// 下次的过期判断与重做海报都依赖它。
fn record_manifest(output_dir: &Path, report: &mut MaterialPipelineReport) {
    if let Err(error) = write_material_manifest(output_dir, report) {
        crate::logging::log_event("材料", &error);
        report.messages.push(format!(
            "{error}。下次深度处理的海报过期判断可能不准，「按新文案重做海报」也可能不可用。"
        ));
        report.status = "partial".into();
    }
}

/// 深度处理收尾（编码成功、材料线程汇合之后）：补上成片清单与处理方式，生成 AI 制作证明并改写清单。
///
/// 只在编码成功后调用：编码失败或停止时不出新证明，上一轮的证明仍与上一轮成片对应。
/// 材料线程失败（没有证明字段）时什么都不做。
pub(crate) fn finalize_deep_proof(
    app: &AppHandle,
    output_dir: &Path,
    report: &mut MaterialPipelineReport,
    processing: &ProofProcessing,
    output_paths: &[PathBuf],
    cancel: &CancelToken,
) {
    let Some(mut payload) = report.proof_payload.take() else {
        return;
    };
    let before = report.messages.len();
    let _ = app.emit(
        "material-progress",
        json!({ "stage": "proof", "label": "正在计算成片指纹并生成 AI 制作证明" }),
    );
    let outputs = match proof_files(output_paths, cancel) {
        Ok(files) => Some(files),
        Err(error) => {
            crate::logging::log_event("材料", format!("成片指纹未算出：{error}"));
            report
                .messages
                .push(format!("成片指纹未算出（证明里写「未记录」）：{error}"));
            None
        }
    };
    attach_proof_outcome(&mut payload, processing, outputs.as_deref());
    match render_ai_proof(app, output_dir, &mut payload) {
        Ok((path, warning)) => {
            report.proof = Some(path);
            report.messages.extend(warning);
        }
        Err(error) => {
            crate::logging::log_event("材料", &error);
            report.messages.push(error);
        }
    }
    if report.messages.len() > before {
        report.status = "partial".into();
    }
    report.proof_payload = Some(payload);
    record_manifest(output_dir, report);
}

/// 过期提示消息的特征文本（材料线程写入、重做成功后移除）。
const STALE_POSTER_MESSAGE_MARK: &str = "海报仍是旧文案";

/// 重做海报成功后改写清单：记下新海报与它所依据的文案，清掉过期标记与过期提示；
/// 没有别的警告了就把状态改回「已完成」。
fn apply_regenerated_poster(
    manifest: &mut serde_json::Value,
    poster: &Path,
    poster_source: &str,
    proof: Option<&Path>,
    basis: PosterBasis,
) {
    manifest["poster"] = json!(poster.display().to_string());
    manifest["posterSource"] = json!(poster_source);
    manifest["posterStale"] = json!(false);
    manifest["posterRegenerable"] = json!(false);
    manifest["posterBasis"] = json!(basis);
    if let Some(path) = proof {
        manifest["proof"] = json!(path.display().to_string());
    }
    if let Some(list) = manifest["messages"].as_array_mut() {
        list.retain(|message| {
            !message
                .as_str()
                .is_some_and(|text| text.contains(STALE_POSTER_MESSAGE_MARK))
        });
        if list.is_empty() {
            manifest["status"] = json!("completed");
        }
    }
}

/// 「按新文案重做海报」的结果。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PosterRegeneration {
    pub poster: PathBuf,
    pub poster_source: String,
    pub proof: Option<PathBuf>,
    pub messages: Vec<String>,
}

/// 从上一次深度处理的清单里取重做海报所需的剧名与简介；清单缺失、损坏或没有剧名时报错。
fn manifest_for_regeneration(output_dir: &Path) -> Result<serde_json::Value, String> {
    let path = output_dir.join(MANIFEST_FILE);
    let raw = std::fs::read(&path).map_err(|error| {
        format!(
            "输出目录里没有深度处理清单（{}）：{error}。请先完成一次深度处理。",
            path.display()
        )
    })?;
    let value: serde_json::Value = serde_json::from_slice(&raw)
        .map_err(|error| format!("深度处理清单无法识别（{}）：{error}", path.display()))?;
    if value["title"]
        .as_str()
        .is_none_or(|title| title.trim().is_empty())
    {
        return Err("清单里没有剧名，无法按新文案重做海报。".into());
    }
    Ok(value)
}

/// 按清单里的最新剧名/简介重做海报（用户在结果区确认后才调用，会产生生图费用）。
///
/// 只允许写本剧的输出目录：目录在剧目目录内，或归属标记属于本剧。成功后才替换旧海报，
/// 并据清单里的 `proofPayload` 重渲 AI 制作证明、改写清单。
pub(crate) fn regenerate_poster_blocking(
    app: &AppHandle,
    drama: &dossier_core::Drama,
    output_dir: &Path,
) -> Result<PosterRegeneration, String> {
    // 顺序：先占用目录（深度处理进行中直接拒绝）→ 清单（没有清单时如实说「先跑一次」，
    // 而不是误报归属）→ 归属 → 设置（严格读取，损坏时如实报错，不拿默认值说成「未配置」）。
    let _busy = crate::paths::claim_output_dir(output_dir).map_err(|()| {
        format!(
            "《{}》的输出目录正在深度处理，请等处理结束后再重做海报。",
            drama.name
        )
    })?;
    let mut manifest = manifest_for_regeneration(output_dir)?;
    let owner_key = crate::paths::material_key(&drama.dir);
    match crate::toolbox::check_output_owner(output_dir, &drama.dir, &owner_key)? {
        crate::toolbox::OwnerState::InsideDramaDir | crate::toolbox::OwnerState::Owned => {}
        _ => {
            return Err(format!(
                "输出目录 {} 不属于《{}》，已拒绝重做海报。",
                output_dir.display(),
                drama.name
            ))
        }
    }
    let settings = dossier_core::try_load_settings(&crate::settings_path(app)?)?;
    if settings.ai_poster_enabled == Some(false) {
        return Err("AI 海报开关已关闭，无法重做海报。请先在转码页打开海报的 AI 开关。".into());
    }
    if !ai::poster_api_ready(&settings) {
        return Err("生图接口未配置，无法重做海报。请在「设置 → 生成设置」配置后重试。".into());
    }
    let title = manifest["title"].as_str().unwrap_or_default().to_string();
    let synopsis = manifest["synopsis"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let old_poster = existing_poster(output_dir);
    // poster_blocking 只在下载完成后才写文件：失败时旧海报原样保留。
    let poster = ai::poster_blocking(&settings, output_dir, &title, &synopsis)?;
    if let Some(old) = old_poster.filter(|old| *old != poster) {
        let _ = std::fs::remove_file(old);
    }
    let poster_source = ai_source_label(&settings.img_model);
    let mut messages = Vec::new();
    let mut proof_render_failed = false;
    let proof = match manifest.get_mut("proofPayload").filter(|v| v.is_object()) {
        Some(payload) => {
            payload["date"] = json!(beijing_date_string());
            payload["posterName"] = json!(poster
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default());
            payload["posterSource"] = json!(poster_source);
            payload["posterPath"] = json!(poster.display().to_string());
            // 就地渲染：重算的 AI 声明随清单一起写回。旧清单没有成片清单 / 处理方式时，
            // 证明里如实写「未记录（上次深度处理未完成）」。
            let outputs_recorded = payload.get("outputs").is_some_and(|v| !v.is_null());
            match render_ai_proof(app, output_dir, payload) {
                Ok((path, warning)) => {
                    messages.extend(warning);
                    if !outputs_recorded {
                        messages.push(
                            "AI 制作证明里的成片清单写的是「未记录」（上次深度处理未完成）；完整跑一次深度处理后证明会补全成片清单。"
                                .into(),
                        );
                    }
                    Some(path)
                }
                Err(error) => {
                    messages.push(format!("海报已重做，但 AI 制作证明重渲失败：{error}"));
                    proof_render_failed = true;
                    None
                }
            }
        }
        None => {
            messages.push(
                "清单缺少证明字段（旧版本生成），AI 制作证明未更新，请重新深度处理一次。".into(),
            );
            None
        }
    };
    apply_regenerated_poster(
        &mut manifest,
        &poster,
        &poster_source,
        proof.as_deref(),
        // 与材料线程记依据的口径一致：简介为空时记为 None（清单里的 synopsis 同样是 null）。
        PosterBasis::of(
            Some(title.as_str()),
            Some(synopsis.as_str()).filter(|text| !text.is_empty()),
        ),
    );
    // 重渲失败时旧证明已被删（它配的是旧海报），清单里不能再指向它。
    if proof_render_failed {
        manifest["proof"] = serde_json::Value::Null;
    }
    write_manifest(output_dir, &manifest)
        .map_err(|error| format!("海报已重做，但改写清单失败：{error}"))?;
    crate::logging::log_event(
        "材料",
        format!("《{}》按新文案重做海报：{}", drama.name, poster.display()),
    );
    Ok(PosterRegeneration {
        poster,
        poster_source,
        proof,
        messages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dossier_runtime::{CommandOutput, FakeRunner};

    /// 本机踩过的真实崩溃：GPU 版意图 + CPU 版 torch，加载模型即崩，应触发 CPU 重试。
    #[test]
    fn cpu_only_torch_failures_trigger_cpu_retry() {
        let cuda_error = "RuntimeError: Attempting to deserialize object on a CUDA device but \
torch.cuda.is_available() is False. If you are running on a CPU-only machine, please use \
torch.load with map_location=torch.device('cpu') to map your storages to the CPU.";
        assert!(asr_gpu_failure_worth_cpu_retry(cuda_error));
        // 其他错误（音频解码失败等）重跑也不会好，不值得再跑一次
        assert!(!asr_gpu_failure_worth_cpu_retry(
            "Invalid data found when processing input"
        ));
    }

    #[test]
    fn title_and_synopsis_follow_source_order_with_honest_labels() {
        let label = "AI 生成（模型 m）";
        let generated = AiText::Generated("AI 文案".into());
        // 本剧已保存优先，即使 AI 本次也有结果；来源不得冒充模型生成。
        let resolved = resolve_title(Some("已存剧名".into()), &generated, label);
        assert_eq!(resolved.value.as_deref(), Some("已存剧名"));
        assert_eq!(resolved.source, SOURCE_SAVED);
        // 素材目录有简介、LLM 未配置 → 复用本地。
        let resolved =
            resolve_synopsis(None, Some("本地简介".into()), &AiText::NotConfigured, label);
        assert_eq!(resolved.value.as_deref(), Some("本地简介"));
        assert_eq!(resolved.source, SOURCE_LOCAL_DIR);
        // 素材目录的简介排在本剧已保存之前：后补的 简介.txt 一定生效。
        let resolved = resolve_synopsis(
            Some("以前保存的 AI 简介".into()),
            Some("后补的本地简介".into()),
            &AiText::NotConfigured,
            label,
        );
        assert_eq!(resolved.value.as_deref(), Some("后补的本地简介"));
        assert_eq!(resolved.source, SOURCE_LOCAL_DIR);
        // 没有本地简介时沿用本剧已保存的。
        let resolved = resolve_synopsis(Some("已存简介".into()), None, &generated, label);
        assert_eq!(resolved.value.as_deref(), Some("已存简介"));
        assert_eq!(resolved.source, SOURCE_SAVED);
        // 只有本次真的生成了才标模型名。
        let resolved = resolve_synopsis(None, None, &generated, label);
        assert_eq!(resolved.source, label);
        // 什么都没有：按原因如实标注。
        for (ai, expected) in [
            (AiText::Disabled, "未填写（本地模式）"),
            (AiText::NotConfigured, "未填写（未配置 LLM）"),
            (AiText::Failed, "AI 生成失败，未填写"),
        ] {
            let resolved = resolve_title(None, &ai, label);
            assert_eq!(resolved.value, None);
            assert_eq!(resolved.source, expected);
        }
    }

    #[test]
    fn poster_decision_never_regenerates_existing_poster() {
        use PosterAction::*;
        // (ai_on, api_ready, has_existing, has_local_cover) → 期望
        let cases = [
            ((true, true, false, false), Generate),
            ((true, true, false, true), Generate),
            // 已有海报：一律沿用，处理中不重做（重做要用户在结果区确认）。
            ((true, true, true, true), ReuseExisting),
            ((false, false, true, false), ReuseExisting),
            // 关掉 AI：有封面就复用封面。
            ((false, true, false, true), CopyLocalCover),
            // 开着 AI 但没配生图接口：不发请求，退回封面。
            ((true, false, false, true), CopyLocalCover),
            ((false, false, false, false), Missing),
        ];
        for ((ai_on, ready, existing, cover), expected) in cases {
            assert_eq!(
                decide_poster(ai_on, ready, existing, cover),
                expected,
                "ai_on={ai_on} ready={ready} existing={existing} cover={cover}"
            );
        }
    }

    #[test]
    fn poster_staleness_follows_previous_manifest() {
        // 旧版本清单（没有 posterBasis）：退回拿清单里的剧名与简介比。
        let ai_prev = PreviousManifest {
            title: Some("旧剧名".into()),
            synopsis: Some("旧简介".into()),
            poster_source: Some("AI 生成（模型 m）".into()),
            poster_basis: None,
        };
        // AI 海报 + 简介变了 → 过期；文案没变 → 不过期。
        assert!(poster_is_stale(
            Some(&ai_prev),
            Some("旧剧名"),
            Some("新简介"),
            false
        ));
        assert!(!poster_is_stale(
            Some(&ai_prev),
            Some("旧剧名"),
            Some("旧简介"),
            false
        ));
        // 上次已是「沿用（…；原为 AI 生成…）」：仍按 AI 海报判断。
        let reused_prev = PreviousManifest {
            poster_source: Some(reused_poster_source(Some("AI 生成（模型 m）"), false)),
            ..ai_prev.clone()
        };
        assert!(poster_is_stale(
            Some(&reused_prev),
            Some("新剧名"),
            Some("旧简介"),
            false
        ));
        // 素材封面复制来的海报：文案变了也不算过期。
        let cover_prev = PreviousManifest {
            poster_source: Some("本地提供（素材封面）".into()),
            ..ai_prev.clone()
        };
        assert!(!poster_is_stale(
            Some(&cover_prev),
            Some("新剧名"),
            Some("新简介"),
            true
        ));
        // 没有上次清单：本次 AI 新生成了文案才算过期。
        assert!(poster_is_stale(None, Some("x"), None, true));
        assert!(!poster_is_stale(None, Some("x"), None, false));
    }

    #[test]
    fn poster_staleness_persists_until_regenerated() {
        // 新版本清单记着海报所依据的文案：上一份清单的文案已是新文案（上一轮沿用时写入），
        // 只要海报没重做，依据仍是旧文案，每次都判过期——这正是以前只提示一次的漏洞。
        let basis = PosterBasis::of(Some("剧名"), Some("第一版简介"));
        let reused_once = PreviousManifest {
            title: Some("剧名".into()),
            synopsis: Some("第二版简介".into()),
            poster_source: Some(reused_poster_source(Some("AI 生成（模型 m）"), true)),
            poster_basis: Some(basis.clone()),
        };
        assert!(poster_is_stale(
            Some(&reused_once),
            Some("剧名"),
            Some("第二版简介"),
            false
        ));
        // 文案改回海报当初用的那一版：不再过期。
        assert!(!poster_is_stale(
            Some(&reused_once),
            Some("剧名"),
            Some("第一版简介"),
            false
        ));
        // 重做后依据更新为新文案：不再过期。
        let regenerated = PreviousManifest {
            poster_source: Some("AI 生成（模型 m）".into()),
            poster_basis: Some(PosterBasis::of(Some("剧名"), Some("第二版简介"))),
            ..reused_once.clone()
        };
        assert!(!poster_is_stale(
            Some(&regenerated),
            Some("剧名"),
            Some("第二版简介"),
            false
        ));
    }

    #[test]
    fn regenerated_manifest_clears_stale_state_and_records_basis() {
        let mut manifest = json!({
            "status": "partial",
            "posterStale": true,
            "posterRegenerable": true,
            "messages": ["剧名或简介已变化，海报仍是旧文案。可在结果区点「按新文案重做海报」"],
        });
        apply_regenerated_poster(
            &mut manifest,
            Path::new("/输出/剧目海报.png"),
            "AI 生成（模型 m）",
            Some(Path::new("/输出/AI制作证明.png")),
            PosterBasis::of(Some("剧名"), Some("新简介")),
        );
        assert_eq!(manifest["posterStale"], false);
        assert_eq!(manifest["posterRegenerable"], false);
        assert_eq!(manifest["messages"].as_array().unwrap().len(), 0);
        assert_eq!(manifest["status"], "completed", "没有别的警告时改回已完成");
        assert_eq!(manifest["posterBasis"]["synopsis"], "新简介");
        assert_eq!(manifest["proof"], "/输出/AI制作证明.png");
        // 还有别的警告：状态保持 partial。
        let mut manifest = json!({
            "status": "partial",
            "messages": ["海报仍是旧文案", "过程图生成失败：x"],
        });
        apply_regenerated_poster(
            &mut manifest,
            Path::new("/p.png"),
            "AI 生成",
            None,
            PosterBasis::default(),
        );
        assert_eq!(manifest["status"], "partial");
        assert_eq!(manifest["messages"].as_array().unwrap().len(), 1);
        // 新清单写出的 posterBasis 能被下一轮读回。
        let dir = std::env::temp_dir().join(format!("dossier-basis-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_manifest(
            &dir,
            &json!({"title": "剧名", "posterBasis": {"title": "剧名", "synopsis": null}}),
        )
        .unwrap();
        let previous = read_previous_manifest(&dir).unwrap();
        assert_eq!(
            previous.effective_poster_basis(),
            PosterBasis::of(Some("剧名"), None)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn reused_poster_source_keeps_original_without_nesting() {
        let once = reused_poster_source(Some("AI 生成（模型 m）"), false);
        assert_eq!(once, "沿用（本次输出目录已有；原为 AI 生成（模型 m））");
        let twice = reused_poster_source(Some(&once), true);
        assert_eq!(
            twice,
            "沿用（文案已变，海报配的是旧剧名/简介；原为 AI 生成（模型 m））"
        );
        assert_eq!(original_poster_source(&twice), "AI 生成（模型 m）");
        assert_eq!(
            reused_poster_source(None, false),
            "沿用（本次输出目录已有）"
        );
    }

    #[test]
    fn invalidate_previous_proof_removes_proof_and_keeps_poster_basis() {
        let dir = std::env::temp_dir().join(format!("dossier-invalidate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 没有清单也没有证明：返回 false，不创建任何文件。
        assert!(!invalidate_previous_proof(&dir, true).unwrap());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

        std::fs::write(dir.join(PROOF_IMAGE_FILE), b"png").unwrap();
        std::fs::write(dir.join(PROOF_TEXT_FILE), b"txt").unwrap();
        let manifest = json!({
            "title": "剧名",
            "posterSource": "AI 生成（模型 m）",
            "posterBasis": {"title": "剧名", "synopsis": "简介"},
            "proof": "/输出/AI制作证明.png",
            "proofPayload": {
                "title": "剧名",
                "outputs": [{"fileName": "第01集.mp4"}],
                "processingLines": ["逐集微帧"],
                "processCount": 4,
                "storyboardSource": "原始素材抽帧",
                "sources": []
            }
        });
        std::fs::write(dir.join(MANIFEST_FILE), manifest.to_string()).unwrap();
        for path in process_image_paths(&dir) {
            std::fs::write(path, b"png").unwrap();
        }

        // 材料流水线关着：过程图不会被重做，文件与计数都原样保留，只作废证明与成片清单。
        let backup = std::fs::read(dir.join(MANIFEST_FILE)).unwrap();
        assert!(invalidate_previous_proof(&dir, false).unwrap());
        assert!(process_image_paths(&dir).iter().all(|p| p.is_file()));
        let kept: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join(MANIFEST_FILE)).unwrap()).unwrap();
        assert_eq!(kept["proofPayload"]["processCount"], 4);
        assert!(kept["proofPayload"].get("outputs").is_none());
        assert!(!dir.join(PROOF_IMAGE_FILE).exists());

        // 材料流水线开着：过程图连同计数一起清掉，磁盘与清单一致。
        std::fs::write(dir.join(MANIFEST_FILE), &backup).unwrap();
        std::fs::write(dir.join(PROOF_IMAGE_FILE), b"png").unwrap();
        std::fs::write(dir.join(PROOF_TEXT_FILE), b"txt").unwrap();
        assert!(invalidate_previous_proof(&dir, true).unwrap());
        assert!(process_image_paths(&dir).iter().all(|p| !p.exists()));
        assert!(!dir.join(PROOF_IMAGE_FILE).exists());
        assert!(!dir.join(PROOF_TEXT_FILE).exists());
        let after: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join(MANIFEST_FILE)).unwrap()).unwrap();
        assert!(after["proof"].is_null());
        assert!(after["proofPayload"].get("outputs").is_none());
        assert!(after["proofPayload"].get("processingLines").is_none());
        // 过程图开跑就会被删除重做：作废后计数清零，重做海报时不会写「已生成 4 张」。
        assert_eq!(after["proofPayload"]["processCount"], 0);
        assert!(after["proofPayload"]["storyboardSource"]
            .as_str()
            .unwrap()
            .starts_with("未生成"));
        assert_eq!(after["proofPayload"]["title"], "剧名");
        assert_eq!(after["posterBasis"]["synopsis"], "简介");
        assert_eq!(after["posterSource"], "AI 生成（模型 m）");
        // 再作废一次：已经没有可作废的了。
        assert!(!invalidate_previous_proof(&dir, true).unwrap());

        // 只有成片清单、没有证明文件也没有 proof 字段：照样算作废（以前返回 false，提示里漏说）。
        let partial = json!({"proof": null, "proofPayload": {"outputs": []}});
        std::fs::write(dir.join(MANIFEST_FILE), partial.to_string()).unwrap();
        assert!(invalidate_previous_proof(&dir, true).unwrap());

        // 清单是数组（非对象）：不动它，不报错。
        std::fs::write(dir.join(MANIFEST_FILE), b"[1, 2]").unwrap();
        assert!(!invalidate_previous_proof(&dir, true).unwrap());
        assert_eq!(std::fs::read(dir.join(MANIFEST_FILE)).unwrap(), b"[1, 2]");

        // 清单损坏：证明照删，清单原样保留，不报错。
        std::fs::write(dir.join(PROOF_IMAGE_FILE), b"png").unwrap();
        std::fs::write(dir.join(MANIFEST_FILE), b"{ broken").unwrap();
        assert!(invalidate_previous_proof(&dir, true).unwrap());
        assert!(!dir.join(PROOF_IMAGE_FILE).exists());
        assert_eq!(std::fs::read(dir.join(MANIFEST_FILE)).unwrap(), b"{ broken");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn previous_manifest_and_regeneration_manifest_handle_missing_or_bad_files() {
        let dir = std::env::temp_dir().join(format!("dossier-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 没有清单：上次清单为空；重做被拒绝。
        assert_eq!(read_previous_manifest(&dir), None);
        assert!(manifest_for_regeneration(&dir)
            .unwrap_err()
            .contains("没有深度处理清单"));
        // 损坏：同样按没有处理。
        std::fs::write(dir.join(MANIFEST_FILE), b"{ broken").unwrap();
        assert_eq!(read_previous_manifest(&dir), None);
        assert!(manifest_for_regeneration(&dir).is_err());
        // 旧格式缺字段：读得出，缺的字段为空；没有剧名时拒绝重做。
        std::fs::write(dir.join(MANIFEST_FILE), r#"{"synopsis":"简介"}"#).unwrap();
        assert_eq!(
            read_previous_manifest(&dir),
            Some(PreviousManifest {
                synopsis: Some("简介".into()),
                ..Default::default()
            })
        );
        assert!(manifest_for_regeneration(&dir)
            .unwrap_err()
            .contains("没有剧名"));
        std::fs::write(
            dir.join(MANIFEST_FILE),
            r#"{"title":"剧名","synopsis":"简介"}"#,
        )
        .unwrap();
        assert_eq!(manifest_for_regeneration(&dir).unwrap()["title"], "剧名");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn existing_poster_only_looks_inside_output_dir() {
        let base = std::env::temp_dir().join(format!("dossier-poster-{}", std::process::id()));
        let drama_a = base.join("A输出");
        let drama_b = base.join("B输出");
        std::fs::create_dir_all(&drama_a).unwrap();
        std::fs::create_dir_all(&drama_b).unwrap();
        std::fs::write(drama_a.join("剧目海报.jpg"), b"a").unwrap();
        assert_eq!(
            existing_poster(&drama_a),
            Some(drama_a.join("剧目海报.jpg"))
        );
        assert_eq!(existing_poster(&drama_b), None, "不能拿到别的剧的海报");
        let _ = std::fs::remove_dir_all(base);
    }

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
        // 假设本机没有 PaddleOCR、也没有系统 OCR：tesseract 是唯一后端。
        let probe = OcrBackendProbe {
            paddle_program: None,
            system_available: false,
        };
        let ready = model_runtime_with_probe(&settings, &runner, probe.clone());
        assert!(ready.ready, "{}", ready.guidance);

        let no_chinese = FakeRunner::new()
            .on(&whisper_program, CommandOutput::ok("whisper 1.0"))
            .on_match("tesseract", "--list-langs", CommandOutput::ok("eng"))
            .on("tesseract", CommandOutput::ok("tesseract 5"));
        let missing = model_runtime_with_probe(&settings, &no_chinese, probe);
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

    /// 中文 Windows 自带系统 OCR：tesseract 缺 chi_sim 时，降级链应选中系统 OCR，
    /// 材料仍然就绪且不提示「只依据对白」。此前该分支依赖真实机器探测、无法桩掉，
    /// 导致同一用例在中文系统上必然失败（见 OcrBackendProbe 的说明）。
    #[test]
    fn model_runtime_falls_back_to_system_ocr_when_tesseract_lacks_chinese() {
        let root = std::env::temp_dir().join(format!(
            "dossier-model-runtime-system-ocr-{}-{}",
            std::process::id(),
            std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let whisper = root.join("whisper");
        std::fs::write(&whisper, b"fake").unwrap();
        let mut settings = AppSettings::default();
        let whisper_program = whisper.display().to_string();
        settings.asr_executable = Some(whisper);

        let no_chinese = FakeRunner::new()
            .on(&whisper_program, CommandOutput::ok("whisper 1.0"))
            .on_match("tesseract", "--list-langs", CommandOutput::ok("eng"))
            .on("tesseract", CommandOutput::ok("tesseract 5"));
        let probe = OcrBackendProbe {
            paddle_program: None,
            system_available: true,
        };
        let status = model_runtime_with_probe(&settings, &no_chinese, probe);
        assert!(
            status.ready,
            "系统 OCR 兜底不应阻塞就绪：{}",
            status.guidance
        );
        assert!(
            status.ocr.ok,
            "系统 OCR 应被视为可用：{}",
            status.ocr.detail
        );
        assert_eq!(status.ocr.detail, "使用 系统 OCR");
        assert!(
            !status.guidance.contains("只依据对白"),
            "OCR 可用就不该提示只依据对白：{}",
            status.guidance
        );
        let _ = std::fs::remove_dir_all(root);
    }

    fn proof_payload_fixture() -> serde_json::Value {
        json!({
            "date": "2026-10-29",
            "version": "0.2.24",
            "dramaName": "测试剧",
            "originalEpisodes": 2,
            "evidence": "本地 ASR/OCR，共 2 集",
            "title": "新剧名",
            "titleSource": "AI 生成（模型 m）",
            "synopsis": "简介",
            "synopsisSource": SOURCE_LOCAL_DIR,
            "posterName": "剧目海报.png",
            "posterSource": "沿用（本次输出目录已有；原为 AI 生成（模型 img））",
            "processCount": 4,
            "storyboardSource": "原始素材抽帧",
        })
    }

    #[test]
    fn declaration_lists_items_by_actual_source() {
        let (ai, text) = proof_declaration(&proof_payload_fixture());
        assert!(ai, "含 AI 项时页眉显示徽章");
        assert!(
            text.contains(
                "由人工智能生成：剧名（AI 生成（模型 m））、海报（AI 生成（模型 img））。"
            ),
            "{text}"
        );
        assert!(
            text.contains("非人工智能生成：简介（本地提供（素材目录））、过程图（原始素材抽帧）。"),
            "{text}"
        );
        assert!(!text.contains("请制作方确认"), "{text}");

        // 全部非 AI：不显示徽章，声明里没有「由人工智能生成」
        let mut payload = proof_payload_fixture();
        payload["titleSource"] = json!(SOURCE_SAVED);
        payload["posterSource"] = json!("本地提供（素材封面）");
        let (ai, text) = proof_declaration(&payload);
        assert!(!ai);
        assert!(!text.contains("由人工智能生成："), "{text}");
        // 沿用已保存的剧名：来源未记录，请制作方确认
        assert!(text.contains("剧名沿用本剧已保存的内容"), "{text}");

        // 失败 / 未生成的项不列；过程图 0 张也不列
        let mut payload = proof_payload_fixture();
        payload["title"] = json!("");
        payload["titleSource"] = json!("AI 生成失败，未填写");
        payload["processCount"] = json!(0);
        payload["storyboardSource"] = json!("未生成");
        let (_, text) = proof_declaration(&payload);
        assert!(!text.contains("剧名"), "{text}");
        assert!(!text.contains("过程图"), "{text}");
    }

    #[test]
    fn processing_lines_follow_actual_switches() {
        let full = processing_lines(&ProofProcessing {
            encoder: "libx264（2 路并行）".into(),
            dedup_seed: Some(42),
            badge_text: Some("内容由AI生成".into()),
        });
        assert_eq!(full[0], "源文件原样保留，成片为重新编码的新文件");
        assert!(full.contains(&"编码：libx264（2 路并行）".to_string()));
        assert!(full
            .iter()
            .any(|line| line.starts_with("去重配方：种子 42")));
        assert!(
            full.iter()
                .any(|line| line.contains("make（iPhone 机型）") && line.contains("creation_time")),
            "启用去重配方时必须如实列出元数据改写"
        );
        assert!(full.contains(&"AI 合规角标：已烧录「内容由AI生成」".to_string()));
        // 证明列出的字段必须与实际写入成片的元数据键完全一致（以前漏了 comment / copyright）。
        let metadata_line = full
            .iter()
            .find(|line| line.starts_with("成片元数据改写："))
            .unwrap();
        let plan = crate::dedup::DedupRecipePlan::build("剧A", 1);
        for (key, _) in plan.metadata_tags(0) {
            assert!(
                metadata_line.contains(&format!("{key}（")),
                "证明漏写 {key}：{metadata_line}"
            );
        }
        assert_eq!(
            metadata_line.matches('（').count(),
            plan.metadata_tags(0).len()
        );

        let bare = processing_lines(&ProofProcessing {
            encoder: "h264_nvenc（3 路并行）".into(),
            dedup_seed: None,
            badge_text: None,
        });
        assert!(bare.contains(&"去重配方：未启用".to_string()));
        assert!(bare.contains(&"AI 合规角标：未烧录".to_string()));
        assert!(!bare.iter().any(|line| line.contains("元数据改写")));
    }

    #[test]
    fn proof_text_lists_files_and_handles_missing_records() {
        let mut payload = proof_payload_fixture();
        let file = |name: &str| ProofFile {
            file_name: name.into(),
            size: 3,
            sha256: "ab".repeat(32),
        };
        attach_proof_outcome(
            &mut payload,
            &ProofProcessing {
                encoder: "libx264（2 路并行）".into(),
                dedup_seed: None,
                badge_text: None,
            },
            Some(&[file("第01集.mp4"), file("第02集.mp4")]),
        );
        payload["sources"] = json!([file("ep1.mp4")]);
        let (_, declaration) = proof_declaration(&payload);
        payload["declaration"] = json!(declaration);
        let body = proof_text_body(&payload);
        assert!(body.contains("生成日期：2026-10-29（北京时间）"), "{body}");
        assert!(body.contains("成片清单（2 个）"), "{body}");
        assert!(body.contains(&format!("第02集.mp4　3 字节　SHA-256 {}", "ab".repeat(32))));
        assert!(body.contains("原始素材（1 个）"), "{body}");
        assert!(body.contains("  · 去重配方：未启用"), "{body}");

        // 旧清单：没有成片清单与处理方式；没有简介、过程图 0 张
        let mut old = proof_payload_fixture();
        old["synopsis"] = json!("");
        old["processCount"] = json!(0);
        let body = proof_text_body(&old);
        assert!(
            body.contains("处理方式：\n  未记录（上次深度处理未完成）"),
            "{body}"
        );
        assert!(
            body.contains("成片清单：未记录（上次深度处理未完成）"),
            "{body}"
        );
        assert!(
            body.contains("原始素材：未记录（素材指纹未能计算）"),
            "{body}"
        );
        assert!(body.contains("过程图：未生成"), "{body}");
        assert!(!body.contains("0 字"), "{body}");
    }

    #[test]
    fn proof_files_hash_each_file_and_report_missing() {
        let dir = std::env::temp_dir().join(format!("dossier-proof-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.mp4"), b"abc").unwrap();
        let cancel = CancelToken::new();
        let files = proof_files(&[dir.join("a.mp4")], &cancel).unwrap();
        assert_eq!(
            files,
            vec![ProofFile {
                file_name: "a.mp4".into(),
                size: 3,
                // sha256("abc")
                sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
            }]
        );
        let error = proof_files(&[dir.join("缺失.mp4")], &cancel).unwrap_err();
        assert!(error.contains("缺失.mp4"), "{error}");
        cancel.cancel();
        assert_eq!(
            proof_files(&[dir.join("a.mp4")], &cancel).unwrap_err(),
            "已停止"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
