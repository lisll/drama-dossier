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
use std::sync::{Arc, Mutex};

use clap::Parser;

use dossier_core::checkpoint::{input_fingerprint, ArtifactRef};
use dossier_core::pipeline_version::{pipeline_version, ArtifactStamp, ToolStamp, ERASE_PIPELINE};
use dossier_inpaint::detect::{
    FixedRoiDetector, FixedRoisDetector, TextDetector, WorkerConfig, WorkerTextDetector,
};
use dossier_inpaint::frames::{FfmpegConfig, FfmpegFrameSink, FfmpegFrameSource};
use dossier_inpaint::inpaint::{ColumnFillInpainter, Inpainter, WorkerInpainter};
use dossier_inpaint::mask::MaskRect;
use dossier_inpaint::pipeline::{run_pipeline, PipelineConfig, ProgressStream};
use dossier_inpaint::probe::probe_video;
use dossier_inpaint::verify::{Verdict, VerifyConfig};
use dossier_inpaint::worker::WorkerSession;

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
    /// 固定区域（字幕或角标），格式 x,y,w,h；可重复给出以支持多个标识。
    ///
    /// 给出后管线会跳过定位阶段，直接在这些区域内生成掩码；
    /// 配合 `--detector worker` 时还可以只在这些区域内采纳模型检测框。
    #[arg(long, value_parser = parse_roi, allow_hyphen_values = true)]
    roi: Vec<MaskRect>,
    /// 只处理从该时间点（秒）开始的画面；之前的帧原样输出
    #[arg(long, allow_hyphen_values = true)]
    start: Option<f64>,
    /// 处理时长（秒）；与 `--start` 配合限定时间窗
    #[arg(long, allow_hyphen_values = true)]
    duration: Option<f64>,
    /// 只处理从该帧开始的画面（优先于 `--start`）
    #[arg(long)]
    start_frame: Option<u64>,
    /// 只处理到该帧之前（不含该帧；优先于 `--duration`）
    #[arg(long)]
    end_frame: Option<u64>,
    /// 掩码时序平滑半径（帧）：相邻帧掩码取并集，抑制漏检导致的闪烁
    #[arg(long, default_value_t = 1)]
    temporal_smooth: u32,
    /// 检测框必须落在区域内的判定外扩量（像素）
    #[arg(long, default_value_t = 8)]
    region_pad: u32,
    /// 区域分组阈值：合并后的外扩 ROI 面积最多是「各自独立处理面积之和」的多少倍。
    ///
    /// 两处标识离得远时，外接 ROI 会框进大片无关画面；此时按阈值拆成多个
    /// 裁剪作业分别送模型，速度与上下文都会更好。`<= 1.0` 表示不分组。
    #[arg(long, default_value_t = 2.0, allow_hyphen_values = true)]
    region_group_area_ratio: f64,
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
    ///
    /// `allow_hyphen_values`：worker 自己的开关（如 `--serve`、`--e2fgvi-repo`）也以
    /// 连字符开头，必须能作为一个值传进来，否则会被当成本工具的未知参数。
    #[arg(long, value_delimiter = ',', allow_hyphen_values = true)]
    worker_args: Vec<String>,
    /// 常驻 worker：整个视频只启动一次后端进程，把解释器启动与模型加载
    /// 摊到所有帧/片段上（逐帧检测时收益最明显）。
    #[arg(long)]
    persistent_worker: bool,
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
    /// 关闭擦除结果自动校验（默认开启）。
    ///
    /// 校验会在输出帧上重跑检测并计算残留 / 闪烁 / 接缝 / 一致性指标，
    /// 结论写入处理日志（`--report-json`），并按结论设置退出码。
    #[arg(long)]
    no_verify: bool,
    /// 残留文本像素占比 警告阈值（0–1）
    #[arg(long, default_value_t = VerifyConfig::default().residual_ratio_warn)]
    verify_residual_ratio_warn: f64,
    /// 残留文本像素占比 失败阈值（0–1）
    #[arg(long, default_value_t = VerifyConfig::default().residual_ratio_fail)]
    verify_residual_ratio_fail: f64,
    /// 闪烁指数 均值警告阈值
    #[arg(long, default_value_t = VerifyConfig::default().flicker_index_warn)]
    verify_flicker_warn: f64,
    /// 闪烁指数 峰值失败阈值
    #[arg(long, default_value_t = VerifyConfig::default().flicker_index_fail)]
    verify_flicker_fail: f64,
    /// 接缝分数 均值警告阈值
    #[arg(long, default_value_t = VerifyConfig::default().seam_score_warn)]
    verify_seam_warn: f64,
    /// 接缝分数 峰值失败阈值
    #[arg(long, default_value_t = VerifyConfig::default().seam_score_fail)]
    verify_seam_fail: f64,
    /// 掩码内一致性 PSNR 均值警告阈值（dB）
    #[arg(long, default_value_t = VerifyConfig::default().psnr_warn_db)]
    verify_psnr_warn: f64,
    /// 掩码内一致性 PSNR 最低失败阈值（dB）
    #[arg(long, default_value_t = VerifyConfig::default().psnr_fail_db)]
    verify_psnr_fail: f64,
    /// 掩码内结构相似度（SSIM）均值 警告阈值
    #[arg(long, default_value_t = VerifyConfig::default().ssim_warn)]
    verify_ssim_warn: f64,
    /// 掩码内结构相似度（SSIM）最低 失败阈值
    #[arg(long, default_value_t = VerifyConfig::default().ssim_fail)]
    verify_ssim_fail: f64,
    /// 处理日志（结构化 JSON）输出路径；缺省写到 `<输出>.inpaint-report.json`
    #[arg(long)]
    report_json: Option<PathBuf>,
    /// 产物戳记（管线版本 / 工具版本 / 参数与输入摘要）输出路径；
    /// 缺省写到 `<输出>.stamp.json`。给定了这个路径时写入失败会让整次执行失败。
    #[arg(long)]
    stamp_json: Option<PathBuf>,
    /// 不写产物戳记。默认**写**：没有戳记，事后就无法回答「这批产物是不是同一套流程出的」。
    #[arg(long)]
    no_stamp: bool,
    /// 进度流输出路径（JSONL，覆盖写）。
    ///
    /// 擦除是逐帧长任务，只靠 stdout 看不到「跑到哪了」。给出这个路径后，管线会把
    /// 「定位 / 修复」两个阶段与逐片段计数按 `dossier-core` 的进度格式追加写进去，
    /// 桌面端与云端 tail 这个文件即可显示真实进度；进程被 kill 也只丢最后半行。
    #[arg(long)]
    progress_json: Option<PathBuf>,
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

/// 参数解析失败时的退出码：clap 默认用法错误退出码是 2，会与本工具的
/// 「自动校验失败」（2）撞码，让上游把「参数写错」误判成「画面没擦干净」。
/// 这里统一映射成 1（执行错误），保证 0 / 2 / 3 的确认流语义唯一；
/// `--help` / `--version` 不是错误，仍为 0。
fn parse_error_exit_code(error: &clap::Error) -> ExitCode {
    ExitCode::from(if error.use_stderr() { 1 } else { 0 })
}

fn main() -> ExitCode {
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(error) => {
            let _ = error.print();
            return parse_error_exit_code(&error);
        }
    };
    match run(&args) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("错误：{message}");
            ExitCode::FAILURE
        }
    }
}

/// 确认流退出码：`0` 自动确认通过，`3` 需要人工抽查，`2` 自动校验失败
/// （应触发自动重试或转人工复核），`1` 为执行错误。
fn verdict_exit_code(verdict: Option<Verdict>) -> ExitCode {
    match verdict {
        Some(Verdict::Fail) => ExitCode::from(2),
        Some(Verdict::Warn) => ExitCode::from(3),
        _ => ExitCode::SUCCESS,
    }
}

fn verify_config_from_args(args: &Args) -> VerifyConfig {
    let defaults = VerifyConfig::default();
    VerifyConfig {
        residual_ratio_warn: args.verify_residual_ratio_warn,
        residual_ratio_fail: args.verify_residual_ratio_fail,
        flicker_index_warn: args.verify_flicker_warn,
        flicker_index_fail: args.verify_flicker_fail,
        seam_score_warn: args.verify_seam_warn,
        seam_score_fail: args.verify_seam_fail,
        psnr_warn_db: args.verify_psnr_warn,
        psnr_fail_db: args.verify_psnr_fail,
        ssim_warn: args.verify_ssim_warn,
        ssim_fail: args.verify_ssim_fail,
        ..defaults
    }
}

fn run(args: &Args) -> Result<ExitCode, String> {
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

    let session = start_worker_session(args)?;
    let detector = build_detector(args, &temp_dir, session.clone())?;
    let inpainter = build_inpainter(args, &temp_dir, session)?;
    let (start_frame, end_frame) = frame_window(args, fps)?;
    if let (Some(start), Some(end)) = (start_frame, end_frame) {
        println!("处理窗口：帧 [{start}, {end})");
    }
    let progress = args.progress_json.clone().map(|path| ProgressStream {
        path,
        task_id: args
            .output
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "dossier-inpaint".to_string()),
        pipeline: ERASE_PIPELINE.to_string(),
        est_total_frames: estimate_window_frames(info.duration_sec, fps, start_frame, end_frame),
    });
    if let Some(progress) = &progress {
        println!("进度流：{}", progress.path.display());
    }

    let config = PipelineConfig {
        clip_frames: args.clip_frames,
        probe_frames: args.probe_frames,
        detect_min_score: args.min_score,
        mask_dilate: args.dilate,
        feather_radius: args.feather,
        context_scale: args.context,
        verify: (!args.no_verify).then(|| verify_config_from_args(args)),
        regions: args.roi.clone(),
        region_pad: args.region_pad,
        region_group_area_ratio: args.region_group_area_ratio,
        temporal_smooth: args.temporal_smooth,
        start_frame,
        end_frame,
        progress,
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
            "标识区域：{} 个，外接 ROI {}x{} @ ({}, {})",
            report.regions.len(),
            roi.width,
            roi.height,
            roi.x,
            roi.y
        ),
        None => println!("未检测到标识；输出为原样重编码"),
    }
    for (index, region) in report.regions.iter().enumerate() {
        println!(
            "  区域 {}：{}x{} @ ({}, {})",
            index + 1,
            region.width,
            region.height,
            region.x,
            region.y
        );
    }
    println!(
        "帧数：{} → {}，片段：{}（其中修复 {}），掩码平均覆盖：{:.2}%",
        report.frames_in,
        report.frames_out,
        report.clips,
        report.inpainted_clips,
        report.mean_mask_coverage * 100.0
    );
    println!(
        "区域分组：{} 组，提交修复后端 {} 次",
        report.roi_jobs, report.inpaint_batches
    );

    print_verification(&report);

    let report_path = args
        .report_json
        .clone()
        .unwrap_or_else(|| args.output.with_extension("inpaint-report.json"));
    write_report(&report_path, &report)?;
    println!("处理日志：{}", report_path.display());

    let mux_label = if lossless_intermediate {
        "交付编码"
    } else {
        "重新封装"
    };
    let mut mux = std::process::Command::new(ffmpeg.program());
    // Windows 的 GUI 进程拉起 ffmpeg 会弹黑色 cmd 窗口，CREATE_NO_WINDOW 静默运行。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        mux.creation_flags(0x0800_0000);
    }
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
    if !args.no_stamp {
        match write_stamp(
            args,
            &info,
            fps,
            start_frame,
            end_frame,
            &report_path,
            lossless_intermediate,
        ) {
            Ok(path) => println!("产物戳记：{}", path.display()),
            Err(message) if args.stamp_json.is_some() => {
                return Err(format!("产物戳记写入失败：{message}"));
            }
            Err(message) => {
                // 默认路径写不进去（例如输出目录只读）不该把已经跑完的擦除判成失败，
                // 但必须让调用方看见——静默丢戳记会让「产物一致性校验」变成一句空话。
                eprintln!("警告：产物戳记写入失败：{message}（本次产物没有版本戳记）");
            }
        }
    }
    if args.keep_temp {
        println!("中间文件保留在：{}", temp_dir.display());
    } else {
        std::fs::remove_dir_all(&temp_dir).ok();
    }
    Ok(verdict_exit_code(
        report.verification.as_ref().map(|report| report.verdict),
    ))
}

/// 打印校验结论；失败时把发现逐条列出，便于直接定位重试参数。
fn print_verification(report: &dossier_inpaint::pipeline::PipelineReport) {
    let Some(verification) = &report.verification else {
        println!("自动校验：未启用");
        return;
    };
    if !verification.applicable {
        println!("自动校验：未执行（没有需要校验的掩码区域）");
        return;
    }
    println!(
        "自动校验：{:?}，残留峰值 {:.2}%，闪烁峰值 {:.2}，接缝峰值 {:.2}，一致性均值 {:.1} dB，SSIM 均值 {:.3}",
        verification.verdict,
        verification.residual.max_ratio * 100.0,
        verification.flicker.max_index,
        verification.seam.max_score,
        verification.consistency.mean_psnr_db,
        verification.ssim.mean_ssim
    );
    for issue in &verification.issues {
        println!(
            "  - [{:?}] {}：{}",
            issue.severity, issue.code, issue.message
        );
    }
}

/// 写产物戳记（`<输出>.stamp.json`）：回答「这份产物是哪套管线、哪个版本、哪组参数、
/// 哪份输入跑出来的」。参数摘要覆盖所有会改变画面结果的开关——漏掉一个，
/// 两次参数不同的跑批就会被判成「同一套参数」，戳记也就失去了意义。
fn write_stamp(
    args: &Args,
    info: &dossier_inpaint::probe::VideoInfo,
    fps: f64,
    start_frame: Option<u64>,
    end_frame: Option<u64>,
    report_path: &std::path::Path,
    lossless_intermediate: bool,
) -> Result<PathBuf, String> {
    let version = pipeline_version(ERASE_PIPELINE).ok_or_else(|| {
        format!("管线 {ERASE_PIPELINE} 没有登记版本（内部错误：请检查 pipeline_version）")
    })?;
    let task_id = args
        .output
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "dossier-inpaint".to_string());
    let source = ArtifactRef::capture(&args.input)
        .map_err(|error| format!("无法读取输入素材摘要：{error}"))?;
    let regions: Vec<String> = args
        .roi
        .iter()
        .map(|rect| format!("{},{},{},{}", rect.x, rect.y, rect.width, rect.height))
        .collect();
    let params = input_fingerprint(&[
        ("detector", args.detector.as_str()),
        ("inpainter", args.inpainter.as_str()),
        ("roi", regions.join(";").as_str()),
        ("start_frame", &start_frame.unwrap_or(0).to_string()),
        (
            "end_frame",
            &end_frame.map(|e| e.to_string()).unwrap_or_default(),
        ),
        ("temporal_smooth", &args.temporal_smooth.to_string()),
        ("region_pad", &args.region_pad.to_string()),
        (
            "region_group_area_ratio",
            &args.region_group_area_ratio.to_string(),
        ),
        ("clip_frames", &args.clip_frames.to_string()),
        ("probe_frames", &args.probe_frames.to_string()),
        ("dilate", &args.dilate.to_string()),
        ("feather", &args.feather.to_string()),
        ("context", &args.context.to_string()),
        ("min_score", &args.min_score.to_string()),
        ("vcodec", args.vcodec.as_str()),
        ("crf", &args.crf.to_string()),
        ("preset", args.preset.as_str()),
        ("tune", args.tune.as_deref().unwrap_or("")),
        ("pix_fmt", args.pix_fmt.as_str()),
        (
            "intermediate",
            if lossless_intermediate {
                "lossless"
            } else {
                "delivery"
            },
        ),
        ("fps", &fps.to_string()),
        ("size", &format!("{}x{}", info.size.width, info.size.height)),
        ("verify", if args.no_verify { "off" } else { "on" }),
        (
            "verify_thresholds",
            &input_fingerprint(&[
                (
                    "residual_warn",
                    &args.verify_residual_ratio_warn.to_string(),
                ),
                (
                    "residual_fail",
                    &args.verify_residual_ratio_fail.to_string(),
                ),
                ("flicker_warn", &args.verify_flicker_warn.to_string()),
                ("flicker_fail", &args.verify_flicker_fail.to_string()),
                ("seam_warn", &args.verify_seam_warn.to_string()),
                ("seam_fail", &args.verify_seam_fail.to_string()),
                ("psnr_warn", &args.verify_psnr_warn.to_string()),
                ("psnr_fail", &args.verify_psnr_fail.to_string()),
                ("ssim_warn", &args.verify_ssim_warn.to_string()),
                ("ssim_fail", &args.verify_ssim_fail.to_string()),
            ]),
        ),
    ]);
    let program = std::env::current_exe()
        .ok()
        .map(|path| path.display().to_string());
    let mut stamp = ArtifactStamp::new(ERASE_PIPELINE, version, task_id, params)
        .with_tool(ToolStamp {
            name: "dossier-inpaint".to_string(),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
            program,
        })
        .with_inputs(vec![source.sha256.clone()]);
    stamp
        .add_output(&args.output)
        .map_err(|error| error.to_string())?;
    if report_path.is_file() {
        stamp
            .add_output(report_path)
            .map_err(|error| error.to_string())?;
    }
    // 戳记本身也要自洽（管线已登记、版本一致、有产物），否则写下去只会误导下游。
    stamp.validate().map_err(|error| error.to_string())?;
    match &args.stamp_json {
        Some(path) => {
            stamp.save(path).map_err(|error| error.to_string())?;
            Ok(path.clone())
        }
        None => stamp
            .save_next_to(&args.output)
            .map_err(|error| error.to_string()),
    }
}

/// 落盘处理日志（结构化 JSON，供自动重试与人工复核读取）。
fn write_report(
    path: &std::path::Path,
    report: &dossier_inpaint::pipeline::PipelineReport,
) -> Result<(), String> {
    let json = serde_json::to_string_pretty(report)
        .map_err(|error| format!("处理日志序列化失败：{error}"))?;
    std::fs::write(path, json).map_err(|error| format!("处理日志写入失败：{error}"))
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

/// 把 `--start/--duration`（秒）与 `--start-frame/--end-frame`（帧）统一成帧窗口。
fn frame_window(args: &Args, fps: f64) -> Result<(Option<u64>, Option<u64>), String> {
    let mut start = args.start_frame;
    let mut end = args.end_frame;
    if let Some(seconds) = args.start {
        if !seconds.is_finite() || seconds < 0.0 {
            return Err("--start 必须是非负秒数".into());
        }
        start = Some((seconds * fps).round() as u64);
    }
    if let Some(duration) = args.duration {
        if !duration.is_finite() || duration <= 0.0 {
            return Err("--duration 必须是大于 0 的秒数".into());
        }
        let base = start.unwrap_or(0);
        end = Some(base + (duration * fps).round().max(1.0) as u64);
    }
    Ok((start, end))
}

/// 修复阶段的帧数估计：`时长 × 帧率` 再按处理窗口截断。
///
/// 只是进度条的分母，不是「必须处理多少帧」的断言：真实帧数超过它时，管线会把分母
/// 跟着实际进度一起抬高（见 `pipeline::inpaint_total`），所以估偏小不会让进度卡住。
/// 素材没写时长时返回 0，表示「总量未知」。
fn estimate_window_frames(
    duration_sec: f64,
    fps: f64,
    start_frame: Option<u64>,
    end_frame: Option<u64>,
) -> u64 {
    if !duration_sec.is_finite() || duration_sec <= 0.0 || !fps.is_finite() || fps <= 0.0 {
        return 0;
    }
    let total = (duration_sec * fps).round() as u64;
    let window_end = end_frame.map(|end| end.min(total)).unwrap_or(total);
    window_end.saturating_sub(start_frame.unwrap_or(0))
}

/// 需要 worker 会话时启动一个常驻进程；否则返回 `None` 走一次性后端。
///
/// 检测与修复共享同一个进程，因此 DBNet 与修复模型的权重只加载一次。
fn start_worker_session(args: &Args) -> Result<Option<Arc<Mutex<WorkerSession>>>, String> {
    let needs_worker = args.detector == "worker" || args.inpainter == "worker";
    if !(args.persistent_worker && needs_worker) {
        return Ok(None);
    }
    let config = worker_config(args)?;
    let session = WorkerSession::spawn(&config).map_err(|error| error.to_string())?;
    println!("常驻 worker：pid {}", session.pid());
    Ok(Some(Arc::new(Mutex::new(session))))
}

fn build_detector(
    args: &Args,
    temp_dir: &std::path::Path,
    session: Option<Arc<Mutex<WorkerSession>>>,
) -> Result<Box<dyn TextDetector>, String> {
    match args.detector.as_str() {
        "fixed" => {
            if args.roi.is_empty() {
                return Err("--detector fixed 需要用 --roi x,y,w,h 指定标识区域".to_string());
            }
            if args.roi.len() == 1 {
                Ok(Box::new(FixedRoiDetector::new(args.roi[0])))
            } else {
                Ok(Box::new(FixedRoisDetector::new(args.roi.clone())))
            }
        }
        "worker" => {
            let mut detector =
                WorkerTextDetector::new(worker_config(args)?, temp_dir.join("detect"));
            if let Some(session) = session {
                detector = detector.with_session(session);
            }
            Ok(Box::new(detector))
        }
        other => Err(format!("未知检测后端 {other}；可选 fixed 或 worker")),
    }
}

fn build_inpainter(
    args: &Args,
    temp_dir: &std::path::Path,
    session: Option<Arc<Mutex<WorkerSession>>>,
) -> Result<Box<dyn Inpainter>, String> {
    match args.inpainter.as_str() {
        "column" => Ok(Box::new(ColumnFillInpainter { sample_rows: 4 })),
        "worker" => {
            let mut inpainter =
                WorkerInpainter::new(worker_config(args)?, temp_dir.join("inpaint"));
            if let Some(session) = session {
                inpainter = inpainter.with_session(session);
            }
            Ok(Box::new(inpainter))
        }
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

#[cfg(test)]
mod desktop_entry_tests {
    //! 桌面端（`erase::build_plan`）拼出来的参数必须被 CLI 自己接受。
    //!
    //! 两套入口各写各的参数名很容易漂移，这里用 clap 反向解析守住契约：
    //! 一旦桌面端少写/写错一个开关，这个测试就会失败。

    use super::Args;
    use clap::Parser;
    use dossier_inpaint::erase::{build_plan, EraseRegion, EraseRequest};
    use std::path::PathBuf;
    use std::process::ExitCode;

    fn request() -> EraseRequest {
        EraseRequest {
            input: PathBuf::from("/tmp/in.mp4"),
            output: PathBuf::from("/tmp/out.mp4"),
            regions: vec![
                EraseRegion {
                    x: 120,
                    y: 300,
                    width: 400,
                    height: 40,
                },
                EraseRegion {
                    x: 900,
                    y: 30,
                    width: 120,
                    height: 60,
                },
            ],
            ..Default::default()
        }
    }

    fn parse(request: &EraseRequest) -> Args {
        let plan = build_plan(request, std::path::Path::new("/bin/dossier-inpaint")).unwrap();
        let mut argv = vec![plan.program.display().to_string()];
        argv.extend(plan.args.iter().cloned());
        Args::try_parse_from(argv).expect("桌面端参数必须能被 CLI 解析")
    }

    #[test]
    fn desktop_plan_matches_the_cli_parser() {
        let request = request();
        let args = parse(&request);
        assert_eq!(args.input, PathBuf::from("/tmp/in.mp4"));
        assert_eq!(args.output, PathBuf::from("/tmp/out.mp4"));
        assert_eq!(args.roi.len(), 2);
        assert_eq!(args.roi[0].x, 120);
        assert_eq!(args.roi[1].width, 120);
        assert_eq!(args.detector, "fixed");
        assert_eq!(args.inpainter, "column");
        assert_eq!(args.dilate, request.dilate);
        assert_eq!(args.feather, request.feather);
        assert!(!args.persistent_worker);
        assert_eq!(
            args.report_json,
            Some(PathBuf::from("/tmp/out.mp4.inpaint-report.json"))
        );
        assert_eq!(args.worker_program, None);
        assert_eq!(args.progress_json, None, "没给进度流就不该凭空多一个参数");
    }

    /// 桌面端给了进度流时，`--progress-json` 必须原样传到 CLI——
    /// 进度条能不能画出来全靠这一条。
    #[test]
    fn desktop_plan_forwards_the_progress_stream() {
        let request = EraseRequest {
            progress_json: Some(PathBuf::from("/tmp/erase.progress.jsonl")),
            ..request()
        };
        let plan = build_plan(&request, std::path::Path::new("/bin/dossier-inpaint")).unwrap();
        // 预览命令里也要出现：预览的意义就是「和真正执行的是同一条命令」。
        assert!(
            plan.command_line.contains("--progress-json"),
            "预览命令应包含进度流参数：{}",
            plan.command_line
        );
        assert_eq!(plan.progress_path, request.progress_json);
        let args = parse(&request);
        assert_eq!(
            args.progress_json,
            Some(PathBuf::from("/tmp/erase.progress.jsonl"))
        );
    }

    #[test]
    fn desktop_worker_plan_matches_the_cli_parser() {
        let mut request = request();
        request.detector = "worker".into();
        request.inpainter = "worker".into();
        request.persistent_worker = true;
        request.start = Some(12.5);
        request.duration = Some(8.0);
        request.worker_program = Some("python3".into());
        request.worker_args = vec!["tools/model_worker/worker.py".into(), "--serve".into()];

        let args = parse(&request);
        assert_eq!(args.detector, "worker");
        assert_eq!(args.inpainter, "worker");
        assert!(args.persistent_worker);
        assert_eq!(args.worker_program.as_deref(), Some("python3"));
        assert_eq!(args.worker_args.len(), 2);
        assert_eq!(args.start, Some(12.5));
        assert_eq!(args.duration, Some(8.0));
    }

    #[test]
    fn clap_usage_errors_never_look_like_a_failed_verification() {
        // 确认流里 2 = 校验失败、3 = 需人工抽查，都代表「跑完了但画面有问题」。
        // 参数写错属于执行错误（1），不能撞上这两个码，否则上游会把「命令拼错」
        // 当成「没擦干净」而去重试或转人工。
        let error = Args::try_parse_from(["dossier-inpaint", "--no-such-flag"])
            .expect_err("未知参数应报错");
        assert!(error.use_stderr(), "未知参数属于用法错误，应写入 stderr");
        assert_eq!(error.exit_code(), 2, "先确认 clap 默认确实是 2（撞码来源）");
        assert_eq!(
            super::parse_error_exit_code(&error),
            ExitCode::from(1),
            "用法错误必须映射成执行错误码 1"
        );
        // --help / --version 不是错误：走 stdout 且应为 0。
        let help = Args::try_parse_from(["dossier-inpaint", "--help"]).expect_err("--help 应短路");
        assert!(!help.use_stderr(), "--help 不应写入 stderr");
        assert_eq!(super::parse_error_exit_code(&help), ExitCode::SUCCESS);
    }

    /// `--progress-json` 缺省为空：不写进度文件，也不该因此改动任何既有行为。
    #[test]
    fn progress_json_is_optional_and_parses_when_given() {
        let args = parse(&request());
        assert_eq!(args.progress_json, None);

        let args = Args::try_parse_from([
            "dossier-inpaint",
            "--input",
            "/tmp/in.mp4",
            "--output",
            "/tmp/out.mp4",
            "--progress-json",
            "/tmp/out.progress.jsonl",
        ])
        .expect("--progress-json 应被接受");
        assert_eq!(
            args.progress_json,
            Some(PathBuf::from("/tmp/out.progress.jsonl"))
        );
    }

    /// 进度分母的估计规则：按窗口截断、素材无时长时退化为「未知（0）」。
    #[test]
    fn frame_estimate_respects_the_processing_window() {
        use super::estimate_window_frames;
        assert_eq!(estimate_window_frames(10.0, 25.0, None, None), 250);
        assert_eq!(estimate_window_frames(10.0, 25.0, Some(50), None), 200);
        assert_eq!(estimate_window_frames(10.0, 25.0, Some(50), Some(100)), 50);
        // 窗口末端超过素材总帧数时按总帧数截断，不虚报分母。
        assert_eq!(estimate_window_frames(10.0, 25.0, None, Some(9999)), 250);
        assert_eq!(estimate_window_frames(0.0, 25.0, None, None), 0);
        assert_eq!(estimate_window_frames(10.0, 0.0, None, None), 0);
    }
}
