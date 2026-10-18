//! 运行时环境探测：外部工具、FFmpeg 组件、GPU、Python 侧模型框架。
//!
//! 所有探测都走 [`CommandRunner`]，因此**解析逻辑**可以在没有对应工具的机器上
//! 用固定输出验证（本机就没有 `nvidia-smi`、没有 torch，靠这一层把分支测出来）。
//!
//! 探测只回答「这台机器上有什么」，不做任何选择决策；决策在 [`crate::caps`]。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::command::{CommandError, CommandRunner, CommandSpec};

/// torch 导入可能很慢（首次加载几秒到几十秒），给一个宽松但有限的上限。
const TOOL_TIMEOUT: Duration = Duration::from_secs(20);
const PYTHON_IMPORT_TIMEOUT: Duration = Duration::from_secs(60);

/// 一个外部工具的存在性与版本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolReport {
    /// 逻辑名（`ffmpeg` / `nvidia-smi` / `openssl` …）。
    pub name: String,
    /// 实际调用的程序（可能是用户指定的绝对路径）。
    pub program: String,
    pub found: bool,
    pub version: Option<String>,
    /// 找到且版本解析成功。
    pub ok: bool,
    /// 人类可读的说明（找不到时是原因，找到时是版本行）。
    pub detail: String,
}

impl ToolReport {
    fn missing(name: &str, program: &str, detail: String) -> Self {
        Self {
            name: name.to_string(),
            program: program.to_string(),
            found: false,
            version: None,
            ok: false,
            detail,
        }
    }

    pub fn display_version(&self) -> &str {
        self.version.as_deref().unwrap_or("未知版本")
    }
}

/// 探测一个「`--version` 风格」的工具。
pub fn probe_tool(
    runner: &dyn CommandRunner,
    name: &str,
    program: &Path,
    version_args: &[&str],
) -> ToolReport {
    let spec = CommandSpec::new(program.display().to_string())
        .args(version_args.iter().copied())
        .timeout(TOOL_TIMEOUT);
    match runner.run(&spec) {
        Ok(output) if output.success() => {
            let version = first_line(&output.combined())
                .as_deref()
                .map(extract_version);
            ToolReport {
                name: name.to_string(),
                program: program.display().to_string(),
                found: true,
                ok: version.is_some(),
                detail: first_line(&output.combined()).unwrap_or_default(),
                version,
            }
        }
        Ok(output) => {
            // Windows 上 cmd 找不到命令以 9009 退出；微软商店的 python 占位符
            // （WindowsApps 里的 stub）同样能被拉起但静默返回 9009。
            // 这两种情况都是「不可用」，不能算「已找到」。
            if cfg!(windows) && output.code == Some(9009) {
                ToolReport::missing(
                    name,
                    &program.display().to_string(),
                    "命令不存在或不可用（Windows 退出码 9009，常见于微软商店占位符；请安装真实版本并加入 PATH）"
                        .to_string(),
                )
            } else {
                ToolReport {
                    name: name.to_string(),
                    program: program.display().to_string(),
                    found: true,
                    version: None,
                    ok: false,
                    detail: format!(
                        "退出码 {}：{}",
                        output.code_label(),
                        first_line(&output.combined()).unwrap_or_default()
                    ),
                }
            }
        }
        Err(error) => ToolReport::missing(name, &program.display().to_string(), error.to_string()),
    }
}

/// 探测 `ffmpeg`。
pub fn probe_ffmpeg(runner: &dyn CommandRunner, program: Option<&Path>) -> ToolReport {
    let program = resolve_program(program, "ffmpeg");
    probe_tool(runner, "ffmpeg", &program, &["-hide_banner", "-version"])
}

/// 探测 `ffprobe`。
pub fn probe_ffprobe(runner: &dyn CommandRunner, program: Option<&Path>) -> ToolReport {
    let program = resolve_program(program, "ffprobe");
    probe_tool(runner, "ffprobe", &program, &["-hide_banner", "-version"])
}

/// 探测一个可选的通用外部工具（不存在不算错误）。
pub fn probe_optional_tool(
    runner: &dyn CommandRunner,
    name: &str,
    program: &str,
    version_args: &[&str],
) -> ToolReport {
    probe_tool(runner, name, Path::new(program), version_args)
}

fn resolve_program(program: Option<&Path>, fallback: &str) -> PathBuf {
    program
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(fallback))
}

fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// 从版本行的下一个 token 里取版本号：`ffmpeg version 4.2.4 Copyright ...` → `4.2.4`。
pub fn extract_version(line: &str) -> String {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    // 常见形态：`<name> version <ver> ...`，退化为「第二个 token」。
    if let Some(position) = tokens.iter().position(|token| *token == "version") {
        if let Some(value) = tokens.get(position + 1) {
            return value
                .trim_matches(|c: char| c == '(' || c == ')')
                .to_string();
        }
    }
    tokens
        .get(1)
        .map(|token| (*token).to_string())
        .unwrap_or_else(|| line.to_string())
}

/// FFmpeg 支持的编码器（按媒体类型分组）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EncoderList {
    pub video: BTreeSet<String>,
    pub audio: BTreeSet<String>,
    pub subtitle: BTreeSet<String>,
}

impl EncoderList {
    pub fn supports(&self, name: &str) -> bool {
        self.video.contains(name) || self.audio.contains(name) || self.subtitle.contains(name)
    }

    /// 按优先级挑第一个可用的编码器。
    pub fn first_available(&self, candidates: &[&str]) -> Option<String> {
        candidates
            .iter()
            .find(|candidate| self.supports(candidate))
            .map(|candidate| (*candidate).to_string())
    }
}

/// FFmpeg 支持的滤镜（按输入输出类型分组）。
///
/// `dynamic` 收的是输入/输出**个数不固定**的滤镜（清单里写成 `V->N`、`N->N`、`A->N`），
/// 例如 `select` / `aselect` / `concat` / `split`。它们同样是「这个构建里有」的事实，
/// 只是没法按「一对一」推导输入端类型，所以单独成组——**不能丢**，
/// 丢了会让 `supports()` 谎报「本机没有 select」，把可用能力判成缺失。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct FilterList {
    pub video: BTreeSet<String>,
    pub audio: BTreeSet<String>,
    pub subtitle: BTreeSet<String>,
    pub source: BTreeSet<String>,
    pub sink: BTreeSet<String>,
    pub dynamic: BTreeSet<String>,
}

impl FilterList {
    pub fn supports(&self, name: &str) -> bool {
        self.video.contains(name)
            || self.audio.contains(name)
            || self.subtitle.contains(name)
            || self.source.contains(name)
            || self.sink.contains(name)
            || self.dynamic.contains(name)
    }

    /// 清单里出现的滤镜总数（各类之和；同一名字只会落在一类里）。
    pub fn total(&self) -> usize {
        self.video.len()
            + self.audio.len()
            + self.subtitle.len()
            + self.source.len()
            + self.sink.len()
            + self.dynamic.len()
    }
}

/// FFmpeg 能力总览（版本 + 编码器 + 滤镜）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FfmpegCapabilities {
    pub ffmpeg: ToolReport,
    #[serde(default)]
    pub encoders: EncoderList,
    #[serde(default)]
    pub filters: FilterList,
    /// 列举编码器/滤镜失败时的原因（工具在但列举不出来）。
    #[serde(default)]
    pub list_error: Option<String>,
}

impl FfmpegCapabilities {
    pub fn usable(&self) -> bool {
        self.ffmpeg.ok
    }
}

/// 探测 FFmpeg 的编码器与滤镜清单。
pub fn probe_ffmpeg_capabilities(
    runner: &dyn CommandRunner,
    program: Option<&Path>,
) -> FfmpegCapabilities {
    let ffmpeg = probe_ffmpeg(runner, program);
    let bin = ffmpeg.program.clone();
    if !ffmpeg.found {
        return FfmpegCapabilities {
            ffmpeg,
            encoders: EncoderList::default(),
            filters: FilterList::default(),
            list_error: Some("未找到 ffmpeg，跳过编码器/滤镜列举".to_string()),
        };
    }
    let mut list_error = None;
    let encoders = match run_listing(runner, &bin, "-encoders") {
        Ok(text) => parse_encoder_listing(&text),
        Err(error) => {
            list_error = Some(format!("列举编码器失败：{error}"));
            EncoderList::default()
        }
    };
    let filters = match run_listing(runner, &bin, "-filters") {
        Ok(text) => parse_filter_listing(&text),
        Err(error) => {
            list_error = Some(match list_error {
                Some(previous) => format!("{previous}；列举滤镜失败：{error}"),
                None => format!("列举滤镜失败：{error}"),
            });
            FilterList::default()
        }
    };
    FfmpegCapabilities {
        ffmpeg,
        encoders,
        filters,
        list_error,
    }
}

fn run_listing(runner: &dyn CommandRunner, program: &str, flag: &str) -> Result<String, String> {
    let output = runner
        .run(
            &CommandSpec::new(program)
                .args(["-hide_banner", flag])
                .timeout(TOOL_TIMEOUT),
        )
        .map_err(|error: CommandError| error.to_string())?;
    // 退出码非 0 时必须显式失败：老版本/裁剪版 ffmpeg 会打一行错误就退出，
    // 那时 stdout 是空的，静默当成「没有这个滤镜」会把「列举不到」伪装成「确实没有」。
    if !output.success() {
        return Err(format!(
            "退出码 {}：{}",
            output.code_label(),
            first_line(&output.combined()).unwrap_or_else(|| "无输出".to_string())
        ));
    }
    Ok(output.combined())
}

/// 解析 `ffmpeg -encoders`。
///
/// 输出结构（实测 4.2.4 / 6.x 一致）：先是图例行，接着一行纯 `-` 分隔符，
/// 之后每行形如 ` V....D libx264   libx264 H.264 ...`。分隔符之前的内容一律不解析，
/// 这样图例里 ` V..... = Video` 这类行不会被误当成编码器。
pub fn parse_encoder_listing(text: &str) -> EncoderList {
    let mut list = EncoderList::default();
    let mut in_body = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if !in_body {
            if trimmed.len() >= 6 && trimmed.bytes().all(|byte| byte == b'-') {
                in_body = true;
            }
            continue;
        }
        let mut tokens = trimmed.split_whitespace();
        let (Some(flags), Some(name)) = (tokens.next(), tokens.next()) else {
            continue;
        };
        if flags.len() < 6 || !flags.as_bytes()[0].is_ascii_alphabetic() {
            continue;
        }
        match flags.as_bytes()[0] {
            b'V' => list.video.insert(name.to_string()),
            b'A' => list.audio.insert(name.to_string()),
            b'S' => list.subtitle.insert(name.to_string()),
            _ => false,
        };
    }
    list
}

/// 解析 `ffmpeg -filters`。
///
/// 该输出**没有**分隔符，图例行形如 `  T.. = Timeline support`，
/// 数据行形如 ` ... abench   A->A  Benchmark ...`。用「第 3 个 token 含 `->`」
/// 作为数据行判据，图例里的 `T..` / `=` 组合天然被排除。
pub fn parse_filter_listing(text: &str) -> FilterList {
    let mut list = FilterList::default();
    for line in text.lines() {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 3 {
            continue;
        }
        let (flags, name, io) = (tokens[0], tokens[1], tokens[2]);
        if flags.len() < 2 || !io.contains("->") || name == "=" {
            continue;
        }
        let (inputs, outputs) = match io.split_once("->") {
            Some(parts) => parts,
            None => continue,
        };
        if outputs == "|" {
            list.sink.insert(name.to_string());
            continue;
        }
        if inputs == "|" {
            list.source.insert(name.to_string());
            continue;
        }
        match outputs.chars().next() {
            Some('V') => list.video.insert(name.to_string()),
            Some('A') => list.audio.insert(name.to_string()),
            Some('S') => list.subtitle.insert(name.to_string()),
            // `N` = 输入/输出个数动态（select / concat / split …）。这些名字同样是事实，
            // 只是推不出类型，所以单独收着，供 `supports()` 回答「有没有」。
            Some('N') => list.dynamic.insert(name.to_string()),
            _ => false,
        };
    }
    list
}

/// 一块 GPU。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuDevice {
    pub index: u32,
    pub name: String,
    pub memory_total_mb: u64,
    pub driver_version: String,
    /// 计算能力（`8.9`）；老驱动查询不到时为 `None`。
    #[serde(default)]
    pub compute_capability: Option<String>,
}

/// GPU 探测结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuReport {
    pub available: bool,
    /// `cuda`（本模块只探测 NVIDIA；其它后端留待扩展）。
    pub backend: String,
    #[serde(default)]
    pub devices: Vec<GpuDevice>,
    /// 是否真的发起了 `nvidia-smi` 调用。
    pub queried: bool,
    pub reason: String,
}

impl GpuReport {
    pub fn total_memory_mb(&self) -> u64 {
        self.devices
            .iter()
            .map(|device| device.memory_total_mb)
            .sum()
    }

    pub fn max_compute_capability(&self) -> Option<&str> {
        self.devices
            .iter()
            .filter_map(|device| device.compute_capability.as_deref())
            .max_by(|left, right| compare_compute_capability(left, right))
    }
}

/// 探测 NVIDIA GPU。
///
/// 先带 `compute_cap` 查询；老驱动不支持该字段会整体失败，于是退化为不带它的查询——
/// 这一条回退路径正是为了让「老驱动机器上仍然能识别出 GPU」这件事可测。
pub fn probe_gpu(runner: &dyn CommandRunner) -> GpuReport {
    const WITH_CAP: &str = "index,name,memory.total,driver_version,compute_cap";
    const WITHOUT_CAP: &str = "index,name,memory.total,driver_version";

    let attempt = |fields: &str| {
        runner.run(
            &CommandSpec::new("nvidia-smi")
                .args([
                    "--query-gpu=".to_string() + fields,
                    "--format=csv,noheader,nounits".to_string(),
                ])
                .timeout(TOOL_TIMEOUT),
        )
    };

    let mut detail = String::new();
    let query = match attempt(WITH_CAP) {
        Ok(output) if output.success() => output,
        Ok(failed) => {
            detail = format!(
                "带 compute_cap 的查询失败（退出码 {}）",
                failed.code_label()
            );
            match attempt(WITHOUT_CAP) {
                Ok(output) if output.success() => output,
                Ok(failed) => {
                    return GpuReport {
                        available: false,
                        backend: "cuda".into(),
                        devices: Vec::new(),
                        queried: true,
                        reason: format!(
                            "nvidia-smi 查询失败（退出码 {}）：{}",
                            failed.code_label(),
                            first_line(&failed.combined()).unwrap_or_default()
                        ),
                    }
                }
                Err(error) => {
                    return GpuReport {
                        available: false,
                        backend: "cuda".into(),
                        devices: Vec::new(),
                        queried: true,
                        reason: error.to_string(),
                    }
                }
            }
        }
        Err(error) => {
            return GpuReport {
                available: false,
                backend: "cuda".into(),
                devices: Vec::new(),
                queried: true,
                reason: error.to_string(),
            }
        }
    };

    let devices = parse_nvidia_smi_csv(&query.stdout);
    if devices.is_empty() {
        return GpuReport {
            available: false,
            backend: "cuda".into(),
            devices: Vec::new(),
            queried: true,
            reason: if detail.is_empty() {
                "nvidia-smi 没有返回任何 GPU".to_string()
            } else {
                format!("{detail}，且回退查询没有返回任何 GPU")
            },
        };
    }
    GpuReport {
        available: true,
        backend: "cuda".into(),
        devices,
        queried: true,
        reason: format!("检测到 {} 块 NVIDIA GPU", query.stdout.lines().count()),
    }
}

/// 解析 `nvidia-smi --format=csv,noheader,nounits` 的输出。
///
/// 每行：`0, NVIDIA GeForce RTX 4090, 24564, 550.54.14[, 8.9]`。
/// 缺列的行会被跳过而不是整段失败——老驱动可能少给字段。
pub fn parse_nvidia_smi_csv(text: &str) -> Vec<GpuDevice> {
    let mut devices = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        if fields.len() < 4 {
            continue;
        }
        let Ok(index) = fields[0].parse::<u32>() else {
            continue;
        };
        let memory_total_mb = fields[2].parse::<u64>().unwrap_or(0);
        let compute_capability = fields
            .get(4)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty() && *value != "[Not Supported]")
            .map(str::to_string);
        devices.push(GpuDevice {
            index,
            name: fields[1].to_string(),
            memory_total_mb,
            driver_version: fields[3].to_string(),
            compute_capability,
        });
    }
    devices
}

/// 比较两个计算能力字符串（`8.10` > `8.9` > `7.5`）。
pub fn compare_compute_capability(left: &str, right: &str) -> std::cmp::Ordering {
    fn parts(value: &str) -> (u32, u32) {
        let mut pieces = value.split('.');
        let major = pieces
            .next()
            .and_then(|p| p.trim().parse().ok())
            .unwrap_or(0);
        let minor = pieces
            .next()
            .and_then(|p| p.trim().parse().ok())
            .unwrap_or(0);
        (major, minor)
    }
    parts(left).cmp(&parts(right))
}

/// Python 侧运行时与模型框架探测结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PythonReport {
    pub program: String,
    pub found: bool,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub torch_version: Option<String>,
    #[serde(default)]
    pub onnxruntime_version: Option<String>,
    /// 单独某次导入失败的原因（不影响其它字段）。
    #[serde(default)]
    pub import_errors: Vec<String>,
}

impl PythonReport {
    /// 有没有可以跑模型 worker 的 Python 环境。
    pub fn usable(&self) -> bool {
        self.found && (self.torch_version.is_some() || self.onnxruntime_version.is_some())
    }
}

/// 探测 Python 解释器以及 torch / onnxruntime 是否可导入。
pub fn probe_python(runner: &dyn CommandRunner, program: &str) -> PythonReport {
    let version_report = probe_tool(runner, "python", Path::new(program), &["--version"]);
    let mut report = PythonReport {
        program: program.to_string(),
        found: version_report.found,
        version: version_report.version.clone(),
        torch_version: None,
        onnxruntime_version: None,
        import_errors: Vec::new(),
    };
    if !report.found {
        report.import_errors.push(version_report.detail);
        return report;
    }
    match probe_module_version(runner, program, "torch") {
        Ok(version) => report.torch_version = Some(version),
        Err(error) => report.import_errors.push(format!("torch：{error}")),
    }
    match probe_module_version(runner, program, "onnxruntime") {
        Ok(version) => report.onnxruntime_version = Some(version),
        Err(error) => report.import_errors.push(format!("onnxruntime：{error}")),
    }
    report
}

/// `python -c "import <module>; print(version)"`。
pub fn probe_module_version(
    runner: &dyn CommandRunner,
    program: &str,
    module: &str,
) -> Result<String, String> {
    let script = format!("import {module} as m; print(getattr(m, '__version__', 'unknown'))");
    let output = runner
        .run(
            &CommandSpec::new(program)
                .args(["-c", script.as_str()])
                .timeout(PYTHON_IMPORT_TIMEOUT),
        )
        .map_err(|error| error.to_string())?;
    if !output.success() {
        return Err(first_line(&output.combined()).unwrap_or_else(|| "导入失败".to_string()));
    }
    first_line(&output.stdout).ok_or_else(|| "没有输出版本".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{CommandOutput, FakeRunner};

    const ENCODER_FIXTURE: &str = "Encoders:\n \
V..... = Video\n \
A..... = Audio\n \
S..... = Subtitle\n \
.F.... = Frame-level multithreading\n \
------\n \
V....D a64multi            Multicolor charset for Commodore 64\n \
V....D libx264             libx264 H.264 / AVC / MPEG-4 AVC (codec h264)\n \
V..... libx264rgb          libx264 H.264 / AVC / MPEG-4 AVC (codec h264)\n \
V....D h264_vaapi          H.264/AVC (VAAPI) (codec h264)\n \
V....D h264_v4l2m2m        V4L2 mem2mem H.264 encoder wrapper (codec h264)\n \
A....D aac                  AAC (Advanced Audio Coding)\n \
S..... srt                  SubRip subtitle\n";

    const FILTER_FIXTURE: &str = "Filters:\n \
  T.. = Timeline support\n \
  .S. = Slice threading\n \
  ..C = Command support\n \
  A = Audio input/output\n \
  V = Video input/output\n \
  N = Dynamic number and/or type of input/output\n \
  | = Source or sink filter\n \
 ... abench            A->A       Benchmark part of a filtergraph.\n \
 ..C drawtext          V->V       Draw text on top of video.\n \
 ... overlay           VV->V      Overlay a video source on top of the input.\n \
 ... ass               V->V       Render ASS subtitles.\n \
 ... anullsrc          |->A       Null audio source.\n \
 ... anullsink         A->|       Do absolutely nothing with the input audio.\n \
 ... select            V->N       Select video frames to pass in output.\n \
 ... concat            N->N       Concatenate audio and video streams.\n";

    #[test]
    fn parses_real_encoder_listing() {
        let list = parse_encoder_listing(ENCODER_FIXTURE);
        assert!(list.video.contains("libx264"));
        assert!(list.video.contains("h264_vaapi"));
        assert!(list.video.contains("h264_v4l2m2m"));
        assert!(list.audio.contains("aac"));
        assert!(list.subtitle.contains("srt"));
        // 图例行不能被吃进来。
        assert!(!list.supports("Video"));
        assert!(!list.supports("="));
        assert_eq!(
            list.first_available(&["h264_nvenc", "h264_vaapi", "libx264"]),
            Some("h264_vaapi".to_string())
        );
        assert_eq!(list.first_available(&["h264_nvenc", "hevc_nvenc"]), None);
    }

    #[test]
    fn parses_real_filter_listing() {
        let list = parse_filter_listing(FILTER_FIXTURE);
        assert!(list.video.contains("drawtext"));
        assert!(list.video.contains("overlay"));
        assert!(list.video.contains("ass"));
        assert!(list.audio.contains("abench"));
        assert!(list.source.contains("anullsrc"));
        assert!(list.sink.contains("anullsink"));
        // `V->N` / `N->N` 是「输入输出个数动态」的滤镜：必须当成「本机有」，
        // 否则会谎报缺滤镜（`media.shot.detect` 就依赖 `select`）。
        assert!(list.dynamic.contains("select"));
        assert!(list.dynamic.contains("concat"));
        assert!(list.supports("select"));
        assert!(list.supports("concat"));
        // 总数 = 数据行条数（8 条），图例行不算。
        assert_eq!(list.total(), 8);
        // 图例行的 `T.. = Timeline support` 不能被当成滤镜。
        assert!(!list.supports("Timeline"));
        assert!(!list.supports("="));
    }

    #[test]
    fn probe_reports_missing_ffmpeg() {
        let runner = FakeRunner::new();
        let report = probe_ffmpeg(&runner, None);
        assert!(!report.found);
        assert!(!report.ok);
        assert!(report.detail.contains("未找到"));
        let caps = probe_ffmpeg_capabilities(&runner, None);
        assert!(!caps.usable());
        assert!(caps.list_error.is_some());
        assert!(caps.encoders.video.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn probe_tool_treats_windows_9009_as_missing() {
        // 微软商店占位符 python.exe 能被拉起但静默退出 9009；
        // CI 的 windows 分身验证它被判定为「未找到」而不是「已找到但失败」。
        let runner = FakeRunner::new().on("python", CommandOutput::new(9009, "", ""));
        let report = probe_tool(&runner, "python", Path::new("python"), &["--version"]);
        assert!(!report.found);
        assert!(!report.ok);
        assert!(report.detail.contains("9009"));
    }

    #[test]
    fn probe_reads_version_and_lists_components() {
        // 规则必须挂在**同一个程序字符串**上：指定了 `--ffmpeg /opt/ffmpeg` 之后，
        // 列举编码器/滤镜也会用这个路径，而不是回到 PATH 里的 `ffmpeg`。
        let runner = FakeRunner::new()
            .on_match(
                "/opt/ffmpeg",
                "-version",
                CommandOutput::ok(
                    "ffmpeg version 4.2.4 Copyright (c) 2000-2020 the FFmpeg developers\n",
                ),
            )
            .on_match(
                "/opt/ffmpeg",
                "-encoders",
                CommandOutput::ok(ENCODER_FIXTURE),
            )
            .on_match("/opt/ffmpeg", "-filters", CommandOutput::ok(FILTER_FIXTURE));
        let caps = probe_ffmpeg_capabilities(&runner, Some(Path::new("/opt/ffmpeg")));
        assert!(caps.usable());
        assert_eq!(caps.ffmpeg.version.as_deref(), Some("4.2.4"));
        assert!(caps.encoders.video.contains("libx264"));
        assert!(caps.filters.video.contains("drawtext"));
        assert!(caps.list_error.is_none());
        // 列举失败只影响该字段，不影响版本探测。
        let partial = FakeRunner::new()
            .on_match(
                "ffmpeg",
                "-version",
                CommandOutput::ok("ffmpeg version 6.1\n"),
            )
            .on_err(
                "ffmpeg",
                CommandError::Timeout {
                    program: "ffmpeg".into(),
                    after: Duration::from_secs(20),
                },
            );
        let caps = probe_ffmpeg_capabilities(&partial, None);
        assert!(caps.usable());
        assert!(caps.list_error.unwrap().contains("超时"));
    }

    #[test]
    fn parses_nvidia_smi_with_and_without_compute_cap() {
        let devices = parse_nvidia_smi_csv("0, NVIDIA GeForce RTX 4090, 24564, 550.54.14, 8.9\n");
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "NVIDIA GeForce RTX 4090");
        assert_eq!(devices[0].memory_total_mb, 24564);
        assert_eq!(devices[0].compute_capability.as_deref(), Some("8.9"));

        let older = parse_nvidia_smi_csv("0, Tesla T4, 15360, 470.82\n");
        assert_eq!(older[0].compute_capability, None);
        assert_eq!(older[0].driver_version, "470.82");

        // 不支持 compute_cap 的驱动会回 `[Not Supported]`。
        let unsupported = parse_nvidia_smi_csv("1, Tesla P4, 7680, 470.82, [Not Supported]\n");
        assert_eq!(unsupported[0].compute_capability, None);

        // 残缺行被跳过，不会污染结果。
        assert!(parse_nvidia_smi_csv("garbage\n\n0, GPU, x\n").is_empty());

        let multi =
            parse_nvidia_smi_csv("0, A100, 40960, 535.1, 8.0\n1, A100, 40960, 535.1, 8.0\n");
        assert_eq!(multi.len(), 2);
    }

    #[test]
    fn gpu_probe_falls_back_when_compute_cap_unsupported() {
        let runner = FakeRunner::new()
            .on_match(
                "nvidia-smi",
                "compute_cap",
                CommandOutput::new(1, "", "unknown field"),
            )
            .on_match(
                "nvidia-smi",
                "-query-gpu",
                CommandOutput::ok("0, Tesla T4, 15360, 470.82\n"),
            );
        let report = probe_gpu(&runner);
        assert!(report.available);
        assert_eq!(report.devices.len(), 1);
        assert_eq!(report.devices[0].compute_capability, None);
        assert_eq!(runner.calls().len(), 2);

        let missing = probe_gpu(&FakeRunner::new());
        assert!(!missing.available);
        assert!(missing.queried);
        assert!(missing.reason.contains("未找到"));
    }

    #[test]
    fn gpu_probe_degrades_when_smi_defaults_empty() {
        let runner = FakeRunner::new().on("nvidia-smi", CommandOutput::ok("\n"));
        let report = probe_gpu(&runner);
        assert!(!report.available);
        assert!(report.reason.contains("没有返回任何 GPU"));
    }

    #[test]
    fn compute_capability_ordering() {
        assert_eq!(
            compare_compute_capability("8.10", "8.9"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_compute_capability("7.5", "8.0"),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_compute_capability("8.9", "8.9"),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn python_probe_reports_torch_and_onnxruntime_independently() {
        // 规则按顺序匹配：先放更具体的「导入探针」，再放通用的 `--version`。
        let runner = FakeRunner::new()
            .on_match(
                "python3.9",
                "import torch",
                CommandOutput::new(0, "2.1.0+cpu\n", ""),
            )
            .on_match(
                "python3.9",
                "import onnxruntime",
                CommandOutput::new(1, "", "No module named 'onnxruntime'"),
            )
            .on("python3.9", CommandOutput::new(0, "Python 3.9.9\n", ""));
        let report = probe_python(&runner, "python3.9");
        assert!(report.found);
        assert_eq!(report.torch_version.as_deref(), Some("2.1.0+cpu"));
        assert!(report.onnxruntime_version.is_none());
        assert_eq!(report.import_errors.len(), 1);
        assert!(report.usable());
    }

    #[test]
    fn python_probe_without_interpreter() {
        let report = probe_python(&FakeRunner::new(), "python3.9");
        assert!(!report.found);
        assert!(!report.usable());
        assert!(report.version.is_none());
    }

    #[test]
    fn version_extraction_handles_common_shapes() {
        assert_eq!(extract_version("ffmpeg version 4.2.4 Copyright"), "4.2.4");
        assert_eq!(extract_version("openssl 3.0.7 7 Feb 2023"), "3.0.7");
        assert_eq!(extract_version("just-one"), "just-one");
    }

    #[test]
    fn optional_tool_probe_is_not_an_error_when_absent() {
        let runner = FakeRunner::new();
        let report = probe_optional_tool(&runner, "rubberband", "rubberband", &["--version"]);
        assert!(!report.found);
        assert!(!report.ok);
        assert_eq!(report.display_version(), "未知版本");
    }
}
