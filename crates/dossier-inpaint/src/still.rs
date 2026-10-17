//! 静止区域检测（M4b-2，总方案 2.3 节 / M0 第 5.2 节）：找出画面里「位置固定、内容不变」的区域。
//!
//! 试用版水印、台标、固定字幕的共同点是**位置固定、内容不变**，所以不需要针对某一款
//! 软件单独识别，用通用的「跨帧静止」判据就够了。
//!
//! 为什么不做成检测器：现有的 [`crate::detect::TextDetector`] 一次只看一帧，而「静止」
//! 本质上要比较多帧。所以这里做成擦除之前的一个**预定位**步骤，结果当作区域交给现有管线，
//! 擦除主流程一行都不用改。
//!
//! 判据（四条同时满足才算）：
//! 1. **帧间方差小**：该像素的亮度在采样帧之间的方差不超过阈值（默认 12）。
//!    静止的内容每帧都一样；移动的物体、变化的背景会远超这个值。
//! 2. **极差小**：这段时间里最亮与最暗之差不超过阈值（默认 18）。方差会被
//!    「两帧各占一半」的情况骗过去（移动物体的边缘恰好落在同一像素上交替出现），
//!    极差不会——它直接回答「这个像素的亮度到底变没变过」。
//! 3. **边缘明显**：该像素的梯度幅值超过阈值（默认 24）。只看边缘是因为纯色的大片背景
//!    虽然也「静止」，但擦它没有意义，而且会把整片画面都选进来。
//! 4. **聚成块之后尺寸合理**：太小的（短边 < 8 像素，多是噪点）、太大的
//!    （面积超过画面的 60%，那是整帧而不是角标）都丢掉。
//!
//! 最后把每一帧的候选块交给 [`crate::detect::stable_rois`] 合并——那是现有的「跨帧把重叠
//! 框合成稳定区域」的逻辑，已经处理了 IoU 聚类与「出现帧数不足就丢弃」。

use serde::{Deserialize, Serialize};

use crate::detect::{stable_rois, Reliability, TextBox};
use crate::mask::{FrameSize, MaskRect};

/// 静止区域检测的判据与阈值。
#[derive(Debug, Clone, PartialEq)]
pub struct StillConfig {
    /// 帧间亮度方差的上限：不超过它算「内容没变」。
    ///
    /// 12 是实测折中值：压缩噪声与轻微抖动通常在个位数，而任何移动物体都在 100 以上。
    /// 这是**绝对**上限；实际判据还会再除以 [`Self::relative_variance`] 得到的相对基准
    /// （两者取小），见文档注释。
    pub max_variance: f64,
    /// 相对基准系数：方差必须不超过「全画面方差中位数 × 这个系数」。
    ///
    /// 真实编码的视频里，静止区域的方差不会是 0（编码噪声会整体抬高），
    /// 而移动区域的方差仍比它高一个数量级。取中位数的 0.5 倍作基准，
    /// 判据就能适应不同画质与内容的素材，不必逐片调参。
    pub relative_variance: f64,
    /// 帧间亮度极差的上限：这段时间里最亮与最暗之差不能超过它。
    ///
    /// 与方差是「且」的关系，专门挡住「两帧各占一半」的抖动：
    /// 那种情况下方差可能很小，但极差很大，说明亮度确实变过。
    pub max_spread: f64,
    /// 相对基准系数：极差必须不超过「全画面极差中位数 × 这个系数」。
    ///
    /// 与 [`Self::relative_variance`] 同理，用来适应真实素材的编码噪声。
    pub relative_spread: f64,
    /// 判定「与第一帧一致」时允许的亮度差。
    ///
    /// 编码噪声会让同一块静止画面每帧的像素值略有出入，这个容差要略大于噪声幅度。
    pub stable_tolerance: f64,
    /// 「每帧都一致」的帧数占比下限（0–1）。
    ///
    /// 用来挡住周期性移动的物体：它在采样时刻可能恰好回到原位，方差与极差都很小，
    /// 但并非**每一帧**都在那里。真标识是每帧都一样。默认 0.9。
    pub stable_ratio: f64,
    /// 边缘梯度幅值下限：不超过它算「平坦区域」，不选。
    pub min_gradient: f64,
    /// 候选块的最小短边（像素），再小多半是噪点。
    pub min_side: u32,
    /// 候选块的最大面积占画面比例，超过就当成整帧丢掉。
    pub max_area_ratio: f64,
    /// 候选块里「静止且有边缘」的像素占比下限。
    ///
    /// 真标识是实心的（占比高）；移动物体在采样时刻偶然留下的细条残影占比很低。
    /// 默认 0.35：实测真角标约 0.6–0.9，移动物体残影约 0.14。
    pub min_score: f32,
    /// 合并时的跨帧可靠性（IoU 阈值 + 至少出现在多少帧里）。
    pub reliability: Reliability,
}

impl Default for StillConfig {
    fn default() -> Self {
        Self {
            max_variance: 12.0,
            relative_variance: 0.5,
            max_spread: 18.0,
            relative_spread: 0.5,
            stable_tolerance: 12.0,
            stable_ratio: 0.9,
            min_gradient: 24.0,
            min_side: 8,
            max_area_ratio: 0.6,
            min_score: 0.35,
            // 采样帧减少时「出现 2 帧」太松（抖动也能凑够），提到 30% 由调用方按采样数算。
            reliability: Reliability {
                iou_threshold: 0.5,
                min_frames: 2,
            },
        }
    }
}

impl StillConfig {
    /// 按采样帧数算出「至少出现多少帧才算稳定」：取 30%，但不少于 2 帧。
    ///
    /// 固定写死 2 帧的话，采样 30 帧时任何抖动都能凑够；按比例算才能随采样密度自适应。
    pub fn min_frames_for(&self, samples: usize) -> usize {
        let ratio = if samples == 0 {
            1
        } else {
            (samples * 3).div_ceil(10)
        };
        ratio.max(2).min(samples.max(1))
    }
}

/// 检测结果中的一个候选区域。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StillCandidate {
    #[serde(flatten)]
    pub rect: MaskRect,
    /// 这个区域里「静止且边缘明显」的像素占该区域的比例（0–1）。
    ///
    /// 界面上用来排序与说明：比例越高越可能是真的固定标识（而不是画面里偶然静止的一块）。
    pub score: f32,
}

/// 检测结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StillReport {
    pub width: u32,
    pub height: u32,
    /// 实际参与比较的采样帧数。
    pub samples: usize,
    /// 判定「至少出现多少帧才稳定」用的阈值。
    pub min_frames: usize,
    /// 候选框，按面积从大到小。
    pub candidates: Vec<StillCandidate>,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum StillError {
    #[error("至少需要 2 帧才能判断静止区域，实际给了 {0} 帧")]
    NotEnoughFrames(usize),
    #[error("帧数据长度不对：{width}×{height} 的 BGR 帧应该是 {expected} 字节，实际 {actual}")]
    BadFrame {
        width: u32,
        height: u32,
        expected: usize,
        actual: usize,
    },
    #[error("静止区域检测的阈值配置不合法：{0}")]
    Config(String),
}

impl StillConfig {
    fn validate(&self) -> Result<(), StillError> {
        if !self.max_variance.is_finite() || self.max_variance < 0.0 {
            return Err(StillError::Config(
                "帧间方差上限必须是非负的有限数值".into(),
            ));
        }
        if !self.max_spread.is_finite() || self.max_spread < 0.0 {
            return Err(StillError::Config(
                "帧间极差上限必须是非负的有限数值".into(),
            ));
        }
        if !(self.relative_variance.is_finite() && self.relative_variance > 0.0) {
            return Err(StillError::Config("相对方差系数必须是正的有限数值".into()));
        }
        if !(self.relative_spread.is_finite() && self.relative_spread > 0.0) {
            return Err(StillError::Config("相对极差系数必须是正的有限数值".into()));
        }
        if !(self.stable_tolerance.is_finite() && self.stable_tolerance >= 0.0) {
            return Err(StillError::Config("一致性容差必须是非负的有限数值".into()));
        }
        if !(self.stable_ratio.is_finite() && (0.0..=1.0).contains(&self.stable_ratio)) {
            return Err(StillError::Config("一致性帧占比必须在 0 到 1 之间".into()));
        }
        if !self.min_gradient.is_finite() || self.min_gradient < 0.0 {
            return Err(StillError::Config(
                "边缘梯度下限必须是非负的有限数值".into(),
            ));
        }
        if self.min_side == 0 {
            return Err(StillError::Config("候选块最小短边必须大于 0".into()));
        }
        if !(self.max_area_ratio.is_finite()
            && self.max_area_ratio > 0.0
            && self.max_area_ratio <= 1.0)
        {
            return Err(StillError::Config(
                "候选块最大面积比例必须在 (0, 1] 之间".into(),
            ));
        }
        if !(self.min_score.is_finite() && (0.0..=1.0).contains(&self.min_score)) {
            return Err(StillError::Config(
                "候选块最低占比必须在 0 到 1 之间".into(),
            ));
        }
        Ok(())
    }
}

/// 对一组采样帧做静止区域检测。
///
/// `frames` 是若干帧的原始 BGR 像素（`width * height * 3` 字节），顺序就是采样顺序。
/// 调用方负责均匀抽样（见 [`sample_indices`]）。
pub fn detect_still_regions(
    frames: &[Vec<u8>],
    size: FrameSize,
    config: &StillConfig,
) -> Result<StillReport, StillError> {
    config.validate()?;
    if frames.len() < 2 {
        return Err(StillError::NotEnoughFrames(frames.len()));
    }
    let expected = (size.width as usize) * (size.height as usize) * 3;
    for frame in frames {
        if frame.len() != expected {
            return Err(StillError::BadFrame {
                width: size.width,
                height: size.height,
                expected,
                actual: frame.len(),
            });
        }
    }

    let stats = pixel_stats(frames, size, config.stable_tolerance);
    let pixels = stats.variance.len();
    // ---- 3. 「静止 + 有边缘」的像素聚成连通块 ----
    // 静止要同时满足三条：
    //   a. 帧间方差小（没在抖动）；
    //   b. 极差小（亮度真的没变过）；
    //   c. **相对**静止：方差明显低于全画面中位数。
    //
    // 为什么需要 c：真实视频有编码噪声，同一块静止画面的方差不会是 0，而是随画质、
    // 码率、素材内容整体抬高；而移动区域的方差仍然比它高一个数量级。
    // 只用绝对阈值的话，换个素材就得重新调参。取「中位数 × 系数」当基准，
    // 判据就自动适应素材：画质差的片子基准跟着抬高，画质好的片子基准低。
    let mut variance_sorted = stats.variance.clone();
    let variance_baseline = median(&mut variance_sorted);
    let variance_limit = config
        .max_variance
        .min(variance_baseline * config.relative_variance);
    // 极差同理：编码噪声也会让「静止」像素的极差抬高。基准取极差的中位数，
    // 上限 = min(绝对值上限, 中位数 × 系数) —— 两者都要过，宁可少报也不误报。
    let mut spread_sorted = stats.spread.clone();
    let spread_baseline = median(&mut spread_sorted);
    let spread_limit = config
        .max_spread
        .min(spread_baseline * config.relative_spread);
    let mut mask = vec![false; pixels];
    // 「每帧都一致」的帧数下限：至少要占采样帧的 90%。
    // 这是挡住「周期性移动物体恰好回到原位」的关键——那种像素只是**多数**帧一致，
    // 真标识是**每一帧**都一致。
    let stable_min = ((frames.len() as f64) * config.stable_ratio).round() as u32;
    let stable_required = stable_min.min(frames.len() as u32 - 1);
    for (index, slot) in mask.iter_mut().enumerate() {
        *slot = stats.variance[index] <= variance_limit
            && stats.spread[index] <= spread_limit
            && stats.stable_frames[index] >= stable_required
            && stats.gradient[index] >= config.min_gradient;
    }
    let mut rects: Vec<MaskRect> = connected_components(&mask, size)
        .into_iter()
        .filter(|rect| {
            rect.width.min(rect.height) >= config.min_side
                && (rect.area() as f64) <= config.max_area_ratio * pixels as f64
        })
        .collect();
    // 邻近的小块（例如一行字被切碎）先合并一次，再交给跨帧聚合。
    rects = crate::detect::merge_overlapping(&rects, 0.1);

    // ---- 4. 交给现有的跨帧聚合：出现在足够多帧里的才保留 ----
    // 这里的判据本身已经在**多帧**上算过（帧间方差），所以每一帧的「检测结果」都是同一批框。
    // `stable_rois` 仍然值得过一遍：它负责 IoU 聚类、把重叠框合成一个外接矩形、
    // 并按 `min_frames` 过滤——正是我们想要的「稳定区域」语义。
    let min_frames = config.min_frames_for(frames.len());
    let samples: Vec<Vec<TextBox>> = vec![rects
        .iter()
        .map(|rect| TextBox::new(rect.x, rect.y, rect.width, rect.height, 1.0))
        .collect()];
    let merged = stable_rois(
        &samples,
        &Reliability {
            iou_threshold: config.reliability.iou_threshold,
            // 判据已经跨帧算过，这里的样本是「这一批框整体」，所以只要 1 帧。
            min_frames: 1,
        },
    );

    // ---- 5. 打分：区域里「静止且有边缘」的像素占比 ----
    let mut candidates: Vec<StillCandidate> = merged
        .into_iter()
        .filter(|rect| {
            rect.width.min(rect.height) >= config.min_side
                && (rect.area() as f64) <= config.max_area_ratio * pixels as f64
        })
        .map(|rect| StillCandidate {
            rect,
            score: still_score(&mask, size, rect),
        })
        // 区块内「静止且有边缘」的像素占比太低就丢掉。
        //
        // 为什么要这一条：判据是逐像素的，一条狭长的边缘（例如移动物体在采样时刻恰好
        // 覆盖过的那一列）也可能凑成一个小连通块。真标识是**实心的**，占比高；
        // 这种残影往往是细条，占比低。默认 0.35 是实测值：真角标约 0.6–0.9，
        // 移动物体残影约 0.14。
        .filter(|candidate| candidate.score >= config.min_score)
        .collect();
    candidates.sort_by(|a, b| {
        b.rect
            .area()
            .cmp(&a.rect.area())
            .then_with(|| b.score.total_cmp(&a.score))
    });

    Ok(StillReport {
        width: size.width,
        height: size.height,
        samples: frames.len(),
        min_frames,
        candidates,
    })
}

/// 按等间隔取出 `count` 个帧序号（尽量铺满整段，末尾不会过度集中）。
///
/// `total` 是总帧数。返回的序号严格递增且不重复；`count >= total` 时返回全部序号。
pub fn sample_indices(total: u64, count: usize) -> Vec<u64> {
    if total == 0 || count == 0 {
        return Vec::new();
    }
    if count as u64 >= total {
        return (0..total).collect();
    }
    // 取每个区间的中点：第一个和最后一个都不会贴到边上，避免开头片头/结尾黑场主导结果。
    (0..count as u64)
        .map(|index| {
            let start = index * total / count as u64;
            let end = (index + 1) * total / count as u64;
            start + (end - start) / 2
        })
        .collect()
}

/// 逐像素的统计量：帧间方差、帧间极差、边缘梯度，以及「有多少帧与第一帧一致」。
struct PixelStats {
    variance: Vec<f64>,
    spread: Vec<f64>,
    gradient: Vec<f64>,
    /// 每个像素「与第一帧的亮度差不超过容差」的采样帧数。
    ///
    /// 用来区分「真的没变过」与「多数帧没变、偶尔变了」：
    /// 移动物体在采样时刻恰好回到同一位置时，方差与极差都可能很小，
    /// 但它并非**每一帧**都在那里；真标识则每帧都一样。
    stable_frames: Vec<u32>,
}

/// 逐像素算帧间方差与极差（只算亮度：静止判断看亮度足够，三个通道都算既慢又没必要）。
fn pixel_stats(frames: &[Vec<u8>], size: FrameSize, tolerance: f64) -> PixelStats {
    let count = frames.len() as f64;
    let pixels = (size.width as usize) * (size.height as usize);
    let mut sum = vec![0.0_f64; pixels];
    let mut sum_squares = vec![0.0_f64; pixels];
    let mut min = vec![f64::MAX; pixels];
    let mut max = vec![f64::MIN; pixels];
    let mut first = vec![0.0_f64; pixels];
    let mut stable_frames = vec![0_u32; pixels];
    for (frame_index, frame) in frames.iter().enumerate() {
        for (index, slot) in sum.iter_mut().enumerate() {
            let at = index * 3;
            let value = luma(frame[at], frame[at + 1], frame[at + 2]);
            *slot += value;
            sum_squares[index] += value * value;
            if value < min[index] {
                min[index] = value;
            }
            if value > max[index] {
                max[index] = value;
            }
            if frame_index == 0 {
                first[index] = value;
            } else if (value - first[index]).abs() <= tolerance {
                stable_frames[index] += 1;
            }
        }
    }
    let mut variance = vec![0.0_f64; pixels];
    let mut spread = vec![0.0_f64; pixels];
    for index in 0..pixels {
        let mean = sum[index] / count;
        // 数值安全：方差不可能为负，浮点误差可能算出极小的负数。
        variance[index] = (sum_squares[index] / count - mean * mean).max(0.0);
        // 极差 = 这段里最亮与最暗之差。方差会被「两帧各占一半」的抖动骗过去
        // （例如移动物体的边缘恰好在同一像素上交替出现），极差不会。
        spread[index] = (max[index] - min[index]).max(0.0);
    }
    PixelStats {
        variance,
        spread,
        gradient: gradient_magnitude(&frames[0], size),
        stable_frames,
    }
}

/// 中位数（会打乱传入的切片顺序；空切片返回 0）。
fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

/// 亮度（BT.601 加权，和 ffmpeg 的 `format=gray` 一致）。
fn luma(b: u8, g: u8, r: u8) -> f64 {
    0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b)
}

/// 每个像素的梯度幅值（4 邻域中心差分，边界按最近像素）。
fn gradient_magnitude(frame: &[u8], size: FrameSize) -> Vec<f64> {
    let w = size.width as usize;
    let h = size.height as usize;
    let mut luma_plane = vec![0.0_f64; w * h];
    for (index, slot) in luma_plane.iter_mut().enumerate() {
        let at = index * 3;
        *slot = luma(frame[at], frame[at + 1], frame[at + 2]);
    }
    let mut gradient = vec![0.0_f64; w * h];
    for y in 0..h {
        let up = y.saturating_sub(1);
        let down = (y + 1).min(h - 1);
        for x in 0..w {
            let left = x.saturating_sub(1);
            let right = (x + 1).min(w - 1);
            let gx = luma_plane[y * w + right] - luma_plane[y * w + left];
            let gy = luma_plane[down * w + x] - luma_plane[up * w + x];
            gradient[y * w + x] = (gx * gx + gy * gy).sqrt();
        }
    }
    gradient
}

/// 把布尔掩码里的连通块（4 邻域）转成外接矩形。
fn connected_components(mask: &[bool], size: FrameSize) -> Vec<MaskRect> {
    let w = size.width as usize;
    let h = size.height as usize;
    let mut visited = vec![false; mask.len()];
    let mut rects = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    for start in 0..mask.len() {
        if !mask[start] || visited[start] {
            continue;
        }
        visited[start] = true;
        stack.push(start);
        let (mut min_x, mut min_y) = (w, h);
        let (mut max_x, mut max_y) = (0usize, 0usize);
        while let Some(index) = stack.pop() {
            let x = index % w;
            let y = index / w;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
            // 4 邻域
            if x > 0 {
                let next = index - 1;
                if mask[next] && !visited[next] {
                    visited[next] = true;
                    stack.push(next);
                }
            }
            if x + 1 < w {
                let next = index + 1;
                if mask[next] && !visited[next] {
                    visited[next] = true;
                    stack.push(next);
                }
            }
            if y > 0 {
                let next = index - w;
                if mask[next] && !visited[next] {
                    visited[next] = true;
                    stack.push(next);
                }
            }
            if y + 1 < h {
                let next = index + w;
                if mask[next] && !visited[next] {
                    visited[next] = true;
                    stack.push(next);
                }
            }
        }
        let width = (max_x + 1 - min_x) as u32;
        let height = (max_y + 1 - min_y) as u32;
        rects.push(MaskRect::new(min_x as u32, min_y as u32, width, height));
    }
    rects
}

/// 矩形内「命中掩码」的像素比例。
fn fill_ratio(mask: &[bool], size: FrameSize, rect: MaskRect) -> f32 {
    let w = size.width as usize;
    let mut hit = 0usize;
    let mut total = 0usize;
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            let index = (y as usize) * w + (x as usize);
            if index < mask.len() {
                total += 1;
                if mask[index] {
                    hit += 1;
                }
            }
        }
    }
    if total == 0 {
        0.0
    } else {
        (hit as f64 / total as f64) as f32
    }
}

/// 一个矩形里「至少有一边挨着掩码」的比例：按行、按列各自看有没有命中。
///
/// 为什么不用纯粹的像素占比：真实素材里一块实心标识，边缘像素往往因为抗锯齿、
/// 编码噪声、以及与背景颜色接近而不满足「静止」判据，只有轮廓附近命中；
/// 但它的**每一行、每一列**都至少有几个命中像素。而移动物体偶然留下的残影
/// 通常是窄窄的一条，只在**一个方向**上连贯，另一个方向大量行列完全没有命中。
///
/// 所以取两个方向的**较小值**：两个方向都连贯才算实心块。
/// （取较大值会让一条细长的残影也算通过——那正是要挡掉的情况。）
fn row_column_ratio(mask: &[bool], size: FrameSize, rect: MaskRect) -> f32 {
    let w = size.width as usize;
    let mut row_seen = vec![false; rect.height as usize];
    let mut column_seen = vec![false; rect.width as usize];
    for y in rect.y..rect.bottom() {
        for x in rect.x..rect.right() {
            let index = (y as usize) * w + (x as usize);
            if index < mask.len() && mask[index] {
                row_seen[(y - rect.y) as usize] = true;
                column_seen[(x - rect.x) as usize] = true;
            }
        }
    }
    let rows_hit = row_seen.iter().filter(|hit| **hit).count() as f32;
    let columns_hit = column_seen.iter().filter(|hit| **hit).count() as f32;
    let rows = rect.height.max(1) as f32;
    let columns = rect.width.max(1) as f32;
    (rows_hit / rows).min(columns_hit / columns)
}

/// 块内「静止且有边缘」的像素占比与行列连贯度的综合分。
fn still_score(mask: &[bool], size: FrameSize, rect: MaskRect) -> f32 {
    let fill = fill_ratio(mask, size, rect);
    let coherence = row_column_ratio(mask, size, rect);
    // 取较大者：实心标识靠 fill，轮廓型标识靠 coherence（两个方向都连贯）。
    fill.max(coherence)
}

/// 诊断用：把判据的中间量导出来，便于对真实素材标定阈值。
///
/// 生产路径不用它；单测与集成测试靠它回答「为什么某个素材检不出东西」——
/// 而不是靠在外面反复试参数。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StillDiagnostics {
    /// 全画面方差中位数。
    pub variance_median: f64,
    /// 实际生效的方差上限（绝对上限与相对基准取小）。
    pub variance_limit: f64,
    /// 全画面极差中位数。
    pub spread_median: f64,
    /// 实际生效的极差上限。
    pub spread_limit: f64,
    /// 同时满足「方差达标 + 极差达标」的像素数。
    pub still_pixels: usize,
    /// 在此之上还有边缘的像素数（= 进入连通块分析的像素）。
    pub masked_pixels: usize,
    /// 连通块个数（过滤前）。
    pub components: usize,
    /// 过滤后剩下的块数。
    pub kept: usize,
}

/// 跑一遍判据并回报中间量。参数与 [`detect_still_regions`] 相同。
pub fn diagnose(
    frames: &[Vec<u8>],
    size: FrameSize,
    config: &StillConfig,
) -> Result<StillDiagnostics, StillError> {
    config.validate()?;
    if frames.len() < 2 {
        return Err(StillError::NotEnoughFrames(frames.len()));
    }
    let expected = (size.width as usize) * (size.height as usize) * 3;
    for frame in frames {
        if frame.len() != expected {
            return Err(StillError::BadFrame {
                width: size.width,
                height: size.height,
                expected,
                actual: frame.len(),
            });
        }
    }
    let stats = pixel_stats(frames, size, config.stable_tolerance);
    let mut variance_sorted = stats.variance.clone();
    let variance_median = median(&mut variance_sorted);
    let variance_limit = config
        .max_variance
        .min(variance_median * config.relative_variance);
    let mut spread_sorted = stats.spread.clone();
    let spread_median = median(&mut spread_sorted);
    let spread_limit = config
        .max_spread
        .min(spread_median * config.relative_spread);

    let mut still_pixels = 0;
    let mut masked_pixels = 0;
    let mut mask = vec![false; (size.width as usize) * (size.height as usize)];
    for (index, slot) in mask.iter_mut().enumerate() {
        if stats.variance[index] <= variance_limit && stats.spread[index] <= spread_limit {
            still_pixels += 1;
            if stats.gradient[index] >= config.min_gradient {
                masked_pixels += 1;
                *slot = true;
            }
        }
    }
    let components: Vec<MaskRect> = connected_components(&mask, size);
    let pixels = mask.len();
    let kept = components
        .iter()
        .filter(|rect| {
            rect.width.min(rect.height) >= config.min_side
                && (rect.area() as f64) <= config.max_area_ratio * pixels as f64
        })
        .count();
    Ok(StillDiagnostics {
        variance_median,
        variance_limit,
        spread_median,
        spread_limit,
        still_pixels,
        masked_pixels,
        components: components.len(),
        kept,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 160;
    const H: u32 = 120;

    fn size() -> FrameSize {
        FrameSize::new(W, H)
    }

    /// 造一帧：背景是平移的纹理（模拟真实镜头/场景在动），再按需画上静止块 / 移动块。
    fn frame(phase: u8, still: bool, moving: bool) -> Vec<u8> {
        let mut buffer = vec![0u8; (W * H * 3) as usize];
        for y in 0..H {
            for x in 0..W {
                let at = ((y * W + x) * 3) as usize;
                // 背景：棋盘格纹理整体向右平移，每帧位置不同 → 一直在动。
                let shifted = (x + u32::from(phase) * 5) % 32;
                let checker = (shifted / 8 + y / 8) % 2;
                let value = if checker == 0 { 60 } else { 170 };
                buffer[at] = value;
                buffer[at + 1] = value;
                buffer[at + 2] = value;
            }
        }
        if still {
            // 固定角标：左上角一块黑白相间的方块，每帧完全一样。
            for y in 10..40 {
                for x in 10..50 {
                    let at = ((y * W + x) * 3) as usize;
                    let value = if (x / 4 + y / 4) % 2 == 0 { 240 } else { 20 };
                    buffer[at] = value;
                    buffer[at + 1] = value;
                    buffer[at + 2] = value;
                }
            }
        }
        if moving {
            // 向右移动的白块：每帧位置不同。
            //
            // 位移取 17（与块宽 40 交叉互质），保证连续几帧不会恰好把同一条竖列
            // 一直盖住——如果每帧只移 3 像素，块宽 40 意味着中间那几十列在每一帧
            // 都被覆盖，那些像素就真的「每帧一样」了，那是构造出来的病态样本，
            // 不是真实运动。
            let shift = u32::from(phase) * 17 % 120;
            for y in 60..90 {
                for x in (60 + shift)..(100 + shift) {
                    if x >= W {
                        continue;
                    }
                    let at = ((y * W + x) * 3) as usize;
                    buffer[at] = 255;
                    buffer[at + 1] = 255;
                    buffer[at + 2] = 255;
                }
            }
        }
        buffer
    }
    #[test]
    fn finds_a_fixed_corner_mark_and_ignores_moving_objects() {
        let frames: Vec<Vec<u8>> = (0..10).map(|i| frame(i, true, true)).collect();
        let report = detect_still_regions(&frames, size(), &StillConfig::default()).expect("检测");
        assert_eq!(report.samples, 10);
        assert!(
            !report.candidates.is_empty(),
            "画面里有固定角标，应该检出来"
        );
        // 固定角标在左上角 (10..50, 10..40)：应当有一个候选框落在那里。
        let hit = report.candidates.iter().any(|candidate| {
            let r = candidate.rect;
            r.x <= 12 && r.y <= 12 && r.right() >= 48 && r.bottom() >= 38
        });
        assert!(
            hit,
            "应该框住左上角的固定角标，实际候选：{:?}",
            report.candidates
        );
        // 移动的白块（x 从 60 起、每帧右移）不该被当成静止区域：
        // 任何候选框都不该完全落在那个横向带状区域里。
        for candidate in &report.candidates {
            let r = candidate.rect;
            let inside_moving_band = r.y >= 58 && r.bottom() <= 92 && r.x >= 55;
            assert!(
                !inside_moving_band,
                "移动物体不该被当成静止区域：{:?}",
                candidate
            );
        }
    }

    #[test]
    fn pure_motion_yields_no_candidates() {
        // 全画面都在动：不该有任何候选。
        let frames: Vec<Vec<u8>> = (0..8)
            .map(|phase: u32| {
                let mut buffer = vec![0u8; (W * H * 3) as usize];
                for y in 0..H {
                    for x in 0..W {
                        let at = ((y * W + x) * 3) as usize;
                        let value = ((x + phase * 7) % 256) as u8;
                        buffer[at] = value;
                        buffer[at + 1] = value;
                        buffer[at + 2] = value;
                    }
                }
                buffer
            })
            .collect();
        let report = detect_still_regions(&frames, size(), &StillConfig::default()).expect("检测");
        assert!(
            report.candidates.is_empty(),
            "整幅画面都在动，不该有候选：{:?}",
            report.candidates
        );
    }

    #[test]
    fn a_static_full_frame_is_rejected_as_too_large() {
        // 整帧都不动（含边缘）：面积超过 60% 上限，应当被丢掉。
        let frames: Vec<Vec<u8>> = (0..6)
            .map(|_| {
                let mut buffer = vec![0u8; (W * H * 3) as usize];
                for y in 0..H {
                    for x in 0..W {
                        let at = ((y * W + x) * 3) as usize;
                        let value = if (x / 8 + y / 8) % 2 == 0 { 30 } else { 220 };
                        buffer[at] = value;
                        buffer[at + 1] = value;
                        buffer[at + 2] = value;
                    }
                }
                buffer
            })
            .collect();
        let report = detect_still_regions(&frames, size(), &StillConfig::default()).expect("检测");
        assert!(
            report.candidates.is_empty(),
            "整帧静止应当被面积上限挡掉：{:?}",
            report.candidates
        );
    }

    #[test]
    fn tiny_noise_specks_are_filtered() {
        // 一个 3×3 的静止小点：短边小于默认 8 像素，应当被丢掉。
        let frames: Vec<Vec<u8>> = (0..6)
            .map(|phase| {
                let mut buffer = frame(phase, false, true);
                for y in 100..103 {
                    for x in 140..143 {
                        let at = ((y * W + x) * 3) as usize;
                        buffer[at] = 250;
                        buffer[at + 1] = 250;
                        buffer[at + 2] = 250;
                    }
                }
                buffer
            })
            .collect();
        let report = detect_still_regions(&frames, size(), &StillConfig::default()).expect("检测");
        for candidate in &report.candidates {
            assert!(
                candidate.rect.width.min(candidate.rect.height) >= 8,
                "太小的块不该留下：{:?}",
                candidate
            );
        }
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(
            detect_still_regions(&[], size(), &StillConfig::default()).unwrap_err(),
            StillError::NotEnoughFrames(0)
        );
        assert_eq!(
            detect_still_regions(&[vec![0; 10]], size(), &StillConfig::default()).unwrap_err(),
            StillError::NotEnoughFrames(1)
        );
        let bad =
            detect_still_regions(&[vec![0; 10], vec![0; 10]], size(), &StillConfig::default())
                .unwrap_err();
        assert!(matches!(bad, StillError::BadFrame { .. }), "{bad}");

        let config = StillConfig {
            max_area_ratio: 1.5,
            ..StillConfig::default()
        };
        let blank = vec![vec![0u8; (W * H * 3) as usize]; 2];
        assert!(matches!(
            detect_still_regions(&blank, size(), &config).unwrap_err(),
            StillError::Config(_)
        ));
    }

    #[test]
    fn sparse_still_pixels_are_rejected_by_score() {
        // 构造一个「细条」场景：只有一条 2 像素宽的竖线在原地不动，其余都在动。
        // 它虽然满足「静止 + 有边缘」，但块内占比低（细条大部分是背景），
        // 而且短边只有 2 像素——两道闸门任意一道都该挡住它。
        let frames: Vec<Vec<u8>> = (0..8)
            .map(|phase: u32| {
                let mut buffer = vec![0u8; (W * H * 3) as usize];
                for y in 0..H {
                    for x in 0..W {
                        let at = ((y * W + x) * 3) as usize;
                        let value = ((x + phase * 11) % 256) as u8;
                        buffer[at] = value;
                        buffer[at + 1] = value;
                        buffer[at + 2] = value;
                    }
                }
                // 一条固定不动的亮线（x = 100..102）。
                for y in 20..100 {
                    for x in 100..102 {
                        let at = ((y * W + x) * 3) as usize;
                        buffer[at] = 255;
                        buffer[at + 1] = 255;
                        buffer[at + 2] = 255;
                    }
                }
                buffer
            })
            .collect();
        let report = detect_still_regions(&frames, size(), &StillConfig::default()).expect("检测");
        // 可能检不出，也可能检出但分数足够高（线本身就是实心的）。
        // 无论哪种，都不能出现「短边太小」或「占比很低」的候选。
        for candidate in &report.candidates {
            assert!(
                candidate.rect.width.min(candidate.rect.height) >= 8
                    || candidate.score >= StillConfig::default().min_score,
                "不该留下又小又稀的残影：{:?}",
                candidate
            );
            assert!(
                candidate.score >= StillConfig::default().min_score,
                "占比低于下限的候选不该留下：{:?}",
                candidate
            );
        }
    }

    #[test]
    fn sampling_covers_the_whole_video() {
        // 取 5 个点：应当均匀铺开、严格递增、不重复。
        let indices = sample_indices(100, 5);
        assert_eq!(indices.len(), 5);
        assert!(
            indices.windows(2).all(|pair| pair[0] < pair[1]),
            "{indices:?}"
        );
        assert!(
            indices[0] > 0 && *indices.last().unwrap() < 100,
            "{indices:?}"
        );
        // 要的比总共还多：全部返回。
        assert_eq!(sample_indices(3, 10), vec![0, 1, 2]);
        // 边界
        assert!(sample_indices(0, 5).is_empty());
        assert!(sample_indices(100, 0).is_empty());
        // 只有一帧时返回那一帧。
        assert_eq!(sample_indices(1, 5), vec![0]);
    }

    #[test]
    fn min_frames_scales_with_sample_count() {
        let config = StillConfig::default();
        assert_eq!(config.min_frames_for(2), 2);
        assert_eq!(config.min_frames_for(10), 3);
        assert_eq!(config.min_frames_for(30), 9);
    }
}
