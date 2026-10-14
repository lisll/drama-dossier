//! 掩码与兴趣区域（ROI）运算。
//!
//! 设计：掩码只保存 ROI 局部尺寸，避免全片 1080p 二值掩码占用过多内存；
//! 全部为纯 Rust 实现，不依赖图像库，便于单元测试。

use serde::{Deserialize, Serialize};

/// 画面尺寸（像素）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameSize {
    pub width: u32,
    pub height: u32,
}

impl FrameSize {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

/// 矩形区域，原点在左上角，坐标为半开区间 `[x, x + width)`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaskRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl MaskRect {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(&self) -> u32 {
        self.x.saturating_add(self.width)
    }

    pub fn bottom(&self) -> u32 {
        self.y.saturating_add(self.height)
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub fn area(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    /// 与画面的交集；完全落在画面外时返回 `None`。
    pub fn clip(&self, frame: FrameSize) -> Option<Self> {
        let x0 = self.x.min(frame.width);
        let y0 = self.y.min(frame.height);
        let x1 = self.right().min(frame.width);
        let y1 = self.bottom().min(frame.height);
        if x1 <= x0 || y1 <= y0 {
            None
        } else {
            Some(Self::new(x0, y0, x1 - x0, y1 - y0))
        }
    }

    /// 与另一个矩形的交集。
    pub fn intersect(&self, other: &Self) -> Option<Self> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = self.right().min(other.right());
        let y1 = self.bottom().min(other.bottom());
        if x1 <= x0 || y1 <= y0 {
            None
        } else {
            Some(Self::new(x0, y0, x1 - x0, y1 - y0))
        }
    }

    /// 向外扩张 `pad` 像素后裁到画面内。
    pub fn expand(&self, pad: u32, frame: FrameSize) -> Option<Self> {
        let rect = Self::new(
            self.x.saturating_sub(pad),
            self.y.saturating_sub(pad),
            self.width.saturating_add(pad.saturating_mul(2)),
            self.height.saturating_add(pad.saturating_mul(2)),
        );
        rect.clip(frame)
    }

    /// 以矩形中心按比例缩放后裁到画面内；用于给 inpaint 模型留上下文。
    pub fn scale(&self, factor: f64, frame: FrameSize) -> Option<Self> {
        if !(factor.is_finite() && factor > 0.0) {
            return self.clip(frame);
        }
        let width = ((f64::from(self.width) * factor).round() as u32).max(1);
        let height = ((f64::from(self.height) * factor).round() as u32).max(1);
        let cx = f64::from(self.x) + f64::from(self.width) / 2.0;
        let cy = f64::from(self.y) + f64::from(self.height) / 2.0;
        let x = (cx - f64::from(width) / 2.0).round().max(0.0) as u32;
        let y = (cy - f64::from(height) / 2.0).round().max(0.0) as u32;
        Self::new(x, y, width, height).clip(frame)
    }

    /// 最小外接矩形；两个空矩形返回 `None`。
    pub fn union(&self, other: &Self) -> Option<Self> {
        match (self.is_empty(), other.is_empty()) {
            (true, true) => None,
            (true, false) => Some(*other),
            (false, true) => Some(*self),
            (false, false) => {
                let x = self.x.min(other.x);
                let y = self.y.min(other.y);
                Some(Self::new(
                    x,
                    y,
                    self.right().max(other.right()) - x,
                    self.bottom().max(other.bottom()) - y,
                ))
            }
        }
    }
}

/// ROI 局部二值掩码：`0` 表示保留原像素，`255` 表示需要修复。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryMask {
    pub width: u32,
    pub height: u32,
    data: Vec<u8>,
}

impl BinaryMask {
    /// 全零（无需修复）掩码。
    pub fn empty(size: FrameSize) -> Self {
        Self {
            width: size.width,
            height: size.height,
            data: vec![0; (size.width as usize) * (size.height as usize)],
        }
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// 可变访问底层掩码数据，便于批量合并（`0` = 保留，非 `0` = 修复）。
    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    pub fn size(&self) -> FrameSize {
        FrameSize::new(self.width, self.height)
    }

    pub fn get(&self, x: u32, y: u32) -> bool {
        if x >= self.width || y >= self.height {
            return false;
        }
        self.data[(y as usize) * (self.width as usize) + (x as usize)] > 0
    }

    pub fn set(&mut self, x: u32, y: u32, value: bool) {
        if x >= self.width || y >= self.height {
            return;
        }
        self.data[(y as usize) * (self.width as usize) + (x as usize)] =
            if value { 255 } else { 0 };
    }

    /// 把 ROI 直角坐标系里的矩形填入掩码（`roi` 为掩码对应的画面区域）。
    /// 把画面坐标下的矩形填入掩码；`roi` 是该掩码在画面中的位置与尺寸。
    ///
    /// 与 ROI 无交集的部分会被忽略，因此可以安全传入任意检测框。
    pub fn paint_rect(&mut self, roi: MaskRect, rect: MaskRect) {
        let Some(overlap) = rect.intersect(&roi) else {
            return;
        };
        for y in overlap.y..overlap.bottom() {
            for x in overlap.x..overlap.right() {
                self.set(x - roi.x, y - roi.y, true);
            }
        }
    }

    /// 以 `radius` 为半径做可分离膨胀（方形结构元素）。
    pub fn dilate(&mut self, radius: u32) {
        if radius == 0 || self.data.is_empty() {
            return;
        }
        let w = self.width as usize;
        let h = self.height as usize;
        let mut horizontal = vec![0_u8; self.data.len()];
        for y in 0..h {
            let row = y * w;
            for x in 0..w {
                let start = x.saturating_sub(radius as usize);
                let end = (x + radius as usize).min(w - 1);
                let hit = (start..=end).any(|i| self.data[row + i] > 0);
                horizontal[row + x] = if hit { 255 } else { 0 };
            }
        }
        for x in 0..w {
            for y in 0..h {
                let start = y.saturating_sub(radius as usize);
                let end = (y + radius as usize).min(h - 1);
                let hit = (start..=end).any(|i| horizontal[i * w + x] > 0);
                self.data[y * w + x] = if hit { 255 } else { 0 };
            }
        }
    }

    /// 掩码覆盖比例，范围 0.0–1.0。
    pub fn coverage(&self) -> f64 {
        if self.data.is_empty() {
            return 0.0;
        }
        let marked = self.data.iter().filter(|value| **value > 0).count();
        marked as f64 / self.data.len() as f64
    }

    pub fn is_empty(&self) -> bool {
        self.data.iter().all(|value| *value == 0)
    }
}

/// 生成羽化后的 alpha 掩码（0–255），用于和原帧做加权合成。
///
/// 做法是对二值掩码做一次半径为 `radius` 的可分离盒式模糊，
/// 掩码内部保持 255，边缘形成线性过渡，避免修复区域出现硬边。
pub fn feather(mask: &BinaryMask, radius: u32) -> Vec<u8> {
    let w = mask.width as usize;
    let h = mask.height as usize;
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let source: Vec<u16> = mask
        .data()
        .iter()
        .map(|value| if *value > 0 { 255 } else { 0 })
        .collect();
    if radius == 0 {
        return source.iter().map(|value| *value as u8).collect();
    }
    let r = radius as usize;

    let mut horizontal = vec![0_u16; source.len()];
    for y in 0..h {
        let row = y * w;
        for x in 0..w {
            let start = x.saturating_sub(r);
            let end = (x + r).min(w - 1);
            let sum: u32 = (start..=end).map(|i| u32::from(source[row + i])).sum();
            horizontal[row + x] = (sum / (end - start + 1) as u32) as u16;
        }
    }

    let mut out = vec![0_u8; source.len()];
    for x in 0..w {
        for y in 0..h {
            let start = y.saturating_sub(r);
            let end = (y + r).min(h - 1);
            let sum: u32 = (start..=end)
                .map(|i| u32::from(horizontal[i * w + x]))
                .sum();
            out[y * w + x] = (sum / (end - start + 1) as u32) as u8;
        }
    }
    out
}

/// 按 alpha 合成一个 bgr 像素：`dst = src * a + dst * (1 - a)`。
///
/// `alpha` 为 0 或缓冲过短时保持原像素。
pub fn blend_pixel(dst: &mut [u8], src: &[u8], alpha: u8) {
    if alpha == 0 {
        return;
    }
    let a = u32::from(alpha);
    for channel in 0..3 {
        let (Some(target), Some(value)) = (dst.get_mut(channel), src.get(channel)) else {
            return;
        };
        let original = u32::from(*target);
        let blended = (u32::from(*value) * a + original * (255 - a) + 127) / 255;
        *target = blended.min(255) as u8;
    }
}

/// 逐像素按 alpha 合成 bgr24 缓冲。
///
/// `alpha` 每个像素 1 字节（不是每个通道 1 字节）；长度不足的像素保持原样。
pub fn blend_bgr(dst: &mut [u8], src: &[u8], alpha: &[u8]) {
    for (index, a) in alpha.iter().enumerate() {
        let base = index * 3;
        if base + 3 > dst.len() || base + 3 > src.len() {
            break;
        }
        blend_pixel(&mut dst[base..base + 3], &src[base..base + 3], *a);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_intersection_and_expansion() {
        let frame = FrameSize::new(640, 360);
        let rect = MaskRect::new(120, 300, 400, 40);
        assert_eq!(rect.right(), 520);
        assert_eq!(rect.bottom(), 340);
        assert_eq!(rect.area(), 16_000);

        let expanded = rect.expand(20, frame).unwrap();
        assert_eq!(expanded, MaskRect::new(100, 280, 440, 80));

        // 超出画面时被裁掉
        let clipped = MaskRect::new(600, 340, 400, 200).clip(frame).unwrap();
        assert_eq!(clipped, MaskRect::new(600, 340, 40, 20));

        // 完全在画面外
        assert!(MaskRect::new(700, 400, 10, 10).clip(frame).is_none());
        assert!(MaskRect::new(0, 0, 0, 0).clip(frame).is_none());
    }

    #[test]
    fn rect_union_takes_bounding_box() {
        let a = MaskRect::new(10, 10, 20, 20);
        let b = MaskRect::new(50, 30, 10, 40);
        assert_eq!(a.union(&b).unwrap(), MaskRect::new(10, 10, 50, 60));
        assert!(MaskRect::new(0, 0, 0, 0)
            .union(&MaskRect::new(0, 0, 0, 0))
            .is_none());
    }

    #[test]
    fn rect_scale_keeps_center_and_clips() {
        let frame = FrameSize::new(640, 360);
        let rect = MaskRect::new(120, 300, 400, 40);
        let scaled = rect.scale(2.0, frame).unwrap();
        // 2 倍后为 800x80，超出画面宽度后被裁到 640
        assert_eq!(scaled.width, 640);
        assert_eq!(scaled.height, 80);
        assert_eq!(scaled.x, 0);
        assert_eq!(scaled.bottom(), 360);
    }

    #[test]
    fn mask_paint_dilate_and_coverage() {
        let roi = MaskRect::new(10, 20, 100, 50);
        let mut mask = BinaryMask::empty(FrameSize::new(roi.width, roi.height));
        assert!(mask.is_empty());

        // 画面坐标 (10,20)-(14,24) 对应 ROI 局部 (0,0)-(4,4)
        mask.paint_rect(roi, MaskRect::new(10, 20, 4, 4));
        assert!(mask.get(0, 0));
        assert!(mask.get(3, 3));
        assert!(!mask.get(4, 4));
        assert_eq!(mask.coverage(), 16.0 / (100.0 * 50.0));

        mask.dilate(2);
        assert!(mask.get(5, 4));
        assert!(mask.get(4, 5));
        assert!(!mask.get(7, 7));

        // ROI 外的矩形不会写入掩码
        let mut other = BinaryMask::empty(FrameSize::new(roi.width, roi.height));
        other.paint_rect(roi, MaskRect::new(500, 500, 10, 10));
        assert!(other.is_empty());
    }

    #[test]
    fn paint_rect_works_for_roi_away_from_origin() {
        // ROI 位于画面 (100, 200)，掩码是 ROI 局部坐标
        let roi = MaskRect::new(100, 200, 40, 20);
        let mut mask = BinaryMask::empty(FrameSize::new(roi.width, roi.height));

        // 画面坐标 (110, 205)-(120, 210) → 局部 (10, 5)-(20, 10)
        mask.paint_rect(roi, MaskRect::new(110, 205, 10, 5));
        assert!(mask.get(10, 5));
        assert!(mask.get(19, 9));
        assert!(!mask.get(20, 10));
        assert!(!mask.get(5, 5));

        // 跨越 ROI 左边界的框只写入交集部分
        let mut crossing = BinaryMask::empty(FrameSize::new(roi.width, roi.height));
        crossing.paint_rect(roi, MaskRect::new(90, 205, 15, 5));
        assert!(crossing.get(0, 5), "左边界内侧应被写入");
        assert!(crossing.get(4, 9));
        assert!(!crossing.get(5, 5), "超出 ROI 的部分不应写入");
    }

    #[test]
    fn dilate_handles_edges_without_overflow() {
        let mut mask = BinaryMask::empty(FrameSize::new(8, 4));
        mask.set(0, 0, true);
        mask.dilate(3);
        assert!(mask.get(3, 3));
        assert!(!mask.get(4, 3));
        assert!(mask.get(0, 3));
    }

    #[test]
    fn feather_fades_edges_and_keeps_interior() {
        // 掩码覆盖画面 (0,0)-(16,16)，其中 (4,4)-(12,12) 需要修复
        let mut mask = BinaryMask::empty(FrameSize::new(16, 16));
        mask.paint_rect(MaskRect::new(0, 0, 16, 16), MaskRect::new(4, 4, 8, 8));
        let alpha = feather(&mask, 2);

        let at = |x: usize, y: usize| alpha[y * 16 + x];
        assert_eq!(at(8, 8), 255, "掩码内部应保持完全不透明");
        assert_eq!(at(0, 0), 0, "远离掩码处应完全透明");
        assert!(at(3, 8) > 0 && at(3, 8) < 255, "边缘应形成过渡");
        assert!(at(2, 8) < at(3, 8), "过渡应随距离递减");
    }

    #[test]
    fn feather_ramps_inside_the_mask_too() {
        // 掩码覆盖 (8,8)-(24,24)，feather 半径 3。
        let mut mask = BinaryMask::empty(FrameSize::new(32, 32));
        mask.paint_rect(MaskRect::new(0, 0, 32, 32), MaskRect::new(8, 8, 16, 16));
        let alpha = feather(&mask, 3);
        let at = |x: usize, y: usize| alpha[y * 32 + x];

        // 边界上只有约一半权重落在修复结果上 → 原始像素会以约 50% 回渗
        let boundary = at(8, 16);
        assert!(
            (100..=160).contains(&boundary),
            "掩码边界应约为半透明，实际 {boundary}"
        );

        // 边界向内 1~2 像素仍然不是全量替换 → 紧贴边界的原内容仍会残留
        assert!(at(9, 16) < 255, "边界内侧 1 像素仍未完全替换");
        assert!(at(10, 16) < 255, "边界内侧 2 像素仍未完全替换");

        // 距离边界 >= 半径处才是完全替换
        assert_eq!(at(11, 16), 255, "掩码内部（距边界 >= 半径）应为全量替换");

        // 因此「掩码膨胀量」必须 >= `feather_radius`，否则目标边缘的像素
        // 只会被部分替换，形成一圈残留轮廓。
    }

    #[test]
    fn blend_bgr_uses_per_pixel_alpha() {
        // 3 个像素：完全替换 / 半透明 / 保留
        let mut dst = vec![0_u8, 0, 0, 0, 0, 0, 7, 7, 7];
        let src = vec![255_u8, 255, 255, 255, 255, 255, 255, 255, 255];
        blend_bgr(&mut dst, &src, &[255, 128, 0]);
        assert_eq!(&dst[0..3], &[255, 255, 255]);
        assert_eq!(&dst[3..6], &[128, 128, 128]);
        assert_eq!(&dst[6..9], &[7, 7, 7], "alpha 为 0 时保留原像素");

        // alpha 长度不足的像素保持原样
        let mut partial = vec![0_u8, 0, 0, 7, 7, 7];
        blend_bgr(&mut partial, &src, &[255]);
        assert_eq!(&partial[0..3], &[255, 255, 255]);
        assert_eq!(&partial[3..6], &[7, 7, 7]);
    }
}
