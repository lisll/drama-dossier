//! 去重配方引擎：由剧名 seed 确定性派生每集扰动参数，编译成 ffmpeg 滤镜链。
//!
//! 语义与仓库内 cloud 技术验证原型（`cloud/src/drama_cloud/recipes/dedup.py` 与
//! `modules/dedup.py`）一致：同一 seed + 同一集序号必然派生完全相同的参数，
//! 因此同一部剧任何时候重跑都得到同一套滤镜链，配方文件可作为存证材料复算。
//!
//! 与 Python 原型的差异（有意为之）：
//! - 每集参数从 `(seed, 集序号)` **独立**派生，而原型是顺序抽样——重组后的
//!   集数变化不会让已有分集的参数漂移，配方对「断点续跑」更稳；
//! - 只实现 atempo 变调后端（rubberband 依赖发行版 ffmpeg 不带的库）；
//!   原型里 `audioTempoCompensation` 随机量在补偿公式中恰好约掉（净变速恒为
//!   `rate/shifted`），这里直接化简掉该参数；
//! - 单轮编码同时完成几何扰动与色彩精修（正式链路只对成片再编码一次，
//!   Python 原型的母版轮 + 成片轮两轮编码合并为一轮）。
//!
//! 本模块是纯函数、无 IO（`save_to` 除外），不碰进程与子命令，便于单测；
//! ffmpeg 的执行在 `toolbox`。

use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

/// 音频重采样基准采样率（与配方存证一致；现代短剧素材基本是 48 kHz）。
pub const SAMPLE_RATE_HZ: u32 = 48_000;

/// 配方存证文件名（落在深度处理输出目录，随成片一起交付）。
pub const RECIPE_FILE_NAME: &str = "去重配方.json";

// ---------------------------------------------------------------------------
// 确定性随机源：SplitMix64（无依赖、跨平台逐字节可复现）
// ---------------------------------------------------------------------------

/// SplitMix64：最简单的可复现 PRNG，参数见 Steele & Lea 的论文。
/// 只用于配方参数派生，不承担任何安全职责。
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// `[low, high)` 上的均匀 f64（53 位有效精度）。
    fn uniform(&mut self, low: f64, high: f64) -> f64 {
        let unit = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        low + (high - low) * unit
    }

    /// `[low, high]` 上的均匀整数（`randint` 语义，两端含）。
    fn int(&mut self, low: i64, high: i64) -> i64 {
        let span = (high - low + 1).max(1) as u64;
        low + (self.next_u64() % span) as i64
    }
}

/// 把一个整数彻底打散（SplitMix64 的 finalizer），用于给每集派生独立随机流。
fn scramble(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// 剧名 → seed：UTF-8 字节的 SHA-256 前 8 字节（大端）。
/// 不混入时间戳——同一部剧永远得到同一套配方（可复现、可存证）。
fn seed_for_drama(drama_name: &str) -> u64 {
    let digest = Sha256::digest(drama_name.trim().as_bytes());
    u64::from_be_bytes(digest[..8].try_into().expect("SHA-256 前 8 字节固定存在"))
}

/// 每集两条独立随机流：偶数序号给画面/音频参数，奇数序号给元数据伪装。
fn variant_seed(seed: u64, episode_index: usize, lane: usize) -> u64 {
    seed ^ scramble(((episode_index as u64) << 1) | (lane as u64 & 1))
}

// ---------------------------------------------------------------------------
// 波形与参数结构（字段与 Python 配方一一对应，序列化为 camelCase 存证）
// ---------------------------------------------------------------------------

/// 正弦波形 `[base, amp, period, phase]`：值 = base + amp·sin(2π·clock/period + phase)。
/// period 必须 > 0；clock 由滤镜决定（几何类用帧号 `n`、色彩类用秒 `t`）。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Wave {
    pub base: f64,
    pub amp: f64,
    pub period: f64,
    pub phase: f64,
}

impl Wave {
    /// 编译成 ffmpeg 表达式（12 位小数，与原型精度一致）。
    fn compile(&self, clock: &str) -> String {
        format!(
            "{:.12}+{:.12}*sin(2*PI*{}/{:.12}+{:.12})",
            self.base, self.amp, clock, self.period, self.phase
        )
    }

    /// 幅值换单位后的副本：base 与 amp 乘以 `factor`；period / phase 与单位无关，保持不变。
    fn scaled(&self, factor: f64) -> Self {
        Self {
            base: self.base * factor,
            amp: self.amp * factor,
            ..*self
        }
    }
}

/// 度 → 弧度。配方里的旋转波形以**度**为单位（±1.2°、摆动 ≤0.8° 的微旋转），
/// 而 ffmpeg `rotate` 滤镜的 `a` 参数是**弧度**（`ffmpeg -h filter=rotate`：set angle (in radians)）。
/// 以前直接把度数当弧度传，实际转了约 60° 以上，防黑边裁切只剩不到一半画面。
const DEG_TO_RAD: f64 = PI / 180.0;

/// 一段 EQ（`equalizer` 滤镜参数）。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EqBand {
    pub frequency_hz: f64,
    pub width_q: f64,
    pub gain_db: f64,
}

/// 压缩器（`acompressor` 滤镜参数）。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Compressor {
    pub threshold_db: f64,
    pub ratio: f64,
    pub attack_ms: f64,
    pub release_ms: f64,
    pub makeup: f64,
}

/// 单集的全部确定性扰动参数。
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DedupVariant {
    /// 播放速度（≥1，画音同步变速）。
    pub speed: f64,
    /// 旋转后裁切比例（裁掉旋转露出的黑边再 cover 回原画幅）。
    pub crop_ratio: f64,
    pub rotation_wave: Wave,
    pub contrast_wave: Wave,
    pub brightness_wave: Wave,
    pub saturation_wave: Wave,
    pub hue_wave: Wave,
    pub hue_saturation_wave: Wave,
    /// 噪点强度（0 表示不加噪点滤镜）。
    pub noise_strength: i64,
    /// 锐化量（0 表示不加锐化滤镜）。
    pub sharpen_amount: f64,
    /// 音调微移比例（asetrate，0.9~1.1）。
    pub asetrate_ratio: f64,
    pub equalizer: [EqBand; 4],
    pub compressor: Compressor,
}

// 参数空间（对照 Grove reference_profile 逆向采集的签发范围，与 Python 原型一致）。
const SPEED_RANGE: (f64, f64) = (1.0, 1.25);
const CROP_RATIO_RANGE: (f64, f64) = (0.8, 1.0);
const NOISE_RANGE: (i64, i64) = (0, 20);
const SHARPEN_RANGE: (f64, f64) = (0.0, 3.0);
const ASETRATE_RATIO_RANGE: (f64, f64) = (0.9, 1.1);
const EQ_FREQ_RANGE: (f64, f64) = (20.0, 18_000.0);
const EQ_WIDTH_Q_RANGE: (f64, f64) = (0.1, 3000.0);
const EQ_GAIN_RANGE: (f64, f64) = (-12.0, 12.0);
const COMPRESSOR_THRESHOLD_RANGE: (f64, f64) = (-60.0, 0.0);
const COMPRESSOR_RATIO_RANGE: (f64, f64) = (1.0, 20.0);
const COMPRESSOR_ATTACK_RANGE: (f64, f64) = (0.01, 2000.0);
const COMPRESSOR_RELEASE_RANGE: (f64, f64) = (0.01, 9000.0);
const COMPRESSOR_MAKEUP_RANGE: (f64, f64) = (1.0, 64.0);

/// 机型池（与 Python 原型同源：Apple iPhone / iOS 组合）。
const DEVICE_POOL: [(&str, &str); 8] = [
    ("Apple iPhone 15 Pro Max", "iOS 17.6.1"),
    ("Apple iPhone 15 Pro", "iOS 17.6.1"),
    ("Apple iPhone 15", "iOS 17.5.1"),
    ("Apple iPhone 14 Pro Max", "iOS 17.5.1"),
    ("Apple iPhone 14 Pro", "iOS 17.4"),
    ("Apple iPhone 14", "iOS 17.4"),
    ("Apple iPhone 13 Pro", "iOS 17.3.1"),
    ("Apple iPhone 13", "iOS 17.3.1"),
];

/// 伪装拍摄时间的确定性纪元（2023-11-14T22:13:20Z）。
const CREATION_EPOCH: i64 = 1_700_000_000;

const PI: f64 = std::f64::consts::PI;

/// 第 `episode_index` 集成片的变速倍数。
///
/// 只由 `(剧名 seed, 集序号)` 决定、与总集数无关，所以重组前就能拿来规划切点，
/// 重组后用同一个剧名签发的 [`DedupRecipePlan`] 给出的是同一个值（单测覆盖）。
pub fn episode_speed(drama_name: &str, episode_index: usize) -> f64 {
    derive_variant(seed_for_drama(drama_name), episode_index).speed
}

/// 从 `(seed, 集序号)` 确定性派生一集参数。同入参必产出同参数（单测覆盖）。
pub fn derive_variant(seed: u64, episode_index: usize) -> DedupVariant {
    let mut rng = SplitMix64::new(variant_seed(seed, episode_index, 0));
    let mut wave =
        |base_low: f64, base_high: f64, amp: f64, period_low: f64, period_high: f64| Wave {
            base: rng.uniform(base_low, base_high),
            amp: amp * rng.uniform(0.0, 1.0),
            period: rng.uniform(period_low, period_high),
            phase: rng.uniform(-PI, PI),
        };
    let rotation = wave(-1.2, 1.2, 0.8, 360.0, 3600.0);
    let contrast = wave(1.0, 1.04, 0.02, 60.0, 600.0);
    let brightness = wave(-0.01, 0.01, 0.008, 60.0, 600.0);
    let saturation = wave(0.97, 1.01, 0.02, 60.0, 600.0);
    let hue = wave(-2.0, 2.0, 2.0, 90.0, 900.0);
    let hue_saturation = wave(0.98, 1.02, 0.02, 90.0, 900.0);
    let mut equalizer = [EqBand {
        frequency_hz: 0.0,
        width_q: 0.0,
        gain_db: 0.0,
    }; 4];
    for band in &mut equalizer {
        let mut freq = rng.uniform(EQ_FREQ_RANGE.0, EQ_FREQ_RANGE.1);
        let nyquist = f64::from(SAMPLE_RATE_HZ) / 2.0;
        if freq >= nyquist {
            freq = nyquist - 100.0;
        }
        *band = EqBand {
            frequency_hz: freq,
            width_q: rng.uniform(EQ_WIDTH_Q_RANGE.0, EQ_WIDTH_Q_RANGE.1),
            gain_db: rng.uniform(EQ_GAIN_RANGE.0, EQ_GAIN_RANGE.1),
        };
    }
    let compressor = Compressor {
        threshold_db: rng.uniform(COMPRESSOR_THRESHOLD_RANGE.0, COMPRESSOR_THRESHOLD_RANGE.1),
        ratio: rng.uniform(COMPRESSOR_RATIO_RANGE.0, COMPRESSOR_RATIO_RANGE.1),
        attack_ms: rng.uniform(COMPRESSOR_ATTACK_RANGE.0, COMPRESSOR_ATTACK_RANGE.1),
        release_ms: rng.uniform(COMPRESSOR_RELEASE_RANGE.0, COMPRESSOR_RELEASE_RANGE.1),
        makeup: rng.uniform(COMPRESSOR_MAKEUP_RANGE.0, COMPRESSOR_MAKEUP_RANGE.1),
    };
    DedupVariant {
        speed: rng.uniform(SPEED_RANGE.0, SPEED_RANGE.1),
        crop_ratio: rng.uniform(CROP_RATIO_RANGE.0, CROP_RATIO_RANGE.1),
        rotation_wave: rotation,
        contrast_wave: contrast,
        brightness_wave: brightness,
        saturation_wave: saturation,
        hue_wave: hue,
        hue_saturation_wave: hue_saturation,
        noise_strength: rng.int(NOISE_RANGE.0, NOISE_RANGE.1),
        sharpen_amount: rng.uniform(SHARPEN_RANGE.0, SHARPEN_RANGE.1),
        asetrate_ratio: rng.uniform(ASETRATE_RATIO_RANGE.0, ASETRATE_RATIO_RANGE.1),
        equalizer,
        compressor,
    }
}

// ---------------------------------------------------------------------------
// 滤镜链编译（纯函数）
// ---------------------------------------------------------------------------

/// 旋转后不露黑边的安全裁切比例（照抄原型 `safe_crop_ratio` 的投影计算）。
/// `rotation` 以度为单位；投影计算用弧度，所以先换算。
fn safe_crop_ratio(width: u32, height: u32, ratio: f64, rotation: &Wave) -> f64 {
    let angle = (rotation.base.abs() + rotation.amp.abs()) * DEG_TO_RAD;
    if angle <= 0.0 {
        return ratio;
    }
    let bound = angle.min(PI / 2.0);
    let projection = |other_ratio: f64| {
        let atan = other_ratio.atan().min(bound);
        atan.cos() + other_ratio * atan.sin()
    };
    let w = f64::from(width);
    let h = f64::from(height);
    let safe = ((w - 2.0) / (w * projection(h / w))).min((h - 2.0) / (h * projection(w / h)));
    ratio.min(safe)
}

impl DedupVariant {
    /// 单集画面滤镜链（`-vf` 或 filter_complex 的 `[0:v]` 段）。
    ///
    /// 流程：归一化画幅 → 波形旋转 → 安全比例裁切 → cover 回原画幅 →
    /// 时域混帧 → 波形调色 → 噪点 → 锐化 → 变速。
    pub fn compile_video_filters(&self, width: u32, height: u32) -> String {
        let cover =
            format!("scale={width}:{height}:force_original_aspect_ratio=increase:flags=lanczos");
        let mut chain: Vec<String> = vec![
            "format=yuv420p".into(),
            cover.clone(),
            format!("crop={width}:{height}"),
            "setsar=1".into(),
            format!(
                "rotate=a='{}':fillcolor=black:ow=iw:oh=ih",
                self.rotation_wave.scaled(DEG_TO_RAD).compile("n")
            ),
        ];
        let ratio = safe_crop_ratio(width, height, self.crop_ratio, &self.rotation_wave);
        chain.push(format!(
            "crop=trunc(iw*{ratio:.12}/2)*2:trunc(ih*{ratio:.12}/2)*2:exact=1"
        ));
        chain.push(cover);
        chain.push(format!("crop={width}:{height}"));
        chain.push("setsar=1".into());
        // 时域混帧：当前帧为主、前后帧低权重（Grove 实测 previousWeight 0.25）。
        chain.push("tmix=frames=3:weights='25 50 25'".into());
        // 色彩波形随秒时钟（t），需要 eval=frame 逐帧求值。
        chain.push(format!(
            "eq=brightness='{}':contrast='{}':saturation='{}':eval=frame",
            self.brightness_wave.compile("t"),
            self.contrast_wave.compile("t"),
            self.saturation_wave.compile("t")
        ));
        chain.push(format!(
            "hue=h='{}':s='{}'",
            self.hue_wave.compile("t"),
            self.hue_saturation_wave.compile("t")
        ));
        if self.noise_strength > 0 {
            chain.push(format!("noise=alls={}:allf=t", self.noise_strength));
        }
        if self.sharpen_amount > 1e-9 {
            chain.push(format!("unsharp=5:5:{:.12}:5:5:0", self.sharpen_amount));
        }
        // 变速：画面时长 ×1/speed，与音频链尾 atempo=speed 对齐。
        chain.push(format!("setpts=PTS/{:.12}", self.speed));
        chain.join(",")
    }

    /// 单集音频滤镜链（`-af`）。净语义：音调 ×asetrateRatio、时长 ×1/speed。
    pub fn compile_audio_filters(&self, sample_rate: u32) -> String {
        let rate = f64::from(sample_rate);
        let shifted = (rate * self.asetrate_ratio + 0.5).floor().max(1.0) as u32;
        // asetrate 把采样率微移（音调变化、时长 ×rate/shifted），
        // 再 atempo=rate/shifted 把时长补回原速，最后整体变速。
        let compensation = rate / f64::from(shifted);
        let mut chain: Vec<String> = vec![
            format!("aresample={sample_rate}"),
            "aformat=sample_fmts=fltp:channel_layouts=stereo".into(),
            format!("asetrate={shifted}"),
            format!("aresample={sample_rate}"),
            format!("atempo={compensation:.12}"),
        ];
        for band in &self.equalizer {
            chain.push(format!(
                "equalizer=f={:.12}:t=q:w={:.12}:g={:.12}",
                band.frequency_hz, band.width_q, band.gain_db
            ));
        }
        let c = &self.compressor;
        chain.push(format!(
            "acompressor=threshold={:.12}dB:ratio={:.12}:attack={:.12}\
             :release={:.12}:makeup={:.12}",
            c.threshold_db, c.ratio, c.attack_ms, c.release_ms, c.makeup
        ));
        chain.push(format!("atempo={:.12}", self.speed));
        chain.join(",")
    }

    /// 给界面/报告的一行可读摘要。
    pub fn describe(&self) -> String {
        // 波形本身就是度数，直接显示（以前又乘了一次 180/π，显示值被放大 57 倍）。
        let rotation_deg = self.rotation_wave.base.abs() + self.rotation_wave.amp.abs();
        format!(
            "速度 {:.3}x、裁切 {:.1}%、旋转幅度 {:.2}°、调色/混帧已注入、噪点 {}、锐化 {:.2}、音调 {:.3}x",
            self.speed,
            self.crop_ratio * 100.0,
            rotation_deg,
            self.noise_strength,
            self.sharpen_amount,
            self.asetrate_ratio
        )
    }
}

// ---------------------------------------------------------------------------
// 一部剧的完整配方（含元数据伪装与存证 JSON）
// ---------------------------------------------------------------------------

/// 一部剧的去重配方：seed + 逐集参数 + 逐集元数据。
pub struct DedupRecipePlan {
    pub seed: u64,
    pub drama_name: String,
    pub variants: Vec<DedupVariant>,
}

impl DedupRecipePlan {
    /// 由剧名签发配方；`episode_count` 是实际要编码的分集数（重组后）。
    pub fn build(drama_name: &str, episode_count: usize) -> Self {
        let seed = seed_for_drama(drama_name);
        let variants = (0..episode_count)
            .map(|index| derive_variant(seed, index))
            .collect();
        Self {
            seed,
            drama_name: drama_name.trim().to_string(),
            variants,
        }
    }

    /// 集序号对应的参数；越界（不应发生）返回 None，调用方退化为无滤镜。
    pub fn variant(&self, index: usize) -> Option<&DedupVariant> {
        self.variants.get(index)
    }

    /// 单集元数据 tags（`-metadata` 逐项）：
    /// comment/copyright 为剧名标注，make/software/creation_time 来自机型池
    /// 与确定性派生的拍摄时间。
    pub fn metadata_tags(&self, index: usize) -> Vec<(&'static str, String)> {
        let mut rng = SplitMix64::new(variant_seed(self.seed, index, 1));
        let (make, software) = DEVICE_POOL[rng.next_u64() as usize % DEVICE_POOL.len()];
        let creation = CREATION_EPOCH + (self.seed % 86_400) as i64 - self.variants.len() as i64
            + index as i64 * 60;
        let stamp = format_utc_compact(creation.max(0) as u64);
        vec![
            ("comment", format!("{} 第{}集", self.drama_name, index + 1)),
            ("copyright", format!("{} {}", self.drama_name, stamp)),
            ("make", make.to_string()),
            ("software", software.to_string()),
            ("creation_time", format_utc(creation.max(0) as u64)),
        ]
    }

    /// 存证 JSON（配方全文 + 逐集元数据，可离线复算滤镜链）。
    pub fn to_json(&self) -> serde_json::Value {
        let episodes: Vec<serde_json::Value> = (0..self.variants.len())
            .map(|index| {
                let tags = self.metadata_tags(index);
                serde_json::json!({
                    "index": index + 1,
                    "comment": tags[0].1,
                    "copyright": tags[1].1,
                    "make": tags[2].1,
                    "software": tags[3].1,
                    "creationTime": tags[4].1,
                })
            })
            .collect();
        serde_json::json!({
            // @2：旋转波形明确按「度」解释、编译时换算成弧度。@1 的存证把同一数值直接当弧度
            // 交给了 ffmpeg，同一 seed 编出的滤镜链与 @2 不同，不能用新版本复算。
            "schema": "drama-dossier/dedup-recipe@2",
            "rotationUnit": "degrees",
            "seed": self.seed,
            "dramaName": self.drama_name,
            "episodeCount": self.variants.len(),
            "sampleRateHz": SAMPLE_RATE_HZ,
            "variants": self.variants,
            "episodes": episodes,
        })
    }

    /// 把存证 JSON 写进深度处理输出目录，返回文件路径。
    pub fn save_to(&self, output_dir: &Path) -> Result<PathBuf, String> {
        let path = output_dir.join(RECIPE_FILE_NAME);
        let body = serde_json::to_vec_pretty(&self.to_json())
            .map_err(|error| format!("配方序列化失败：{error}"))?;
        std::fs::write(&path, body).map_err(|error| format!("写入配方存证失败：{error}"))?;
        Ok(path)
    }
}

/// 报告里的配方摘要（`TranscodeRunReport.dedup`）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DedupRecipeSummary {
    /// 以字符串发给界面：seed 是 64 位整数，超过 JS 能精确表示的 2^53，按数字传会被舍入。
    #[serde(serialize_with = "serialize_u64_as_string")]
    pub seed: u64,
    pub episode_count: usize,
    /// 存证文件绝对路径；写入失败时为 None（不阻塞成片）。
    pub recipe_file: Option<String>,
}

fn serialize_u64_as_string<S: serde::Serializer>(
    value: &u64,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_str(value)
}

/// Unix 秒 → `YYYY-MM-DDTHH:MM:SSZ`（确定性伪拍摄时间，不引 chrono 依赖）。
fn format_utc(epoch_secs: u64) -> String {
    let (date, time) = civil_and_time(epoch_secs);
    format!("{date}T{time}Z")
}

/// Unix 秒 → `YYYYMMDDHHMMSS`（copyright 标注用的紧凑戳）。
fn format_utc_compact(epoch_secs: u64) -> String {
    let (date, time) = civil_and_time(epoch_secs);
    format!("{date}{time}").replace('-', "").replace(':', "")
}

/// 公历换算（Howard Hinnant 的 civil_from_days 公开算法）+ 当日时分秒。
fn civil_and_time(epoch_secs: u64) -> (String, String) {
    let days = (epoch_secs / 86_400) as i64;
    let secs_of_day = epoch_secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (
        format!("{y:04}-{m:02}-{d:02}"),
        format!(
            "{:02}:{:02}:{:02}",
            secs_of_day / 3600,
            (secs_of_day % 3600) / 60,
            secs_of_day % 60
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_seed_is_serialized_as_exact_string() {
        let summary = DedupRecipeSummary {
            seed: 15_978_750_346_754_301_024,
            episode_count: 3,
            recipe_file: None,
        };
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(json["seed"], "15978750346754301024");
    }

    #[test]
    fn episode_speed_matches_recipe_regardless_of_episode_count() {
        // 重组前用来规划的倍速，必须与重组后按实际集数签发的配方完全一致。
        for count in [1_usize, 3, 12] {
            let plan = DedupRecipePlan::build("测试剧", count);
            for (index, variant) in plan.variants.iter().enumerate() {
                assert_eq!(episode_speed("测试剧", index), variant.speed);
            }
        }
    }

    #[test]
    fn seed_is_stable_and_timestamp_free() {
        assert_eq!(seed_for_drama("测试剧"), seed_for_drama("测试剧"));
        assert_ne!(seed_for_drama("测试剧"), seed_for_drama("另一部剧"));
        assert_eq!(seed_for_drama("  测试剧 "), seed_for_drama("测试剧"));
    }

    #[test]
    fn variants_are_deterministic_per_index() {
        let a = derive_variant(42, 3);
        let b = derive_variant(42, 3);
        assert_eq!(
            a.compile_video_filters(1080, 1920),
            b.compile_video_filters(1080, 1920)
        );
        assert_eq!(
            a.compile_audio_filters(SAMPLE_RATE_HZ),
            b.compile_audio_filters(SAMPLE_RATE_HZ)
        );
        // 不同集 / 不同 seed 必然不同。
        assert_ne!(
            a.compile_video_filters(1080, 1920),
            derive_variant(42, 4).compile_video_filters(1080, 1920)
        );
        assert_ne!(
            a.compile_video_filters(1080, 1920),
            derive_variant(43, 3).compile_video_filters(1080, 1920)
        );
    }

    #[test]
    fn derived_params_stay_in_ranges() {
        for index in 0..32 {
            let v = derive_variant(7, index);
            assert!(
                (SPEED_RANGE.0..=SPEED_RANGE.1).contains(&v.speed),
                "{}",
                v.speed
            );
            assert!(
                (CROP_RATIO_RANGE.0..=CROP_RATIO_RANGE.1).contains(&v.crop_ratio),
                "{}",
                v.crop_ratio
            );
            assert!((0..=20).contains(&v.noise_strength));
            assert!((0.0..=3.0).contains(&v.sharpen_amount));
            assert!((0.9..=1.1).contains(&v.asetrate_ratio));
            for band in &v.equalizer {
                assert!((20.0..18_000.0).contains(&band.frequency_hz));
                assert!((0.1..=3000.0).contains(&band.width_q));
                assert!((-12.0..=12.0).contains(&band.gain_db));
            }
            assert!((-60.0..=0.0).contains(&v.compressor.threshold_db));
            assert!((1.0..=20.0).contains(&v.compressor.ratio));
            assert!((1.0..=64.0).contains(&v.compressor.makeup));
            for wave in [
                v.rotation_wave,
                v.contrast_wave,
                v.brightness_wave,
                v.saturation_wave,
                v.hue_wave,
                v.hue_saturation_wave,
            ] {
                assert!(wave.period > 1e-12);
            }
        }
    }

    #[test]
    fn video_chain_has_expected_filters() {
        let v = derive_variant(1, 0);
        let chain = v.compile_video_filters(1080, 1920);
        for expected in [
            "rotate=a='",
            "crop=trunc(iw*",
            "tmix=",
            "eq=",
            "hue=h='",
            "setpts=PTS/",
        ] {
            assert!(chain.contains(expected), "缺少 {expected}：{chain}");
        }
        assert!(!chain.contains("noise=") || v.noise_strength > 0);
        assert!(!chain.contains("unsharp") || v.sharpen_amount > 1e-9);
        // 逗号分隔的单条 -vf 字符串里不能出现会破坏参数的逗号。
        assert!(!chain.contains(",,"));
    }

    #[test]
    fn audio_chain_has_expected_filters_and_matches_speed() {
        let v = derive_variant(5, 2);
        let chain = v.compile_audio_filters(SAMPLE_RATE_HZ);
        assert!(chain.starts_with("aresample=48000"));
        assert!(chain.contains("asetrate="));
        assert!(chain.matches("equalizer=f=").count() == 4);
        assert!(chain.contains("acompressor=threshold="));
        assert!(chain.ends_with(&format!("atempo={:.12}", v.speed)));
        // asetrate 位移量必须落在采样率 ±10% 内。
        let shifted: u32 = chain
            .split("asetrate=")
            .nth(1)
            .and_then(|rest| rest.split(',').next())
            .and_then(|value| value.parse().ok())
            .expect("asetrate 必须是整数");
        assert!((43_200..=52_800).contains(&shifted));
    }

    #[test]
    fn metadata_tags_are_deterministic_and_from_pool() {
        let plan = DedupRecipePlan::build("示例剧", 5);
        let first = plan.metadata_tags(2);
        assert_eq!(first, plan.metadata_tags(2));
        assert_eq!(first[0].0, "comment");
        assert!(first[0].1.ends_with("第3集"));
        assert!(DEVICE_POOL.iter().any(|(make, _)| *make == first[2].1));
        assert!(first[4].1.ends_with('Z'));
        // 不同集的标签不同（copyright 戳随集号递增）。
        assert_ne!(plan.metadata_tags(1)[1].1, plan.metadata_tags(2)[1].1);
    }

    #[test]
    fn recipe_json_and_save_round_trip() {
        let plan = DedupRecipePlan::build("存证剧", 3);
        let json = plan.to_json();
        assert_eq!(json["schema"], "drama-dossier/dedup-recipe@2");
        assert_eq!(json["rotationUnit"], "degrees");
        assert_eq!(json["episodeCount"], 3);
        assert_eq!(json["variants"].as_array().map(Vec::len), Some(3));

        let dir = std::env::temp_dir().join(format!("dedup-recipe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = plan.save_to(&dir).unwrap();
        assert!(path.is_file());
        let body = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["seed"], plan.seed);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn utc_formatting_matches_known_instant() {
        assert_eq!(format_utc(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(format_utc_compact(1_700_000_000), "20231114221320");
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn safe_crop_ratio_never_reveals_edges() {
        // 旋转幅度大时安全比例必须小于 1（裁进画内），且不超过请求比例。
        let wild = Wave {
            base: -1.2,
            amp: 0.8,
            period: 720.0,
            phase: 0.5,
        };
        let ratio = safe_crop_ratio(1080, 1920, 0.95, &wild);
        assert!(ratio < 0.95, "{ratio}");
        // 没有旋转时原样返回。
        let still = Wave {
            base: 0.0,
            amp: 0.0,
            period: 720.0,
            phase: 0.0,
        };
        assert_eq!(safe_crop_ratio(1080, 1920, 0.95, &still), 0.95);
        // 最坏情况（|base| + amp = 2°）也只裁掉很窄一圈：以前把度数当弧度时只剩 0.49。
        let worst = Wave {
            base: 1.2,
            amp: 0.8,
            period: 720.0,
            phase: 0.0,
        };
        for (w, h) in [(1080, 1920), (1920, 1080), (360, 640)] {
            let ratio = safe_crop_ratio(w, h, 1.0, &worst);
            assert!((0.93..1.0).contains(&ratio), "{w}x{h}: {ratio}");
        }
    }

    /// 从 `rotate=a='<base>+<amp>*sin(...)'` 里取出 base 与 amp。
    fn rotate_terms(chain: &str) -> (f64, f64) {
        let start = chain.find("rotate=a='").expect("缺少 rotate") + "rotate=a='".len();
        let expr = &chain[start..chain[start..].find('\'').unwrap() + start];
        let (base, rest) = expr.split_once('+').unwrap();
        let amp = rest.split_once('*').unwrap().0;
        (base.parse().unwrap(), amp.parse().unwrap())
    }

    #[test]
    fn rotation_is_converted_to_radians_for_ffmpeg() {
        let v = derive_variant(7, 3);
        let (base, amp) = rotate_terms(&v.compile_video_filters(1080, 1920));
        // 交给 ffmpeg 的是弧度：等于配方度数 × π/180（12 位小数精度内）。
        assert!(
            (base - v.rotation_wave.base * DEG_TO_RAD).abs() < 1e-9,
            "{base}"
        );
        assert!(
            (amp - v.rotation_wave.amp * DEG_TO_RAD).abs() < 1e-9,
            "{amp}"
        );
        // ±1.2° + 0.8° 换成弧度最多约 0.035；以前把度数当弧度时可达 2.0。
        assert!(base.abs() + amp.abs() < 0.05, "{base} {amp}");
    }

    #[test]
    fn safe_crop_keeps_most_of_the_frame_for_every_seed() {
        // 防止以后又把单位写错：任何 seed、任何画幅，防黑边裁切都只裁很窄一圈。
        for seed in 0..200u64 {
            let v = derive_variant(seed, (seed % 7) as usize);
            for (w, h) in [(1080, 1920), (1920, 1080)] {
                let safe = safe_crop_ratio(w, h, v.crop_ratio, &v.rotation_wave);
                assert!(
                    safe >= v.crop_ratio.min(0.93),
                    "seed={seed} {w}x{h}: safe={safe} crop_ratio={}",
                    v.crop_ratio
                );
            }
            // 说明文字里的旋转幅度是真实度数（不再被放大 57 倍）。
            let shown = v.rotation_wave.base.abs() + v.rotation_wave.amp.abs();
            assert!(shown <= 2.0 + 1e-9, "seed={seed}: {shown}°");
            assert!(v.describe().contains(&format!("旋转幅度 {shown:.2}°")));
        }
    }
}
