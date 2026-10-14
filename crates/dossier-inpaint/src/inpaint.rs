//! 时序修复后端。
//!
//! 真实质量方案是 E2FGVI（见 `tools/model_worker`），通过文件交换的
//! worker 协议接入；本地另提供一个列填充后端，用于在没有模型的环境下
//! 验证整条管线是否连通。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use thiserror::Error;

use crate::detect::WorkerConfig;
use crate::mask::{feather, BinaryMask, FrameSize};
use crate::patch::ClipPatch;
use crate::worker::WorkerSession;

#[derive(Debug, Error)]
pub enum InpaintError {
    #[error("修复后端调用失败：{0}")]
    Backend(String),
    #[error("修复后端返回的帧数不符：期望 {expected}，实际 {actual}")]
    FrameCount { expected: usize, actual: usize },
    #[error("修复后端返回的帧尺寸不符：期望 {}x{}，实际 {actual} 字节", expected.width, expected.height)]
    FrameBytes { expected: FrameSize, actual: usize },
    #[error("掩码尺寸与片段尺寸不符：掩码 {}x{}，片段 {}x{}", mask.width, mask.height, patch.width, patch.height)]
    MaskSize { mask: FrameSize, patch: FrameSize },
    #[error("修复后端 I/O 失败：{0}")]
    Io(#[from] std::io::Error),
}

/// 修复后端：就地修复片段中被掩码标记的像素。
pub trait Inpainter {
    /// `feather_radius` 为合成时的边缘羽化半径。
    fn inpaint(
        &self,
        patch: &mut ClipPatch,
        mask: &BinaryMask,
        feather_radius: u32,
    ) -> Result<(), InpaintError>;

    /// 用**逐帧掩码**修复：`masks[i]` 只作用于 `patch.frames[i]`。
    ///
    /// 默认实现退化为「所有帧共用一个并集掩码」，这是目前 worker 协议
    /// （单 `mask.pgm`）的语义；支持逐帧掩码的后端重写本方法后，
    /// 掩码边界内的原始像素不会被无谓改写，模型上下文也更干净。
    fn inpaint_frames(
        &self,
        patch: &mut ClipPatch,
        masks: &[BinaryMask],
        feather_radius: u32,
    ) -> Result<(), InpaintError> {
        if masks.len() != patch.len() {
            return Err(InpaintError::FrameCount {
                expected: patch.len(),
                actual: masks.len(),
            });
        }
        let mask = union_masks(masks)
            .ok_or_else(|| InpaintError::Backend("逐帧掩码全部为空，没有需要修复的像素".into()))?;
        self.inpaint(patch, &mask, feather_radius)
    }
}

/// 按位或合并多个掩码；空输入或尺寸不一致时返回 `None`。
pub fn union_masks(masks: &[BinaryMask]) -> Option<BinaryMask> {
    let first = masks.first()?;
    let size = first.size();
    let mut out = BinaryMask::empty(size);
    let mut any = false;
    for mask in masks {
        if mask.size() != size {
            continue;
        }
        for y in 0..size.height {
            for x in 0..size.width {
                if mask.get(x, y) {
                    out.set(x, y, true);
                    any = true;
                }
            }
        }
    }
    any.then_some(out)
}

/// 列填充修复（本地基线后端，非质量方案）。
///
/// 对 ROI 内每一列，取掩码外侧紧邻的若干行做逐通道中位数填充，
/// 再按羽化 alpha 与原画面合成。
///
/// 局限：复杂背景会出现竖向拖影，运动镜头没有时序补偿；
/// 它的价值是在没有 GPU / 模型时把「检测 → 掩码 → 修复 → 回贴」跑通。
#[derive(Debug, Clone, Copy)]
pub struct ColumnFillInpainter {
    /// 采样行数，至少 1。
    pub sample_rows: u32,
}

impl Default for ColumnFillInpainter {
    fn default() -> Self {
        Self { sample_rows: 4 }
    }
}

impl ColumnFillInpainter {
    /// 用单个掩码修复一帧（列中位数填充 + 羽化合成）。
    fn fill_frame(
        &self,
        frame: &mut [u8],
        size: FrameSize,
        mask: &BinaryMask,
        feather_radius: u32,
    ) -> Result<(), InpaintError> {
        if size != mask.size() {
            return Err(InpaintError::MaskSize {
                mask: mask.size(),
                patch: size,
            });
        }
        let width = size.width as usize;
        let height = size.height as usize;
        let frame_bytes = crate::frames::frame_bytes(size);
        let sample_rows = self.sample_rows.max(1) as usize;
        let alpha = feather(mask, feather_radius);

        if frame.len() < frame_bytes {
            return Err(InpaintError::FrameBytes {
                expected: size,
                actual: frame.len(),
            });
        }
        let mut filled = frame.to_vec();

        for x in 0..width {
            let masked_rows: Vec<usize> = (0..height)
                .filter(|y| mask.get(x as u32, *y as u32))
                .collect();
            if masked_rows.is_empty() {
                continue;
            }

            // 优先从掩码上方向上采样；上方不足时改为向下采样。
            let mut source_rows: Vec<usize> = Vec::new();
            let mut cursor = masked_rows[0];
            while cursor > 0 && source_rows.len() < sample_rows {
                cursor -= 1;
                if !mask.get(x as u32, cursor as u32) {
                    source_rows.push(cursor);
                }
            }
            if source_rows.is_empty() {
                cursor = masked_rows[masked_rows.len() - 1] + 1;
                while cursor < height && source_rows.len() < sample_rows {
                    if !mask.get(x as u32, cursor as u32) {
                        source_rows.push(cursor);
                    }
                    cursor += 1;
                }
            }
            if source_rows.is_empty() {
                // 该列全部被掩码覆盖，没有可用参考像素
                continue;
            }

            let mut color = [0_u8; 3];
            for (channel, value) in color.iter_mut().enumerate() {
                let mut samples: Vec<u8> = source_rows
                    .iter()
                    .map(|y| frame[(y * width + x) * 3 + channel])
                    .collect();
                *value = median(&mut samples);
            }
            for y in &masked_rows {
                let index = (y * width + x) * 3;
                filled[index..index + 3].copy_from_slice(&color);
            }
        }

        crate::mask::blend_bgr(frame, &filled, &alpha);
        Ok(())
    }
}

impl Inpainter for ColumnFillInpainter {
    fn inpaint(
        &self,
        patch: &mut ClipPatch,
        mask: &BinaryMask,
        feather_radius: u32,
    ) -> Result<(), InpaintError> {
        let size = patch.size;
        for frame in patch.frames.iter_mut() {
            self.fill_frame(frame, size, mask, feather_radius)?;
        }
        Ok(())
    }

    fn inpaint_frames(
        &self,
        patch: &mut ClipPatch,
        masks: &[BinaryMask],
        feather_radius: u32,
    ) -> Result<(), InpaintError> {
        if masks.len() != patch.len() {
            return Err(InpaintError::FrameCount {
                expected: patch.len(),
                actual: masks.len(),
            });
        }
        let size = patch.size;
        for (frame, mask) in patch.frames.iter_mut().zip(masks.iter()) {
            self.fill_frame(frame, size, mask, feather_radius)?;
        }
        Ok(())
    }
}

/// 取中位数；偶数个样本时取偏上的中位。
fn median(values: &mut [u8]) -> u8 {
    values.sort_unstable();
    values[values.len() / 2]
}

/// 通过外部进程（Python worker）做修复。
///
/// 交换协议（全部在 `work_dir` 内，便于人工复现与调试）：
/// - 输入：`in/frame_%04d.raw`（ROI 尺寸 bgr24）、`mask.pgm`（P5，0/255）、`request.json`
/// - 输出：`out/frame_%04d.raw`，帧数与尺寸必须与输入一致
///
/// 模型只负责 ROI 内的像素，回贴与羽化由 Rust 侧完成。
pub struct WorkerInpainter {
    pub config: WorkerConfig,
    pub work_dir: PathBuf,
    /// 可选的常驻 worker 会话；`None` 时每个片段单独起进程。
    session: Option<Arc<Mutex<WorkerSession>>>,
}

impl WorkerInpainter {
    pub fn new(config: WorkerConfig, work_dir: impl Into<PathBuf>) -> Self {
        Self {
            config,
            work_dir: work_dir.into(),
            session: None,
        }
    }

    /// 复用外部传入的常驻 worker（与检测后端共享同一个进程）。
    pub fn with_session(mut self, session: Arc<Mutex<WorkerSession>>) -> Self {
        self.session = Some(session);
        self
    }

    /// 把 `work_dir` 里的请求交给后端：常驻会话优先，否则一次性起进程。
    fn invoke_backend(&self) -> Result<(), InpaintError> {
        if let Some(session) = &self.session {
            let mut guard = session
                .lock()
                .map_err(|_| InpaintError::Backend("常驻 worker 会话已中毒".into()))?;
            return guard
                .run(&self.work_dir)
                .map_err(|error| InpaintError::Backend(error.to_string()));
        }
        let output = Command::new(&self.config.program)
            .args(&self.config.args)
            .arg(&self.work_dir)
            .output()
            .map_err(|error| {
                InpaintError::Backend(format!(
                    "无法启动修复后端 {}：{error}",
                    self.config.program.display()
                ))
            })?;
        if !output.status.success() {
            return Err(InpaintError::Backend(format!(
                "修复后端退出码 {:?}：{}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }
}

impl Inpainter for WorkerInpainter {
    fn inpaint(
        &self,
        patch: &mut ClipPatch,
        mask: &BinaryMask,
        _feather_radius: u32,
    ) -> Result<(), InpaintError> {
        if patch.size != mask.size() {
            return Err(InpaintError::MaskSize {
                mask: mask.size(),
                patch: patch.size,
            });
        }
        let size = patch.size;
        let frame_bytes = patch.frame_bytes();

        let in_dir = self.work_dir.join("in");
        let out_dir = self.work_dir.join("out");
        if self.work_dir.exists() {
            fs::remove_dir_all(&self.work_dir)?;
        }
        fs::create_dir_all(&in_dir)?;
        fs::create_dir_all(&out_dir)?;

        for (index, frame) in patch.frames.iter().enumerate() {
            if frame.len() < frame_bytes {
                return Err(InpaintError::FrameBytes {
                    expected: size,
                    actual: frame.len(),
                });
            }
            fs::write(in_dir.join(frame_name(index)), &frame[..frame_bytes])?;
        }
        fs::write(
            self.work_dir.join("mask.pgm"),
            encode_pgm(size, mask.data()),
        )?;
        let request = serde_json::json!({
            "mode": "inpaint",
            "width": size.width,
            "height": size.height,
            "frameCount": patch.len(),
            "inputDir": "in",
            "outputDir": "out",
            "maskFile": "mask.pgm",
        });
        fs::write(
            self.work_dir.join("request.json"),
            serde_json::to_vec_pretty(&request)
                .map_err(|error| InpaintError::Backend(format!("请求序列化失败：{error}")))?,
        )?;

        self.invoke_backend()?;

        let mut frames = Vec::with_capacity(patch.len());
        for index in 0..patch.len() {
            let path = out_dir.join(frame_name(index));
            let data = fs::read(&path).map_err(|error| {
                InpaintError::Backend(format!("缺少输出帧 {}：{error}", path.display()))
            })?;
            if data.len() != frame_bytes {
                return Err(InpaintError::FrameBytes {
                    expected: size,
                    actual: data.len(),
                });
            }
            frames.push(data);
        }
        if frames.len() != patch.len() {
            return Err(InpaintError::FrameCount {
                expected: patch.len(),
                actual: frames.len(),
            });
        }
        patch.frames = frames;
        Ok(())
    }
}

fn frame_name(index: usize) -> String {
    format!("frame_{index:04}.raw")
}

/// 编码 P5 灰度 PGM（每像素 1 字节），便于 OpenCV / PIL / imageio 读取。
pub fn encode_pgm(size: FrameSize, data: &[u8]) -> Vec<u8> {
    let mut out = format!("P5\n{} {}\n255\n", size.width, size.height).into_bytes();
    out.extend_from_slice(data);
    out
}

/// 解码 P5 灰度 PGM；只接受 8 位、非注释的规范输入。
pub fn decode_pgm(bytes: &[u8]) -> Result<(FrameSize, Vec<u8>), InpaintError> {
    let mut fields = Vec::new();
    let mut position = 0;
    while fields.len() < 4 && position < bytes.len() {
        while position < bytes.len() && bytes[position].is_ascii_whitespace() {
            position += 1;
        }
        if position < bytes.len() && bytes[position] == b'#' {
            while position < bytes.len() && bytes[position] != b'\n' {
                position += 1;
            }
            continue;
        }
        let start = position;
        while position < bytes.len() && !bytes[position].is_ascii_whitespace() {
            position += 1;
        }
        fields.push(String::from_utf8_lossy(&bytes[start..position]).to_string());
    }
    if fields.len() < 4 || fields[0] != "P5" {
        return Err(InpaintError::Backend("不是合法的 P5 PGM".into()));
    }
    let width: u32 = fields[1]
        .parse()
        .map_err(|_| InpaintError::Backend(format!("PGM 宽度非法：{}", fields[1])))?;
    let height: u32 = fields[2]
        .parse()
        .map_err(|_| InpaintError::Backend(format!("PGM 高度非法：{}", fields[2])))?;
    if fields[3] != "255" {
        return Err(InpaintError::Backend(format!(
            "只支持 8 位 PGM，最大值为 {}",
            fields[3]
        )));
    }
    position += 1; // 跳过头部后的单个空白
    let expected = (width as usize) * (height as usize);
    let data = bytes
        .get(position..position + expected)
        .ok_or_else(|| InpaintError::Backend("PGM 像素数据不完整".into()))?
        .to_vec();
    Ok((FrameSize::new(width, height), data))
}

/// 便捷函数：把检测到的矩形写成 ROI 局部掩码。
pub fn mask_from_rects(roi: crate::mask::MaskRect, rects: &[crate::mask::MaskRect]) -> BinaryMask {
    let mut mask = BinaryMask::empty(FrameSize::new(roi.width, roi.height));
    for rect in rects {
        mask.paint_rect(roi, *rect);
    }
    mask
}

/// 读取 worker 产出的掩码文件（供调试与测试使用）。
pub fn load_mask_file(path: &Path) -> Result<BinaryMask, InpaintError> {
    let bytes = fs::read(path)?;
    let (size, data) = decode_pgm(&bytes)?;
    let mut mask = BinaryMask::empty(size);
    for (index, value) in data.iter().enumerate() {
        if *value > 0 {
            let x = (index % size.width as usize) as u32;
            let y = (index / size.width as usize) as u32;
            mask.set(x, y, true);
        }
    }
    Ok(mask)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mask::MaskRect;

    fn patch_with_subtitle(size: FrameSize, roi: MaskRect, subtitle_rows: (u32, u32)) -> ClipPatch {
        // ROI 内：字幕上方为背景色 40，字幕行填充 250，下方为背景色 40
        let mut patch = ClipPatch::new(size);
        let stride = (size.width as usize) * 3;
        for _ in 0..3 {
            let mut frame = vec![40_u8; stride * size.height as usize];
            for y in subtitle_rows.0..subtitle_rows.1 {
                let start = y as usize * stride;
                frame[start..start + stride].fill(250);
            }
            patch.push(frame);
        }
        let _ = roi;
        patch
    }

    #[test]
    fn column_fill_replaces_masked_pixels_with_background() {
        let size = FrameSize::new(8, 10);
        let roi = MaskRect::new(0, 0, size.width, size.height);
        let mut mask = BinaryMask::empty(size);
        mask.paint_rect(roi, MaskRect::new(0, 4, 8, 3));

        let mut patch = patch_with_subtitle(size, roi, (4, 7));
        ColumnFillInpainter { sample_rows: 3 }
            .inpaint(&mut patch, &mask, 0)
            .unwrap();

        let stride = (size.width as usize) * 3;
        for frame in &patch.frames {
            for x in 0..8 {
                let masked = (4..7).contains(&5) && frame[(5 * stride) + x * 3] == 250;
                assert!(!masked, "字幕行应已被填充");
                // 取上方背景色的中位数，应恢复为 40
                assert_eq!(frame[5 * stride + x * 3], 40, "第 5 行应为背景色");
                assert_eq!(frame[6 * stride + x * 3], 40);
                // 掩码之外的像素保持不变
                assert_eq!(frame[x * 3], 40);
            }
        }
    }

    #[test]
    fn column_fill_falls_back_to_rows_below() {
        let size = FrameSize::new(4, 6);
        let roi = MaskRect::new(0, 0, size.width, size.height);
        let mut mask = BinaryMask::empty(size);
        mask.paint_rect(roi, MaskRect::new(0, 0, 4, 2));

        let stride = (size.width as usize) * 3;
        let mut frame = vec![30_u8; stride * 6];
        for y in 0..2 {
            let start = y * stride;
            frame[start..start + stride].fill(200);
        }
        let mut patch = ClipPatch::new(size);
        patch.push(frame);

        ColumnFillInpainter { sample_rows: 2 }
            .inpaint(&mut patch, &mask, 0)
            .unwrap();
        // 上方没有可用行，应改用下方背景色 30
        assert_eq!(patch.frames[0][0], 30);
    }

    #[test]
    fn column_fill_rejects_size_mismatch() {
        let mut patch = ClipPatch::new(FrameSize::new(4, 4));
        patch.push(vec![0_u8; 4 * 4 * 3]);
        let mask = BinaryMask::empty(FrameSize::new(5, 5));
        let error = ColumnFillInpainter::default()
            .inpaint(&mut patch, &mask, 0)
            .unwrap_err();
        assert!(matches!(error, InpaintError::MaskSize { .. }));
    }

    #[test]
    fn pgm_round_trip() {
        let size = FrameSize::new(3, 2);
        let data = vec![0_u8, 255, 0, 255, 0, 128];
        let encoded = encode_pgm(size, &data);
        let (decoded_size, decoded) = decode_pgm(&encoded).unwrap();
        assert_eq!(decoded_size, size);
        assert_eq!(decoded, data);

        assert!(decode_pgm(b"P2\n1 1\n255\n0\n").is_err(), "只接受 P5");
        assert!(decode_pgm(b"P5\n1 1\n65535\n\0\0").is_err(), "只接受 8 位");
        assert!(decode_pgm(b"P5\n2 2\n255\n\0").is_err(), "像素不足应报错");
    }

    #[test]
    fn worker_protocol_passes_frames_through_shell_stub() {
        // 用 shell 桩替代 Python worker：把 in/*.raw 原样复制到 out/
        let dir =
            std::env::temp_dir().join(format!("dossier-inpaint-worker-{}", std::process::id()));
        let stub = dir.join("stub.sh");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            &stub,
            "#!/bin/sh\nset -e\nmkdir -p \"$1/out\"\ncp \"$1\"/in/*.raw \"$1/out/\"\n",
        )
        .unwrap();

        let size = FrameSize::new(3, 2);
        let roi = MaskRect::new(0, 0, 3, 2);
        let mut mask = BinaryMask::empty(size);
        mask.paint_rect(roi, MaskRect::new(1, 0, 1, 2));

        let mut patch = ClipPatch::new(size);
        patch.push(vec![1_u8; 18]);
        patch.push(vec![2_u8; 18]);

        let worker = WorkerInpainter::new(
            WorkerConfig {
                program: PathBuf::from("sh"),
                args: vec![stub.display().to_string()],
            },
            dir.join("job"),
        );
        worker.inpaint(&mut patch, &mask, 2).unwrap();

        assert_eq!(patch.len(), 2);
        assert_eq!(patch.frames[0][0], 1);
        assert_eq!(patch.frames[1][0], 2);
        // 桩也应当收到规范化的掩码文件
        let mask_file = dir.join("job").join("mask.pgm");
        assert!(mask_file.is_file());
        let request: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("job").join("request.json")).unwrap())
                .unwrap();
        assert_eq!(request["frameCount"], 2);
        assert_eq!(request["width"], 3);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn worker_protocol_reports_missing_output_frames() {
        let dir = std::env::temp_dir().join(format!(
            "dossier-inpaint-worker-fail-{}",
            std::process::id()
        ));
        let stub = dir.join("stub.sh");
        fs::create_dir_all(&dir).unwrap();
        fs::write(&stub, "#!/bin/sh\nmkdir -p \"$1/out\"\n").unwrap();

        let size = FrameSize::new(2, 2);
        let mask = BinaryMask::empty(size);
        let mut patch = ClipPatch::new(size);
        patch.push(vec![0_u8; 12]);

        let worker = WorkerInpainter::new(
            WorkerConfig {
                program: PathBuf::from("sh"),
                args: vec![stub.display().to_string()],
            },
            dir.join("job"),
        );
        let error = worker.inpaint(&mut patch, &mask, 0).unwrap_err();
        assert!(
            error.to_string().contains("缺少输出帧"),
            "错误应指出缺失的帧"
        );

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn union_masks_combines_and_ignores_size_mismatch() {
        let roi = MaskRect::new(0, 0, 4, 4);
        let mut a = BinaryMask::empty(FrameSize::new(4, 4));
        a.paint_rect(roi, MaskRect::new(0, 0, 2, 2));
        let mut b = BinaryMask::empty(FrameSize::new(4, 4));
        b.paint_rect(roi, MaskRect::new(2, 2, 2, 2));

        let merged = union_masks(&[a.clone(), b]).expect("并集应非空");
        assert!(merged.get(0, 0));
        assert!(merged.get(3, 3));

        // 尺寸不一致的掩码被跳过，不会污染结果
        let other = BinaryMask::empty(FrameSize::new(8, 8));
        let merged = union_masks(&[a, other]).expect("仍应有有效掩码");
        assert_eq!(merged.size(), FrameSize::new(4, 4));
        assert!(merged.get(0, 0));
        assert!(!merged.get(3, 3));

        assert!(union_masks(&[]).is_none());
        assert!(union_masks(&[BinaryMask::empty(FrameSize::new(2, 2))]).is_none());
    }

    #[test]
    fn column_fill_honours_per_frame_masks() {
        // 两帧同一 ROI：第 0 帧第 4 行是字幕（250），第 1 帧没有字幕。
        let size = FrameSize::new(4, 6);
        let stride = size.width as usize * 3;
        let mut patch = ClipPatch::new(size);
        for has_subtitle in [true, false] {
            let mut frame = vec![40_u8; stride * size.height as usize];
            if has_subtitle {
                frame[4 * stride..5 * stride].fill(250);
            }
            patch.push(frame);
        }

        let mut first = BinaryMask::empty(size);
        first.paint_rect(MaskRect::new(0, 0, 4, 6), MaskRect::new(0, 4, 4, 1));
        let masks = vec![first, BinaryMask::empty(size)];

        ColumnFillInpainter { sample_rows: 2 }
            .inpaint_frames(&mut patch, &masks, 0)
            .unwrap();

        assert_eq!(patch.frames[0][4 * stride], 40, "第 0 帧字幕应被填充");
        // 第 1 帧掩码为空，任何像素都不应被改写
        assert!(patch.frames[1].iter().all(|value| *value == 40));

        // 掩码数量与帧数不符时报错
        let error = ColumnFillInpainter::default()
            .inpaint_frames(&mut patch, &masks[..1], 0)
            .unwrap_err();
        assert!(matches!(error, InpaintError::FrameCount { .. }));
    }
}
