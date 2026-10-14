//! dossier-inpaint 命令行：在真实文件上跑硬字幕擦除管线。
//!
//! 示例：
//! ```text
//! cargo run -p dossier-inpaint -- \
//!   --input sub.mp4 --output fixed.mp4 --roi 120,300,400,40
//! ```
//!
//! 用途仅限技术学习与处理自有素材：可清理自有素材的硬字幕与角标/台标，但不做
//! DRM 绕过，也不处理他人作品的版权标识。

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use dossier_inpaint::detect::{FixedRoiDetector, TextDetector, WorkerConfig, WorkerTextDetector};
use dossier_inpaint::frames::{FfmpegConfig, FfmpegFrameSink, FfmpegFrameSource};
use dossier_inpaint::inpaint::{ColumnFillInpainter, Inpainter, WorkerInpainter};
use dossier_inpaint::mask::MaskRect;
use dossier_inpaint::pipeline::{run_pipeline, PipelineConfig};
use dossier_inpaint::probe::probe_video;

#[derive(Parser, Debug)]
#[command(
    name = "dossier-inpaint",
    about = "字幕 / 角标擦除管线（逐片段检测 + 时序修复 + 回贴）",
    long_about = None
)]
struct Args {
    /// 输入视频
    #[arg(long)]
    input: PathBuf,
    /// 输出视频（音轨从输入素材直接复制）
    #[arg(long)]
    output: PathBuf,
    /// 固定区域（字幕或角标），格式 x,y,w,h；配合 --detector fixed
    #[arg(long, value_parser = parse_roi, allow_hyphen_values = true)]
    roi: Option<MaskRect>,
    /// 检测后端：fixed（需 --roi）或 worker（DBNet Python worker）
    #[arg(long, default_value = "fixed")]
    detector: String,
    /// 修复后端：column（本地基线，无需模型）或 worker（E2FGVI Python worker）
    #[arg(long, default_value = "column")]
    inpainter: String,
    /// worker 可执行文件；worker 模式必填（如 python3）
    #[arg(long)]
    worker_program: Option<String>,
    /// worker 参数，可重复或用逗号分隔（如 tools/model_worker/worker.py）
    #[arg(long, value_delimiter = ',')]
    worker_args: Vec<String>,
    /// ffmpeg 可执行文件路径；缺省从 PATH 查找
    #[arg(long)]
    ffmpeg: Option<PathBuf>,
    /// ffprobe 可执行文件路径；缺省从 PATH 查找
    #[arg(long)]
    ffprobe: Option<PathBuf>,
    /// 覆盖素材帧率（素材帧率不可信时使用）
    #[arg(long)]
    fps: Option<f64>,
    /// 每个片段的帧数
    #[arg(long, default_value_t = 30)]
    clip_frames: usize,
    /// 定位阶段读取并检测的帧数
    #[arg(long, default_value_t = 60)]
    probe_frames: usize,
    /// 掩码膨胀半径
    #[arg(long, default_value_t = 4)]
    dilate: u32,
    /// 回贴羽化半径
    #[arg(long, default_value_t = 2)]
    feather: u32,
    /// ROI 相对检测框的外扩比例
    #[arg(long, default_value_t = 1.6)]
    context: f64,
    /// 检测置信度下限
    #[arg(long, default_value_t = 0.5)]
    min_score: f32,
    /// 视频编码器
    #[arg(long, default_value = "libx264")]
    vcodec: String,
    /// 编码 CRF（越小越清晰、体积越大；`0` 为 x264 无损）
    #[arg(long, default_value_t = 16)]
    crf: u32,
    /// 编码 preset（无损中间态不使用）
    #[arg(long, default_value = "medium")]
    preset: String,
    /// 编码 tune（如 `grain`，用于保留颗粒/抑制压缩伪影复发；可选）
    #[arg(long)]
    tune: Option<String>,
    /// 中间态策略：`delivery` = 直接按交付参数编码一次；`lossless` = 先无损中间态、
    /// 最后再单独做一次交付编码（多留一代画质，代价是磁盘与一次额外转码）
    #[arg(long, default_value = "delivery")]
    intermediate: String,
    /// 无损中间态的像素格式（仅 `--intermediate lossless` 生效）
    #[arg(long, default_value = "bgr0")]
    intermediate_pix_fmt: String,
    /// 输出像素格式。
    ///
    /// 源素材多为 yuv420p：解码时色度被上采样，若再编码成 yuv420p 就会经历
    /// 第二次色度抽样。角标/水印这类彩色小目标对色度偏移很敏感，此时应改用
    /// `yuv444p`（配 `--crf 0` 可做到无损中间态）。
    #[arg(long, default_value = "yuv420p")]
    pix_fmt: String,
    /// 保留中间文件（临时视频与 worker 交换目录）
    #[arg(long)]
    keep_temp: bool,
}

fn parse_roi(value: &str) -> Result<MaskRect, String> {
    let parts: Vec<&str> = value.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        return Err("区域格式应为 x,y,w,h，例如 120,300,400,40".into());
    }
    let mut numbers = [0_u32; 4];
    for (index, part) in parts.iter().enumerate() {
        numbers[index] = part.parse().map_err(|_| format!("区域数值非法：{part}"))?;
    }
    let rect = MaskRect::new(numbers[0], numbers[1], numbers[2], numbers[3]);
    if rect.is_empty() {
        return Err("区域宽高必须大于 0".into());
    }
    Ok(rect)
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("错误：{message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<(), String> {
    let lossless_intermediate = match args.intermediate.as_str() {
        "delivery" => false,
        "lossless" => true,
        other => {
            return Err(format!("未知中间态模式 {other}；可选 delivery 或 lossless"));
        }
    };
    let ffmpeg = FfmpegConfig {
        bin: args.ffmpeg.clone(),
    };
    let info =
        probe_video(args.ffprobe.as_deref(), &args.input).map_err(|error| error.to_string())?;
    let fps = info
        .fps
        .or(args.fps)
        .filter(|value| *value > 0.0)
        .ok_or_else(|| "无法从素材确定帧率，请用 --fps 指定（本工具按固定帧率处理）".to_string())?;
    println!(
        "素材：{}x{} @ {:.3}fps，音频：{}",
        info.size.width,
        info.size.height,
        fps,
        if info.has_audio { "有" } else { "无" }
    );

    let temp_dir = args.output.with_extension("inpaint-tmp");
    // 无损中间态用 matroska 承载 FFV1；交付模式直接写 mp4。
    let temp_video = temp_dir.join(if lossless_intermediate {
        "video.mkv"
    } else {
        "video.mp4"
    });
    if temp_dir.exists() {
        std::fs::remove_dir_all(&temp_dir).map_err(|error| format!("无法清理临时目录：{error}"))?;
    }
    std::fs::create_dir_all(&temp_dir).map_err(|error| format!("无法创建临时目录：{error}"))?;

    let detector = build_detector(args, &temp_dir)?;
    let inpainter = build_inpainter(args, &temp_dir)?;

    let config = PipelineConfig {
        clip_frames: args.clip_frames,
        probe_frames: args.probe_frames,
        detect_min_score: args.min_score,
        mask_dilate: args.dilate,
        feather_radius: args.feather,
        context_scale: args.context,
        ..Default::default()
    };

    let (encoder_args, encoder_pix_fmt) = if lossless_intermediate {
        (
            vec!["-c:v".to_string(), "ffv1".to_string()],
            args.intermediate_pix_fmt.clone(),
        )
    } else {
        (delivery_encoder_args(args), args.pix_fmt.clone())
    };
    let mut sink = FfmpegFrameSink::open(
        &ffmpeg,
        &temp_video,
        info.size,
        fps,
        &encoder_args,
        &encoder_pix_fmt,
    )
    .map_err(|error| format!("无法启动编码器：{error}"))?;

    let report = run_pipeline(
        || FfmpegFrameSource::open(&ffmpeg, &args.input, info.size),
        &mut sink,
        info.size,
        detector.as_ref(),
        inpainter.as_ref(),
        &config,
    )
    .map_err(|error| format!("管线执行失败：{error}"))?;

    let code = sink
        .finish()
        .map_err(|error| format!("等待编码器失败：{error}"))?;
    if code != 0 {
        return Err(format!(
            "编码器退出码 {code}，可加 --keep-temp 保留中间文件排查"
        ));
    }

    match report.roi {
        Some(roi) => println!(
            "字幕区域：{}x{} @ ({}, {})",
            roi.width, roi.height, roi.x, roi.y
        ),
        None => println!("未检测到字幕；输出为原样重编码"),
    }
    println!(
        "帧数：{} → {}，片段：{}（其中修复 {}），掩码平均覆盖：{:.2}%",
        report.frames_in,
        report.frames_out,
        report.clips,
        report.inpainted_clips,
        report.mean_mask_coverage * 100.0
    );

    let mux_label = if lossless_intermediate {
        "交付编码"
    } else {
        "重新封装"
    };
    let mut mux = std::process::Command::new(ffmpeg.program());
    mux.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .arg("-i")
        .arg(&temp_video)
        .arg("-i")
        .arg(&args.input)
        .args(["-map", "0:v:0", "-map", "1:a?"]);
    if lossless_intermediate {
        // 中间态是 FFV1 无损，这是整条链路唯一一次有损交付编码。
        mux.args(delivery_encoder_args(args))
            .args(["-pix_fmt", args.pix_fmt.as_str()]);
    } else {
        mux.args(["-c", "copy"]);
    }
    let status = mux
        .args(["-movflags", "+faststart", "-y"])
        .arg(&args.output)
        .status()
        .map_err(|error| format!("{mux_label}失败：{error}"))?;
    if !status.success() {
        return Err(format!("{mux_label}失败（退出码 {:?}）", status.code()));
    }

    println!("输出：{}", args.output.display());
    if args.keep_temp {
        println!("中间文件保留在：{}", temp_dir.display());
    } else {
        std::fs::remove_dir_all(&temp_dir).ok();
    }
    Ok(())
}

/// 交付编码参数：`--intermediate delivery` 时用于中间态，`lossless` 时用于最后一步。
fn delivery_encoder_args(args: &Args) -> Vec<String> {
    let mut encoder_args = vec![
        "-c:v".to_string(),
        args.vcodec.clone(),
        "-preset".to_string(),
        args.preset.clone(),
    ];
    if let Some(tune) = &args.tune {
        encoder_args.push("-tune".to_string());
        encoder_args.push(tune.clone());
    }
    encoder_args.push("-crf".to_string());
    encoder_args.push(args.crf.to_string());
    encoder_args
}

fn build_detector(
    args: &Args,
    temp_dir: &std::path::Path,
) -> Result<Box<dyn TextDetector>, String> {
    match args.detector.as_str() {
        "fixed" => {
            let roi = args
                .roi
                .ok_or_else(|| "--detector fixed 需要用 --roi x,y,w,h 指定字幕区域".to_string())?;
            Ok(Box::new(FixedRoiDetector::new(roi)))
        }
        "worker" => Ok(Box::new(WorkerTextDetector::new(
            worker_config(args)?,
            temp_dir.join("detect"),
        ))),
        other => Err(format!("未知检测后端 {other}；可选 fixed 或 worker")),
    }
}

fn build_inpainter(args: &Args, temp_dir: &std::path::Path) -> Result<Box<dyn Inpainter>, String> {
    match args.inpainter.as_str() {
        "column" => Ok(Box::new(ColumnFillInpainter { sample_rows: 4 })),
        "worker" => Ok(Box::new(WorkerInpainter::new(
            worker_config(args)?,
            temp_dir.join("inpaint"),
        ))),
        other => Err(format!("未知修复后端 {other}；可选 column 或 worker")),
    }
}

fn worker_config(args: &Args) -> Result<WorkerConfig, String> {
    let program = args.worker_program.clone().ok_or_else(|| {
        "worker 模式需要用 --worker-program 指定解释器或可执行文件（例如 python3）".to_string()
    })?;
    Ok(WorkerConfig {
        program: PathBuf::from(program),
        args: args.worker_args.clone(),
    })
}
