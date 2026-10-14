//! ffmpeg rawvideo 帧 I/O。
//!
//! 设计：不链接 `libav*`，而是把 ffmpeg 当外部进程使用（与项目既有的
//! 「ffprobe 不捆绑」策略一致）。这样发布体积与许可证都更可控，
//! 也便于在只装了 ffmpeg 的机器上直接验证。

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::mask::FrameSize;

/// 管线内部统一使用 bgr24（每像素 3 字节，行间无填充）。
pub const CHANNELS: usize = 3;

/// bgr24 帧的字节数。
pub fn frame_bytes(size: FrameSize) -> usize {
    (size.width as usize) * (size.height as usize) * CHANNELS
}

/// 原始像素缓冲（bgr24），不保证与任何 `FrameSize` 匹配。
pub type FrameBuffer = Vec<u8>;

/// 读取一帧原始像素。
pub struct FrameReader<R: Read> {
    inner: R,
    size: FrameSize,
    bytes: usize,
}

impl<R: Read> FrameReader<R> {
    pub fn new(inner: R, size: FrameSize) -> Self {
        Self {
            inner,
            size,
            bytes: frame_bytes(size),
        }
    }

    pub fn size(&self) -> FrameSize {
        self.size
    }

    /// 读取一帧；流正常结束时返回 `Ok(None)`。
    ///
    /// 读到「不足一帧」的尾部数据视为流结束，避免把半个字节当像素使用。
    pub fn read_frame(&mut self) -> io::Result<Option<FrameBuffer>> {
        let mut buffer = vec![0_u8; self.bytes];
        if self.read_into(&mut buffer)? {
            Ok(Some(buffer))
        } else {
            Ok(None)
        }
    }

    /// 读取一帧到已有缓冲；返回 `false` 表示流已结束。
    pub fn read_into(&mut self, buffer: &mut FrameBuffer) -> io::Result<bool> {
        if buffer.len() < self.bytes {
            buffer.resize(self.bytes, 0);
        }
        let target = &mut buffer[..self.bytes];
        let mut filled = 0;
        while filled < self.bytes {
            match self.inner.read(&mut target[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(filled == self.bytes)
    }
}

/// ffmpeg 可执行文件位置；`None` 表示从 `PATH` 查找 `ffmpeg`。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FfmpegConfig {
    pub bin: Option<PathBuf>,
}

impl FfmpegConfig {
    pub fn program(&self) -> PathBuf {
        self.bin.clone().unwrap_or_else(|| PathBuf::from("ffmpeg"))
    }
}

/// 构造「解码为 bgr24 rawvideo」的 ffmpeg 命令。
pub fn decoder_command(cfg: &FfmpegConfig, input: &Path) -> Command {
    let mut command = Command::new(cfg.program());
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-nostdin")
        .arg("-i")
        .arg(input)
        .arg("-map")
        .arg("0:v:0")
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg("bgr24")
        .arg("-");
    command
}

/// 构造「读取 bgr24 rawvideo 并编码」的 ffmpeg 命令。
pub fn encoder_command(
    cfg: &FfmpegConfig,
    output: &Path,
    size: FrameSize,
    fps: f64,
    encoder_args: &[String],
    pix_fmt: &str,
) -> Command {
    let mut command = Command::new(cfg.program());
    // 显式插入带 flags 的 scale：libx264 的 BGR→YUV 转换发生在滤镜图里，全局
    // `-sws_flags` 传不进去（实测无效）。默认 flags 的色度下采样比
    // full_chroma_int+accurate_rnd 粗糙：实测同素材下 Y 偏高 1、色度最大差 43，
    // 交付输出相对无损母版低约 1.8 dB。固定 flags 后两条转换路径结果一致。
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-nostdin")
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg("bgr24")
        .arg("-s")
        .arg(format!("{}x{}", size.width, size.height))
        .arg("-r")
        .arg(format!("{fps}"))
        .arg("-i")
        .arg("-")
        .arg("-an")
        .arg("-vf")
        .arg("scale=flags=full_chroma_int+accurate_rnd");
    for arg in encoder_args {
        command.arg(arg);
    }
    command.arg("-pix_fmt").arg(pix_fmt).arg("-y").arg(output);
    command
}

/// 构造「把新视频与原素材的音轨重新封装」的 ffmpeg 命令。
pub fn remux_command(
    cfg: &FfmpegConfig,
    video: &Path,
    audio_source: &Path,
    output: &Path,
) -> Command {
    let mut command = Command::new(cfg.program());
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-nostdin")
        .arg("-i")
        .arg(video)
        .arg("-i")
        .arg(audio_source)
        .arg("-map")
        .arg("0:v:0")
        .arg("-map")
        .arg("1:a?")
        .arg("-c")
        .arg("copy")
        .arg("-movflags")
        .arg("+faststart")
        .arg("-y")
        .arg(output);
    command
}

/// 已启动的 ffmpeg 解码进程。
pub struct FfmpegFrameSource {
    child: Child,
    reader: FrameReader<ChildStdout>,
}

impl FfmpegFrameSource {
    /// 启动 ffmpeg 并按 `size` 读取帧。
    pub fn open(cfg: &FfmpegConfig, input: &Path, size: FrameSize) -> io::Result<Self> {
        let mut child = decoder_command(cfg, input).stdout(Stdio::piped()).spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "无法获取 ffmpeg 标准输出"))?;
        Ok(Self {
            child,
            reader: FrameReader::new(stdout, size),
        })
    }

    pub fn read_frame(&mut self) -> io::Result<Option<FrameBuffer>> {
        self.reader.read_frame()
    }

    /// 等待 ffmpeg 退出，返回其退出码。
    pub fn finish(mut self) -> io::Result<i32> {
        let status = self.child.wait()?;
        Ok(status.code().unwrap_or(-1))
    }
}

impl Drop for FfmpegFrameSource {
    /// 定位阶段只需要前若干帧，读完就丢弃解码器；此时 ffmpeg 仍在往
    /// stdout 写帧。主动结束子进程可避免它在 stderr 打出 "Broken pipe"，
    /// 同时回收进程，防止留下僵尸。
    fn drop(&mut self) {
        // try_wait 返回 Ok(None) 表示仍在运行，此时才需要终止。
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// 已启动的 ffmpeg 编码进程。
pub struct FfmpegFrameSink {
    child: Child,
    stdin: std::process::ChildStdin,
}

impl FfmpegFrameSink {
    pub fn open(
        cfg: &FfmpegConfig,
        output: &Path,
        size: FrameSize,
        fps: f64,
        encoder_args: &[String],
        pix_fmt: &str,
    ) -> io::Result<Self> {
        let mut child = encoder_command(cfg, output, size, fps, encoder_args, pix_fmt)
            .stdin(Stdio::piped())
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "无法获取 ffmpeg 标准输入"))?;
        Ok(Self { child, stdin })
    }

    pub fn write_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        self.stdin.write_all(frame)
    }

    pub fn finish(mut self) -> io::Result<i32> {
        drop(self.stdin);
        let status = self.child.wait()?;
        Ok(status.code().unwrap_or(-1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch;
    use std::io::Cursor;

    #[test]
    fn frame_reader_chunks_exactly() {
        let size = FrameSize::new(4, 2);
        assert_eq!(frame_bytes(size), 24);

        // 两帧 + 半个帧的尾部数据
        let data: Vec<u8> = (0..60).collect();
        let mut reader = FrameReader::new(Cursor::new(data), size);

        let first = reader.read_frame().unwrap().unwrap();
        assert_eq!(first, (0..24).collect::<Vec<u8>>());
        let second = reader.read_frame().unwrap().unwrap();
        assert_eq!(second, (24..48).collect::<Vec<u8>>());
        assert!(
            reader.read_frame().unwrap().is_none(),
            "尾部残帧应按流结束处理"
        );
    }

    #[test]
    fn frame_reader_handles_empty_and_short_streams() {
        let size = FrameSize::new(2, 2);
        let mut empty = FrameReader::new(Cursor::new(Vec::<u8>::new()), size);
        assert!(empty.read_frame().unwrap().is_none());

        let mut short = FrameReader::new(Cursor::new(vec![1_u8, 2, 3]), size);
        assert!(short.read_frame().unwrap().is_none());
    }

    #[test]
    fn read_into_reuses_buffer() {
        let size = FrameSize::new(2, 1);
        let mut reader = FrameReader::new(Cursor::new(vec![7_u8, 8, 9, 10, 11, 12]), size);
        let mut buffer = Vec::new();
        assert!(reader.read_into(&mut buffer).unwrap());
        assert_eq!(buffer, vec![7, 8, 9, 10, 11, 12]);
        let capacity = buffer.capacity();
        assert!(!reader.read_into(&mut buffer).unwrap());
        assert_eq!(buffer.capacity(), capacity, "复用缓冲不应重新分配");
    }

    #[test]
    fn decoder_command_maps_video_to_rawvideo() {
        let cfg = FfmpegConfig::default();
        let command = decoder_command(&cfg, Path::new("in.mp4"));
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();
        assert_eq!(command.get_program().to_string_lossy(), "ffmpeg");
        assert!(args.windows(2).any(|w| w == ["-map", "0:v:0"]));
        assert!(args.windows(2).any(|w| w == ["-f", "rawvideo"]));
        assert!(args.windows(2).any(|w| w == ["-pix_fmt", "bgr24"]));
        assert_eq!(args.last().unwrap(), "-");
    }

    #[test]
    fn encoder_command_declares_input_geometry_and_drops_audio() {
        let cfg = FfmpegConfig {
            bin: Some(PathBuf::from("/opt/ffmpeg/bin/ffmpeg")),
        };
        let encoder_args = vec![
            "-c:v".to_string(),
            "libx264".to_string(),
            "-crf".to_string(),
            "16".to_string(),
        ];
        let command = encoder_command(
            &cfg,
            Path::new("out.mp4"),
            FrameSize::new(640, 360),
            30.0,
            &encoder_args,
            "yuv420p",
        );
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            command.get_program().to_string_lossy(),
            "/opt/ffmpeg/bin/ffmpeg"
        );
        assert!(args.windows(2).any(|w| w == ["-s", "640x360"]));
        assert!(args.windows(2).any(|w| w == ["-r", "30"]));
        assert!(args.contains(&"-an".to_string()), "编码阶段应丢弃音轨");
        assert!(args.contains(&"libx264".to_string()));
        assert_eq!(args.last().unwrap(), "out.mp4");
        assert!(args.windows(2).any(|w| w == ["-pix_fmt", "yuv420p"]));
    }

    #[test]
    fn encoder_command_pins_swscale_flags_for_the_bgr_to_yuv_conversion() {
        // 不固定 flags 时，同一份 bgr24 帧经管道与经容器的转换结果不同：Y 差 1、
        // 色度最大差 43，交付相对无损母版低约 1.8 dB。全局 `-sws_flags` 对 libx264
        // 无效，必须用显式 scale 滤镜钉住。
        let cfg = FfmpegConfig::default();
        let encoder_args = vec!["-c:v".to_string(), "libx264".to_string()];
        let command = encoder_command(
            &cfg,
            Path::new("out.mp4"),
            FrameSize::new(640, 360),
            25.0,
            &encoder_args,
            "yuv420p",
        );
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();
        assert!(
            args.windows(2)
                .any(|w| w == ["-vf", "scale=flags=full_chroma_int+accurate_rnd"]),
            "应显式钉住 swscale flags：{args:?}"
        );
        assert!(
            !args.contains(&"-sws_flags".to_string()),
            "全局 -sws_flags 对 libx264 无效，不要用它：{args:?}"
        );
    }

    #[test]
    fn remux_keeps_original_audio_track() {
        let cfg = FfmpegConfig::default();
        let command = remux_command(
            &cfg,
            Path::new("fixed.mp4"),
            Path::new("original.mp4"),
            Path::new("final.mp4"),
        );
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();
        assert!(args.windows(2).any(|w| w == ["-map", "0:v:0"]));
        assert!(
            args.windows(2).any(|w| w == ["-map", "1:a?"]),
            "音轨映射必须允许源素材没有音频"
        );
        assert!(args.windows(2).any(|w| w == ["-c", "copy"]));
    }

    #[test]
    fn crop_and_paste_round_trip_through_patch() {
        let frame_size = FrameSize::new(4, 3);
        // 每像素编码为 (x, y, 0)，便于核对裁剪位置
        let mut frame = vec![0_u8; frame_bytes(frame_size)];
        for y in 0..3_u32 {
            for x in 0..4_u32 {
                let index = ((y as usize) * 4 + x as usize) * 3;
                frame[index] = x as u8;
                frame[index + 1] = y as u8;
            }
        }

        let roi = crate::mask::MaskRect::new(1, 1, 2, 2);
        let patch = patch::crop_roi(&frame, frame_size, roi);
        assert_eq!(patch.len(), 2 * 2 * 3);
        assert_eq!(&patch[0..3], &[1, 1, 0], "裁剪起点应为 ROI 左上角");
        assert_eq!(&patch[3..6], &[2, 1, 0]);
        assert_eq!(&patch[6..9], &[1, 2, 0]);

        // 全量回贴（alpha 全 255）
        let replacement = vec![9_u8; patch.len()];
        let alpha = vec![255_u8; 2 * 2];
        patch::paste_roi(&mut frame, frame_size, roi, &replacement, Some(&alpha));
        let at = |x: usize, y: usize| frame[(y * 4 + x) * 3];
        assert_eq!(at(1, 1), 9);
        assert_eq!(at(2, 2), 9);
        assert_eq!(at(0, 0), 0, "ROI 之外不应被改写");
        assert_eq!(at(3, 0), 3);
    }
}
