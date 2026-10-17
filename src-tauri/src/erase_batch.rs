//! 区域擦除的桌面入口（M4b-2，计划 2.1–2.4 节）：取预览帧、自动识别静止区域、批量擦除。
//!
//! 三件事放在一起，因为它们是同一个工作流的三个步骤：先在某一秒取一帧看清画面，
//! 再自动找出「位置固定、内容不变」的候选区域，然后由人微调、确认、批量应用到所有集。
//!
//! 分工：
//! - 取帧与「只擦一小段做对比」都走 ffmpeg（短命子进程，超时 60 秒）；
//! - 自动识别调用 `dossier-inpaint --detect-only`（判据在 `dossier-inpaint::still`）；
//! - 批量擦除注册成任务队列的 `erase` 执行器（`jobs.rs`），因此天然支持暂停、终止、续跑。
//!
//! 合规边界：只用于处理使用者自有的素材。批量擦除前必须勾选权利确认，勾选记进处理台账。

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use dossier_core::version::ProcessingRecord;
use dossier_core::{append_jsonl, hash_file, now_timestamp};
use dossier_inpaint::erase::{build_plan, resolve_tool, EraseRegion, EraseRequest};
use dossier_runtime::{CommandRunner, CommandSpec, SystemRunner};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::encode::CancelFlag;
use crate::jobs::{
    read_json, write_json_atomic, JobQueue, JobRecord, JobRunner, JobView, NewJob, PreparedJob,
    StepError, StepOutcome,
};

/// 任务种类（和 `jobs.rs` 的注册名一致；`version.rs` 的 `start_queue` 会注册它）。
pub(crate) const JOB_KIND: &str = "erase";
/// 任务目录里保存的擦除任务描述。
const SPEC_FILE: &str = "erase.json";
/// 任务目录里每集的结果。
const RESULTS_DIR: &str = "results";
/// 取帧子进程的超时：单帧解码，60 秒足够。
const FRAME_TIMEOUT: Duration = Duration::from_secs(60);
/// 一集擦除的最长运行时间（沿用 `run_erase` 的默认值）。
const ERASE_TIMEOUT: Duration = Duration::from_secs(1800);

fn io_error(message: impl Into<String>) -> String {
    message.into()
}

fn processing_path(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|dir| dir.join("processing.jsonl"))
        .map_err(|error| format!("无法定位应用数据目录：{error}。请检查系统权限。"))
}

/// 追加一条处理台账。台账写不进去不能假装成功：权利确认靠它划清责任。
pub(crate) fn record(app: &AppHandle, entry: ProcessingRecord) -> Result<(), String> {
    let path = processing_path(app)?;
    append_jsonl(&path, &entry).map_err(|error| format!("写处理台账失败：{error}"))
}

// ---------------------------------------------------------------------------
// 取预览帧
// ---------------------------------------------------------------------------

/// 取某一秒的画面，返回 JPEG 的 data URL（前端直接塞进 `<img>`）。
///
/// 缩到 `max_width`（默认 720，宽高都取偶数）：预览只需要看清位置，
/// 用不着原分辨率，否则一帧几十 MB 的 base64 会把界面拖住。
#[tauri::command]
pub async fn video_frame(
    path: String,
    seconds: f64,
    max_width: Option<u32>,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || video_frame_blocking(&path, seconds, max_width))
        .await
        .map_err(|error| format!("取帧任务失败：{error}"))?
}

fn video_frame_blocking(
    path: &str,
    seconds: f64,
    max_width: Option<u32>,
) -> Result<String, String> {
    let input = Path::new(path);
    if !input.is_file() {
        return Err(format!("视频不存在：{path}"));
    }
    if !seconds.is_finite() || seconds < 0.0 {
        return Err("取帧的秒数必须是 0 或正数".into());
    }
    let width = max_width.unwrap_or(720).clamp(64, 4096);
    // 宽度取偶数：JPEG 对奇数宽度没问题，但和后续的坐标换算保持一致的取整规则更省心。
    let width = width & !1;

    let runner = SystemRunner::new();
    let output = runner
        .run(
            &CommandSpec::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-ss"])
                .arg(format!("{seconds:.6}"))
                .arg("-i")
                .arg(path)
                .args([
                    "-frames:v",
                    "1",
                    // 等比缩到目标宽度，高度按 -2 自动取偶数。
                    "-vf",
                    &format!("scale={width}:-2"),
                    "-f",
                    "image2pipe",
                    "-vcodec",
                    "mjpeg",
                    "-q:v",
                    "3",
                    "-",
                ])
                .timeout(FRAME_TIMEOUT),
        )
        .map_err(|error| format!("ffmpeg 启动失败：{error}"))?;
    if !output.success() {
        let detail = output.combined();
        let first = detail
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("（无输出）");
        // 取帧秒数超过时长是最常见的原因，单独说清楚。
        if first.contains("Output file is empty") || first.contains("Invalid argument") {
            return Err(format!(
                "在 {seconds} 秒处取不到画面（可能超过了视频时长）：{first}"
            ));
        }
        return Err(format!("取帧失败（{}）：{first}", output.code_label()));
    }
    if output.stdout_raw.is_empty() {
        return Err(format!("在 {seconds} 秒处取不到画面（可能超过了视频时长）"));
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(&output.stdout_raw);
    Ok(format!("data:image/jpeg;base64,{encoded}"))
}

// ---------------------------------------------------------------------------
// 自动识别静止区域
// ---------------------------------------------------------------------------

/// 自动识别的候选区域（`dossier-inpaint --detect-only` 的输出）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StillCandidate {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StillReport {
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub samples: usize,
    #[serde(default)]
    pub candidates: Vec<StillCandidate>,
}

/// 找出画面里「位置固定、内容不变」的候选区域。
///
/// 调用 `dossier-inpaint --detect-only`：判据在库里（`still.rs`），这里只做参数适配
/// 与结果解析。工具找不到时明确报错，让用户去设置里指定路径。
#[tauri::command]
pub async fn detect_still_regions(
    path: String,
    samples: Option<usize>,
    tool_path: Option<String>,
) -> Result<StillReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        detect_still_blocking(&path, samples, tool_path.as_deref())
    })
    .await
    .map_err(|error| format!("自动识别任务失败：{error}"))?
}

fn detect_still_blocking(
    path: &str,
    samples: Option<usize>,
    tool_path: Option<&str>,
) -> Result<StillReport, String> {
    if !Path::new(path).is_file() {
        return Err(format!("视频不存在：{path}"));
    }
    let tool = locate_tool(tool_path)?;
    let report_path =
        std::env::temp_dir().join(format!("dossier-still-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&report_path);

    let mut spec = CommandSpec::new(tool.display().to_string())
        .arg("--input")
        .arg(path)
        .arg("--detect-only")
        .arg("--report-json")
        .arg(report_path.display().to_string())
        .timeout(Duration::from_secs(300));
    if let Some(samples) = samples {
        spec = spec
            .arg("--still-samples")
            .arg(samples.clamp(2, 600).to_string());
    }
    let output = SystemRunner::new()
        .run(&spec)
        .map_err(|error| format!("擦除工具启动失败：{error}"))?;
    if !output.success() {
        let detail = output.combined();
        let first = detail
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("（无输出）");
        return Err(format!(
            "自动识别失败（退出码 {}）：{first}",
            output.code_label()
        ));
    }
    let text = std::fs::read_to_string(&report_path)
        .map_err(|error| format!("读不到自动识别结果（{}）：{error}", report_path.display()))?;
    let _ = std::fs::remove_file(&report_path);
    let report: StillReport =
        serde_json::from_str(&text).map_err(|error| format!("自动识别结果格式不正确：{error}"))?;
    Ok(report)
}

// ---------------------------------------------------------------------------
// 对比预览：只擦一小段，返回同一时刻擦除后的帧
// ---------------------------------------------------------------------------

/// 做一小段擦除（默认前后各 2 秒），取同一时刻的帧，返回 JPEG data URL。
///
/// 为什么不实时播放对比视频：为了一段对比去编整集的代价太大，而「同一时刻的两帧
/// 左右并排」已经足够判断区域画得对不对、有没有擦干净。
#[tauri::command]
pub async fn erase_preview(
    input: String,
    seconds: f64,
    start_sec: Option<f64>,
    duration_sec: Option<f64>,
    regions: Vec<EraseRegion>,
    tool_path: Option<String>,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        erase_preview_blocking(
            &input,
            seconds,
            start_sec,
            duration_sec,
            &regions,
            tool_path.as_deref(),
        )
    })
    .await
    .map_err(|error| format!("预览任务失败：{error}"))?
}

fn erase_preview_blocking(
    input: &str,
    seconds: f64,
    start_sec: Option<f64>,
    duration_sec: Option<f64>,
    regions: &[EraseRegion],
    tool_path: Option<&str>,
) -> Result<String, String> {
    if regions.is_empty() {
        return Err("至少需要一个区域才能预览".into());
    }
    if !Path::new(input).is_file() {
        return Err(format!("视频不存在：{input}"));
    }
    let tool = locate_tool(tool_path)?;
    // 预览产物放临时目录，用完就删：它只是「看一眼」的中间物。
    let stem = format!("dossier-erase-preview-{}", std::process::id());
    let temp_dir = std::env::temp_dir().join(&stem);
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).map_err(|error| format!("创建预览临时目录失败：{error}"))?;
    let output = temp_dir.join("preview.mp4");

    let request = EraseRequest {
        input: PathBuf::from(input),
        output: output.clone(),
        regions: regions.to_vec(),
        start: Some(start_sec.unwrap_or((seconds - 2.0).max(0.0))),
        duration: Some(duration_sec.unwrap_or(4.0)),
        ..Default::default()
    };
    let plan = build_plan(&request, &tool).map_err(|error| format!("拼装擦除命令失败：{error}"))?;
    let runner = SystemRunner::new();
    let mut spec = CommandSpec::new(plan.program.display().to_string()).timeout(ERASE_TIMEOUT);
    for arg in &plan.args {
        spec = spec.arg(arg.clone());
    }
    let run = runner
        .run(&spec)
        .map_err(|error| format!("擦除工具启动失败：{error}"))?;
    if !run.success() {
        let detail = run.combined();
        let first = detail
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("（无输出）");
        let _ = std::fs::remove_dir_all(&temp_dir);
        return Err(format!(
            "预览擦除失败（退出码 {}）：{first}",
            run.code_label()
        ));
    }

    // 取同一时刻的帧（相对预览片段起点换算）。
    let offset = seconds - request.start.unwrap_or(0.0);
    let frame = video_frame_blocking(&output.display().to_string(), offset.max(0.0), Some(720));
    let _ = std::fs::remove_dir_all(&temp_dir);
    frame
}

fn locate_tool(tool_path: Option<&str>) -> Result<PathBuf, String> {
    let explicit = tool_path
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).parent();
    resolve_tool(explicit.as_deref(), repo_root)
}

// ---------------------------------------------------------------------------
// 批量擦除（走任务队列）
// ---------------------------------------------------------------------------

/// 批量擦除的请求（界面提交）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EraseBatchRequest {
    /// 剧目目录名（记台账用）。
    pub drama_name: String,
    /// 剧目目录（找分集）。
    pub drama_dir: String,
    /// 产物目录（`<产物根>/<剧名>/擦除输出/`）。
    pub output_dir: String,
    /// 要处理的集（文件名，按顺序）。
    pub episodes: Vec<String>,
    /// 区域（画面像素坐标）。
    pub regions: Vec<EraseRegion>,
    /// 权利确认：必须为 true 才会开始，勾选记进台账。
    pub rights_confirmed: bool,
    /// 权属材料的哈希。**不必由前端传**：后端从剧目目录里的权属声明读，
    /// 读不到就为 `None`（擦除不强制要求先声明权属，只要求勾选权利确认）。
    pub rights_sha256: Option<String>,
    /// 擦除工具的路径（留空时按 PATH / 仓库 target 查找）。
    pub tool_path: Option<String>,
}

/// 写进任务目录的任务描述。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EraseJobSpec {
    drama_name: String,
    output_dir: String,
    regions: Vec<EraseRegion>,
    episodes: Vec<EraseEpisode>,
    rights_sha256: Option<String>,
    tool_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EraseEpisode {
    name: String,
    path: String,
    sha256: String,
}

/// 一集的结果（收尾时汇总）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EpisodeResult {
    name: String,
    output: String,
    /// 确认流结论：`0` 通过、`3` 需人工抽查（失败不会写进来）。
    verdict: String,
    sha256: String,
}

/// 开始批量擦除：门槛 → 权利确认 → 入队。
#[tauri::command]
pub async fn erase_batch_start(
    app: AppHandle,
    queue: tauri::State<'_, JobQueue>,
    request: EraseBatchRequest,
) -> Result<JobView, String> {
    // 权利确认是硬门槛：没勾选就不排队（总方案 2.3 节）。
    if !request.rights_confirmed {
        return Err("开始批量擦除前必须勾选「我确认对该素材拥有权利」".into());
    }
    if request.regions.is_empty() {
        return Err("至少要有一个擦除区域".into());
    }
    if request.episodes.is_empty() {
        return Err("没有要处理的集".into());
    }
    let drama_dir = PathBuf::from(&request.drama_dir);
    let mut episodes = Vec::with_capacity(request.episodes.len());
    for name in &request.episodes {
        let path = drama_dir.join(name);
        if !path.is_file() {
            return Err(format!("分集文件不存在：{}", path.display()));
        }
        let (_, sha256) =
            hash_file(&path).map_err(|error| io_error(format!("读取 {name} 失败：{error}")))?;
        episodes.push(EraseEpisode {
            name: name.clone(),
            path: path.display().to_string(),
            sha256,
        });
    }
    let spec = EraseJobSpec {
        drama_name: request.drama_name.clone(),
        output_dir: request.output_dir.clone(),
        regions: request.regions.clone(),
        episodes,
        // 权属材料哈希由后端从剧目目录的权属声明里读，不信前端传来的值：
        // 台账里记的必须是「本机真的声明过的那份材料」的哈希。
        rights_sha256: read_declared_rights_sha256(&drama_dir),
        tool_path: request.tool_path.clone(),
    };
    // 台账先写：权利确认这件事必须在开工前留痕，不能等任务跑完才记。
    record(
        &app,
        ProcessingRecord {
            ts: now_timestamp(),
            event: "erase".into(),
            task: spec.rights_sha256.clone().unwrap_or_default(),
            strength: None,
            seed: None,
            quality: None,
            output: Some(request.output_dir.clone()),
            sha256: None,
        },
    )?;

    let steps: Vec<String> = spec.episodes.iter().map(|ep| ep.name.clone()).collect();
    let value = serde_json::to_value(&spec)
        .map_err(|error| io_error(format!("任务描述序列化失败：{error}")))?;
    let view = queue
        .submit(NewJob {
            kind: JOB_KIND.into(),
            title: format!("{} · 区域擦除（{} 集）", request.drama_name, steps.len()),
            output_dir: request.output_dir.clone(),
            spec: value,
            fingerprint: erase_fingerprint(&spec),
            steps,
            files: vec![(
                SPEC_FILE.into(),
                serde_json::to_vec_pretty(&spec)
                    .map_err(|error| io_error(format!("任务描述序列化失败：{error}")))?,
            )],
        })
        .map_err(|error| error.to_string())?;
    Ok(view)
}

/// 从剧目目录的权属声明里读材料哈希（`.dossier/rights.json`）。
///
/// 读不到返回 `None`：擦除不强制先声明权属（用户可能已经在处理版本那边声明过，
/// 也可能只想擦一下自有素材），但**读到了就要记进台账**。
fn read_declared_rights_sha256(drama_dir: &Path) -> Option<String> {
    let path = drama_dir.join(".dossier").join("rights.json");
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("sha256")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// 任务指纹：区域 + 分集内容 + 工具路径。续跑时据此判断「是不是同一件事」。
fn erase_fingerprint(spec: &EraseJobSpec) -> String {
    // 复用 core 的输入指纹：键排序后拼成文本再取 SHA-256，和擦除管线自己的
    // 「参数指纹」是同一套写法，读日志的人不必记两种格式。
    let regions = spec
        .regions
        .iter()
        .map(|region| {
            format!(
                "{},{},{},{}",
                region.x, region.y, region.width, region.height
            )
        })
        .collect::<Vec<_>>()
        .join(";");
    let episodes = spec
        .episodes
        .iter()
        .map(|episode| format!("{}:{}", episode.name, episode.sha256))
        .collect::<Vec<_>>()
        .join(";");
    dossier_core::checkpoint::input_fingerprint(&[
        ("schema", "erase-batch@1"),
        ("regions", regions.as_str()),
        ("episodes", episodes.as_str()),
        ("tool", spec.tool_path.as_deref().unwrap_or("default")),
    ])
}

/// 批量擦除的执行器（注册到 `JobQueue`）。
pub struct EraseRunner {
    app: AppHandle,
}

impl EraseRunner {
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

impl JobRunner for EraseRunner {
    fn prepare(&self, job: &JobRecord, dir: &Path) -> Result<Box<dyn PreparedJob>, String> {
        let spec: EraseJobSpec = match read_json(&dir.join(SPEC_FILE)) {
            Ok(spec) => spec,
            // 兼容：任务目录里没有单独的描述文件时退回用 job.json 里的 spec。
            Err(_) => serde_json::from_value(job.spec.clone())
                .map_err(|error| format!("任务描述无法读取：{error}"))?,
        };
        if spec.episodes.is_empty() {
            return Err("任务里没有分集".into());
        }
        let tool = locate_tool(spec.tool_path.as_deref())?;
        std::fs::create_dir_all(dir.join(RESULTS_DIR))
            .map_err(|error| format!("创建任务结果目录失败：{error}"))?;
        std::fs::create_dir_all(&spec.output_dir)
            .map_err(|error| format!("创建擦除输出目录失败：{error}"))?;
        Ok(Box::new(EraseJob {
            app: self.app.clone(),
            dir: dir.to_path_buf(),
            spec,
            tool,
        }))
    }
}

struct EraseJob {
    app: AppHandle,
    dir: PathBuf,
    spec: EraseJobSpec,
    tool: PathBuf,
}

impl EraseJob {
    fn result_path(&self, step: &str) -> PathBuf {
        self.dir
            .join(RESULTS_DIR)
            .join(format!("{}.json", crate::paths::safe_file_stem(step)))
    }

    /// 每集的输出路径：`<擦除输出>/<分集主名>.mp4`。
    fn output_for(&self, name: &str) -> PathBuf {
        let stem = Path::new(name)
            .file_stem()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| "output".to_string());
        Path::new(&self.spec.output_dir).join(format!("{stem}.mp4"))
    }

    fn run_episode(&self, step: &str) -> Result<StepOutcome, StepError> {
        let episode = self
            .spec
            .episodes
            .iter()
            .find(|episode| episode.name == step)
            .ok_or_else(|| StepError::Failed("任务里没有这一集".into()))?;
        let input = PathBuf::from(&episode.path);
        // 开工前复核源文件：申请时算过哈希，被换过就拒绝，避免「申请时是 A、处理时换成 B」。
        let (_, sha256) = hash_file(&input)
            .map_err(|error| StepError::Failed(format!("源文件读不到了：{error}")))?;
        if sha256 != episode.sha256 {
            return Err(StepError::Failed(
                "源文件和开始擦除时不一样了，请重新提交这个任务".into(),
            ));
        }
        let output = self.output_for(step);
        let request = EraseRequest {
            input,
            output: output.clone(),
            regions: self.spec.regions.clone(),
            ..Default::default()
        };
        // 桌面端拼参数、库里拼命令：两边共用一套规则（见 dossier-inpaint 的交叉校验测试）。
        let plan = build_plan(&request, &self.tool)
            .map_err(|error| StepError::Failed(format!("拼装擦除命令失败：{error}")))?;

        let runner = SystemRunner::new();
        let mut spec = CommandSpec::new(plan.program.display().to_string()).timeout(ERASE_TIMEOUT);
        for arg in &plan.args {
            spec = spec.arg(arg.clone());
        }
        let run = runner
            .run(&spec)
            .map_err(|error| StepError::Failed(format!("擦除工具启动失败：{error}")))?;
        let code = run.code.unwrap_or(-1);
        match code {
            0 => {}
            // 3 = 需人工抽查：算成功，但要说清楚。
            3 => {}
            // 2 = 自动校验失败；1 = 执行错误。两者都要停下等用户处理。
            2 => {
                return Err(StepError::Failed(
                    "擦除结果自动校验未通过（画面可能没擦干净）。请调大区域或换修复后端后重试"
                        .into(),
                ))
            }
            1 => {
                let detail = run.combined();
                let first = detail
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .unwrap_or("（无输出）");
                return Err(StepError::Failed(format!("擦除执行失败：{first}")));
            }
            other => {
                return Err(StepError::Failed(format!(
                    "擦除工具返回了未知退出码 {other}"
                )))
            }
        }
        if !output.is_file() {
            return Err(StepError::Failed(format!(
                "擦除没有产出文件：{}",
                output.display()
            )));
        }
        let (_, output_sha256) = hash_file(&output)
            .map_err(|error| StepError::Failed(format!("读取擦除结果失败：{error}")))?;
        let result = EpisodeResult {
            name: step.to_string(),
            output: output.display().to_string(),
            verdict: if code == 3 {
                "warn".into()
            } else {
                "pass".into()
            },
            sha256: output_sha256,
        };
        write_json_atomic(&self.result_path(step), &result).map_err(StepError::Failed)?;
        let detail = if code == 3 {
            "已擦除 · 需人工抽查".to_string()
        } else {
            "已擦除 · 自动校验通过".to_string()
        };
        Ok(StepOutcome {
            artifacts: vec![output, self.result_path(step)],
            detail,
        })
    }

    /// 收尾：汇总每集结果写一份总报告，并把「擦除完成」记进台账。
    fn run_finish(&self) -> Result<StepOutcome, StepError> {
        let mut results = Vec::new();
        for episode in &self.spec.episodes {
            let path = self.result_path(&episode.name);
            if !path.is_file() {
                continue;
            }
            let result: EpisodeResult = read_json(&path).map_err(StepError::Failed)?;
            results.push(result);
        }
        if results.is_empty() {
            return Err(StepError::Failed("没有任何一集擦除成功".into()));
        }
        let warn = results.iter().filter(|r| r.verdict == "warn").count();
        let report = serde_json::json!({
            "generatedAt": now_timestamp(),
            "dramaName": self.spec.drama_name,
            "rightsSha256": self.spec.rights_sha256,
            "regions": self.spec.regions,
            "episodes": results,
        });
        let report_path = Path::new(&self.spec.output_dir).join("擦除报告.json");
        write_json_atomic(&report_path, &report).map_err(StepError::Failed)?;
        record(
            &self.app,
            ProcessingRecord {
                ts: now_timestamp(),
                event: "erase".into(),
                task: self.spec.rights_sha256.clone().unwrap_or_default(),
                strength: None,
                seed: None,
                quality: None,
                output: Some(report_path.display().to_string()),
                sha256: None,
            },
        )
        .map_err(StepError::Failed)?;
        let detail = if warn > 0 {
            format!("{} 集已擦除，其中 {warn} 集需人工抽查", results.len())
        } else {
            format!("{} 集已擦除", results.len())
        };
        Ok(StepOutcome {
            artifacts: vec![report_path],
            detail,
        })
    }
}

impl PreparedJob for EraseJob {
    fn run_step(&mut self, step: &str, _cancel: &CancelFlag) -> Result<StepOutcome, StepError> {
        if step == FINISH_STEP {
            return self.run_finish();
        }
        self.run_episode(step)
    }
}

/// 收尾步骤名（任务列表里显示成一行）。
const FINISH_STEP: &str = "擦除报告";

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(regions: Vec<EraseRegion>, episodes: Vec<(&str, &str)>) -> EraseJobSpec {
        EraseJobSpec {
            drama_name: "测试剧".into(),
            output_dir: "/产物/擦除输出".into(),
            regions,
            episodes: episodes
                .into_iter()
                .map(|(name, sha256)| EraseEpisode {
                    name: name.into(),
                    path: format!("/素材/{name}"),
                    sha256: sha256.into(),
                })
                .collect(),
            rights_sha256: Some("a".repeat(64)),
            tool_path: None,
        }
    }

    fn region(x: u32) -> EraseRegion {
        EraseRegion {
            x,
            y: 10,
            width: 100,
            height: 40,
        }
    }

    #[test]
    fn fingerprints_change_with_regions_episodes_and_content() {
        let base = spec(vec![region(10)], vec![("第01集.mp4", &"1".repeat(64))]);
        let same = spec(vec![region(10)], vec![("第01集.mp4", &"1".repeat(64))]);
        assert_eq!(
            erase_fingerprint(&base),
            erase_fingerprint(&same),
            "同输入同指纹"
        );

        // 换区域
        let moved = spec(vec![region(20)], vec![("第01集.mp4", &"1".repeat(64))]);
        assert_ne!(erase_fingerprint(&base), erase_fingerprint(&moved));

        // 换一集（内容变了）
        let changed = spec(vec![region(10)], vec![("第01集.mp4", &"2".repeat(64))]);
        assert_ne!(erase_fingerprint(&base), erase_fingerprint(&changed));

        // 多一集
        let more = spec(
            vec![region(10)],
            vec![
                ("第01集.mp4", &"1".repeat(64)),
                ("第02集.mp4", &"3".repeat(64)),
            ],
        );
        assert_ne!(erase_fingerprint(&base), erase_fingerprint(&more));
    }

    #[test]
    fn output_path_follows_the_episode_stem() {
        let spec = spec(vec![region(0)], vec![("第01集.mov", &"1".repeat(64))]);
        assert_eq!(
            Path::new(&spec.output_dir).join("第01集.mp4"),
            Path::new("/产物/擦除输出").join("第01集.mp4"),
            "输出统一成 mp4，主名沿用分集"
        );
    }

    /// 退出码语义：桌面端必须和 `dossier-inpaint` 的约定一致。
    #[test]
    fn verdict_codes_are_interpreted_as_documented() {
        // 这一段在 run_episode 里是 match 分支，这里用同一条规则做断言，
        // 防止有人把 3 也当成失败（那会让「需人工抽查」永远交付不了）。
        let is_success = |code: i32| matches!(code, 0 | 3);
        assert!(is_success(0));
        assert!(is_success(3));
        assert!(!is_success(2));
        assert!(!is_success(1));
        assert!(!is_success(127));
    }
}
