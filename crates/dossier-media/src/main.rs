//! `dossier-media` 命令行：把后期能力探测与各模块的「计划」暴露出来。
//!
//! 命令层刻意保持极薄：所有参数校验、路径推导、命令拼装都在库里
//! （`plan_*` / `build_command`），这里只做两件事——**解析参数**、**打印结果**。
//! 这样「同一条命令」既能在 CLI 里跑，也能被云端任务直接构造出来。

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use dossier_media::{
    asr::{AsrPlan, WhisperFlavor},
    ocr::{recognize_region, RecognizePlan, Region, RegionFramePlan},
    render::{ImageOverlay, Position, RenderElement, RenderPlan, SubtitleBurn, TextOverlay},
    separate::{SeparateMode, SeparatePlan},
    shot::ShotDetectConfig,
    timestretch::plan_time_stretch,
    tools::{probe_media_tools, MediaToolConfig},
};
use dossier_media::{CommandRunner, CommandSpec, SystemRunner};

#[derive(Parser)]
#[command(
    name = "dossier-media",
    about = "剧备后期工具链（技术学习用途）：能力探测 / 合成渲染 / 镜头切点 / 变速变调 / 分离 / ASR / OCR"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 探测本机后期工具链与能力标签
    Probe {
        #[arg(long)]
        ffmpeg: Option<String>,
        #[arg(long)]
        ffprobe: Option<String>,
        #[arg(long)]
        rubberband: Option<String>,
        #[arg(long)]
        demucs: Option<String>,
        #[arg(long)]
        whisper: Option<String>,
        #[arg(long)]
        tesseract: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// 打印合成渲染的 ffmpeg 命令（--run 会真的执行）
    Render {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        output: PathBuf,
        /// 叠加文字；可重复
        #[arg(long = "text")]
        texts: Vec<String>,
        #[arg(long)]
        font_size: Option<u32>,
        #[arg(long)]
        font_color: Option<String>,
        #[arg(long)]
        font_file: Option<PathBuf>,
        /// 图片叠加；可重复（`路径[,宽]`）
        #[arg(long = "logo")]
        logos: Vec<String>,
        /// 字幕烧录（srt/ass）
        #[arg(long)]
        subtitles: Option<PathBuf>,
        /// 字幕样式覆盖（libass `force_style`），例如 `Fontsize=32,Outline=2,MarginV=24`
        #[arg(long)]
        subtitle_style: Option<String>,
        #[arg(long)]
        ffmpeg: Option<String>,
        #[arg(long)]
        run: bool,
    },
    /// 镜头切点检测（--run 会真的执行）
    Shots {
        #[arg(long)]
        input: PathBuf,
        #[arg(long, default_value_t = 0.35)]
        threshold: f64,
        #[arg(long, default_value_t = 0.2)]
        min_gap: f64,
        /// 素材总时长；给了就同时输出镜头区间
        #[arg(long)]
        duration: Option<f64>,
        #[arg(long)]
        ffmpeg: Option<String>,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        run: bool,
    },
    /// 变速（音频）
    Speed {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value_t = 1.0)]
        speed: f64,
        #[arg(long, default_value_t = 0.0)]
        pitch: f64,
        /// 显式接受「变速同时变调」
        #[arg(long)]
        allow_pitch_shift: bool,
        #[arg(long)]
        ffmpeg: Option<String>,
        #[arg(long)]
        rubberband: Option<String>,
        #[arg(long)]
        json: bool,
        /// 真的执行（默认只打印将要执行的命令）
        #[arg(long)]
        run: bool,
    },
    /// 音源分离（Demucs）
    Separate {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        output_dir: PathBuf,
        #[arg(long, default_value = "two")]
        mode: String,
        #[arg(long, default_value = "htdemucs")]
        model: String,
        #[arg(long, default_value = "cpu")]
        device: String,
        #[arg(long, default_value_t = 1)]
        jobs: u32,
        #[arg(long)]
        demucs: Option<String>,
        #[arg(long)]
        json: bool,
        /// 真的执行（默认只打印将要执行的命令）
        #[arg(long)]
        run: bool,
    },
    /// 语音识别（Whisper）
    Asr {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        output_dir: PathBuf,
        #[arg(long, default_value = "openai")]
        flavor: String,
        #[arg(long, default_value = "small")]
        model: String,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        threads: Option<u32>,
        #[arg(long)]
        program: Option<String>,
        #[arg(long)]
        json: bool,
        /// 真的执行（默认只打印将要执行的命令）
        #[arg(long)]
        run: bool,
    },
    /// OCR：抽指定区域帧 + 识别
    Ocr {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        output_dir: PathBuf,
        #[arg(long, default_value_t = 0)]
        x: u32,
        #[arg(long, default_value_t = 0)]
        y: u32,
        #[arg(long)]
        width: u32,
        #[arg(long)]
        height: u32,
        #[arg(long, default_value_t = 1.0)]
        interval: f64,
        /// 只识别已抽好的图片（不再抽帧）
        #[arg(long)]
        image: Option<PathBuf>,
        #[arg(long, default_value = "chi_sim+eng")]
        language: String,
        #[arg(long)]
        ffmpeg: Option<String>,
        #[arg(long)]
        tesseract: Option<String>,
        #[arg(long)]
        json: bool,
        /// 真的执行（默认只打印将要执行的命令）
        #[arg(long)]
        run: bool,
        /// 置信度门槛：低于它的文本行仍会被输出，但计入 `低置信` 计数
        #[arg(long, default_value_t = 60.0)]
        confidence: f64,
    },
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let runner = SystemRunner::new();
    match run(&cli, &runner) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("失败：{message}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli, runner: &SystemRunner) -> Result<(), String> {
    match &cli.command {
        Command::Probe {
            ffmpeg,
            ffprobe,
            rubberband,
            demucs,
            whisper,
            tesseract,
            json,
        } => {
            let config = MediaToolConfig {
                ffmpeg: ffmpeg.clone().unwrap_or_else(|| "ffmpeg".to_string()),
                ffprobe: ffprobe.clone().unwrap_or_else(|| "ffprobe".to_string()),
                rubberband: rubberband
                    .clone()
                    .unwrap_or_else(|| "rubberband".to_string()),
                demucs: demucs.clone().unwrap_or_else(|| "demucs".to_string()),
                whisper: whisper.clone().unwrap_or_else(|| "whisper".to_string()),
                tesseract: tesseract.clone().unwrap_or_else(|| "tesseract".to_string()),
            };
            let report = probe_media_tools(runner, &config);
            print_json(&report, *json)?;
            if !json {
                for tool in &report.tools {
                    println!(
                        "{:<11} {:<4} {:<24} {}",
                        tool.name,
                        if tool.tool.found { "OK" } else { "缺失" },
                        tool.tool.display_version(),
                        tool.purpose
                    );
                }
                println!("\n能力标签：{}", report.capabilities.join(", "));
                if !report.missing.is_empty() {
                    println!("\n缺失能力：");
                    for item in &report.missing {
                        println!("  {} ← {}", item.capability, item.reason);
                    }
                }
                for note in &report.limitations {
                    println!("限制：{note}");
                }
            }
            Ok(())
        }
        Command::Render {
            input,
            output,
            texts,
            font_size,
            font_color,
            font_file,
            logos,
            subtitles,
            subtitle_style,
            ffmpeg,
            run,
        } => {
            let mut plan = RenderPlan::new(input, output);
            for text in texts {
                let mut overlay = TextOverlay::caption(
                    text.clone(),
                    font_size.unwrap_or(28),
                    font_color.clone().unwrap_or_else(|| "white".to_string()),
                );
                overlay.font_file = font_file.clone();
                plan.elements.push(RenderElement::Text(overlay));
            }
            for logo in logos {
                let (path, width) = match logo.split_once(',') {
                    Some((path, width)) => (path.to_string(), width.trim().parse::<u32>().ok()),
                    None => (logo.clone(), None),
                };
                plan.elements.push(RenderElement::Image(ImageOverlay {
                    path: PathBuf::from(path),
                    x: Position::Expression("w-overlay_w-16".to_string()),
                    y: Position::Pixels(16),
                    width,
                    range: None,
                    opacity: 1.0,
                }));
            }
            if let Some(subtitles) = subtitles {
                plan.elements.push(RenderElement::Subtitles(SubtitleBurn {
                    path: subtitles.clone(),
                    force_style: subtitle_style.clone(),
                }));
            }
            let ffmpeg = ffmpeg.clone().unwrap_or_else(|| "ffmpeg".to_string());
            let spec = plan
                .build_command(&ffmpeg)
                .map_err(|error| error.to_string())?;
            println!("{}", spec.display());
            if *run {
                plan.render(&ffmpeg).map_err(|error| error.to_string())?;
                println!("已渲染：{}", plan.output.display());
            }
            Ok(())
        }
        Command::Shots {
            input,
            threshold,
            min_gap,
            duration,
            ffmpeg,
            json,
            run,
        } => {
            let ffmpeg = ffmpeg.clone().unwrap_or_else(|| "ffmpeg".to_string());
            let config = ShotDetectConfig {
                threshold: *threshold,
                min_gap_sec: *min_gap,
            };
            let spec = dossier_media::shot::build_detect_command(&ffmpeg, input, &config)
                .map_err(|error| error.to_string())?;
            println!("{}", spec.display());
            if !run {
                return Ok(());
            }
            let report = dossier_media::shot::detect_shots(&ffmpeg, input, &config)
                .map_err(|error| error.to_string())?;
            print_json(&report, *json)?;
            if !json {
                println!(
                    "阈值 {}：{} 个切点",
                    report.threshold,
                    report.boundaries.len()
                );
                for boundary in &report.boundaries {
                    println!("  {:.3}s  分数 {:.3}", boundary.time_sec, boundary.score);
                }
                if let Some(duration) = duration {
                    for range in report.to_ranges(*duration) {
                        println!(
                            "  镜头 {}：{:.3}s → {:.3}s（{:.3}s）",
                            range.index,
                            range.start_sec,
                            range.end_sec,
                            range.duration()
                        );
                    }
                }
            }
            Ok(())
        }
        Command::Speed {
            input,
            output,
            speed,
            pitch,
            allow_pitch_shift,
            ffmpeg,
            rubberband,
            json,
            run,
        } => {
            let ffmpeg = ffmpeg.clone().unwrap_or_else(|| "ffmpeg".to_string());
            let rubberband = rubberband
                .clone()
                .unwrap_or_else(|| "rubberband".to_string());
            let available = runner
                .run(&CommandSpec::new(rubberband.clone()).arg("--version"))
                .map(|output| output.success())
                .unwrap_or(false);
            let plan =
                plan_time_stretch(input, output, *speed, *pitch, available, *allow_pitch_shift)
                    .map_err(|error| error.to_string())?;
            let spec = plan
                .build_command(&ffmpeg, &rubberband)
                .map_err(|error| error.to_string())?;
            print_json(&plan, *json)?;
            println!("{}", spec.display());
            if let Some(note) = &plan.degradation {
                println!("注意：{note}");
            }
            if *run {
                let outcome = plan
                    .execute(runner, &ffmpeg, &rubberband)
                    .map_err(|error| error.to_string())?;
                print_json(&outcome, *json)?;
                println!(
                    "已变速（{}）：{}",
                    match outcome.backend {
                        dossier_media::timestretch::TimeStretchBackend::RubberBand => "rubberband",
                        dossier_media::timestretch::TimeStretchBackend::FfmpegAtempo => "atempo",
                    },
                    outcome.output.display()
                );
            }
            Ok(())
        }
        Command::Separate {
            input,
            output_dir,
            mode,
            model,
            device,
            jobs,
            demucs,
            json,
            run,
        } => {
            let demucs = demucs.clone().unwrap_or_else(|| "demucs".to_string());
            let available = runner
                .run(&CommandSpec::new(demucs.clone()).arg("--version"))
                .map(|output| output.success())
                .unwrap_or(false);
            let mut plan = SeparatePlan::new(input, output_dir);
            plan.mode = match mode.as_str() {
                "four" => SeparateMode::FourStems,
                _ => SeparateMode::TwoStems,
            };
            plan.model = model.clone();
            plan.device = device.clone();
            plan.jobs = *jobs;
            let spec = plan
                .build_command(&demucs, available)
                .map_err(|error| error.to_string())?;
            print_json(&plan, *json)?;
            println!("{}", spec.display());
            if *run {
                let outcome = plan
                    .execute(runner, &demucs)
                    .map_err(|error| error.to_string())?;
                print_json(&outcome, *json)?;
                for path in &outcome.outputs {
                    println!("已分离：{}", path.display());
                }
            } else {
                for path in plan.expected_outputs() {
                    println!("预期产物：{}", path.display());
                }
            }
            Ok(())
        }
        Command::Asr {
            input,
            output_dir,
            flavor,
            model,
            language,
            threads,
            program,
            json,
            run,
        } => {
            let flavor = match flavor.as_str() {
                "cpp" | "whisper-cpp" => WhisperFlavor::WhisperCpp,
                _ => WhisperFlavor::OpenAi,
            };
            let program = program.clone().unwrap_or_else(|| match flavor {
                WhisperFlavor::OpenAi => "whisper".to_string(),
                WhisperFlavor::WhisperCpp => "whisper-cli".to_string(),
            });
            let available = runner
                .run(&CommandSpec::new(program.clone()).arg("--help"))
                .map(|output| output.success())
                .unwrap_or(false);
            let mut plan = AsrPlan::new(input, output_dir, flavor, model.clone());
            plan.language = language.clone();
            plan.threads = *threads;
            let spec = plan
                .build_command(&program, available)
                .map_err(|error| error.to_string())?;
            print_json(&plan, *json)?;
            println!("{}", spec.display());
            if *run {
                let outcome = plan
                    .execute(runner, &program)
                    .map_err(|error| error.to_string())?;
                print_json(&outcome, *json)?;
                println!(
                    "已识别：{}（{} 条字幕）",
                    outcome.subtitle.display(),
                    outcome.cues
                );
            } else {
                println!("预期字幕：{}", plan.expected_subtitle_path().display());
            }
            Ok(())
        }
        Command::Ocr {
            input,
            output_dir,
            x,
            y,
            width,
            height,
            interval,
            image,
            language,
            ffmpeg,
            tesseract,
            json,
            run,
            confidence,
        } => {
            let ffmpeg = ffmpeg.clone().unwrap_or_else(|| "ffmpeg".to_string());
            let tesseract = tesseract.clone().unwrap_or_else(|| "tesseract".to_string());
            let region = Region::new(*x, *y, *width, *height);
            if let Some(image) = image {
                let mut plan = RecognizePlan::new(image, output_dir.join("ocr"));
                plan.language = language.clone();
                let available = runner
                    .run(&CommandSpec::new(tesseract.clone()).arg("--version"))
                    .map(|output| output.success())
                    .unwrap_or(false);
                let spec = plan
                    .build_command(&tesseract, available)
                    .map_err(|error| error.to_string())?;
                print_json(&plan, *json)?;
                println!("{}", spec.display());
                if *run {
                    let lines = plan
                        .execute(runner, &tesseract)
                        .map_err(|error| error.to_string())?;
                    print_json(&lines, *json)?;
                    let confident = lines
                        .iter()
                        .filter(|line| line.is_confident(*confidence))
                        .count();
                    println!(
                        "已识别 {} 行（置信度 ≥ {:.0}：{} 行）",
                        lines.len(),
                        confidence,
                        confident
                    );
                } else {
                    println!("预期 TSV：{}", plan.tsv_path().display());
                }
                return Ok(());
            }
            let mut plan = RegionFramePlan::new(input, output_dir, region);
            plan.interval_sec = *interval;
            plan.language = language.clone();
            let spec = plan
                .build_command(&ffmpeg)
                .map_err(|error| error.to_string())?;
            print_json(&plan, *json)?;
            println!("{}", spec.display());
            if *run {
                let outcome = recognize_region(&plan, runner, &ffmpeg, &tesseract, *confidence)
                    .map_err(|error| error.to_string())?;
                print_json(&outcome, *json)?;
                println!(
                    "已抽 {} 帧、识别 {} 行（置信度 ≥ {:.0}：{} 行）",
                    outcome.frames.len(),
                    outcome.lines.len(),
                    confidence,
                    outcome.confident
                );
            } else {
                println!("预期图片：{}", plan.expected_frame_path(1).display());
            }
            Ok(())
        }
    }
}

fn print_json<T: serde::Serialize>(value: &T, json: bool) -> Result<(), String> {
    if json {
        let text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
        println!("{text}");
    }
    Ok(())
}
