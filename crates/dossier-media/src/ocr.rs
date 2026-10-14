//! OCR 文字识别（Tesseract）。
//!
//! 补的是一个很具体的缺口：项目此前只有**文本检测**（DBNet 输出「哪里有字」的框），
//! 没有**文本识别**（「那些字是什么」）。没有识别，就没法做
//! 「判断这个角标是不是播出平台标识」「核对片头标题有没有被改」这类事。
//!
//! 两段式设计（这也是短剧里最实用的形态）：
//! 1. [`plan_region_frames`]：先用 ffmpeg 把**指定区域 + 指定时间**抽成图片序列——
//!    角标/硬字幕只在画面的一小块里，整帧丢给 OCR 既慢又容易被背景干扰；
//! 2. [`plan_recognize`]：对图片跑 tesseract TSV 输出，得到带**置信度**的文本行。
//!
//! 置信度必须留着：OCR 的错字是常态，下游要靠它决定「自动确认还是转人工」。

use std::path::{Path, PathBuf};

use dossier_runtime::{CommandRunner, CommandSpec};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::failure_message;
use crate::render::TimeRange;

/// OCR 错误。
#[derive(Debug, Error)]
pub enum OcrError {
    #[error("输入文件不存在：{0}")]
    InputMissing(PathBuf),
    #[error("区域非法：宽高必须大于 0（实际 {width}x{height}）")]
    InvalidRegion { width: u32, height: u32 },
    #[error("本机没有 tesseract：{0}")]
    ToolMissing(String),
    #[error("识别失败（{command}）：{message}")]
    RunFailed { command: String, message: String },
    #[error("命令成功但没有抽到图片，检查区域是否落在画面内：{0}")]
    NoFrames(String),
    #[error("识别输出不存在：{0}（命令成功但没有产物）")]
    OutputMissing(PathBuf),
}

/// 画面区域（像素，左上角为原点）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Region {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn validate(&self) -> Result<(), OcrError> {
        if self.width == 0 || self.height == 0 {
            return Err(OcrError::InvalidRegion {
                width: self.width,
                height: self.height,
            });
        }
        Ok(())
    }

    /// `crop=w:h:x:y`（注意 ffmpeg 的参数顺序是 宽:高:X:Y）。
    pub fn crop_filter(&self) -> String {
        format!("crop={}:{}:{}:{}", self.width, self.height, self.x, self.y)
    }
}

/// 抽帧计划。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegionFramePlan {
    pub input: PathBuf,
    pub output_dir: PathBuf,
    pub region: Region,
    /// 抽帧间隔（秒）。OCR 不需要每帧都跑：角标是静态的，1 秒 1 帧就够。
    pub interval_sec: f64,
    #[serde(default)]
    pub range: Option<TimeRange>,
    /// 缩放倍率：小字放大后再识别，准确率明显更高。
    #[serde(default = "default_scale")]
    pub scale: f64,
    /// 输出图片前缀。
    pub prefix: String,
    /// tesseract 语言包（`chi_sim+eng`）。
    #[serde(default = "default_language")]
    pub language: String,
}

fn default_scale() -> f64 {
    2.0
}

fn default_language() -> String {
    "chi_sim+eng".to_string()
}

impl RegionFramePlan {
    pub fn new(input: impl Into<PathBuf>, output_dir: impl Into<PathBuf>, region: Region) -> Self {
        Self {
            input: input.into(),
            output_dir: output_dir.into(),
            region,
            interval_sec: 1.0,
            range: None,
            scale: default_scale(),
            prefix: "region".to_string(),
            language: default_language(),
        }
    }

    /// 预期抽出的图片路径（按 ffmpeg 的 `%04d` 编号规则）。
    pub fn expected_frame_path(&self, index: usize) -> PathBuf {
        self.output_dir
            .join(format!("{}_{index:04}.png", self.prefix))
    }

    pub fn build_command(&self, ffmpeg: &str) -> Result<CommandSpec, OcrError> {
        if !self.input.is_file() {
            return Err(OcrError::InputMissing(self.input.clone()));
        }
        self.region.validate()?;
        if !(self.interval_sec.is_finite() && self.interval_sec > 0.0) {
            return Err(OcrError::RunFailed {
                command: "plan".to_string(),
                message: format!("抽帧间隔必须大于 0，实际 {}", self.interval_sec),
            });
        }
        let mut chain = vec![self.region.crop_filter()];
        if (self.scale - 1.0).abs() > f64::EPSILON {
            let width = (f64::from(self.region.width) * self.scale).round().max(1.0) as u32;
            chain.push(format!("scale={width}:-1"));
        }
        // 灰度化能让 tesseract 更稳（去彩色背景干扰），放在缩放之后。
        chain.push("format=gray".to_string());

        let mut spec = CommandSpec::new(ffmpeg.to_string())
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(self.input.display().to_string());
        if let Some(range) = &self.range {
            if range.start > 0.0 {
                spec = spec.args(["-ss", &format_float(range.start)]);
            }
            if let Some(end) = range.end {
                spec = spec.args(["-t", &format_float((end - range.start).max(0.0))]);
            }
        }
        Ok(spec
            .args(["-vf", &chain.join(","), "-r"])
            .arg(format_float(1.0 / self.interval_sec))
            .arg(
                self.output_dir
                    .join(format!("{}_%04d.png", self.prefix))
                    .display()
                    .to_string(),
            ))
    }
}

/// 单张图片的识别计划。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecognizePlan {
    pub image: PathBuf,
    /// TSV 输出路径（`tesseract <img> <out> -l <lang> tsv` 会写成 `<out>.tsv`）。
    pub output_base: PathBuf,
    /// 语言包，例如 `chi_sim+eng`。
    pub language: String,
    /// 页面分割模式；`6` = 「当作一整块文本」，角标/字幕这种单行文本用它最稳。
    pub psm: u32,
}

impl RecognizePlan {
    pub fn new(image: impl Into<PathBuf>, output_base: impl Into<PathBuf>) -> Self {
        Self {
            image: image.into(),
            output_base: output_base.into(),
            language: "chi_sim+eng".to_string(),
            psm: 6,
        }
    }

    pub fn tsv_path(&self) -> PathBuf {
        with_extension(&self.output_base, "tsv")
    }

    pub fn build_command(&self, tesseract: &str, available: bool) -> Result<CommandSpec, OcrError> {
        if !self.image.is_file() {
            return Err(OcrError::InputMissing(self.image.clone()));
        }
        if !available {
            return Err(OcrError::ToolMissing(
                "请安装 tesseract 与对应语言包（chi_sim）".to_string(),
            ));
        }
        Ok(CommandSpec::new(tesseract.to_string())
            .arg(self.image.display().to_string())
            .arg(self.output_base.display().to_string())
            .args(["-l", &self.language])
            .args(["--psm", &self.psm.to_string()])
            .arg("tsv"))
    }
}

/// 一个识别出来的文本行。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrLine {
    pub text: String,
    /// tesseract 的置信度（0~100，-1 表示无意义）。
    pub confidence: f64,
    pub left: i64,
    pub top: i64,
    pub width: i64,
    pub height: i64,
}

impl OcrLine {
    /// 可信度是否达标。默认门槛 60：低于它的结果通常是把背景纹理认成了字。
    pub fn is_confident(&self, threshold: f64) -> bool {
        self.confidence >= threshold
    }
}

/// 解析 tesseract 的 TSV 输出。
///
/// 列定义（官方文档）：`level page_num block_num par_num line_num word_num
/// left top width height conf text`，共 12 列；表头行以 `level` 开头。
/// 同一行的多个 word 会被合并成一个 [`OcrLine`]（短剧场景下我们关心的是「这一行写的是
/// 什么」，不是逐词坐标）。
pub fn parse_tsv(text: &str) -> Vec<OcrLine> {
    let mut lines: Vec<OcrLine> = Vec::new();
    let mut current_key: Option<(String, String, String, String)> = None;
    let mut buffer: Vec<(String, f64, i64, i64, i64, i64)> = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        if index == 0 && raw.starts_with("level") {
            continue;
        }
        let columns: Vec<&str> = raw.split('\t').collect();
        if columns.len() < 12 {
            continue;
        }
        let key = (
            columns[1].to_string(),
            columns[2].to_string(),
            columns[3].to_string(),
            columns[4].to_string(),
        );
        let confidence: f64 = columns[10].parse().unwrap_or(-1.0);
        let word = columns[11].trim();
        if word.is_empty() {
            continue;
        }
        // 换行/换块时把上一行落下来。
        if current_key
            .as_ref()
            .is_some_and(|previous| previous != &key)
            && !buffer.is_empty()
        {
            lines.push(flush_line(&mut buffer));
        }
        current_key = Some(key);
        buffer.push((
            word.to_string(),
            confidence,
            columns[6].parse().unwrap_or_default(),
            columns[7].parse().unwrap_or_default(),
            columns[8].parse().unwrap_or_default(),
            columns[9].parse().unwrap_or_default(),
        ));
    }
    if !buffer.is_empty() {
        lines.push(flush_line(&mut buffer));
    }
    lines
}

fn flush_line(buffer: &mut Vec<(String, f64, i64, i64, i64, i64)>) -> OcrLine {
    let left = buffer.iter().map(|item| item.2).min().unwrap_or_default();
    let top = buffer.iter().map(|item| item.3).min().unwrap_or_default();
    let right = buffer
        .iter()
        .map(|item| item.2 + item.4)
        .max()
        .unwrap_or_default();
    let bottom = buffer
        .iter()
        .map(|item| item.3 + item.5)
        .max()
        .unwrap_or_default();
    let confidences: Vec<f64> = buffer.iter().map(|item| item.1).collect();
    let text = buffer
        .iter()
        .map(|item| item.0.clone())
        .collect::<Vec<String>>()
        .join("");
    let line = OcrLine {
        text,
        // 用均值而不是最小值：置信度本来就是按词给的，取最小值会让一行里
        // 一个走神的词拖垮整行判定。
        confidence: if confidences.is_empty() {
            -1.0
        } else {
            confidences.iter().sum::<f64>() / confidences.len() as f64
        },
        left,
        top,
        width: right - left,
        height: bottom - top,
    };
    buffer.clear();
    line
}

/// 读取并解析 TSV 文件。
pub fn load_tsv(path: &Path) -> Result<Vec<OcrLine>, OcrError> {
    if !path.is_file() {
        return Err(OcrError::RunFailed {
            command: "tesseract".to_string(),
            message: format!("输出文件不存在：{}", path.display()),
        });
    }
    let raw = std::fs::read_to_string(path).map_err(|source| OcrError::RunFailed {
        command: "tesseract".to_string(),
        message: source.to_string(),
    })?;
    Ok(parse_tsv(&raw))
}

fn with_extension(base: &Path, extension: &str) -> PathBuf {
    let mut path = base.to_path_buf();
    path.set_extension(extension);
    path
}

fn format_float(value: f64) -> String {
    let text = format!("{value:.4}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 一次区域 OCR 的执行结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegionOcrOutcome {
    /// 实际抽出的区域图片。
    pub frames: Vec<PathBuf>,
    /// 全部识别到的文本行（带置信度，是否采纳由调用方按阈值决定）。
    pub lines: Vec<OcrLine>,
    /// 置信度达标（>= 60）的行数。
    pub confident: usize,
}

impl RegionFramePlan {
    /// 真的抽一次区域帧，返回实际产出的图片（按编号顺序）。
    ///
    /// 抽帧是「区域 + 时间窗」两重过滤后的产物，数量事先算不出来（取决于时长与间隔），
    /// 所以这里以**磁盘上连续编号的图片**为准，而不是猜一个数量。
    pub fn execute(
        &self,
        runner: &dyn CommandRunner,
        ffmpeg: &str,
    ) -> Result<Vec<PathBuf>, OcrError> {
        let spec = self.build_command(ffmpeg)?;
        let output = runner.run(&spec).map_err(|error| OcrError::RunFailed {
            command: spec.display(),
            message: error.to_string(),
        })?;
        if !output.success() {
            return Err(OcrError::RunFailed {
                command: spec.display(),
                message: failure_message(&output),
            });
        }
        let mut frames = Vec::new();
        for index in 1..=100_000usize {
            let path = self.expected_frame_path(index);
            if !path.is_file() {
                break;
            }
            frames.push(path);
        }
        if frames.is_empty() {
            return Err(OcrError::NoFrames(format!(
                "区域 {}x{} @ ({}, {})，输出前缀 {}",
                self.region.width, self.region.height, self.region.x, self.region.y, self.prefix
            )));
        }
        Ok(frames)
    }
}

impl RecognizePlan {
    /// 真的跑一次 tesseract，并把 TSV 解析成带置信度的文本行。
    pub fn execute(
        &self,
        runner: &dyn CommandRunner,
        tesseract: &str,
    ) -> Result<Vec<OcrLine>, OcrError> {
        let available = runner
            .run(&CommandSpec::new(tesseract.to_string()).arg("--version"))
            .map(|output| output.success())
            .unwrap_or(false);
        let spec = self.build_command(tesseract, available)?;
        let output = runner.run(&spec).map_err(|error| OcrError::RunFailed {
            command: spec.display(),
            message: error.to_string(),
        })?;
        if !output.success() {
            return Err(OcrError::RunFailed {
                command: spec.display(),
                message: failure_message(&output),
            });
        }
        let tsv = self.tsv_path();
        if !tsv.is_file() {
            return Err(OcrError::OutputMissing(tsv));
        }
        load_tsv(&tsv)
    }
}

/// 区域抽帧 + 逐帧识别（CLI 与云端共用的入口，避免各自写一遍循环）。
pub fn recognize_region(
    plan: &RegionFramePlan,
    runner: &dyn CommandRunner,
    ffmpeg: &str,
    tesseract: &str,
    confidence: f64,
) -> Result<RegionOcrOutcome, OcrError> {
    let frames = plan.execute(runner, ffmpeg)?;
    let mut lines: Vec<OcrLine> = Vec::new();
    for (index, frame) in frames.iter().enumerate() {
        let mut recognize =
            RecognizePlan::new(frame, plan.output_dir.join(format!("ocr_{index:04}")));
        recognize.language = plan.language.clone();
        let mut found = recognize.execute(runner, tesseract)?;
        lines.append(&mut found);
    }
    let confident = lines
        .iter()
        .filter(|line| line.is_confident(confidence))
        .count();
    Ok(RegionOcrOutcome {
        frames,
        lines,
        confident,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dossier_runtime::{CommandRunner, SystemRunner};

    const TSV: &str = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
1\t1\t0\t0\t0\t0\t0\t0\t320\t160\t-1\t\n\
5\t1\t1\t1\t1\t1\t10\t12\t40\t14\t96.5\t龙\n\
5\t1\t1\t1\t1\t2\t52\t12\t40\t14\t93.0\t腾\n\
5\t1\t2\t1\t1\t1\t10\t60\t120\t16\t41.2\t背景纹理\n";

    #[test]
    fn tsv_rows_are_merged_into_lines_with_mean_confidence() {
        let lines = parse_tsv(TSV);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "龙腾");
        assert!((lines[0].confidence - 94.75).abs() < 1e-9);
        assert_eq!(lines[0].left, 10);
        assert_eq!(lines[0].width, 82);
        assert_eq!(lines[1].text, "背景纹理");
        assert!(!lines[1].is_confident(60.0));
        assert!(lines[0].is_confident(60.0));
    }

    #[test]
    fn empty_words_and_short_rows_are_skipped() {
        let text = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
5\t1\t1\t1\t1\t1\t0\t0\t10\t10\t90\t\n\
badrow\n";
        assert!(parse_tsv(text).is_empty());
    }

    #[test]
    fn region_frames_crop_scale_and_greyscale() {
        let dir = std::env::temp_dir().join(format!("dossier-ocr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.mp4");
        std::fs::write(&input, b"fake").unwrap();
        let mut plan =
            RegionFramePlan::new(&input, dir.join("frames"), Region::new(10, 20, 120, 40));
        plan.interval_sec = 0.5;
        plan.range = Some(TimeRange::new(2.0, Some(4.0)));
        let spec = plan.build_command("ffmpeg").unwrap();
        let filter = &spec.args[spec.args.index_of("-vf") + 1];
        assert_eq!(filter, "crop=120:40:10:20,scale=240:-1,format=gray");
        // `-ss` 取区间起点、`-t` 取区间长度、`-r` 取「每 interval 秒一帧」。
        assert_eq!(
            option_value(&spec.args, "-ss"),
            Some("2"),
            "{:?}",
            spec.args
        );
        assert_eq!(option_value(&spec.args, "-t"), Some("2"), "{:?}", spec.args);
        assert_eq!(option_value(&spec.args, "-r"), Some("2"), "{:?}", spec.args);
        assert_eq!(
            plan.expected_frame_path(3),
            plan.output_dir.join("region_0003.png")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 取命令行里某个开关后面的值；`-ss 2` → `Some("2")`。
    fn option_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        let index = args.iter().position(|arg| arg == flag)?;
        args.get(index + 1).map(String::as_str)
    }

    #[test]
    fn recognize_command_uses_tsv_output() {
        let dir = std::env::temp_dir().join(format!("dossier-ocr-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("region_0001.png");
        std::fs::write(&image, b"fake").unwrap();
        let plan = RecognizePlan::new(&image, dir.join("out/region_0001"));
        assert_eq!(plan.tsv_path(), dir.join("out/region_0001.tsv"));
        let spec = plan.build_command("tesseract", true).unwrap();
        assert_eq!(spec.args[0], image.display().to_string());
        assert_eq!(spec.args[spec.args.len() - 1], "tsv");
        assert!(spec.args.contains(&"chi_sim+eng".to_string()));
        assert!(matches!(
            plan.build_command("tesseract", false).unwrap_err(),
            OcrError::ToolMissing(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_zero_sized_regions() {
        assert!(matches!(
            Region::new(0, 0, 0, 10).validate().unwrap_err(),
            OcrError::InvalidRegion { .. }
        ));
    }

    /// 真跑一次抽帧：确认 `crop` 参数顺序没写反（写反了会得到错误的画面尺寸）。
    #[test]
    fn real_region_extraction_produces_images_of_the_requested_size() {
        let runner = SystemRunner::new();
        if !runner
            .run(&CommandSpec::new("ffmpeg").arg("-version"))
            .map(|output| output.success())
            .unwrap_or(false)
        {
            eprintln!("跳过：本机没有可用的 ffmpeg");
            return;
        }
        let dir = std::env::temp_dir().join(format!("dossier-ocr-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("in.mp4");
        let created = runner
            .run(
                &CommandSpec::new("ffmpeg")
                    .args([
                        "-hide_banner",
                        "-loglevel",
                        "error",
                        "-y",
                        "-f",
                        "lavfi",
                        "-i",
                        "testsrc2=s=320x240:r=5:d=3",
                        "-pix_fmt",
                        "yuv420p",
                    ])
                    .arg(clip.display().to_string()),
            )
            .unwrap();
        assert!(created.success(), "造素材失败：{}", created.combined());

        let frames = dir.join("frames");
        std::fs::create_dir_all(&frames).unwrap();
        let mut plan = RegionFramePlan::new(&clip, &frames, Region::new(20, 30, 100, 40));
        plan.scale = 1.0;
        plan.interval_sec = 1.0;
        let spec = plan.build_command("ffmpeg").unwrap();
        let output = runner.run(&spec).unwrap();
        assert!(output.success(), "抽帧失败：{}", output.combined());
        let first = plan.expected_frame_path(1);
        assert!(first.is_file(), "应当产出 {}", first.display());
        assert!(plan.expected_frame_path(3).is_file(), "3 秒素材应至少 3 张");

        // 用 ffprobe 确认尺寸就是裁剪区域（而不是整帧）。
        let probe = runner
            .run(
                &CommandSpec::new("ffprobe")
                    .args([
                        "-v",
                        "quiet",
                        "-select_streams",
                        "v:0",
                        "-show_entries",
                        "stream=width,height",
                        "-of",
                        "csv=p=0",
                    ])
                    .arg(first.display().to_string()),
            )
            .unwrap();
        assert_eq!(probe.stdout.trim(), "100,40", "裁剪区域尺寸不对");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 便于上面的测试读参数位置。
    trait IndexOf {
        fn index_of(&self, needle: &str) -> usize;
    }

    impl IndexOf for Vec<String> {
        fn index_of(&self, needle: &str) -> usize {
            self.iter().position(|item| item == needle).unwrap()
        }
    }
}

#[cfg(test)]
mod execute_tests {
    use super::*;
    use dossier_runtime::{CommandOutput, FakeRunner};

    const TSV: &str = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
5\t1\t1\t1\t1\t1\t10\t12\t40\t14\t96.5\t龙腾\n";

    fn frame_plan(tag: &str) -> (TempDir, RegionFramePlan) {
        let dir =
            std::env::temp_dir().join(format!("dossier-ocr-exec-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.mp4");
        std::fs::write(&input, b"fake").unwrap();
        let plan = RegionFramePlan::new(&input, dir.join("frames"), Region::new(0, 900, 640, 80));
        (TempDir(dir), plan)
    }

    #[test]
    fn execute_lists_the_frames_that_were_actually_written() {
        let (_dir, plan) = frame_plan("frames");
        std::fs::create_dir_all(&plan.output_dir).unwrap();
        std::fs::write(plan.expected_frame_path(1), b"png").unwrap();
        std::fs::write(plan.expected_frame_path(2), b"png").unwrap();
        let runner = FakeRunner::new().on("ffmpeg", CommandOutput::new(0, "", ""));
        let frames = plan.execute(&runner, "ffmpeg").unwrap();
        assert_eq!(frames.len(), 2);
    }

    #[test]
    fn execute_reports_no_frames_when_the_region_is_outside_the_picture() {
        let (_dir, plan) = frame_plan("noframes");
        std::fs::create_dir_all(&plan.output_dir).unwrap();
        let runner = FakeRunner::new().on("ffmpeg", CommandOutput::new(0, "", ""));
        let error = plan.execute(&runner, "ffmpeg").unwrap_err();
        assert!(matches!(error, OcrError::NoFrames(_)), "{error}");
    }

    #[test]
    fn recognize_region_runs_the_frames_then_tesseract() {
        let (_dir, plan) = frame_plan("recognize");
        std::fs::create_dir_all(&plan.output_dir).unwrap();
        std::fs::write(plan.expected_frame_path(1), b"png").unwrap();
        std::fs::write(plan.output_dir.join("ocr_0000.tsv"), TSV).unwrap();
        let runner = FakeRunner::new()
            .on("ffmpeg", CommandOutput::new(0, "", ""))
            .on("tesseract", CommandOutput::new(0, "", ""));
        let outcome = recognize_region(&plan, &runner, "ffmpeg", "tesseract", 60.0).unwrap();
        assert_eq!(outcome.frames.len(), 1);
        assert_eq!(outcome.lines.len(), 1);
        assert_eq!(outcome.lines[0].text, "龙腾");
        assert_eq!(outcome.confident, 1);
    }

    #[test]
    fn recognize_plan_fails_when_tesseract_writes_no_tsv() {
        let (_dir, plan) = frame_plan("notsv");
        let image = plan.expected_frame_path(1);
        std::fs::create_dir_all(&plan.output_dir).unwrap();
        std::fs::write(&image, b"png").unwrap();
        let mut recognize = RecognizePlan::new(&image, plan.output_dir.join("ocr_0000"));
        recognize.language = "chi_sim+eng".to_string();
        let runner = FakeRunner::new().on("tesseract", CommandOutput::new(0, "", ""));
        let error = recognize.execute(&runner, "tesseract").unwrap_err();
        assert!(matches!(error, OcrError::OutputMissing(_)), "{error}");
    }

    /// 只负责在用例结束时清理临时目录。
    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
