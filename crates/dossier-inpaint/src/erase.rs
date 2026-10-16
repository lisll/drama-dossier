//! 桌面端擦除入口的**命令构建**（Tauri 命令层只做参数适配，规则在这里）。
//!
//! 为什么单独放一层：桌面壳需要「先给用户看一条可复制的命令，确认后再执行」，
//! 这与 CLI 的 `clap` 解析是两套输入路径。把参数拼装放在库里，就能用单测守住
//! 「桌面端拼出来的参数一定是 CLI 认得的」（见 `main.rs` 里的交叉校验测试），
//! 而不是等用户点了按钮才发现参数写错。
//!
//! 合规边界：只用于处理使用者自有的素材，不做 DRM 绕过，不处理他人作品的版权标识。

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use dossier_core::ProgressEvent;
use serde::{Deserialize, Serialize};

/// 一个矩形区域（像素，原点左上角）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EraseRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl EraseRegion {
    fn cli_value(self) -> String {
        format!("{},{},{},{}", self.x, self.y, self.width, self.height)
    }
}

/// 桌面端提交的擦除请求（对应界面上的表单）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EraseRequest {
    pub input: PathBuf,
    pub output: PathBuf,
    /// 固定区域；`detector = "fixed"` 时至少给一个。
    pub regions: Vec<EraseRegion>,
    /// `fixed`（按区域）或 `worker`（DBNet 检测）。
    pub detector: String,
    /// `column`（本地基线）或 `worker`（时序模型）。
    pub inpainter: String,
    /// 只处理该时间点之后的画面（秒）。
    pub start: Option<f64>,
    pub duration: Option<f64>,
    /// 掩码膨胀半径。
    pub dilate: u32,
    /// 回贴羽化半径。
    pub feather: u32,
    /// ROI 外扩比例。
    pub context: f64,
    /// 时间窗内的检测置信度下限。
    pub min_score: f64,
    /// 常驻 worker（`--persistent-worker`）。
    pub persistent_worker: bool,
    /// 处理日志路径；缺省写到 `<输出>.inpaint-report.json`。
    pub report_json: Option<PathBuf>,
    /// 进度流路径（JSONL，覆盖写）；缺省不写。
    ///
    /// 擦除是逐帧长任务，桌面端只轮询进程会拿不到「跑到哪了」。给了路径后，
    /// 管线会把 `locate` / `inpaint` 两个阶段与逐片段帧计数按
    /// `dossier-core::progress` 的格式写进这个文件，桌面端边跑边读即可画确定进度条。
    pub progress_json: Option<PathBuf>,
    /// worker 可执行文件；worker 模式必填。
    pub worker_program: Option<String>,
    /// worker 参数。
    pub worker_args: Vec<String>,
    /// 关闭自动校验（默认开启）。
    pub no_verify: bool,
    /// ffprobe 可执行文件；缺省时管线自己从 PATH 找。
    /// 桌面端必须传：GUI 进程的 PATH 很干净，找到 ffprobe 全靠应用自己解析好的路径。
    pub ffprobe: Option<String>,
}

impl Default for EraseRequest {
    fn default() -> Self {
        Self {
            input: PathBuf::new(),
            output: PathBuf::new(),
            regions: Vec::new(),
            detector: "fixed".into(),
            inpainter: "column".into(),
            start: None,
            duration: None,
            dilate: 4,
            feather: 2,
            context: 1.6,
            min_score: 0.5,
            persistent_worker: false,
            report_json: None,
            progress_json: None,
            worker_program: None,
            worker_args: Vec::new(),
            no_verify: false,
            ffprobe: None,
        }
    }
}

impl EraseRequest {
    /// 校验表单；返回可直接展示的中文错误。
    pub fn validate(&self) -> Result<(), String> {
        if self.input.as_os_str().is_empty() {
            return Err("请选择输入视频。".into());
        }
        if self.output.as_os_str().is_empty() {
            return Err("请指定输出视频路径。".into());
        }
        // 输出指向输入本身时，管线会边读边写同一个文件，直接毁掉源素材。
        if dossier_core::same_path(&self.input, &self.output) {
            return Err("输出视频不能与输入视频相同：那会覆盖原素材。请换一个输出路径。".into());
        }
        match self.detector.as_str() {
            "fixed" => {
                if self.regions.is_empty() {
                    return Err("按固定区域擦除时至少要框选一个区域。".into());
                }
            }
            "worker" => {}
            other => return Err(format!("未知检测后端 {other}；可选 fixed / worker。")),
        }
        match self.inpainter.as_str() {
            "column" | "worker" => {}
            other => return Err(format!("未知修复后端 {other}；可选 column / worker。")),
        }
        if self.regions.iter().any(|r| r.width == 0 || r.height == 0) {
            return Err("区域宽高必须大于 0。".into());
        }
        if self.dilate > 64 || self.feather > 64 {
            return Err("掩码膨胀与羽化半径不超过 64 像素。".into());
        }
        if !self.context.is_finite() || self.context < 1.0 || self.context > 8.0 {
            return Err("ROI 外扩比例必须在 1.0–8.0 之间。".into());
        }
        if !(0.0..=1.0).contains(&self.min_score) {
            return Err("检测置信度下限必须在 0–1 之间。".into());
        }
        if let Some(start) = self.start {
            if !start.is_finite() || start < 0.0 {
                return Err("起始时间必须是非负秒数。".into());
            }
        }
        if let Some(duration) = self.duration {
            if !duration.is_finite() || duration <= 0.0 {
                return Err("处理时长必须是大于 0 的秒数。".into());
            }
        }
        if self.needs_worker() && self.worker_program.is_none() {
            return Err("使用模型后端时需要给出 worker 可执行文件（如 python3）。".into());
        }
        Ok(())
    }

    /// 是否需要模型 worker。
    pub fn needs_worker(&self) -> bool {
        self.detector == "worker" || self.inpainter == "worker"
    }

    /// 处理日志路径（缺省派生）。
    pub fn effective_report_path(&self) -> PathBuf {
        self.report_json.clone().unwrap_or_else(|| {
            let mut name = self.output.as_os_str().to_os_string();
            name.push(".inpaint-report.json");
            PathBuf::from(name)
        })
    }
}

/// 可直接执行 / 展示的命令。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErasePlan {
    /// 可执行文件路径。
    pub program: PathBuf,
    /// 完整参数（不含 program 本身）。
    pub args: Vec<String>,
    /// 供界面复制的完整命令（shell 已转义）。
    pub command_line: String,
    pub report_path: PathBuf,
    /// 进度流路径（`None` = 本次不写进度）；桌面端据此边跑边读。
    pub progress_path: Option<PathBuf>,
    pub requires_worker: bool,
}

/// 退出码 → 结论语义（与 `main.rs` 的确认流退出码一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EraseOutcome {
    /// 0：自动确认通过。
    Passed,
    /// 2：自动校验失败（应加力重跑或转人工）。
    VerifyFailed,
    /// 3：需要人工抽查。
    NeedsReview,
    /// 1 / 其他：执行错误。
    ExecError,
}

impl EraseOutcome {
    /// 按退出码判定结论。
    pub fn from_exit_code(code: i32) -> Self {
        match code {
            0 => Self::Passed,
            2 => Self::VerifyFailed,
            3 => Self::NeedsReview,
            _ => Self::ExecError,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Passed => "自动确认通过",
            Self::VerifyFailed => "自动校验失败",
            Self::NeedsReview => "需要人工抽查",
            Self::ExecError => "执行错误",
        }
    }
}

/// 拼装命令（不执行）。
pub fn build_plan(request: &EraseRequest, tool: &Path) -> Result<ErasePlan, String> {
    request.validate()?;
    let report_path = request.effective_report_path();
    let mut args: Vec<String> = vec![
        "--input".into(),
        request.input.display().to_string(),
        "--output".into(),
        request.output.display().to_string(),
        "--report-json".into(),
        report_path.display().to_string(),
    ];
    if let Some(progress) = &request.progress_json {
        args.push("--progress-json".into());
        args.push(progress.display().to_string());
    }
    // CLI 缺省会从 PATH 找 ffprobe，而 GUI 子进程的 PATH 几乎是空的；
    // 桌面端把应用解析好的 ffprobe 路径显式传进来，行为才与「本机环境就绪」一致。
    if let Some(ffprobe) = request
        .ffprobe
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        args.push("--ffprobe".into());
        args.push(ffprobe.to_string());
    }
    args.extend([
        "--detector".into(),
        request.detector.clone(),
        "--inpainter".into(),
        request.inpainter.clone(),
        "--dilate".into(),
        request.dilate.to_string(),
        "--feather".into(),
        request.feather.to_string(),
        "--context".into(),
        format!("{:.3}", request.context),
        "--min-score".into(),
        format!("{:.3}", request.min_score),
    ]);
    for region in &request.regions {
        args.push("--roi".into());
        args.push(region.cli_value());
    }
    if let Some(start) = request.start {
        args.push("--start".into());
        args.push(format!("{start:.3}"));
    }
    if let Some(duration) = request.duration {
        args.push("--duration".into());
        args.push(format!("{duration:.3}"));
    }
    if request.persistent_worker && request.needs_worker() {
        args.push("--persistent-worker".into());
    }
    if request.no_verify {
        args.push("--no-verify".into());
    }
    if request.needs_worker() {
        let program = request
            .worker_program
            .as_deref()
            .ok_or_else(|| "使用模型后端时需要给出 worker 可执行文件。".to_string())?;
        args.push("--worker-program".into());
        args.push(program.to_string());
        for item in &request.worker_args {
            args.push("--worker-args".into());
            args.push(item.clone());
        }
    }

    let mut command_line = shell_quote(&tool.display().to_string());
    for arg in &args {
        command_line.push(' ');
        command_line.push_str(&shell_quote(arg));
    }

    Ok(ErasePlan {
        program: tool.to_path_buf(),
        args,
        command_line,
        report_path,
        progress_path: request.progress_json.clone(),
        requires_worker: request.needs_worker(),
    })
}

/// 定位 `dossier-inpaint` 可执行文件：显式路径 → `PATH` → 仓库 `target/`。
pub fn resolve_tool(explicit: Option<&Path>, repo_root: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        return Err(format!("指定的擦除工具不存在：{}", path.display()));
    }
    if let Some(found) = search_path("dossier-inpaint") {
        return Ok(found);
    }
    if let Some(root) = repo_root {
        for profile in ["release", "debug"] {
            let candidate = root
                .join("target")
                .join(profile)
                .join(binary_name("dossier-inpaint"));
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(
        "找不到 dossier-inpaint 可执行文件；请先执行 cargo build --release -p dossier-inpaint，\
         或在设置里指定它的路径。"
            .into(),
    )
}

/// 平台可执行文件名：Windows 上带 `.exe` 后缀，其余平台原样。
pub fn binary_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}

fn search_path(binary: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let candidates: Vec<String> = if cfg!(windows) {
        // Windows 的 PATHEXT 决定可执行后缀；常见顺序里 .exe 覆盖绝大多数场景。
        vec![binary.to_string(), format!("{binary}.exe")]
    } else {
        vec![binary.to_string()]
    };
    for dir in std::env::split_paths(&path_var) {
        for candidate in &candidates {
            let full = dir.join(candidate);
            if full.is_file() {
                return Some(full);
            }
        }
    }
    None
}

/// 执行结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EraseRun {
    pub exit_code: i32,
    pub outcome: EraseOutcome,
    pub outcome_label: String,
    pub stdout_tail: String,
    pub stderr_tail: String,
    /// 处理日志路径（界面用来指路）。
    pub report_path: PathBuf,
    /// 处理日志（若工具写出了合法 JSON）。
    pub report: Option<serde_json::Value>,
}

/// 执行命令并按确认流退出码归档结论。
///
/// `timeout_secs` 为 0 表示不超时；超时后会结束子进程并返回执行错误。
pub fn run_plan(plan: &ErasePlan, timeout_secs: u64) -> Result<EraseRun, String> {
    run_plan_with_progress(plan, timeout_secs, |_| {})
}

/// 进度流游标：只消费**完整行**，把可能写到一半的尾部留到下一次读取。
///
/// 为什么要按字节而不是 `read_to_string`：进度里的 `message` 是中文，
/// 写到一半时尾部可能停在某个多字节字符中间；按 UTF-8 整体解码会直接报错，
/// 把这一段已经落盘的事件一起丢掉。按 `\n` 切行再逐行解码就不会有这个问题
/// （换行是 ASCII，不会被多字节字符误伤）。
#[derive(Default)]
struct ProgressTail {
    offset: u64,
    pending: Vec<u8>,
}

impl ProgressTail {
    /// 读取自上次调用以来新增的完整行；文件还不存在或读不动时返回空。
    fn drain(&mut self, path: &Path) -> Vec<ProgressEvent> {
        let Ok(mut file) = std::fs::File::open(path) else {
            return Vec::new();
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut chunk = Vec::new();
        if file.read_to_end(&mut chunk).is_err() {
            return Vec::new();
        }
        self.offset += chunk.len() as u64;
        self.pending.extend_from_slice(&chunk);

        let mut events = Vec::new();
        while let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=newline).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]);
            let trimmed = text.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(event) = serde_json::from_str::<ProgressEvent>(trimmed) {
                events.push(event);
            }
        }
        events
    }
}

/// 执行擦除，并把进度流里的新事件实时交给 `on_progress`。
///
/// 进度文件由子进程追加写，这里只读；进程被杀/超时也只会丢掉最后半行。
pub fn run_plan_with_progress(
    plan: &ErasePlan,
    timeout_secs: u64,
    mut on_progress: impl FnMut(&ProgressEvent),
) -> Result<EraseRun, String> {
    let mut command = Command::new(&plan.program);
    command
        .args(&plan.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Windows 的 GUI 进程拉起擦除工具会弹黑色 cmd 窗口，CREATE_NO_WINDOW 静默运行。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("无法启动擦除工具（{}）：{error}", plan.program.display()))?;

    let deadline = if timeout_secs == 0 {
        None
    } else {
        Some(Instant::now() + Duration::from_secs(timeout_secs))
    };
    let mut tail = ProgressTail::default();
    let status = loop {
        if let Some(path) = plan.progress_path.as_deref() {
            for event in tail.drain(path) {
                on_progress(&event);
            }
        }
        if let Some(exit) = child
            .try_wait()
            .map_err(|error| format!("等待擦除工具失败：{error}"))?
        {
            break exit;
        }
        if let Some(deadline) = deadline {
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("擦除工具超过 {timeout_secs} 秒未结束，已中止。"));
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    // 进程退出后可能还有最后几条事件没读：补一次，免得界面少收一个终态。
    if let Some(path) = plan.progress_path.as_deref() {
        for event in tail.drain(path) {
            on_progress(&event);
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|error| format!("读取擦除工具输出失败：{error}"))?;
    let exit_code = status.code().unwrap_or(-1);
    let stdout_tail = tail_text(&String::from_utf8_lossy(&output.stdout));
    let stderr_tail = tail_text(&String::from_utf8_lossy(&output.stderr));
    let report = std::fs::read_to_string(&plan.report_path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    let outcome = EraseOutcome::from_exit_code(exit_code);

    Ok(EraseRun {
        exit_code,
        outcome,
        outcome_label: outcome.label().to_string(),
        stdout_tail,
        stderr_tail,
        report_path: plan.report_path.clone(),
        report,
    })
}

fn tail_text(text: &str) -> String {
    let trimmed = text.trim();
    const LIMIT: usize = 4000;
    if trimmed.chars().count() <= LIMIT {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().rev().take(LIMIT).collect();
    out = out.chars().rev().collect();
    format!("…{out}")
}

/// 单引号转义（供界面复制命令用）。
pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-/=:+,@%".contains(c))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> EraseRequest {
        EraseRequest {
            input: PathBuf::from("/tmp/in.mp4"),
            output: PathBuf::from("/tmp/out.mp4"),
            regions: vec![EraseRegion {
                x: 520,
                y: 20,
                width: 90,
                height: 60,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn rejects_output_pointing_at_input() {
        let mut req = request();
        req.output = req.input.clone();
        let err = req.validate().unwrap_err();
        assert!(err.contains("不能与输入视频相同"), "{err}");

        // 结尾分隔符/`.` 之类的写法差异也要认出来。
        let mut variant = request();
        variant.output = PathBuf::from("/tmp/./in.mp4");
        assert!(variant.validate().is_err());
    }

    #[test]
    fn builds_column_baseline_plan_without_worker() {
        let plan = build_plan(&request(), Path::new("/usr/local/bin/dossier-inpaint")).unwrap();
        assert!(!plan.requires_worker);
        assert_eq!(
            plan.report_path,
            PathBuf::from("/tmp/out.mp4.inpaint-report.json")
        );
        let args = plan.args.join(" ");
        assert!(args.contains("--roi 520,20,90,60"), "{args}");
        assert!(args.contains("--detector fixed"), "{args}");
        assert!(args.contains("--inpainter column"), "{args}");
        assert!(!args.contains("--worker-program"));
        assert!(!args.contains("--persistent-worker"));
        assert!(plan
            .command_line
            .starts_with("/usr/local/bin/dossier-inpaint --input"));
    }

    #[test]
    fn ffprobe_path_lands_in_plan_args() {
        // GUI 子进程 PATH 很干净：桌面端解析好的 ffprobe 必须原样进命令行。
        let mut req = request();
        req.ffprobe = Some("/opt/homebrew/bin/ffprobe".into());
        let args = build_plan(&req, Path::new("/bin/tool"))
            .unwrap()
            .args
            .join(" ");
        assert!(
            args.contains("--ffprobe /opt/homebrew/bin/ffprobe"),
            "{args}"
        );

        // 缺省时不加 --ffprobe，交给 CLI 自己从 PATH 找。
        let args = build_plan(&request(), Path::new("/bin/tool"))
            .unwrap()
            .args
            .join(" ");
        assert!(!args.contains("--ffprobe"), "{args}");
    }

    #[test]
    fn worker_request_requires_program_and_can_go_persistent() {
        let mut req = request();
        req.detector = "worker".into();
        req.inpainter = "worker".into();
        req.persistent_worker = true;
        assert!(build_plan(&req, Path::new("/bin/tool")).is_err());

        req.worker_program = Some("python3".into());
        req.worker_args = vec!["tools/model_worker/worker.py".into(), "--serve".into()];
        let plan = build_plan(&req, Path::new("/bin/tool")).unwrap();
        assert!(plan.requires_worker);
        let args = plan.args.join(" ");
        assert!(args.contains("--persistent-worker"), "{args}");
        assert!(args.contains("--worker-program python3"), "{args}");
        assert!(
            args.contains("--worker-args tools/model_worker/worker.py"),
            "{args}"
        );
        assert!(args.contains("--worker-args --serve"), "{args}");
    }

    #[test]
    fn persistent_flag_is_ignored_for_non_worker_backends() {
        let mut req = request();
        req.persistent_worker = true;
        let plan = build_plan(&req, Path::new("/bin/tool")).unwrap();
        assert!(!plan.args.iter().any(|a| a == "--persistent-worker"));
    }

    #[test]
    fn time_window_and_no_verify_flags_are_forwarded() {
        let mut req = request();
        req.start = Some(12.5);
        req.duration = Some(8.0);
        req.no_verify = true;
        req.report_json = Some(PathBuf::from("/tmp/custom.json"));
        let plan = build_plan(&req, Path::new("/bin/tool")).unwrap();
        let args = plan.args.join(" ");
        assert!(args.contains("--start 12.500"), "{args}");
        assert!(args.contains("--duration 8.000"), "{args}");
        assert!(args.contains("--no-verify"), "{args}");
        assert!(args.contains("--report-json /tmp/custom.json"), "{args}");
        assert_eq!(plan.report_path, PathBuf::from("/tmp/custom.json"));
    }

    #[test]
    fn quote_escapes_paths_with_spaces() {
        let mut req = request();
        req.input = PathBuf::from("/tmp/my drama/in.mp4");
        let plan = build_plan(&req, Path::new("/bin/tool")).unwrap();
        assert!(plan.command_line.contains("'/tmp/my drama/in.mp4'"));
    }

    #[test]
    fn validates_form_fields() {
        assert!(request().validate().is_ok());
        let mut missing_input = request();
        missing_input.input = PathBuf::new();
        assert!(missing_input.validate().is_err());
        let mut no_regions = request();
        no_regions.regions.clear();
        assert!(no_regions.validate().is_err());
        let mut bad_context = request();
        bad_context.context = 0.5;
        assert!(bad_context.validate().is_err());
        let mut bad_detector = request();
        bad_detector.detector = "magic".into();
        assert!(bad_detector.validate().is_err());
    }

    #[test]
    fn outcome_maps_exit_codes() {
        assert_eq!(EraseOutcome::from_exit_code(0), EraseOutcome::Passed);
        assert_eq!(EraseOutcome::from_exit_code(2), EraseOutcome::VerifyFailed);
        assert_eq!(EraseOutcome::from_exit_code(3), EraseOutcome::NeedsReview);
        assert_eq!(EraseOutcome::from_exit_code(1), EraseOutcome::ExecError);
        assert_eq!(EraseOutcome::from_exit_code(-1), EraseOutcome::ExecError);
    }

    #[test]
    fn resolve_tool_reports_actionable_error_and_accepts_explicit_path() {
        let missing = resolve_tool(Some(Path::new("/nonexistent/dossier-inpaint")), None);
        assert!(missing.is_err());
        let me = std::env::current_exe().unwrap();
        assert_eq!(resolve_tool(Some(&me), None).unwrap(), me);
        let no_tool = resolve_tool(None, Some(Path::new("/nonexistent-repo")));
        assert!(
            no_tool.is_err(),
            "无显式路径时应报错，实际返回：{:?}；PATH={:?}",
            no_tool,
            std::env::var_os("PATH"),
        );
    }

    use std::io::Write;

    fn progress_dir(tag: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("dossier-inpaint-{tag}-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn progress_line(seq: u64, kind: &str, done: u64, message: &str) -> String {
        format!(
            r#"{{"seq":{seq},"at":"2026-10-15T00:00:00Z","kind":"{kind}","taskId":"t1","pipeline":"drama.logo.erase","stage":"inpaint","stageIndex":1,"stageTotal":2,"unitDone":{done},"unitTotal":10,"message":"{message}"}}"#
        )
    }

    /// 写入器把半行留在磁盘上时，游标不能把它当成一条事件，也不能吞掉它。
    #[test]
    fn progress_tail_waits_for_a_complete_line() {
        let dir = progress_dir("tail");
        let path = dir.join("run.progress.jsonl");
        let line = progress_line(1, "progress", 4, "片段修复完成");

        // 先写「少一个字节」的半行（切在中文中间，故意制造半个多字节字符）。
        let bytes = line.as_bytes();
        std::fs::write(&path, &bytes[..bytes.len() - 3]).unwrap();
        let mut tail = ProgressTail::default();
        assert!(tail.drain(&path).is_empty(), "半行不应被解析成事件");

        // 补齐剩余字节 + 换行，这时才应该读到 1 条。
        let mut rest = bytes[bytes.len() - 3..].to_vec();
        rest.push(b'\n');
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(&rest).unwrap();
        drop(file);

        let events = tail.drain(&path);
        assert_eq!(events.len(), 1, "补齐后应读到 1 条事件");
        assert_eq!(events[0].progress.unit_done, 4);
        assert_eq!(events[0].progress.message, "片段修复完成");
        assert!(tail.drain(&path).is_empty(), "重复读取不应重复上报");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 工具怎么写，桌面端就怎么读：两份事件按顺序交回，终态可被识别。
    #[test]
    fn run_plan_streams_progress_written_by_the_tool() {
        let dir = progress_dir("stream");
        let progress = dir.join("run.progress.jsonl");
        std::fs::write(
            &progress,
            format!(
                "{}\n{}\n",
                progress_line(1, "started", 0, "逐片段修复"),
                progress_line(2, "finished", 10, "擦除完成")
            ),
        )
        .unwrap();

        let plan = ErasePlan {
            program: PathBuf::from("sh"),
            args: vec!["-c".into(), "exit 0".into()],
            command_line: String::new(),
            report_path: dir.join("report.json"),
            progress_path: Some(progress),
            requires_worker: false,
        };

        let seen = std::cell::RefCell::new(Vec::new());
        let run = run_plan_with_progress(&plan, 10, |event| seen.borrow_mut().push(event.clone()))
            .unwrap();

        assert_eq!(run.exit_code, 0);
        let seen = seen.into_inner();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].progress.stage, "inpaint");
        assert_eq!(seen[0].kind, dossier_core::ProgressEventKind::Started);
        assert!(seen[1].is_terminal());
        assert_eq!(seen[1].progress.unit_done, 10);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 没有进度流时是空操作：不能因为多了这个能力就改变既有行为。
    #[test]
    fn run_plan_without_progress_stream_still_works() {
        let plan = ErasePlan {
            program: PathBuf::from("sh"),
            args: vec!["-c".into(), "exit 0".into()],
            command_line: String::new(),
            report_path: PathBuf::from("/tmp/does-not-exist-report.json"),
            progress_path: None,
            requires_worker: false,
        };
        let mut calls = 0;
        let run = run_plan_with_progress(&plan, 10, |_| calls += 1).unwrap();
        assert_eq!(run.exit_code, 0);
        assert_eq!(calls, 0);
    }

    #[test]
    fn run_plan_reports_exec_error_for_a_failing_command() {
        // 用 PATH 里的 `sh` 而不是绝对路径：CI 的 Windows runner 没有盘符根下的
        // /bin/sh，但 Git Bash 提供了 sh.exe（detect.rs / inpaint.rs 的 shell 桩
        // 测试同款做法，已在三平台通过）。测试意图是「退出码 3 → NeedsReview」，
        // 不依赖具体某个 shell。
        let plan = ErasePlan {
            program: PathBuf::from("sh"),
            args: vec!["-c".into(), "exit 3".into()],
            command_line: String::new(),
            report_path: PathBuf::from("/tmp/does-not-exist-report.json"),
            progress_path: None,
            requires_worker: false,
        };
        let run = run_plan(&plan, 10).unwrap();
        assert_eq!(run.exit_code, 3);
        assert_eq!(run.outcome, EraseOutcome::NeedsReview);
        assert!(run.report.is_none());
        assert_eq!(
            run.report_path,
            PathBuf::from("/tmp/does-not-exist-report.json")
        );
        assert_eq!(run.outcome_label, "需要人工抽查");
    }
}
