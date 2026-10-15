//! 合成渲染：把文字/图片/字幕真的画进画面（ffmpeg `drawtext` / `overlay` / `ass`）。
//!
//! 为什么这个模块值得第一个写：**它是本机唯一能端到端真跑通、并逐像素验证的后期能力**，
//! 而且它是「成片交付」的最后一公里——片头片尾、角标、台标、硬字幕都落在这里。
//!
//! 设计要点：
//! - 一切参数先落成 [`RenderPlan`]（纯数据、可序列化），再由
//!   [`RenderPlan::build_command`] 变成一条可审阅的 ffmpeg 命令行；
//! - **表达式位置**（`x`/`y`）直接透传给 ffmpeg，因为它本身就是 ffmpeg 的通用能力
//!   （`w-tw-10` 这类），自己实现一套只会更差；
//! - 文本转义在库里做（[`escape_drawtext_text`]），不要求调用方懂 ffmpeg 转义规则。

use std::path::{Path, PathBuf};

use dossier_runtime::{CommandRunner, CommandSpec, SystemRunner};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 渲染错误。
#[derive(Debug, Error)]
pub enum RenderError {
    #[error("输入文件不存在：{0}")]
    InputMissing(PathBuf),
    #[error("渲染计划里没有任何元素")]
    NoElements,
    #[error("文字元素内容为空")]
    EmptyText,
    #[error("图片元素不存在：{0}")]
    ImageMissing(PathBuf),
    #[error("字幕文件不存在：{0}")]
    SubtitleMissing(PathBuf),
    #[error("元素 {index} 的时间区间非法：start={start}，end={end:?}")]
    InvalidRange {
        index: usize,
        start: f64,
        end: Option<f64>,
    },
    #[error("字号必须大于 0（元素 {0}）")]
    InvalidFontSize(usize),
    #[error("没有可用的视频编码器：{0}")]
    NoEncoder(String),
    #[error("渲染失败（{command}）：{message}")]
    RenderFailed { command: String, message: String },
}

/// 时间区间（秒）；`end` 为 `None` 表示「到片尾」。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeRange {
    pub start: f64,
    #[serde(default)]
    pub end: Option<f64>,
}

impl TimeRange {
    pub fn new(start: f64, end: Option<f64>) -> Self {
        Self { start, end }
    }

    /// 转成 ffmpeg `enable=` 表达式。
    ///
    /// 到片尾时用 `gte(t,start)`，有结束时间时用 `between(t,start,end)`——
    /// 后者是闭区间，与「字幕显示到 end 为止」的直觉一致。
    pub fn enable_expression(&self) -> Option<String> {
        if self.start <= 0.0 && self.end.is_none() {
            return None;
        }
        Some(match self.end {
            Some(end) => format!("between(t,{},{})", self.start, end),
            None => format!("gte(t,{})", self.start),
        })
    }

    fn validate(&self) -> bool {
        self.start.is_finite()
            && self.start >= 0.0
            && self
                .end
                .map(|end| end.is_finite() && end > self.start)
                .unwrap_or(true)
    }
}

/// 位置：绝对像素或 ffmpeg 表达式。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Position {
    /// 绝对像素。
    Pixels(i64),
    /// ffmpeg 表达式（例如 `(w-tw)/2`、`w-tw-10`）。
    Expression(String),
}

impl Position {
    fn render(&self) -> String {
        match self {
            Self::Pixels(value) => value.to_string(),
            Self::Expression(text) => text.clone(),
        }
    }
}

impl From<i64> for Position {
    fn from(value: i64) -> Self {
        Self::Pixels(value)
    }
}

impl From<&str> for Position {
    fn from(value: &str) -> Self {
        Self::Expression(value.to_string())
    }
}

/// 文字叠加。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextOverlay {
    pub text: String,
    pub x: Position,
    pub y: Position,
    pub font_size: u32,
    /// 字体颜色，ffmpeg 写法（`white`、`0xFF0000`、`red@0.8`）。
    pub font_color: String,
    /// 指定字体文件（中文必须指定，否则 fontconfig 会挑到没有汉字的字体）。
    #[serde(default)]
    pub font_file: Option<PathBuf>,
    /// 描边宽度（0 = 不描边）。
    #[serde(default)]
    pub border_width: u32,
    /// 描边颜色。
    #[serde(default)]
    pub border_color: Option<String>,
    /// 背景框颜色（`black@0.5`）。
    #[serde(default)]
    pub box_color: Option<String>,
    /// 背景框内边距。
    #[serde(default)]
    pub box_border: u32,
    #[serde(default)]
    pub range: Option<TimeRange>,
}

impl TextOverlay {
    /// 右下角字幕的常用构造：距右下各 `margin` 像素。
    pub fn caption(text: impl Into<String>, font_size: u32, font_color: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            x: Position::Expression("w-tw-24".to_string()),
            y: Position::Expression("h-th-24".to_string()),
            font_size,
            font_color: font_color.into(),
            font_file: None,
            border_width: 2,
            border_color: Some("black@0.85".to_string()),
            box_color: None,
            box_border: 0,
            range: None,
        }
    }

    fn build(&self) -> Result<String, RenderError> {
        if self.text.trim().is_empty() {
            return Err(RenderError::EmptyText);
        }
        let mut parts = vec![
            // `expansion=none` 关掉 drawtext 自己的 `%{...}` 展开，`%` 就是普通字符；
            // 文本一律**不加引号、全用反斜杠转义**——实测用单引号包裹时，
            // 文本里真的出现 `'` 会让引号提前闭合，后面所有选项都被当成文本的一部分。
            "expansion=none".to_string(),
            format!("text={}", escape_filter_value(&self.text)),
            format!("x={}", self.x.render()),
            format!("y={}", self.y.render()),
            format!("fontsize={}", self.font_size),
            format!("fontcolor={}", self.font_color),
        ];
        if let Some(font) = &self.font_file {
            parts.push(format!(
                "fontfile={}",
                escape_filter_value(&font.display().to_string())
            ));
        }
        if self.border_width > 0 {
            parts.push(format!("borderw={}", self.border_width));
            parts.push(format!(
                "bordercolor={}",
                self.border_color
                    .clone()
                    .unwrap_or_else(|| "black".to_string())
            ));
        }
        if let Some(box_color) = &self.box_color {
            parts.push("box=1".to_string());
            parts.push(format!("boxcolor={box_color}"));
            parts.push(format!("boxborderw={}", self.box_border));
        }
        if let Some(range) = &self.range {
            if let Some(expression) = range.enable_expression() {
                parts.push(format!("enable='{expression}'"));
            }
        }
        Ok(format!("drawtext={}", parts.join(":")))
    }
}

/// 图片叠加（角标、台标、水印贴图）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageOverlay {
    pub path: PathBuf,
    pub x: Position,
    pub y: Position,
    /// 目标宽度；`None` 表示保持原尺寸（`scale` 仍会用于等比缩放）。
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub range: Option<TimeRange>,
    /// 透明度 0.0~1.0。
    #[serde(default = "default_opacity")]
    pub opacity: f64,
}

fn default_opacity() -> f64 {
    1.0
}

/// 字幕烧录（`ass` 滤镜；`srt` 也走这条路，ffmpeg 会自动转）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtitleBurn {
    pub path: PathBuf,
    /// libass 样式覆盖，例如 `Fontsize=32,Outline=2,MarginV=24`。
    ///
    /// 默认样式是按 1080p 设计的：直接烧到短片/低分辨率素材上会小到看不清，
    /// 所以这里必须留一个显式的样式入口，而不是让用户去猜 ASS 默认值。
    #[serde(default)]
    pub force_style: Option<String>,
}

/// 一个渲染元素。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RenderElement {
    Text(TextOverlay),
    Image(ImageOverlay),
    Subtitles(SubtitleBurn),
}

impl RenderElement {
    fn range(&self) -> Option<TimeRange> {
        match self {
            Self::Text(text) => text.range,
            Self::Image(image) => image.range,
            Self::Subtitles(_) => None,
        }
    }
}

/// 渲染计划：一份纯数据，落盘即可复现。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderPlan {
    pub input: PathBuf,
    pub output: PathBuf,
    pub elements: Vec<RenderElement>,
    /// 视频编码器；`None` 用 `libx264`。
    #[serde(default)]
    pub video_codec: Option<String>,
    /// 恒定质量因子；`None` 用 20。
    #[serde(default)]
    pub crf: Option<u32>,
    /// 编码预设。
    #[serde(default)]
    pub preset: Option<String>,
}

impl RenderPlan {
    pub fn new(input: impl Into<PathBuf>, output: impl Into<PathBuf>) -> Self {
        Self {
            input: input.into(),
            output: output.into(),
            elements: Vec::new(),
            video_codec: None,
            crf: None,
            preset: None,
        }
    }

    #[must_use]
    pub fn with(mut self, element: RenderElement) -> Self {
        self.elements.push(element);
        self
    }

    /// 静态校验：能在**不调用 ffmpeg** 的前提下发现的错误，一律在这里报出来。
    pub fn validate(&self) -> Result<(), RenderError> {
        if !self.input.is_file() {
            return Err(RenderError::InputMissing(self.input.clone()));
        }
        if self.elements.is_empty() {
            return Err(RenderError::NoElements);
        }
        for (index, element) in self.elements.iter().enumerate() {
            if let Some(range) = element.range() {
                if !range.validate() {
                    return Err(RenderError::InvalidRange {
                        index,
                        start: range.start,
                        end: range.end,
                    });
                }
            }
            match element {
                RenderElement::Text(text) => {
                    if text.text.trim().is_empty() {
                        return Err(RenderError::EmptyText);
                    }
                    if text.font_size == 0 {
                        return Err(RenderError::InvalidFontSize(index));
                    }
                    if let Some(font) = &text.font_file {
                        if !font.is_file() {
                            return Err(RenderError::ImageMissing(font.clone()));
                        }
                    }
                }
                RenderElement::Image(image) => {
                    if !image.path.is_file() {
                        return Err(RenderError::ImageMissing(image.path.clone()));
                    }
                    if !(0.0..=1.0).contains(&image.opacity) {
                        return Err(RenderError::InvalidRange {
                            index,
                            start: image.opacity,
                            end: None,
                        });
                    }
                }
                RenderElement::Subtitles(burn) => {
                    if !burn.path.is_file() {
                        return Err(RenderError::SubtitleMissing(burn.path.clone()));
                    }
                }
            }
        }
        Ok(())
    }

    /// 需要作为额外输入打开的图片（`overlay` 的第二个输入）。
    pub fn image_inputs(&self) -> Vec<&Path> {
        self.elements
            .iter()
            .filter_map(|element| match element {
                RenderElement::Image(image) => Some(image.path.as_path()),
                _ => None,
            })
            .collect()
    }

    /// 构造 `-filter_complex` 表达式。
    pub fn filter_complex(&self) -> Result<String, RenderError> {
        let mut chain: Vec<String> = Vec::new();
        let mut current = "[0:v]".to_string();
        for (index, element) in self.elements.iter().enumerate() {
            match element {
                RenderElement::Text(text) => {
                    let next = format!("[v{index}]");
                    chain.push(format!("{current}{}{next}", text.build()?));
                    current = next;
                }
                RenderElement::Image(image) => {
                    let next = format!("[v{index}]");
                    // 图片输入的序号从 1 开始（0 是视频本体），按出现顺序排。
                    let image_index = self
                        .elements
                        .iter()
                        .take(index + 1)
                        .filter(|element| matches!(element, RenderElement::Image(_)))
                        .count();
                    let mut filters = Vec::new();
                    if let Some(width) = image.width {
                        filters.push(format!("scale={width}:-1"));
                    }
                    if image.opacity < 1.0 {
                        filters.push(format!(
                            "format=rgba,colorchannelmixer=aa={}",
                            format_float(image.opacity)
                        ));
                    }
                    let image_ref = if filters.is_empty() {
                        // 不需要预处理时**不能**凭空造一个 `[1:v][img0]` 空链——
                        // 那会被 ffmpeg 报成 `No such filter: ''`。
                        format!("[{image_index}:v]")
                    } else {
                        chain.push(format!(
                            "[{image_index}:v]{}[img{index}]",
                            filters.join(",")
                        ));
                        format!("[img{index}]")
                    };
                    let mut overlay = format!("overlay={}:{}", image.x.render(), image.y.render());
                    if let Some(range) = &image.range {
                        if let Some(expression) = range.enable_expression() {
                            overlay.push_str(&format!(":enable='{expression}'"));
                        }
                    }
                    chain.push(format!("{current}{image_ref}{overlay}{next}"));
                    current = next;
                }
                RenderElement::Subtitles(burn) => {
                    let next = format!("[v{index}]");
                    let mut options = format!(
                        // 用 `subtitles` 而不是 `ass`：它同样走 libass，但能直接吃 SRT。
                        "subtitles=filename={}",
                        escape_filter_value(&burn.path.display().to_string())
                    );
                    if let Some(style) = &burn.force_style {
                        // `force_style` 里的 `=` 和 `,` 会被 libass 当分隔符，
                        // 所以整段必须留在引号里；样式串本身不含引号。
                        options.push_str(&format!(":force_style='{style}'"));
                    }
                    chain.push(format!("{current}{options}{next}"));
                    current = next;
                }
            }
        }
        if chain.is_empty() {
            return Err(RenderError::NoElements);
        }
        // 落一个固定出口标签，`-map [vout]` 就不必关心最后一个元素是哪种类型。
        chain.push(format!("{current}null[vout]"));
        Ok(chain.join(";"))
    }

    /// 生成一条可审阅的 ffmpeg 命令。
    pub fn build_command(&self, ffmpeg: &str) -> Result<CommandSpec, RenderError> {
        self.validate()?;
        let mut spec = CommandSpec::new(ffmpeg.to_string())
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-y")
            .arg("-i")
            .arg(self.input.display().to_string());
        for image in self.image_inputs() {
            spec = spec.arg("-i").arg(image.display().to_string());
        }
        spec = spec
            .arg("-filter_complex")
            .arg(self.filter_complex()?)
            .arg("-map")
            .arg("[vout]");
        // 叠加/烧录一定要重编码，所以这里没有「copy 视频流」的分支——那会静默丢掉效果。
        spec = spec
            .arg("-c:v")
            .arg(
                self.video_codec
                    .clone()
                    .unwrap_or_else(|| "libx264".to_string()),
            )
            .arg("-crf")
            .arg(self.crf.unwrap_or(20).to_string())
            .arg("-preset")
            .arg(
                self.preset
                    .clone()
                    .unwrap_or_else(|| "veryfast".to_string()),
            );
        // 有音轨就照抄，没有音轨时 ffmpeg 会因为「映射不到流」而报错，所以用 `-map 0:a?`
        // 的可选映射（`?` 表示「有就带上，没有就算了」）。
        spec = spec
            .arg("-map")
            .arg("0:a?")
            .arg("-c:a")
            .arg("copy")
            .arg(self.output.display().to_string());
        Ok(spec)
    }

    /// 真的跑一次渲染。返回 ffmpeg 的原始输出，便于把 stderr 首行报给用户。
    pub fn render(&self, ffmpeg: &str) -> Result<String, RenderError> {
        let spec = self.build_command(ffmpeg)?;
        let runner = SystemRunner::new();
        let output = match runner.run(&spec) {
            Ok(output) => output,
            Err(error) => {
                return Err(RenderError::RenderFailed {
                    command: spec.display(),
                    message: error.to_string(),
                })
            }
        };
        if !output.success() {
            return Err(RenderError::RenderFailed {
                command: spec.display(),
                message: format!(
                    "退出码 {}：{}",
                    output.code_label(),
                    first_line(&output.combined())
                ),
            });
        }
        Ok(output.combined())
    }
}

/// 数字渲染成 ffmpeg 认识的短形式（`1` 而不是 `1.0`，`0.35` 保持原样）。
fn format_float(value: f64) -> String {
    let text = format!("{value:.4}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 滤镜参数值转义（**不加引号**的形态）。
///
/// ffmpeg 解析 `-filter_complex` 时是**两层**分词，而且两层的转义规则不同：
///
/// 1. 第一层（filtergraph）按 `,` 分滤镜、`;` 分链、`[`/`]` 标标签、`'` 引号；
/// 2. 第二层（filter options）按 `:` 分选项。
///
/// 所以同一段文本要依次穿过两层，反斜杠数量必须按「哪一层认识这个字符」来定。
/// 下面这张表是**实测**出来的：用 `textfile=` 渲染出参考帧，再暴力枚举反斜杠个数，
/// 找出与参考帧**逐像素相同**的最小值
/// （见 `escape_table_matches_textfile_reference_pixel_for_pixel`）。
///
/// 表里没有 `%`：`expansion=none` 已经关掉 drawtext 自己的 `%{...}` 展开，`%` 就是普通字符。
/// 表里也没有 `=`：第一层不把它当分隔符，第二层只在选项名位置认它。
///
/// 不用 `'...'` 包裹的原因：引号在第一层就被吃掉了，第二层照样按 `:` 切；
/// 而且文本里真的出现 `'` 时引号会提前闭合，报出
/// `Cannot find color 'white[v0];[v0]null[vout]'` 这种莫名其妙的错。
pub fn escape_filter_value(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        match ch {
            // 第二层的分隔符：要活过两层，2 个反斜杠。
            ':' => out.push_str("\\\\:"),
            // 第一层的分隔符：1 个反斜杠。
            ';' | ',' | '[' | ']' => {
                out.push('\\');
                out.push(ch);
            }
            // 引号在两层里都有意义：实测 3 个反斜杠 + 引号本身。
            '\'' => {
                out.push_str("\\\\\\");
                out.push('\'');
            }
            // 反斜杠自己：实测**一共** 4 个。这里不能再追加 `ch`——层数每差 2 个反斜杠，
            // 画面就从「一个反斜杠」变成「什么都不画」或「画出两个反斜杠」，像素测试能抓到。
            '\\' => out.push_str("\\\\\\\\"),
            // 换行：**原样透传裸 LF**。
            //
            // 这里踩过一个坑：想当然写成 `\n`（反斜杠 + n）的话，本机 ffmpeg 4.2 会把它
            // 当成**字面量两个字符**画出来（A\nB 变成一行 "A\nB"，宽度也证实是 4 个字形），
            // 真正的换行反而是裸 LF。实测 `text=空格 与<LF>换行` 与 `textfile=` 逐像素一致，
            // 见 `escape_table_matches_textfile_reference_pixel_for_pixel`。
            '\n' => out.push('\n'),
            '\r' => {}
            other => out.push(other),
        }
    }
    out
}

fn first_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dossier_runtime::probe::probe_ffmpeg_capabilities;
    use dossier_runtime::{CommandRunner, CommandSpec, SystemRunner};

    fn plan_for(input: &str, output: &str) -> RenderPlan {
        RenderPlan::new(input, output)
    }

    #[test]
    fn escapes_special_characters_for_filter_values() {
        // `:` 是选项分隔符、`'` 是引号、`%` 会触发 `%{...}` 展开，都必须转义。
        assert_eq!(escape_filter_value("a:b"), "a\\\\:b");
        assert_eq!(escape_filter_value("it's"), "it\\\\\\'s");
        // `%` 不再是特殊字符：`expansion=none` 已经关掉了 drawtext 的展开。
        assert_eq!(escape_filter_value("100%"), "100%");
        // 单个反斜杠要 4 个反斜杠才活得过两层解析（实测值，别"化简"成 3 个）。
        assert_eq!(escape_filter_value("a\\b"), format!("a{}b", "\\".repeat(4)));
        // 换行原样保留裸 LF（写成 `\n` 会被本机 ffmpeg 当字面量画出来）。
        assert_eq!(escape_filter_value("行1\n行2"), "行1\n行2");
        assert_eq!(escape_filter_value("a,b[c]"), "a\\,b\\[c\\]");
        assert_eq!(escape_filter_value("a;b"), "a\\;b");
        // 第一层不把 `=` 当分隔符，所以不需要转义。
        assert_eq!(escape_filter_value("k=v"), "k=v");
    }

    #[test]
    fn time_range_becomes_enable_expression() {
        assert_eq!(TimeRange::new(0.0, None).enable_expression(), None);
        assert_eq!(
            TimeRange::new(2.0, Some(5.0)).enable_expression(),
            Some("between(t,2,5)".to_string())
        );
        assert_eq!(
            TimeRange::new(2.0, None).enable_expression(),
            Some("gte(t,2)".to_string())
        );
    }

    #[test]
    fn builds_a_filter_chain_for_text_and_image() {
        let mut plan = plan_for("/tmp/in.mp4", "/tmp/out.mp4");
        plan.elements.push(RenderElement::Text(TextOverlay {
            text: "标题".into(),
            x: Position::Pixels(10),
            y: Position::Expression("(h-th)/2".into()),
            font_size: 32,
            font_color: "white".into(),
            font_file: None,
            border_width: 2,
            border_color: None,
            box_color: Some("black@0.5".into()),
            box_border: 8,
            range: Some(TimeRange::new(1.0, Some(3.0))),
        }));
        plan.elements.push(RenderElement::Image(ImageOverlay {
            path: PathBuf::from("/tmp/logo.png"),
            x: Position::Expression("w-overlay_w-16".into()),
            y: Position::Pixels(16),
            width: Some(48),
            range: None,
            opacity: 0.8,
        }));
        let chain = plan.filter_complex().unwrap();
        // 文字：转义 + 位置 + 描边 + 背景框 + 时间区间。
        assert!(chain.contains(
            "drawtext=expansion=none:text=标题:x=10:y=(h-th)/2:fontsize=32:fontcolor=white"
        ));
        assert!(chain.contains("borderw=2:bordercolor=black"));
        assert!(chain.contains("box=1:boxcolor=black@0.5:boxborderw=8"));
        assert!(chain.contains("enable='between(t,1,3)'"));
        // 图片：第二个输入 + 缩放 + 透明度 + 叠加。
        assert!(chain.contains("[1:v]scale=48:-1,format=rgba,colorchannelmixer=aa=0.8[img1]"));
        assert!(chain.contains("overlay=w-overlay_w-16:16"));
        // 无论挂了什么元素，出口标签固定是 [vout]；这里只有 2 个元素，所以末段是 [v1]。
        assert!(chain.ends_with("[v1]null[vout]"), "实际链路：{chain}");
    }

    #[test]
    fn validates_static_problems_without_calling_ffmpeg() {
        let plan = plan_for("/definitely/not/here.mp4", "/tmp/out.mp4");
        assert!(matches!(plan.validate(), Err(RenderError::InputMissing(_))));

        let dir = std::env::temp_dir().join(format!("dossier-render-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.mp4");
        std::fs::write(&input, b"not-a-real-video").unwrap();
        let empty = RenderPlan::new(&input, "/tmp/out.mp4");
        assert!(matches!(empty.validate(), Err(RenderError::NoElements)));

        let mut empty_text = RenderPlan::new(&input, "/tmp/out.mp4");
        empty_text
            .elements
            .push(RenderElement::Text(TextOverlay::caption(
                "   ", 24, "white",
            )));
        assert!(matches!(empty_text.validate(), Err(RenderError::EmptyText)));

        let mut bad_size = RenderPlan::new(&input, "/tmp/out.mp4");
        bad_size
            .elements
            .push(RenderElement::Text(TextOverlay::caption("x", 0, "white")));
        assert!(matches!(
            bad_size.validate(),
            Err(RenderError::InvalidFontSize(0))
        ));

        let mut bad_range = RenderPlan::new(&input, "/tmp/out.mp4");
        let mut text = TextOverlay::caption("x", 24, "white");
        text.range = Some(TimeRange::new(5.0, Some(1.0)));
        bad_range.elements.push(RenderElement::Text(text));
        assert!(matches!(
            bad_range.validate(),
            Err(RenderError::InvalidRange { index: 0, .. })
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn caption_defaults_to_bottom_right_with_outline() {
        let caption = TextOverlay::caption("对白", 28, "white");
        assert_eq!(caption.x.render(), "w-tw-24");
        assert_eq!(caption.y.render(), "h-th-24");
        assert_eq!(caption.border_width, 2);
        assert!(caption.box_color.is_none());
    }

    /// 真跑一次「黑底 + 白字」渲染，逐像素确认字真的画上去了。
    ///
    /// 为什么值得这么测：滤镜链只要有一处转义或标签写错，ffmpeg 要么直接失败，要么
    /// **成功输出一张什么都没画的原图**——只看退出码是抓不到后者的。
    #[test]
    fn real_render_draws_text_pixels_into_the_frame() {
        let runner = SystemRunner::new();
        if !ffmpeg_available(&runner) {
            eprintln!("跳过：本机没有可用的 ffmpeg");
            return;
        }
        if !filter_ready(&runner, &["drawtext"]) {
            eprintln!("跳过：本机 ffmpeg 不带 drawtext 滤镜，无法真跑文字渲染");
            return;
        }
        let dir = test_dir("render-text");
        let source = dir.join("black.mp4");
        run_ok(
            &runner,
            &CommandSpec::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=black:s=200x120:r=10:d=1",
                    "-pix_fmt",
                    "yuv420p",
                ])
                .arg(source.display().to_string()),
        );

        let output = dir.join("text.mp4");
        let mut plan = RenderPlan::new(&source, &output);
        plan.elements.push(RenderElement::Text(TextOverlay {
            text: "TEST:1'2%3".into(),
            x: Position::Pixels(10),
            y: Position::Pixels(10),
            font_size: 28,
            font_color: "white".into(),
            font_file: None,
            border_width: 0,
            border_color: None,
            box_color: None,
            box_border: 0,
            range: None,
        }));
        plan.render("ffmpeg").unwrap();

        let before = grab_frame(&runner, &source, "rgb24");
        let after = grab_frame(&runner, &output, "rgb24");
        assert_eq!(before.len(), after.len(), "渲染不应改变分辨率");
        assert_eq!(count_bright_pixels(&before), 0, "黑底原图不该有亮像素");
        let bright = count_bright_pixels(&after);
        assert!(bright > 50, "白字应当画出足够多的亮像素，实际 {bright} 个");
        // 字画在左上角：把画面竖着切两半，上半部分必须有亮像素。
        assert!(count_bright_pixels(&after[..after.len() / 2]) > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真跑一次图片叠加，确认角标像素来自图片本身。
    #[test]
    fn real_render_overlays_an_image_input() {
        let runner = SystemRunner::new();
        if !ffmpeg_available(&runner) {
            eprintln!("跳过：本机没有可用的 ffmpeg");
            return;
        }
        let dir = test_dir("render-image");
        let source = dir.join("black.mp4");
        run_ok(
            &runner,
            &CommandSpec::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=black:s=120x120:r=10:d=1",
                    "-pix_fmt",
                    "yuv420p",
                ])
                .arg(source.display().to_string()),
        );
        let logo = dir.join("logo.png");
        run_ok(
            &runner,
            &CommandSpec::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=red:s=20x20",
                    "-frames:v",
                    "1",
                ])
                .arg(logo.display().to_string()),
        );

        let output = dir.join("logo.mp4");
        let mut plan = RenderPlan::new(&source, &output);
        plan.elements.push(RenderElement::Image(ImageOverlay {
            path: logo.clone(),
            x: Position::Pixels(0),
            y: Position::Pixels(0),
            width: None,
            range: None,
            opacity: 1.0,
        }));
        plan.render("ffmpeg").unwrap();

        let frame = grab_frame(&runner, &output, "rgb24");
        let (width, pixel) = (120_usize, 5_usize);
        let index = (pixel * width + pixel) * 3;
        let (r, g, b) = (frame[index], frame[index + 1], frame[index + 2]);
        assert!(
            r > 150 && g < 100 && b < 100,
            "角标位置应当是红色，实际 rgb({r},{g},{b})"
        );
        // 远离角标的位置仍是黑的。
        let far = (100 * width + 100) * 3;
        assert!(frame[far] < 60 && frame[far + 1] < 60 && frame[far + 2] < 60);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真跑一次字幕烧录：SRT → libass → 画面变化。
    #[test]
    fn real_render_burns_subtitles_from_an_srt_file() {
        let runner = SystemRunner::new();
        if !ffmpeg_available(&runner) {
            eprintln!("跳过：本机没有可用的 ffmpeg");
            return;
        }
        if !filter_ready(&runner, &["subtitles"]) {
            eprintln!("跳过：本机 ffmpeg 不带 subtitles 滤镜（需 libass），无法真跑字幕烧录");
            return;
        }
        let dir = test_dir("render-subs");
        let source = dir.join("black.mp4");
        run_ok(
            &runner,
            &CommandSpec::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=black:s=320x160:r=10:d=2",
                    "-pix_fmt",
                    "yuv420p",
                ])
                .arg(source.display().to_string()),
        );
        let srt = dir.join("subs.srt");
        crate::srt::save_srt(
            &srt,
            &[crate::srt::SubtitleCue::new(0.0, 2.0, "HELLO SUBTITLE")],
        )
        .unwrap();

        let output = dir.join("burned.mp4");
        let mut plan = RenderPlan::new(&source, &output);
        plan.elements.push(RenderElement::Subtitles(SubtitleBurn {
            path: srt,
            // 320x160 的素材上必须放大字号，否则默认样式小到看不见。
            force_style: Some("Fontsize=24,Outline=2,MarginV=6".to_string()),
        }));
        plan.render("ffmpeg").unwrap();

        let before = grab_frame(&runner, &source, "rgb24");
        let after = grab_frame(&runner, &output, "rgb24");
        assert_eq!(count_bright_pixels(&before), 0);
        // libass 会做抗锯齿，笔画边缘不是纯白，所以这里数「明显非黑」的像素。
        let lit = count_lit_pixels(&after);
        assert!(lit > 200, "字幕应当画出亮像素，实际 {}", lit);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn ffmpeg_available(runner: &SystemRunner) -> bool {
        runner
            .run(&CommandSpec::new("ffmpeg").arg("-version"))
            .map(|output| output.success())
            .unwrap_or(false)
    }

    /// drawtext / subtitles 这类滤镜**不是 ffmpeg 的必编项**：精简构建（实测
    /// Homebrew 的默认 bottle 就不含 libfreetype/libass）会整个没有这些滤镜。
    /// 真跑前按能力跳过，与本仓库「缺工具跳过」的约定一致；能力冒烟用例
    /// （tools.rs）会把缺失项连原因一起打出来，不会静默。
    fn filter_ready(runner: &SystemRunner, needed: &[&str]) -> bool {
        let caps = probe_ffmpeg_capabilities(runner, None);
        needed.iter().all(|name| caps.filters.supports(name))
    }

    fn test_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dossier-media-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run_ok(runner: &SystemRunner, spec: &CommandSpec) {
        let output = runner.run(spec).unwrap();
        assert!(output.success(), "命令失败：{}", output.combined());
    }

    /// 取第 0 帧的原始像素。
    fn grab_frame(runner: &SystemRunner, input: &Path, pix_fmt: &str) -> Vec<u8> {
        let output = runner
            .run(
                &CommandSpec::new("ffmpeg")
                    .args(["-hide_banner", "-loglevel", "error", "-i"])
                    .arg(input.display().to_string())
                    .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", pix_fmt, "-"]),
            )
            .unwrap();
        assert!(output.success(), "抽帧失败：{}", output.combined());
        // rawvideo 走 stdout，必须用原始字节（lossy 解码会破坏像素）。
        output.stdout_raw
    }

    fn count_bright_pixels(frame: &[u8]) -> usize {
        frame
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|pixel| pixel[0] > 160 && pixel[1] > 160 && pixel[2] > 160)
            .count()
    }

    /// 「明显不是纯黑」的像素数：字幕有抗锯齿，笔画边缘会是灰色。
    fn count_lit_pixels(frame: &[u8]) -> usize {
        frame
            .as_chunks::<3>()
            .0
            .iter()
            .filter(|pixel| pixel[0] > 40 || pixel[1] > 40 || pixel[2] > 40)
            .count()
    }

    /// 找一个真的含汉字的字体文件；找不到时返回 `None`，调用方自行降级。
    ///
    /// 顺序是刻意的：先试项目里约定的路径，再看常见发行版位置。都不在就说明这台机器
    /// 没装中文字体，此时**不能假装测试通过**——调用方会把中文用例剔掉并打印跳过原因。
    fn cjk_font() -> Option<PathBuf> {
        const CANDIDATES: [&str; 4] = [
            "/usr/share/fonts/chinese/simsun.ttc",
            "/usr/share/fonts/truetype/arphic/uming.ttc",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        ];
        CANDIDATES
            .iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
    }

    /// 转义表的守门测试：把「内联转义」的渲染结果和「`textfile=` 原始字节」的渲染结果
    /// **逐像素比对**。
    ///
    /// 参考帧走的是完全不过滤的路径（drawtext 直接从文件读字节），所以只要两者像素一致，
    /// 就证明我们的转义没有丢掉、多出或改动任何字符。这比断言命令行字符串强得多——
    /// 字符串对了但层数不对的话，画面上会少一个字，肉眼不一定看得出来。
    #[test]
    fn escape_table_matches_textfile_reference_pixel_for_pixel() {
        let runner = SystemRunner::new();
        if !ffmpeg_available(&runner) {
            eprintln!("跳过：本机没有可用的 ffmpeg");
            return;
        }
        if !filter_ready(&runner, &["drawtext"]) {
            eprintln!("跳过：本机 ffmpeg 不带 drawtext 滤镜，无法真跑转义表比对");
            return;
        }
        // 中文字符必须显式指定含汉字的字体文件：fontconfig 的默认匹配
        // (`fc-match :lang=zh`) 在本机会选中没有汉字的字体，渲染结果是**一个字都不画**。
        // 那样「参考帧 == 候选帧」会退化成 0 == 0 的假通过，测不出任何东西。
        let font = cjk_font();
        // fontfile 是文件路径，要过 `escape_filter_value`：Windows 路径里的
        // `C:` 冒号和反斜杠都是滤镜语法的特殊字符，不转义会被 ffmpeg 拆坏。
        // 产品代码（TextOverlay::build）对 fontfile 早就是这样处理的。
        let font_option = font
            .as_ref()
            .map(|path| {
                format!(
                    ":fontfile={}",
                    escape_filter_value(&path.display().to_string())
                )
            })
            .unwrap_or_default();
        let dir = test_dir("render-escape");
        let source = dir.join("black.mp4");
        run_ok(
            &runner,
            &CommandSpec::new("ffmpeg")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=black:s=420x120:r=5:d=1",
                    "-pix_fmt",
                    "yuv420p",
                ])
                .arg(source.display().to_string()),
        );

        // 纯 ASCII 用例到哪台机器都能画出来，是转义表的主证据。
        let mut messages: Vec<&str> = vec![
            "TEST:1'2%3",
            "it's",
            "'start",
            "end'",
            "a''b",
            "a\\b",
            "semi;colon",
            "brack[et]s",
            "k=v",
            "no-special",
            "multi\nline",
        ];
        if font.is_some() {
            // 中文用例顺带验证「换行 + 全角标点」这条最容易出错的组合。
            messages.extend(["旁白：'你好'", "100% 完成", "空格 与\n换行"]);
        } else {
            eprintln!("跳过中文用例：本机没有找到含汉字的字体文件");
        }
        for (index, message) in messages.iter().enumerate() {
            let textfile = dir.join(format!("msg{index}.txt"));
            std::fs::write(&textfile, message).unwrap();
            let reference = dir.join(format!("ref{index}.mp4"));
            let candidate = dir.join(format!("cand{index}.mp4"));

            let reference_spec = text_spec(
                &source,
                &reference,
                // textfile 同样是文件路径，同样要转义（原因见上面 font_option 的注释）。
                // 转义只作用于**路径**，文件内容仍是原始字节，参考帧的语义不变。
                &format!(
                    "textfile={}",
                    escape_filter_value(&textfile.display().to_string())
                ),
                &font_option,
            );
            run_ok(&runner, &reference_spec);
            let candidate_spec = text_spec(
                &source,
                &candidate,
                &format!("text={}", escape_filter_value(message)),
                &font_option,
            );
            run_ok(&runner, &candidate_spec);

            assert_eq!(
                grab_frame(&runner, &reference, "rgb24"),
                grab_frame(&runner, &candidate, "rgb24"),
                "转义后的文本与 textfile 参考不一致：{message:?}"
            );
            assert!(
                count_lit_pixels(&grab_frame(&runner, &candidate, "rgb24")) > 0,
                "文本没有被画出来：{message:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn text_spec(
        source: &Path,
        output: &Path,
        text_option: &str,
        font_option: &str,
    ) -> CommandSpec {
        let filter = format!(
            "[0:v]drawtext=expansion=none:{text_option}:x=10:y=10:fontsize=26:fontcolor=white{font_option}[v0];[v0]null[vout]"
        );
        CommandSpec::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(source.display().to_string())
            .args([
                "-filter_complex",
                &filter,
                "-map",
                "[vout]",
                "-c:v",
                "libx264",
            ])
            .arg(output.display().to_string())
    }
}
