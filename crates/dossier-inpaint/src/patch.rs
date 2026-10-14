//! ROI 裁剪与回贴。
//!
//! 修复只在字幕区域进行：模型（含 Python worker）拿到的是 ROI 尺寸的
//! bgr24 数据，回贴时只有掩码覆盖的像素被改写，其余画面保持原样。

use crate::frames::{frame_bytes, CHANNELS};
use crate::mask::{FrameSize, MaskRect};

/// 一个时序片段在 ROI 内的像素缓冲：每帧都是 ROI 尺寸的 bgr24 数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipPatch {
    pub size: FrameSize,
    pub frames: Vec<Vec<u8>>,
}

impl ClipPatch {
    pub fn new(size: FrameSize) -> Self {
        Self {
            size,
            frames: Vec::new(),
        }
    }

    pub fn frame_bytes(&self) -> usize {
        frame_bytes(self.size)
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    pub fn push(&mut self, frame: Vec<u8>) {
        self.frames.push(frame);
    }
}

/// 从整帧中裁剪 ROI 区域；ROI 需要已经裁到画面内。
pub fn crop_roi(frame: &[u8], frame_size: FrameSize, roi: MaskRect) -> Vec<u8> {
    let mut out = vec![0_u8; frame_bytes(roi_to_size(roi))];
    if roi.is_empty() {
        return out;
    }
    let frame_stride = (frame_size.width as usize) * CHANNELS;
    let roi_stride = (roi.width as usize) * CHANNELS;
    for y in 0..roi.height as usize {
        let src_start = (roi.y as usize + y) * frame_stride + (roi.x as usize) * CHANNELS;
        let dst_start = y * roi_stride;
        let src_end = src_start + roi_stride;
        if src_end > frame.len() {
            break;
        }
        out[dst_start..dst_start + roi_stride].copy_from_slice(&frame[src_start..src_end]);
    }
    out
}

/// 把 ROI 数据回贴到整帧。
///
/// `alpha` 为 ROI 局部的逐像素透明度（0–255）：`None` 表示整体替换，
/// 长度为 0、超出 alpha 长度或 alpha 为 0 的像素保持原样。
pub fn paste_roi(
    dst: &mut [u8],
    frame_size: FrameSize,
    roi: MaskRect,
    patch: &[u8],
    alpha: Option<&[u8]>,
) {
    if roi.is_empty() {
        return;
    }
    let frame_stride = (frame_size.width as usize) * CHANNELS;
    let roi_stride = (roi.width as usize) * CHANNELS;
    for y in 0..roi.height as usize {
        let dst_row = (roi.y as usize + y) * frame_stride + (roi.x as usize) * CHANNELS;
        let src_row = y * roi_stride;
        if dst_row + roi_stride > dst.len() || src_row + roi_stride > patch.len() {
            break;
        }
        for x in 0..roi.width as usize {
            let a = match alpha {
                None => 255,
                Some(values) => match values.get(y * (roi.width as usize) + x) {
                    Some(value) => *value,
                    // 没有对应 alpha 的像素不应被改写
                    None => continue,
                },
            };
            if a == 0 {
                continue;
            }
            let di = dst_row + x * CHANNELS;
            let si = src_row + x * CHANNELS;
            crate::mask::blend_pixel(&mut dst[di..di + CHANNELS], &patch[si..si + CHANNELS], a);
        }
    }
}

fn roi_to_size(roi: MaskRect) -> FrameSize {
    FrameSize::new(roi.width, roi.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient_frame(size: FrameSize) -> Vec<u8> {
        let mut frame = vec![0_u8; frame_bytes(size)];
        for y in 0..size.height as usize {
            for x in 0..size.width as usize {
                let index = (y * size.width as usize + x) * CHANNELS;
                frame[index] = x as u8;
                frame[index + 1] = y as u8;
                frame[index + 2] = 200;
            }
        }
        frame
    }

    #[test]
    fn crop_and_paste_are_symmetric() {
        let size = FrameSize::new(6, 4);
        let roi = MaskRect::new(2, 1, 3, 2);
        let frame = gradient_frame(size);

        let patch = crop_roi(&frame, size, roi);
        assert_eq!(patch.len(), 3 * 2 * CHANNELS);
        assert_eq!(&patch[0..3], &[2, 1, 200]);

        let mut dst = vec![0_u8; frame.len()];
        paste_roi(&mut dst, size, roi, &patch, None);
        // ROI 内的像素应逐字节一致
        let mut expected = vec![0_u8; frame.len()];
        for y in 0..roi.height as usize {
            for x in 0..roi.width as usize {
                let di =
                    ((roi.y as usize + y) * size.width as usize + roi.x as usize + x) * CHANNELS;
                let si = (y * roi.width as usize + x) * CHANNELS;
                expected[di..di + CHANNELS].copy_from_slice(&patch[si..si + CHANNELS]);
            }
        }
        assert_eq!(dst, expected);
    }

    #[test]
    fn paste_respects_per_pixel_alpha() {
        let size = FrameSize::new(2, 1);
        let roi = MaskRect::new(0, 0, 2, 1);
        let mut dst = vec![0_u8, 0, 0, 10, 10, 10];
        let patch = vec![100_u8, 100, 100, 200, 200, 200];

        // 第一个像素 alpha=0（保留），第二个 alpha=255（替换）
        paste_roi(&mut dst, size, roi, &patch, Some(&[0, 255]));
        assert_eq!(&dst[0..3], &[0, 0, 0]);
        assert_eq!(&dst[3..6], &[200, 200, 200]);

        // alpha 长度不足时不应越界，也不应改写
        let mut partial = vec![0_u8, 0, 0, 10, 10, 10];
        paste_roi(&mut partial, size, roi, &patch, Some(&[255]));
        assert_eq!(&partial[0..3], &[100, 100, 100]);
        assert_eq!(&partial[3..6], &[10, 10, 10]);
    }

    #[test]
    fn empty_roi_is_a_noop() {
        let size = FrameSize::new(2, 2);
        let frame = gradient_frame(size);
        let roi = MaskRect::new(0, 0, 0, 0);
        assert!(
            crop_roi(&frame, size, roi).is_empty()
                || crop_roi(&frame, size, roi).iter().all(|v| *v == 0)
        );
        let mut dst = vec![7_u8; frame.len()];
        paste_roi(&mut dst, size, roi, &frame, None);
        assert!(dst.iter().all(|value| *value == 7));
    }
}
