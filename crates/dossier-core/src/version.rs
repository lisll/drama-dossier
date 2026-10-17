//! 处理版本生成（M4b-1）：模板结构、滤镜图、画质结果、版本说明。
//!
//! 数据流见 `.narrafork/plans/plan-plan-m4b-production-features--CrkkLBaibNYlV6Qr.md` 2.1–2.5 节：
//! 配方（`dossier-license::recipe::Recipe`）带两层参数——第一层是输出规格（分辨率、帧率、码率上限），
//! 第二层是细微处理（颗粒、放大裁边、亮度、变速……）。本模块把「模板 + 配方 + 一集」变成
//! 一条可审阅、可复现的 ffmpeg 参数序列（[`VersionGraph`]），并把画质检测结果解析回来。
//!
//! 纯计算，不碰文件系统、不跑进程：调用方（`src-tauri/src/version.rs`）负责探测、落盘、编码器选择
//! 和子进程管理。这样滤镜图的每个细节都能直接单测。
//!
//! 画质检测的口径（计划 2.4 节）：同一次 ffmpeg 里分出两路——
//! 画质检测的口径(计划 2.4 节):同一次 ffmpeg 里分出两路——
//! 一路做完整处理并编码输出;另一路只做「几何处理」(缩放、裁边、裁帧、变速),
//! 处理后的画面在送进编码器之前分一份出来,用 `ssim` / `psnr` 滤镜和「几何参考」对比。
//! 这样量出来的是第二层细微处理(颗粒、亮度饱和度色相)造成的损失,不含编码损失
//! (滤镜图跑在编码器之前,同一次 ffmpeg 里拿不到编码后的画面;编码质量由码率上限和 crf 保证)。
//! 放大裁边本身会让逐像素对比大幅下降,量它没有意义。
//!
//! 「有意加的内容」不参与对比,一律放在两路分开之后:字幕、剧名水印、AI 标识、调色预设。
//! 调色预设属于第一层(看得见的内容),它的改变是用户明确选的;算进损失的话,
//! 一个「暖色」预设就能让整部剧都过不了下限。
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 滤镜图或参数解析的错误。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum VersionError {
    #[error("模板里缺少输出规格（配方参数不完整）")]
    MissingSpec,
    #[error("输出规格不合法：{0}")]
    BadSpec(String),
    #[error("细微处理参数不合法：{0}")]
    BadSubtle(String),
    #[error("模板不合法：{0}")]
    BadTemplate(String),
    #[error("水印位置只能是四个角")]
    BadWatermarkCorner,
    #[error("画质结果解析失败：{0}")]
    BadQuality(String),
}

impl VersionError {
    pub fn code(&self) -> &'static str {
        match self {
            VersionError::MissingSpec => "missing_spec",
            VersionError::BadSpec(_) => "bad_spec",
            VersionError::BadSubtle(_) => "bad_subtle",
            VersionError::BadTemplate(_) => "bad_template",
            VersionError::BadWatermarkCorner => "bad_watermark_corner",
            VersionError::BadQuality(_) => "bad_quality",
        }
    }
}

/// 输出规格（配方 `params.platform`）。用户不能改：防止和「三家最严值」对不上。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputSpec {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub video_bitrate_kbps: u64,
    pub audio_bitrate_kbps: u64,
    pub max_duration_sec: u64,
    pub container: String,
    pub video_codec: String,
    pub audio_codec: String,
}

impl OutputSpec {
    pub fn from_params(params: &Value) -> Result<Self, VersionError> {
        let object = params.as_object().ok_or(VersionError::MissingSpec)?;
        let get_u64 = |name: &str| {
            object
                .get(name)
                .and_then(Value::as_u64)
                .ok_or_else(|| VersionError::BadSpec(format!("{name} 不是非负整数")))
        };
        let get_text = |name: &str| {
            object
                .get(name)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .ok_or_else(|| VersionError::BadSpec(format!("{name} 不是非空字符串")))
        };
        let spec = Self {
            width: u32::try_from(get_u64("width")?)
                .map_err(|_| VersionError::BadSpec("width 超出范围".into()))?,
            height: u32::try_from(get_u64("height")?)
                .map_err(|_| VersionError::BadSpec("height 超出范围".into()))?,
            fps: u32::try_from(get_u64("fps")?)
                .map_err(|_| VersionError::BadSpec("fps 超出范围".into()))?,
            video_bitrate_kbps: get_u64("video_bitrate_kbps")?,
            audio_bitrate_kbps: get_u64("audio_bitrate_kbps")?,
            max_duration_sec: get_u64("max_duration_sec")?,
            container: get_text("container")?.to_owned(),
            video_codec: get_text("video_codec")?.to_owned(),
            audio_codec: get_text("audio_codec")?.to_owned(),
        };
        if spec.width == 0 || spec.height == 0 || spec.fps == 0 {
            return Err(VersionError::BadSpec("宽、高、帧率必须是正数".into()));
        }
        Ok(spec)
    }

    /// 等比缩放 + 补边的缩放窗口：按「完整放进目标」的比例缩放（不放大大边），
    /// 空出的边由 `pad` 补黑。横屏素材进竖屏规格时上下留黑边，就是常见的邮箱模式。
    pub fn fit_scale_crop(&self, source_width: u32, source_height: u32) -> (u32, u32) {
        let width_scale = self.width as f64 / source_width.max(1) as f64;
        let height_scale = self.height as f64 / source_height.max(1) as f64;
        let scale = width_scale.min(height_scale);
        let width = ((source_width as f64 * scale).round() as u32)
            .max(1)
            .min(self.width);
        let height = ((source_height as f64 * scale).round() as u32)
            .max(1)
            .min(self.height);
        (width, height)
    }
}

/// 细微处理参数（配方 `params.subtle`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubtleParams {
    pub grain: u32,
    pub zoom_percent: f64,
    pub brightness: f64,
    pub saturation: f64,
    pub hue_degrees: f64,
    pub tempo: f64,
    pub pitch_semitones: f64,
    pub trim_head_frames: u32,
    pub trim_tail_frames: u32,
}

impl SubtleParams {
    pub fn from_params(params: &Value) -> Result<Self, VersionError> {
        let object = params
            .as_object()
            .ok_or_else(|| VersionError::BadSubtle("subtle 不是对象".into()))?;
        let number = |name: &str| {
            object
                .get(name)
                .and_then(Value::as_f64)
                .ok_or_else(|| VersionError::BadSubtle(format!("{name} 不是数字")))
        };
        let int = |name: &str| {
            let value = number(name)?;
            if value < 0.0 || value.fract() != 0.0 || value > u32::MAX as f64 {
                return Err(VersionError::BadSubtle(format!("{name} 不是非负整数")));
            }
            Ok(value as u32)
        };
        let params = Self {
            grain: int("grain")?,
            zoom_percent: number("zoom_percent")?,
            brightness: number("brightness")?,
            saturation: number("saturation")?,
            hue_degrees: number("hue_degrees")?,
            tempo: number("tempo")?,
            pitch_semitones: number("pitch_semitones")?,
            trim_head_frames: int("trim_head_frames")?,
            trim_tail_frames: int("trim_tail_frames")?,
        };
        if params.tempo <= 0.0 {
            return Err(VersionError::BadSubtle("tempo 必须是正数".into()));
        }
        Ok(params)
    }
}

/// 画质下限（配方 `params.quality`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityFloor {
    pub min_ssim: f64,
    pub min_psnr: f64,
}

impl QualityFloor {
    pub fn from_params(params: &Value) -> Result<Self, VersionError> {
        let object = params
            .as_object()
            .ok_or_else(|| VersionError::BadQuality("quality 不是对象".into()))?;
        let number = |name: &str| {
            object
                .get(name)
                .and_then(Value::as_f64)
                .ok_or_else(|| VersionError::BadQuality(format!("{name} 不是数字")))
        };
        let floor = Self {
            min_ssim: number("min_ssim")?,
            min_psnr: number("min_psnr")?,
        };
        // 下限本身必须讲得通。配方是服务端签的，正常不会出现这些值；但万一出现，
        // 负的 PSNR 下限会让任何画质都「通过」——这是朝放行方向失效，必须拦住。
        if !(floor.min_ssim > 0.0 && floor.min_ssim <= 1.0) {
            return Err(VersionError::BadQuality(
                "min_ssim 必须在 0 到 1 之间".into(),
            ));
        }
        if !(floor.min_psnr > 0.0 && floor.min_psnr.is_finite()) {
            return Err(VersionError::BadQuality("min_psnr 必须是正数".into()));
        }
        Ok(floor)
    }
}

/// 水印或 AI 标识放在哪个角。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Corner {
    #[default]
    TopRight,
    TopLeft,
    BottomLeft,
    BottomRight,
}

impl Corner {
    /// drawtext 的 x/y 表达式（留 24 像素边距；1080×1920 下和竞品角标位置一致）。
    pub fn expressions(self) -> (&'static str, &'static str) {
        match self {
            Corner::TopLeft => ("24", "24"),
            Corner::TopRight => ("w-tw-24", "24"),
            Corner::BottomLeft => ("24", "h-th-24"),
            Corner::BottomRight => ("w-tw-24", "h-th-24"),
        }
    }
}

/// 调色预设：五档固定参数（`eq` / `colorbalance`），界面下拉选择。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ColorPreset {
    #[default]
    Original,
    Warm,
    Cool,
    HighContrast,
    LowSaturation,
}

impl ColorPreset {
    /// 预设的滤镜串；`Original` 返回空（不加滤镜，保持原色）。
    pub fn filters(self) -> &'static str {
        match self {
            ColorPreset::Original => "",
            ColorPreset::Warm => "colorbalance=rs=0.06:gs=0.02:bs=-0.06",
            ColorPreset::Cool => "colorbalance=rs=-0.06:gs=0.02:bs=0.06",
            ColorPreset::HighContrast => "eq=contrast=1.12",
            ColorPreset::LowSaturation => "eq=saturation=0.85",
        }
    }
}

/// 第一层模板：看得见的内容。存剧目目录的 `版本模板.json`，界面可编辑。
///
/// 路径字段（片头、片尾、背景音乐、字幕）由调用方校验存在性；这里只做静态合法性检查。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionTemplate {
    /// 片头视频文件路径；`None` 表示没有片头。
    #[serde(default)]
    pub intro_path: Option<String>,
    /// 片尾视频文件路径。
    #[serde(default)]
    pub outro_path: Option<String>,
    /// 封面取自第 1 集的秒数。
    #[serde(default = "default_cover_at")]
    pub cover_at_sec: f64,
    /// 封面文字；`None` 不叠字。
    #[serde(default)]
    pub cover_text: Option<String>,
    /// 字幕目录（按集名对应 `.srt`）；`None` 不烧字幕。
    #[serde(default)]
    pub subtitle_dir: Option<String>,
    /// 字幕字号(输出分辨率的像素,1080×1920 下 56 像素)。
    /// 靠 `subtitles` 滤镜的 `PlayResX` / `PlayResY` 抵消 libass 的默认缩放,见滤镜图里的注释。
    #[serde(default = "default_subtitle_size")]
    pub subtitle_size: u32,
    /// 字幕离底边的距离(输出分辨率的像素),和 `subtitle_size` 同一套坐标。
    #[serde(default)]
    pub subtitle_margin_v: Option<u32>,
    #[serde(default)]
    pub color_preset: ColorPreset,
    /// 剧名水印文字（空串表示不放水印）。
    #[serde(default)]
    pub watermark_text: String,
    #[serde(default)]
    pub watermark_corner: Corner,
    /// 水印透明度（0.0–1.0）。
    #[serde(default = "default_opacity")]
    pub watermark_opacity: f64,
    /// 「本剧视频含 AI 生成内容」：显式标识 + 元数据隐式标识（GB 45438-2025）。
    #[serde(default)]
    pub ai_marked: bool,
    /// AI 标识里的生成合成服务提供者名称或编码（只允许附录 E 的字符集）。
    #[serde(default)]
    pub ai_content_producer: String,
    /// 重新划分分集（先跑分集重组再进本流程）。
    #[serde(default)]
    pub recut: bool,
    /// 重新分集的目标集数；`None` 表示和原来一样多。
    #[serde(default)]
    pub recut_count: Option<u32>,
    /// 背景音乐文件路径。
    #[serde(default)]
    pub music_path: Option<String>,
    /// 背景音乐音量（0.0–1.0，相对原声）。
    #[serde(default = "default_opacity")]
    pub music_volume: f64,
    /// 用户确认有这段音乐的使用授权（不勾就不生成）。
    #[serde(default)]
    pub music_licensed: bool,
}

fn default_cover_at() -> f64 {
    1.0
}
fn default_subtitle_size() -> u32 {
    56
}
fn default_opacity() -> f64 {
    0.5
}

impl Default for VersionTemplate {
    fn default() -> Self {
        Self {
            intro_path: None,
            outro_path: None,
            cover_at_sec: default_cover_at(),
            cover_text: None,
            subtitle_dir: None,
            subtitle_size: default_subtitle_size(),
            subtitle_margin_v: None,
            color_preset: ColorPreset::Original,
            watermark_text: String::new(),
            watermark_corner: Corner::TopRight,
            watermark_opacity: default_opacity(),
            ai_marked: false,
            ai_content_producer: String::new(),
            recut: false,
            recut_count: None,
            music_path: None,
            music_volume: default_opacity(),
            music_licensed: false,
        }
    }
}

impl VersionTemplate {
    /// 静态校验：不碰文件系统就能发现的错误在这里报出来。
    pub fn validate(&self) -> Result<(), VersionError> {
        if self.cover_at_sec < 0.0 || !self.cover_at_sec.is_finite() {
            return Err(VersionError::BadTemplate("封面取帧秒数不能是负数".into()));
        }
        if self.subtitle_size == 0 {
            return Err(VersionError::BadTemplate("字幕字号必须大于 0".into()));
        }
        let unit = |name: &str, value: f64| {
            if !(0.0..=1.0).contains(&value) {
                Err(VersionError::BadTemplate(format!(
                    "{name} 必须在 0 到 1 之间"
                )))
            } else {
                Ok(())
            }
        };
        unit("水印透明度", self.watermark_opacity)?;
        unit("背景音乐音量", self.music_volume)?;
        if let Some(margin) = self.subtitle_margin_v {
            if margin == 0 {
                return Err(VersionError::BadTemplate("字幕垂直边距必须大于 0".into()));
            }
        }
        if self.ai_marked && !is_ai_producer_chars(&self.ai_content_producer) {
            return Err(VersionError::BadTemplate(
                "AI 生成合成服务提供者名称只能用字母、数字和常见英文符号（GB 45438 附录 E 的字符集）".into(),
            ));
        }
        // AI 标识固定在右下角；剧名水印和它叠在一起就看不清了（计划 2.8 节）。
        if self.ai_marked
            && !self.watermark_text.is_empty()
            && self.watermark_corner == Corner::BottomRight
        {
            return Err(VersionError::BadTemplate(
                "勾选了 AI 标识时，剧名水印不能放在右下角（那里是 AI 标识的位置）".into(),
            ));
        }
        if self.music_path.is_some() && !self.music_licensed {
            return Err(VersionError::BadTemplate(
                "使用背景音乐前要先勾选「我有这段音乐的使用授权」".into(),
            ));
        }
        if self.recut && self.recut_count == Some(0) {
            return Err(VersionError::BadTemplate("重新分集的集数必须大于 0".into()));
        }
        Ok(())
    }

    /// 是否要在画面上写字（水印、AI 标识）：要写字就必须有中文字体。
    pub fn needs_font(&self) -> bool {
        !self.watermark_text.is_empty()
            || self.ai_marked
            || self
                .cover_text
                .as_deref()
                .is_some_and(|text| !text.is_empty())
    }
}

/// GB 45438-2025 附录 E j)：各要素的值由 0x21、0x23–0x5B、0x5D–0x7E 以及转义的 `"` 构成。
/// 本程序不写入 `"` 和 `\`，所以只要落在可见 ASCII 范围且不含这两个字符即可。
fn is_ai_producer_chars(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| (0x21..=0x7e).contains(&byte) && byte != b'"' && byte != b'\\')
}

/// 一集的输入（探测规格 + 字幕文件）。
#[derive(Debug, Clone, PartialEq)]
pub struct EpisodeInput<'a> {
    /// 分集文件的完整路径。
    pub path: &'a str,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration_sec: f64,
    /// 有没有音轨：没有的话补一段静音，保证每集都有声音轨道（拼接和平台都要求）。
    pub has_audio: bool,
    /// 对应的字幕文件（`.srt`）；`None` 不烧字幕。
    pub subtitle_path: Option<&'a str>,
}

/// 片头或片尾（调用方探测后传入）。
#[derive(Debug, Clone, PartialEq)]
pub struct ClipInput<'a> {
    pub path: &'a str,
    pub duration_sec: f64,
    pub has_audio: bool,
}

/// 模板里路径字段的探测结果 + 解析好的字体。和模板分开传：字体是本机解析出来的，
/// 不进任务哈希（换台电脑字体路径不同，但内容一样）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GraphExtras<'a> {
    pub intro: Option<ClipInput<'a>>,
    pub outro: Option<ClipInput<'a>>,
    /// 背景音乐文件（模板勾了授权才会传进来）。
    pub music: Option<&'a str>,
    /// 中文字体文件；模板要写字时必须有（[`VersionTemplate::needs_font`]）。
    pub font: Option<&'a str>,
}

/// 一集的 ffmpeg 参数（不含编码器参数：调用方按 caps 选择后拼接）。
#[derive(Debug, Clone, PartialEq)]
pub struct VersionGraph {
    /// 全部输入参数（`-i …`，背景音乐带 `-stream_loop -1`）。
    pub input_args: Vec<String>,
    /// `-filter_complex` 的值；画质统计文件是占位符，用 [`fill_stats_paths`] 换成真实路径。
    pub filter_complex: String,
    /// 输出映射（`-map [vout] -map [aout]`）。
    pub map_args: Vec<String>,
    /// 元数据：`-metadata` 的键值对（AI 隐式标识等）。非空时编码要加 `use_metadata_tags`。
    pub metadata: Vec<(String, String)>,
    /// 预计输出时长（秒，含片头片尾）：进度显示和规格上限检查用。
    pub output_duration_sec: f64,
    /// 输出文件名（和源同名，后缀换成输出容器）。
    pub output_name: String,
}

/// 画质统计文件占位符。
pub const SSIM_STATS_PLACEHOLDER: &str = "__SSIM_STATS__";
pub const PSNR_STATS_PLACEHOLDER: &str = "__PSNR_STATS__";

/// 把滤镜图里的统计文件占位符换成真实路径（路径会按滤镜参数规则转义，Windows 盘符的 `:` 也安全）。
pub fn fill_stats_paths(filter_complex: &str, ssim: &str, psnr: &str) -> String {
    filter_complex
        .replace(SSIM_STATS_PLACEHOLDER, &escape_filter_path(ssim))
        .replace(PSNR_STATS_PLACEHOLDER, &escape_filter_path(psnr))
}

/// 统一音频格式：拼接（concat）和混音（amix）要求各路采样率、声道一致。
const AUDIO_FORMAT: &str = "aformat=sample_fmts=fltp:sample_rates=48000:channel_layouts=stereo";

/// 给一集生成 ffmpeg 参数。纯函数：同一模板 + 同一配方 + 同一集，两次生成的结果完全相同。
///
/// `-filter_complex` 布局（同一个滤镜标签只能被消费一次，要复用的先 `split`）：
/// ```text
/// [0:v] 几何（裁帧、变速、缩放补边、放大裁边） → split → [geo_out] [geo_ref]
/// [geo_out] 颗粒 / 细微调色 / 调色预设 → split=3 → [q_main] [q_a] [q_b]
/// [geo_ref] split → [ref_a] [ref_b]
/// [q_a][ref_a] ssim → nullsink；[q_b][ref_b] psnr → nullsink
/// [q_main] 字幕 → [main_v]；（片头）[main_v]（片尾）concat → 水印、AI 标识 → [vout]
/// [0:a]（或静音）裁帧、变调、变速 → [main_a] →（背景音乐 amix）→ concat → [aout]
/// ```
/// 画质对比放在颗粒和调色之后、写字之前：字幕、水印、AI 标识是有意加的内容，不算画质损失；
/// 编码损失在滤镜图里量不到（滤镜图看到的是编码前的画面），由输出规格的码率上限保证。
pub fn build_version_graph(
    template: &VersionTemplate,
    spec: &OutputSpec,
    subtle: &SubtleParams,
    episode: &EpisodeInput,
    extras: &GraphExtras,
    seed: u64,
) -> Result<VersionGraph, VersionError> {
    template.validate()?;
    if episode.width == 0 || episode.height == 0 || episode.fps <= 0.0 {
        return Err(VersionError::BadSpec(
            "分集规格不合法（宽、高、帧率）".into(),
        ));
    }
    if template.needs_font() && extras.font.is_none() {
        return Err(VersionError::BadTemplate(
            "画面上要写字（水印、AI 标识或封面文字），但没有找到中文字体；请在设置里指定字体文件"
                .into(),
        ));
    }
    let font = extras.font.map(font_option).unwrap_or_default();

    // ---- 时间：裁掉片头片尾几帧，再整体变速 ----
    let fps = episode.fps;
    let head = subtle.trim_head_frames as f64 / fps;
    let tail = subtle.trim_tail_frames as f64 / fps;
    let keep = episode.duration_sec - head - tail;
    if keep <= 0.5 {
        return Err(VersionError::BadSpec(format!(
            "分集太短（{:.1} 秒），裁掉首尾帧后没有内容",
            episode.duration_sec
        )));
    }
    let tempo = subtle.tempo;
    let main_duration = keep / tempo;
    let intro_duration = extras.intro.as_ref().map_or(0.0, |clip| clip.duration_sec);
    let outro_duration = extras.outro.as_ref().map_or(0.0, |clip| clip.duration_sec);
    let output_duration = intro_duration + main_duration + outro_duration;
    if output_duration > spec.max_duration_sec as f64 {
        return Err(VersionError::BadSpec(format!(
            "处理后单集时长 {:.0} 秒，超过输出规格上限 {} 秒（含片头片尾）",
            output_duration, spec.max_duration_sec
        )));
    }

    // ---- 输入 ----
    let mut input_args = vec!["-i".to_string(), episode.path.to_string()];
    let mut next_input = 1usize;
    let mut add_input = |args: &mut Vec<String>, path: &str, looped: bool| {
        if looped {
            args.push("-stream_loop".into());
            args.push("-1".into());
        }
        args.push("-i".into());
        args.push(path.to_string());
        let index = next_input;
        next_input += 1;
        index
    };
    let intro_index = extras
        .intro
        .as_ref()
        .map(|clip| add_input(&mut input_args, clip.path, false));
    let outro_index = extras
        .outro
        .as_ref()
        .map(|clip| add_input(&mut input_args, clip.path, false));
    let music_index = extras
        .music
        .map(|path| add_input(&mut input_args, path, true));

    let (w, h) = (spec.width, spec.height);
    let mut chains: Vec<String> = Vec::new();

    // ---- 几何链（输出和画质参考共用） ----
    // 放大裁边：在补好边的目标画面上放大 zoom 倍再从中心裁回，尺寸取偶数（yuv420p 要求）。
    let (fit_w, fit_h) = spec.fit_scale_crop(episode.width, episode.height);
    let zoom = (1.0 + subtle.zoom_percent / 100.0).max(1.0);
    let zoomed_w = even(((w as f64 * zoom).round() as u32).max(w));
    let zoomed_h = even(((h as f64 * zoom).round() as u32).max(h));
    let geometry = format!(
        "[0:v]trim=start={head:.6}:duration={keep:.6},setpts=(PTS-STARTPTS)/{tempo:.6},\
         fps={fps_out},scale={fit_w}:{fit_h}:flags=lanczos,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,\
         scale={zoomed_w}:{zoomed_h}:flags=lanczos,crop={w}:{h},setsar=1,format=yuv420p,\
         split=2[geo_out][geo_ref]",
        fps_out = spec.fps,
    );
    chains.push(geometry);

    // ---- 细微处理（颗粒带种子：同一配方两次跑出的画面一样） ----
    let mut treat: Vec<String> = Vec::new();
    if subtle.grain > 0 {
        treat.push(format!(
            "noise=alls={}:allf=t+u:all_seed={}",
            subtle.grain,
            seed % i32::MAX as u64
        ));
    }
    treat.push(format!(
        "eq=brightness={:.4}:saturation={:.4}",
        subtle.brightness, subtle.saturation
    ));
    if subtle.hue_degrees != 0.0 {
        treat.push(format!("hue=h={:.4}", subtle.hue_degrees));
    }
    chains.push(format!(
        "[geo_out]{},split=3[q_main][q_a][q_b]",
        treat.join(",")
    ));
    chains.push("[geo_ref]split=2[ref_a][ref_b]".to_string());
    // 调色预设属于第一层「看得见的内容」(和片头片尾、字幕、水印同类),是有意做的改变,
    // 不是「处理过头」,所以加在画质对比之后、只作用于输出这一路。
    // 放进对比里量的话,光一个「暖色」预设就能在合成素材上把 SSIM 压到 0.89 以下,
    // 用户选了预设反而整剧都交付不了。
    let graded = if template.color_preset.filters().is_empty() {
        "[q_main]".to_string()
    } else {
        chains.push(format!(
            "[q_main]{}[q_graded]",
            template.color_preset.filters()
        ));
        "[q_graded]".to_string()
    };
    chains.push(format!(
        "[q_a][ref_a]ssim=stats_file={SSIM_STATS_PLACEHOLDER},nullsink"
    ));
    chains.push(format!(
        "[q_b][ref_b]psnr=stats_file={PSNR_STATS_PLACEHOLDER},nullsink"
    ));

    // ---- 字幕(在本集时间线上烧;字幕时间是原片时间,先把时间戳临时换回去) ----
    let main_v = if let Some(subtitle) = episode.subtitle_path {
        let fonts_dir = extras
            .font
            .and_then(parent_dir)
            .map(|dir| format!(":fontsdir={}", escape_filter_path(dir)))
            .unwrap_or_default();
        // `PlayResX` / `PlayResY` 必须显式给出,否则 `Fontsize` 不是像素:
        // libass 把字幕脚本的坐标按「画面高 / PlayResY」缩放,而 srt 转 ass 用的是默认
        // PlayRes(384×288)。1920 高的画面会因此把字号放大 6.7 倍,默认 56 的字幕变成
        // 两百多像素高、还换行,盖住画面中部。声明成输出规格后缩放系数是 1,
        // `Fontsize` 和 `MarginV` 都按输出分辨率的像素算,和界面上「字幕字号(像素)」
        // 「字幕离底边(像素)」的说法一致。字幕加在这一步时,几何链已经把画面缩到了输出规格。
        chains.push(format!(
            "{}setpts=PTS*{tempo:.6}+{head:.6}/TB,subtitles=filename={}{fonts_dir}:\
                 force_style='Fontsize={},Outline=2,PlayResX={},PlayResY={}{}',\
                 setpts=(PTS-{head:.6}/TB)/{tempo:.6}[main_v]",
            graded,
            escape_filter_path(subtitle),
            template.subtitle_size,
            spec.width,
            spec.height,
            template
                .subtitle_margin_v
                .map(|margin| format!(",MarginV={margin}"))
                .unwrap_or_default(),
        ));
        "[main_v]".to_string()
    } else {
        // 没有字幕:输出这一路就是分流后(可能套过调色预设)的那一路。
        graded
    };

    // ---- 音频：裁帧、变调（asetrate 改音高同时改了速度，atempo 补回来）、变速 ----
    let mut audio = if episode.has_audio {
        format!(
            "[0:a]atrim=start={head:.6}:duration={keep:.6},asetpts=PTS-STARTPTS,aresample=48000"
        )
    } else {
        format!("anullsrc=r=48000:cl=stereo,atrim=duration={keep:.6}")
    };
    if subtle.pitch_semitones != 0.0 {
        // asetrate 只收整数采样率：先取整，再按取整后的实际倍率补偿速度，时长才对得上。
        let rate = (48000.0 * 2.0_f64.powf(subtle.pitch_semitones / 12.0)).round();
        let pitch = rate / 48000.0;
        audio.push_str(&format!(
            ",asetrate={rate:.0},aresample=48000,atempo={:.6}",
            tempo / pitch
        ));
    } else if (tempo - 1.0).abs() > f64::EPSILON {
        audio.push_str(&format!(",atempo={tempo:.6}"));
    }
    audio.push(',');
    audio.push_str(AUDIO_FORMAT);
    if let Some(index) = music_index {
        chains.push(format!("{audio}[voice]"));
        chains.push(format!(
            "[{index}:a]aresample=48000,{AUDIO_FORMAT},volume={},atrim=duration={main_duration:.6}[music]",
            format_short(template.music_volume)
        ));
        chains.push("[voice][music]amix=inputs=2:duration=first:normalize=0[main_a]".to_string());
    } else {
        chains.push(format!("{audio}[main_a]"));
    }

    // ---- 片头片尾：统一到输出规格再首尾拼接；没有音轨的补静音 ----
    let mut segments: Vec<(String, String)> = Vec::new();
    let clip_chain = |chains: &mut Vec<String>, label: &str, index: usize, clip: &ClipInput| {
        chains.push(format!(
            "[{index}:v]scale={w}:{h}:force_original_aspect_ratio=decrease:flags=lanczos,\
             pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,fps={},setsar=1,format=yuv420p[{label}_v]",
            spec.fps
        ));
        if clip.has_audio {
            chains.push(format!(
                "[{index}:a]aresample=48000,{AUDIO_FORMAT}[{label}_a]"
            ));
        } else {
            chains.push(format!(
                "anullsrc=r=48000:cl=stereo,atrim=duration={:.6},{AUDIO_FORMAT}[{label}_a]",
                clip.duration_sec
            ));
        }
        (format!("[{label}_v]"), format!("[{label}_a]"))
    };
    if let (Some(index), Some(clip)) = (intro_index, extras.intro.as_ref()) {
        segments.push(clip_chain(&mut chains, "intro", index, clip));
    }
    segments.push((main_v.to_string(), "[main_a]".to_string()));
    if let (Some(index), Some(clip)) = (outro_index, extras.outro.as_ref()) {
        segments.push(clip_chain(&mut chains, "outro", index, clip));
    }

    // ---- 写字：剧名水印、AI 显式标识（拼接之后加，「起始画面」就是成片的开头） ----
    let mut overlays: Vec<String> = Vec::new();
    if !template.watermark_text.is_empty() {
        let (x, y) = template.watermark_corner.expressions();
        overlays.push(format!(
            "drawtext=expansion=none:text={}:x={x}:y={y}:fontsize={}:\
             fontcolor=white@{}:borderw=2:bordercolor=black@0.6{font}",
            escape_filter_text(&template.watermark_text),
            text_height(spec, 0.04),
            format_short(template.watermark_opacity),
        ));
    }
    if template.ai_marked {
        // GB 45438-2025 5.4：位于边或角；起始画面持续不少于 2 秒（这里 3 秒）；
        // 字高不低于短边的 5%（1080 宽取 60 像素）。之后同一位置常驻「AI生成」，字高同样达标。
        let (x, y) = Corner::BottomRight.expressions();
        let size = text_height(spec, 0.055);
        overlays.push(format!(
            "drawtext=expansion=none:text={}:x={x}:y={y}:fontsize={size}:fontcolor=white:\
             borderw=3:bordercolor=black@0.8:enable='between(t,0,3)'{font}",
            escape_filter_text("人工智能生成合成"),
        ));
        overlays.push(format!(
            "drawtext=expansion=none:text={}:x={x}:y={y}:fontsize={size}:fontcolor=white@0.85:\
             borderw=2:bordercolor=black@0.6:enable='gt(t,3)'{font}",
            escape_filter_text("AI生成"),
        ));
    }

    let (video_tail, audio_label) = if segments.len() == 1 {
        let (video, audio) = segments.remove(0);
        (video, audio)
    } else {
        let count = segments.len();
        let inputs: String = segments
            .iter()
            .map(|(video, audio)| format!("{video}{audio}"))
            .collect();
        chains.push(format!("{inputs}concat=n={count}:v=1:a=1[cat_v][cat_a]"));
        ("[cat_v]".to_string(), "[cat_a]".to_string())
    };
    if overlays.is_empty() {
        chains.push(format!("{video_tail}null[vout]"));
    } else {
        chains.push(format!("{video_tail}{}[vout]", overlays.join(",")));
    }
    chains.push(format!("{audio_label}anull[aout]"));

    // ---- 元数据（AI 隐式标识，GB 45438-2025 附录 E；首次写入时传播方与制作方相同） ----
    let mut metadata = Vec::new();
    if template.ai_marked {
        let producer = &template.ai_content_producer;
        let produce_id = deterministic_produce_id(seed, file_name(episode.path));
        metadata.push((
            "AIGC".to_string(),
            format!(
                "{{\"AIGC\":{{\"Label\":\"1\",\"ContentProducer\":\"{producer}\",\
                 \"ProduceID\":\"{produce_id}\",\"ReservedCode1\":\"\",\
                 \"ContentPropagator\":\"{producer}\",\"PropagateID\":\"{produce_id}\",\
                 \"ReservedCode2\":\"\"}}}}"
            ),
        ));
    }

    let name = file_name(episode.path);
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    Ok(VersionGraph {
        input_args,
        filter_complex: chains.join(";"),
        map_args: vec![
            "-map".into(),
            "[vout]".into(),
            "-map".into(),
            "[aout]".into(),
        ],
        metadata,
        output_duration_sec: output_duration,
        output_name: format!("{stem}.{}", spec.container),
    })
}

/// 封面：第 1 集某一秒的画面，统一到输出规格，可叠封面文字，输出 JPEG。
/// 返回完整的 ffmpeg 参数（含 `-y` 和输出路径）。
pub fn build_cover_args(
    template: &VersionTemplate,
    spec: &OutputSpec,
    episode_path: &str,
    font: Option<&str>,
    output: &str,
) -> Result<Vec<String>, VersionError> {
    let (w, h) = (spec.width, spec.height);
    let mut filter = format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease:flags=lanczos,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1"
    );
    if let Some(text) = template
        .cover_text
        .as_deref()
        .filter(|text| !text.is_empty())
    {
        let font =
            font.ok_or_else(|| VersionError::BadTemplate("封面要写字，但没有找到中文字体".into()))?;
        filter.push_str(&format!(
            ",drawtext=expansion=none:text={}:x=(w-tw)/2:y=h*0.72:fontsize={}:fontcolor=white:\
             borderw=4:bordercolor=black@0.8{}",
            escape_filter_text(text),
            text_height(spec, 0.09),
            font_option(font),
        ));
    }
    Ok(vec![
        "-y".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-ss".into(),
        format!("{:.3}", template.cover_at_sec),
        "-i".into(),
        episode_path.to_string(),
        "-frames:v".into(),
        "1".into(),
        "-vf".into(),
        filter,
        "-q:v".into(),
        "2".into(),
        output.to_string(),
    ])
}

/// 字高：短边的若干比例，向上取整。
fn text_height(spec: &OutputSpec, ratio: f64) -> u32 {
    (spec.width.min(spec.height) as f64 * ratio).ceil() as u32
}

fn even(value: u32) -> u32 {
    value + value % 2
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn parent_dir(path: &str) -> Option<&str> {
    let index = path.rfind(['/', '\\'])?;
    Some(&path[..index.max(1)])
}

/// `format_short`：数字渲染成 ffmpeg 认识的短形式（`0.5` 而不是 `0.5000`）。
fn format_short(value: f64) -> String {
    let text = format!("{value:.4}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn font_option(path: &str) -> String {
    format!(":fontfile={}", escape_filter_path(path))
}

/// 滤镜参数值转义（路径形态：`:[]'` 和 `\` 都要活过两层解析）。
fn escape_filter_path(text: &str) -> String {
    escape_filter_text(text)
}

fn escape_filter_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        match ch {
            ':' => out.push_str("\\\\:"),
            ';' | ',' | '[' | ']' => {
                out.push('\\');
                out.push(ch);
            }
            '\'' => out.push_str("\\\\\\'"),
            '\\' => out.push_str("\\\\\\\\"),
            _ => out.push(ch),
        }
    }
    out
}

/// 一集的内容制作编号（附录 E：服务提供者对内容的唯一编号）。
/// 同一任务（同种子）里每集不同、可复现：取种子和文件名的 SHA-256 前 32 个十六进制位。
fn deterministic_produce_id(seed: u64, name: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(seed.to_be_bytes());
    hasher.update(name.as_bytes());
    hasher
        .finalize()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 画质检测结果（`ssim` / `psnr` 滤镜的 stats 文件解析结果）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QualityResult {
    pub ssim: f64,
    pub psnr: f64,
}

impl QualityResult {
    /// 是否通过配方下限。
    pub fn passes(&self, floor: &QualityFloor) -> bool {
        self.ssim >= floor.min_ssim && self.psnr >= floor.min_psnr
    }

    /// 解析 `stats_file` 的汇总行。两个滤镜各写一个文件，各取最后一行的 `All:` / `psnr_avg`。
    ///
    /// 行格式（本机 ffmpeg 6.1 实测）：
    /// `n:1 Y:0.457700 U:0.562953 V:0.611528 All:0.500880 (3.017950)`
    /// `n:1 mse_avg:122.55 ... psnr_avg:27.25 ...`
    pub fn from_stats(ssim_stats: &str, psnr_stats: &str) -> Result<Self, VersionError> {
        let ssim = last_field(ssim_stats, "All")
            .ok_or_else(|| VersionError::BadQuality("SSIM 统计里没有 All 字段".into()))?;
        let psnr = last_field(psnr_stats, "psnr_avg")
            .ok_or_else(|| VersionError::BadQuality("PSNR 统计里没有 psnr_avg 字段".into()))?;
        if !(0.0..=1.0).contains(&ssim) {
            return Err(VersionError::BadQuality(format!("SSIM 超出范围：{ssim}")));
        }
        Ok(Self { ssim, psnr })
    }
}

/// 取统计文件最后一行里 `字段:值` 的值。
fn last_field(stats: &str, field: &str) -> Option<f64> {
    let pattern = format!("{field}:");
    stats.lines().rev().find_map(|line| {
        let position = line.find(&pattern)?;
        let rest = &line[position + pattern.len()..];
        let value: String = rest
            .chars()
            .take_while(|ch| ch.is_ascii_digit() || *ch == '.' || *ch == '-')
            .collect();
        value.parse().ok()
    })
}

/// `版本说明.json`：一次任务的整体结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionReport {
    /// 生成完成的时间。
    pub generated_at: String,
    /// 任务哈希（配方绑定的）。
    pub task: String,
    /// 档位。
    pub strength: String,
    /// 配方编号。
    pub recipe_id: String,
    /// 配方种子（参数由它决定）。
    pub seed: u64,
    /// 输出规格。
    pub spec: OutputSpec,
    /// 第二层细微处理参数（来自配方，原样记下便于复核）。
    pub subtle: SubtleParams,
    /// 画质下限。
    pub quality_floor: QualityFloor,
    /// 权属材料 SHA-256。
    pub rights_sha256: String,
    /// 第一层模板的规范化哈希。
    pub template_sha256: String,
    /// 背景音乐文件 SHA-256（没用背景音乐时为空）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub music_sha256: Option<String>,
    /// 是否加了 AI 显式和隐式标识。
    pub ai_marked: bool,
    /// 每集结果。
    pub episodes: Vec<EpisodeReport>,
}

/// 一集的结果记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpisodeReport {
    pub name: String,
    /// 画质分数；`None` 表示没做检测（不启用检测的场景）。
    pub quality: Option<QualityResult>,
    /// 实际使用的编码器。
    #[serde(default)]
    pub encoder: String,
    /// 是否改用了软件编码（硬件失败时的显式记录，不静默降级）。
    pub software_encode: bool,
    /// 输出文件名（含容器后缀）。
    pub output_name: String,
    /// 输出文件 SHA-256。
    #[serde(default)]
    pub sha256: String,
}

/// `processing.jsonl` 的单条记录（和 `ledger.jsonl` 同目录；追加写）。
///
/// 只记哈希、档位、种子、画质结果和产物路径，不记剧名、人物、联系方式：
/// 台账是本地文件，但日志纪律和报审包一致。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessingRecord {
    /// 记录时间（本地时区，ISO-8601 带时区偏移）。
    pub ts: String,
    /// 事件类型：`version`（处理版本生成）、`rights`（权属声明）、`music`（背景音乐授权）、
    /// `erase`（M4b-2 的擦除确认）。
    pub event: String,
    /// 任务哈希（事件针对的任务；权属声明用权属材料哈希）。
    pub task: String,
    /// 档位（`version` 事件）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strength: Option<String>,
    /// 配方种子。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    /// 每集画质（`version` 事件）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<Vec<QualityResult>>,
    /// 产物路径。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// 事件涉及的文件哈希（`music` 事件：背景音乐文件）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// 权属材料声明（剧目目录 `.dossier/rights.json`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RightsDeclaration {
    /// 声明时间。
    pub declared_at: String,
    /// 权属材料文件路径（本机）。
    pub file: String,
    /// 材料内容 SHA-256。
    pub sha256: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FONT: &str = "/字体/NotoSansCJK-Regular.ttc";

    fn spec() -> OutputSpec {
        OutputSpec::from_params(&json!({
            "width": 1080, "height": 1920, "fps": 30,
            "video_bitrate_kbps": 5000, "audio_bitrate_kbps": 128,
            "max_duration_sec": 900, "container": "mp4",
            "video_codec": "h264", "audio_codec": "aac",
        }))
        .unwrap()
    }

    fn subtle() -> SubtleParams {
        SubtleParams::from_params(&json!({
            "grain": 4, "zoom_percent": 2.0, "brightness": 0.01, "saturation": 1.02,
            "hue_degrees": 1.5, "tempo": 1.01, "pitch_semitones": 0.2,
            "trim_head_frames": 2, "trim_tail_frames": 2,
        }))
        .unwrap()
    }

    fn sample_episode() -> EpisodeInput<'static> {
        EpisodeInput {
            path: "/素材/剧A/第01集.mp4",
            width: 1080,
            height: 1920,
            fps: 30.0,
            duration_sec: 60.0,
            has_audio: true,
            subtitle_path: None,
        }
    }

    fn graph(template: &VersionTemplate, extras: &GraphExtras, seed: u64) -> VersionGraph {
        build_version_graph(
            template,
            &spec(),
            &subtle(),
            &sample_episode(),
            extras,
            seed,
        )
        .unwrap()
    }

    fn with_font() -> GraphExtras<'static> {
        GraphExtras {
            font: Some(FONT),
            ..GraphExtras::default()
        }
    }

    /// 每个 `[标签]` 作为输出只出现一次、作为输入只消费一次（ffmpeg 的硬性要求）。
    fn assert_labels_consumed_once(filter: &str) {
        use std::collections::HashMap;
        let mut produced: HashMap<String, usize> = HashMap::new();
        let mut consumed: HashMap<String, usize> = HashMap::new();
        for chain in filter.split(';') {
            let mut rest = chain;
            // 链首的输入标签。
            while let Some(stripped) = rest.strip_prefix('[') {
                let end = stripped.find(']').unwrap();
                *consumed.entry(stripped[..end].to_string()).or_default() += 1;
                rest = &stripped[end + 1..];
            }
            // 链尾的输出标签。
            let mut tail = chain;
            while tail.ends_with(']') {
                let start = tail.rfind('[').unwrap();
                *produced
                    .entry(tail[start + 1..tail.len() - 1].to_string())
                    .or_default() += 1;
                tail = &tail[..start];
            }
        }
        for (label, count) in &produced {
            assert_eq!(*count, 1, "{label} 被产出 {count} 次");
            if label != "vout" && label != "aout" {
                assert_eq!(consumed.get(label), Some(&1), "{label} 应被消费恰好一次");
            }
        }
        for label in consumed.keys() {
            let is_input = label.contains(':');
            assert!(is_input || produced.contains_key(label), "{label} 没有来源");
        }
    }

    #[test]
    fn spec_and_subtle_parse_from_recipe_params() {
        assert_eq!(spec().video_bitrate_kbps, 5000);
        assert_eq!(subtle().grain, 4);
        assert!(OutputSpec::from_params(&json!({"width": 1080})).is_err());
        assert!(SubtleParams::from_params(&json!({"grain": "很多"})).is_err());
        assert!(SubtleParams::from_params(&json!({
            "grain": 4, "zoom_percent": 2.0, "brightness": 0.0, "saturation": 1.0,
            "hue_degrees": 0.0, "tempo": 0.0, "pitch_semitones": 0.0,
            "trim_head_frames": 0, "trim_tail_frames": 0,
        }))
        .is_err());
    }

    #[test]
    fn fit_scale_keeps_ratio_and_covers_target() {
        assert_eq!(spec().fit_scale_crop(1080, 1920), (1080, 1920));
        // 横屏素材按宽度放进竖屏：高 = 1080 × (1080/1920) = 607.5 → 608，上下补黑边。
        assert_eq!(spec().fit_scale_crop(1920, 1080), (1080, 608));
    }

    #[test]
    fn graph_is_deterministic_snapshot() {
        let template = VersionTemplate::default();
        let a = graph(&template, &GraphExtras::default(), 42);
        let b = graph(&template, &GraphExtras::default(), 42);
        assert_eq!(a, b, "同一输入两次生成必须完全相同");
        assert_eq!(a.output_name, "第01集.mp4");
        assert_eq!(a.input_args, vec!["-i", "/素材/剧A/第01集.mp4"]);
        assert!(a.filter_complex.starts_with("[0:v]trim=start=0.066667"));
        assert!(a
            .filter_complex
            .contains("noise=alls=4:allf=t+u:all_seed=42"));
        assert!(a
            .filter_complex
            .contains("eq=brightness=0.0100:saturation=1.0200"));
        assert!(a.filter_complex.contains("hue=h=1.5000"));
        assert!(a
            .filter_complex
            .contains("ssim=stats_file=__SSIM_STATS__,nullsink"));
        assert!(a
            .filter_complex
            .contains("psnr=stats_file=__PSNR_STATS__,nullsink"));
        assert_eq!(a.map_args, vec!["-map", "[vout]", "-map", "[aout]"]);
        assert!(a.metadata.is_empty());
        // (60 - 4/30) / 1.01
        assert!((a.output_duration_sec - 59.273927).abs() < 1e-5);
        assert_labels_consumed_once(&a.filter_complex);
    }

    #[test]
    fn watermark_and_ai_marks_end_up_in_graph() {
        let template = VersionTemplate {
            watermark_text: "剧名水印".into(),
            watermark_corner: Corner::TopLeft,
            ..VersionTemplate::default()
        };
        let g = graph(&template, &with_font(), 1);
        assert!(g.filter_complex.contains("x=24:y=24"));
        assert!(g.filter_complex.contains("text=剧名水印"));
        assert!(g
            .filter_complex
            .contains("fontfile=/字体/NotoSansCJK-Regular.ttc"));
        assert_labels_consumed_once(&g.filter_complex);

        let template = VersionTemplate {
            ai_marked: true,
            ai_content_producer: "drama-dossier".into(),
            ..template
        };
        let g = graph(&template, &with_font(), 7);
        assert!(g.filter_complex.contains("text=人工智能生成合成"));
        assert!(g.filter_complex.contains("text=AI生成"));
        assert!(g.filter_complex.contains("between(t,0,3)"));
        // 字高不低于短边 5%：1080 × 0.055 = 59.4 → 60。
        assert!(g.filter_complex.contains("fontsize=60"));
        assert_eq!(g.metadata.len(), 1);
        assert_eq!(g.metadata[0].0, "AIGC");
        let value: Value = serde_json::from_str(&g.metadata[0].1).unwrap();
        let aigc = value.get("AIGC").unwrap();
        assert_eq!(aigc["Label"], json!("1"));
        assert_eq!(aigc["ContentProducer"], json!("drama-dossier"));
        assert_eq!(aigc["ProduceID"], aigc["PropagateID"]);
        assert_eq!(aigc["ContentPropagator"], json!("drama-dossier"));
        assert_eq!(aigc.as_object().unwrap().len(), 7);

        let again = graph(&template, &with_font(), 7);
        assert_eq!(g.metadata[0].1, again.metadata[0].1);
        let other = graph(&template, &with_font(), 8);
        assert_ne!(g.metadata[0].1, other.metadata[0].1);
    }

    #[test]
    fn text_without_font_is_refused() {
        let template = VersionTemplate {
            watermark_text: "剧名".into(),
            ..VersionTemplate::default()
        };
        let error = build_version_graph(
            &template,
            &spec(),
            &subtle(),
            &sample_episode(),
            &GraphExtras::default(),
            1,
        )
        .unwrap_err();
        assert_eq!(error.code(), "bad_template");
    }

    #[test]
    fn ai_mark_corner_is_reserved() {
        let template = VersionTemplate {
            watermark_text: "剧名".into(),
            watermark_corner: Corner::BottomRight,
            ai_marked: true,
            ai_content_producer: "tool".into(),
            ..VersionTemplate::default()
        };
        assert!(template.validate().is_err());
        let template = VersionTemplate {
            ai_marked: false,
            ..template
        };
        assert!(template.validate().is_ok());
    }

    #[test]
    fn intro_outro_and_music_are_concatenated_and_mixed() {
        let template = VersionTemplate {
            music_path: Some("/音乐/bgm.mp3".into()),
            music_licensed: true,
            music_volume: 0.3,
            ..VersionTemplate::default()
        };
        let extras = GraphExtras {
            intro: Some(ClipInput {
                path: "/片头.mp4",
                duration_sec: 3.0,
                has_audio: false,
            }),
            outro: Some(ClipInput {
                path: "/片尾.mp4",
                duration_sec: 2.0,
                has_audio: true,
            }),
            music: Some("/音乐/bgm.mp3"),
            font: None,
        };
        let g = graph(&template, &extras, 1);
        assert_eq!(
            g.input_args,
            vec![
                "-i",
                "/素材/剧A/第01集.mp4",
                "-i",
                "/片头.mp4",
                "-i",
                "/片尾.mp4",
                "-stream_loop",
                "-1",
                "-i",
                "/音乐/bgm.mp3",
            ]
        );
        assert!(g
            .filter_complex
            .contains("concat=n=3:v=1:a=1[cat_v][cat_a]"));
        // 片头没有音轨：补静音。
        assert!(g
            .filter_complex
            .contains("anullsrc=r=48000:cl=stereo,atrim=duration=3.000000"));
        assert!(g.filter_complex.contains("[3:a]aresample=48000"));
        assert!(g.filter_complex.contains("volume=0.3"));
        assert!(g
            .filter_complex
            .contains("amix=inputs=2:duration=first:normalize=0"));
        assert!((g.output_duration_sec - (5.0 + 59.273927)).abs() < 1e-5);
        assert_labels_consumed_once(&g.filter_complex);
    }

    #[test]
    fn music_requires_license_and_duration_cap_holds() {
        let template = VersionTemplate {
            music_path: Some("/音乐/bgm.mp3".into()),
            ..VersionTemplate::default()
        };
        assert!(template.validate().is_err());

        let long = EpisodeInput {
            duration_sec: 1000.0,
            ..sample_episode()
        };
        let error = build_version_graph(
            &VersionTemplate::default(),
            &spec(),
            &subtle(),
            &long,
            &GraphExtras::default(),
            1,
        )
        .unwrap_err();
        assert_eq!(error.code(), "bad_spec");
    }

    #[test]
    fn subtitle_burn_and_audio_filter() {
        let template = VersionTemplate::default();
        let episode = EpisodeInput {
            subtitle_path: Some("/素材/剧A/字幕/第01集.srt"),
            ..sample_episode()
        };
        let g =
            build_version_graph(&template, &spec(), &subtle(), &episode, &with_font(), 1).unwrap();
        assert!(g
            .filter_complex
            .contains("subtitles=filename=/素材/剧A/字幕/第01集.srt"));
        assert!(g.filter_complex.contains("fontsdir=/字体"));
        assert!(g.filter_complex.contains("Fontsize=56,Outline=2"));
        // 变调：asetrate 改音高 → atempo 把速度补成 tempo。
        assert!(g
            .filter_complex
            .contains("asetrate=48558,aresample=48000,atempo=0.998394"));
        assert_labels_consumed_once(&g.filter_complex);

        // 没有变调、没有变速时音频只裁帧。
        let plain = SubtleParams::from_params(&json!({
            "grain": 0, "zoom_percent": 0.0, "brightness": 0.0, "saturation": 1.0,
            "hue_degrees": 0.0, "tempo": 1.0, "pitch_semitones": 0.0,
            "trim_head_frames": 0, "trim_tail_frames": 0,
        }))
        .unwrap();
        let g = build_version_graph(
            &template,
            &spec(),
            &plain,
            &sample_episode(),
            &GraphExtras::default(),
            1,
        )
        .unwrap();
        assert!(!g.filter_complex.contains("atempo"));
        assert!(!g.filter_complex.contains("noise="));
        assert_labels_consumed_once(&g.filter_complex);
    }

    #[test]
    fn silent_episode_gets_silence_track() {
        let episode = EpisodeInput {
            has_audio: false,
            ..sample_episode()
        };
        let g = build_version_graph(
            &VersionTemplate::default(),
            &spec(),
            &subtle(),
            &episode,
            &GraphExtras::default(),
            1,
        )
        .unwrap();
        assert!(!g.filter_complex.contains("[0:a]"));
        assert!(g.filter_complex.contains("anullsrc=r=48000:cl=stereo"));
    }

    #[test]
    fn stats_placeholders_are_filled_and_escaped() {
        let g = graph(&VersionTemplate::default(), &GraphExtras::default(), 1);
        let filled = fill_stats_paths(&g.filter_complex, "C:\\tmp\\s.log", "/tmp/p.log");
        assert!(!filled.contains(SSIM_STATS_PLACEHOLDER));
        assert!(filled.contains("stats_file=C\\\\:\\\\\\\\tmp\\\\\\\\s.log"));
        assert!(filled.contains("stats_file=/tmp/p.log"));
    }

    #[test]
    fn cover_args_follow_template() {
        let template = VersionTemplate {
            cover_at_sec: 2.5,
            cover_text: Some("封面字".into()),
            ..VersionTemplate::default()
        };
        assert!(build_cover_args(&template, &spec(), "/a.mp4", None, "/封面.jpg").is_err());
        let args = build_cover_args(&template, &spec(), "/a.mp4", Some(FONT), "/封面.jpg").unwrap();
        let joined = args.join(" ");
        assert!(joined.contains("-ss 2.500 -i /a.mp4 -frames:v 1"));
        assert!(joined.contains("text=封面字"));
        assert_eq!(args.last().unwrap(), "/封面.jpg");
    }

    #[test]
    fn producer_charset_follows_appendix_e() {
        let template = |producer: &str| VersionTemplate {
            ai_marked: true,
            ai_content_producer: producer.into(),
            ..VersionTemplate::default()
        };
        assert!(template("剧备").validate().is_err());
        assert!(template("drama.dossier-1").validate().is_ok());
        for bad in ["a\"b", "a\\b", "a b", ""] {
            assert!(template(bad).validate().is_err(), "{bad} 应被拒绝");
        }
    }

    #[test]
    fn quality_stats_parse_last_line() {
        let ssim = "n:1 Y:0.997700 All:0.998100 (28.21)\nn:2 Y:0.997700 All:0.998200 (28.23)\n";
        let psnr = "n:1 mse_avg:12.55 psnr_avg:38.25 \nn:2 mse_avg:11.54 psnr_avg:38.44 \n";
        let result = QualityResult::from_stats(ssim, psnr).unwrap();
        assert!((result.ssim - 0.9982).abs() < 1e-9);
        assert!((result.psnr - 38.44).abs() < 1e-9);

        let floor = QualityFloor {
            min_ssim: 0.95,
            min_psnr: 35.0,
        };
        assert!(result.passes(&floor));
        assert!(!QualityResult {
            ssim: 0.94,
            psnr: 40.0
        }
        .passes(&floor));
        assert!(QualityResult::from_stats("", psnr).is_err());
        assert!(QualityResult::from_stats(ssim, "nothing here").is_err());
        assert!(QualityResult::from_stats("All:1.5\n", psnr).is_err());
    }

    #[test]
    fn filter_escaping_survives_two_layers() {
        assert_eq!(escape_filter_text("a:b"), "a\\\\:b");
        assert_eq!(escape_filter_text("it's"), "it\\\\\\'s");
        assert_eq!(escape_filter_text("a\\b"), "a\\\\\\\\b");
        assert_eq!(escape_filter_text("第01集"), "第01集");
    }

    #[test]
    fn template_rejects_bad_values() {
        let base = |opacity: f64, size: u32, cover: f64| VersionTemplate {
            watermark_opacity: opacity,
            subtitle_size: size,
            cover_at_sec: cover,
            ..VersionTemplate::default()
        };
        assert!(base(1.5, 56, 1.0).validate().is_err());
        assert!(base(0.5, 0, 1.0).validate().is_err());
        assert!(base(0.5, 56, -1.0).validate().is_err());
        assert!(base(0.5, 56, 1.0).validate().is_ok());
    }

    #[test]
    fn template_round_trips_through_json() {
        let template = VersionTemplate::default();
        let text = serde_json::to_string(&template).unwrap();
        let back: VersionTemplate = serde_json::from_str(&text).unwrap();
        assert_eq!(template, back);
        let back: VersionTemplate = serde_json::from_str("{}").unwrap();
        assert_eq!(back, VersionTemplate::default());
    }
}
