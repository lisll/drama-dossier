//! 处理版本的真机链路测试（M4b-1，计划第 5 节「AI 标识」「画质」）。
//!
//! 前面几个测试文件只验证「生成的 ffmpeg 参数对不对」；这里**真跑一遍 ffmpeg**，
//! 验证那些参数确实产生了要的效果：
//! - 输出时长、分辨率、编码符合输出规格；
//! - AI 隐式标识（GB 45438 附录 E）能写进 MP4 并用 `ffprobe` 读回来；
//! - AI 显式标识真的画在了右下角（对比同一段素材加标识前后的像素）；
//! - 画质统计文件确实写了、能解析，且重编码后的分数在合理范围；
//! - 颗粒加得很重时分数会掉下来——下限机制才有意义。
//!
//! 依赖本机 ffmpeg。缺 ffmpeg、缺 libx264、缺 drawtext / ssim / psnr 滤镜时按仓库约定跳过并说明原因
//! （不静默通过）。

use std::path::{Path, PathBuf};
use std::time::Duration;

use dossier_core::version::{
    build_version_graph, fill_stats_paths, ColorPreset, EpisodeInput, GraphExtras, OutputSpec,
    QualityFloor, QualityResult, SubtleParams, VersionTemplate,
};
use dossier_runtime::{CommandRunner, CommandSpec, SystemRunner};
use serde_json::json;

/// 中文测试字体：系统里找得到就用它；找不到则跳过需要写字的用例。
const FONT_CANDIDATES: &[&str] = &[
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    "/usr/share/fonts/chinese/simsun.ttc",
];

fn spec() -> OutputSpec {
    OutputSpec::from_params(&json!({
        "width": 1080, "height": 1920, "fps": 30,
        "video_bitrate_kbps": 5000, "audio_bitrate_kbps": 128,
        "max_duration_sec": 900, "container": "mp4",
        "video_codec": "h264", "audio_codec": "aac",
    }))
    .expect("通用规格")
}

fn subtle(grain: u32) -> SubtleParams {
    SubtleParams::from_params(&json!({
        "grain": grain, "zoom_percent": 1.5, "brightness": 0.0, "saturation": 1.0,
        "hue_degrees": 0.0, "tempo": 1.0, "pitch_semitones": 0.0,
        "trim_head_frames": 0, "trim_tail_frames": 0,
    }))
    .expect("参数")
}

fn test_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("dossier-version-run-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

fn ffmpeg_available(runner: &SystemRunner) -> bool {
    runner
        .run(&CommandSpec::new("ffmpeg").arg("-version"))
        .map(|output| output.success())
        .unwrap_or(false)
}

/// 需要的滤镜是不是都编进了本机的 ffmpeg。
fn filters_available(runner: &SystemRunner, needed: &[&str]) -> Vec<String> {
    let output = match runner.run(&CommandSpec::new("ffmpeg").args(["-hide_banner", "-filters"])) {
        Ok(output) if output.success() => output,
        _ => return needed.iter().map(|name| name.to_string()).collect(),
    };
    let listing = output.combined();
    needed
        .iter()
        .filter(|name| {
            !listing
                .lines()
                .any(|line| line.split_whitespace().nth(1) == Some(**name))
        })
        .map(|name| name.to_string())
        .collect()
}

fn system_font() -> Option<&'static str> {
    FONT_CANDIDATES
        .iter()
        .copied()
        .find(|path| Path::new(path).is_file())
}

/// 用 ffmpeg 造一段合成素材（测试画面 + 正弦音）。
fn make_source(runner: &SystemRunner, path: &Path, seconds: f64) {
    make_video(
        runner,
        path,
        &format!("testsrc2=size=1080x1920:rate=30:duration={seconds}"),
        seconds,
    );
}

/// 纯黑素材：用来判断「字有没有画上去」。
///
/// testsrc2 这种动态测试画面每帧都在变，两次编码取到的帧内容本身就不同，
/// 拿两帧直接比像素会被画面自身的差异淹没。黑底上任何亮像素都只可能来自字幕或水印。
fn make_black_source(runner: &SystemRunner, path: &Path, seconds: f64) {
    make_video(
        runner,
        path,
        &format!("color=c=black:size=1080x1920:rate=30:duration={seconds}"),
        seconds,
    );
}

fn make_video(runner: &SystemRunner, path: &Path, video: &str, seconds: f64) {
    let output = runner
        .run(
            &CommandSpec::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-y"])
                .args(["-f", "lavfi", "-i"])
                .arg(video.to_string())
                .args(["-f", "lavfi", "-i"])
                .arg(format!("sine=frequency=440:duration={seconds}"))
                .args([
                    "-c:v",
                    "libx264",
                    "-preset",
                    "veryfast",
                    "-crf",
                    "26",
                    "-pix_fmt",
                    "yuv420p",
                    "-c:a",
                    "aac",
                    "-b:a",
                    "128k",
                    "-movflags",
                    "+faststart",
                ])
                .arg(path.display().to_string())
                .timeout(Duration::from_secs(180)),
        )
        .expect("跑 ffmpeg 造素材");
    assert!(output.success(), "造素材失败：{}", output.combined());
}

/// 按 `build_version_graph` 的结果真跑一次编码，返回（输出路径、SSIM 文件、PSNR 文件）。
fn run_graph(
    runner: &SystemRunner,
    dir: &Path,
    tag: &str,
    graph: &dossier_core::version::VersionGraph,
) -> (PathBuf, PathBuf, PathBuf) {
    let output = dir.join(format!("{tag}-{}", graph.output_name));
    let stats = dir.join("stats");
    std::fs::create_dir_all(&stats).expect("建统计目录");
    let ssim_path = stats.join(format!("{tag}.ssim.log"));
    let psnr_path = stats.join(format!("{tag}.psnr.log"));
    let _ = std::fs::remove_file(&ssim_path);
    let _ = std::fs::remove_file(&psnr_path);

    let mut spec =
        CommandSpec::new("ffmpeg").args(["-hide_banner", "-loglevel", "error", "-y", "-nostdin"]);
    spec = spec.args(graph.input_args.iter().cloned());
    spec = spec.arg("-filter_complex").arg(fill_stats_paths(
        &graph.filter_complex,
        &ssim_path.display().to_string(),
        &psnr_path.display().to_string(),
    ));
    spec = spec.args(graph.map_args.iter().cloned());
    spec = spec.arg("-r").arg(spec_fps().to_string());
    for (key, value) in &graph.metadata {
        spec = spec.arg("-metadata").arg(format!("{key}={value}"));
    }
    // 和 src-tauri::encode 的软件分支一致：crf 起步 + maxrate/bufsize 封顶。
    spec = spec.args([
        "-c:v", "libx264", "-preset", "veryfast", "-crf", "23", "-maxrate", "5000k", "-bufsize",
        "10000k", "-c:a", "aac", "-b:a", "128k",
    ]);
    let movflags = if graph.metadata.is_empty() {
        "+faststart"
    } else {
        // 不加 use_metadata_tags 的话自定义键会被静默丢掉。
        "+faststart+use_metadata_tags"
    };
    spec = spec.args(["-movflags", movflags]);
    spec = spec
        .arg(output.display().to_string())
        .timeout(Duration::from_secs(600));

    let result = runner.run(&spec).expect("跑 ffmpeg");
    assert!(result.success(), "编码失败：{}", result.combined());
    assert!(output.is_file(), "应该产出视频文件");
    (output, ssim_path, psnr_path)
}

fn spec_fps() -> u32 {
    spec().fps
}

/// 读 MP4 的格式级元数据（`ffprobe -show_format`）。
fn format_metadata(runner: &SystemRunner, path: &Path) -> String {
    let output = runner
        .run(
            &CommandSpec::new("ffprobe")
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-show_format",
                    "-print_format",
                    "json",
                ])
                .arg(path.display().to_string()),
        )
        .expect("跑 ffprobe");
    assert!(output.success(), "ffprobe 失败：{}", output.combined());
    output.combined()
}

/// 取某一秒的原始像素（rgb24）。
fn grab_frame(runner: &SystemRunner, input: &Path, at_sec: f64) -> Vec<u8> {
    let output = runner
        .run(
            &CommandSpec::new("ffmpeg")
                .args(["-hide_banner", "-loglevel", "error", "-ss"])
                .arg(format!("{at_sec}"))
                .arg("-i")
                .arg(input.display().to_string())
                .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
                .timeout(Duration::from_secs(60)),
        )
        .expect("抽帧");
    assert!(output.success(), "抽帧失败：{}", output.combined());
    output.stdout_raw
}

/// 把一帧切成九宫格，返回各自「亮像素」的个数。
///
/// **必须和对照组相减才有意义**：testsrc2 这种测试画面本身就有大片亮色，
/// 绝对值会被素材本身盖住。水印默认透明度 0.5，叠上去只是把原像素提亮一点，
/// 所以这里的阈值取得比「纯白」低，配合相减才稳定。
fn bright_by_region(frame: &[u8], width: usize, height: usize) -> [usize; 9] {
    let mut counts = [0usize; 9];
    for y in 0..height {
        for x in 0..width {
            let at = (y * width + x) * 3;
            if at + 2 >= frame.len() {
                continue;
            }
            let (r, g, b) = (frame[at], frame[at + 1], frame[at + 2]);
            if r > 200 && g > 200 && b > 200 {
                let column = x * 3 / width.max(1);
                let row = y * 3 / height.max(1);
                counts[row * 3 + column] += 1;
            }
        }
    }
    counts
}

/// 九宫格里「多了多少亮像素」：带处理减对照组，负值截成 0。
fn region_gain(before: &[usize; 9], after: &[usize; 9]) -> [i64; 9] {
    let mut gain = [0i64; 9];
    for index in 0..9 {
        gain[index] = after[index] as i64 - before[index] as i64;
    }
    gain
}

/// 上下都出现过亮像素的行范围（用来量文字实际占多高）。
/// 上下都出现过亮像素的行范围（用来量文字实际占多高）。
fn lit_rows(frame: &[u8], width: usize, height: usize) -> (usize, usize) {
    lit_rows_in(frame, width, height, 0, width)
}

/// 同上，但只在给定的横向区间里找。
///
/// 量字幕高度时必须排除右上角的水印：两个元素同时在画面上时，
/// 全画面扫出来的上下界会把水印和字幕连成一片。
fn lit_rows_in(
    frame: &[u8],
    width: usize,
    height: usize,
    x_from: usize,
    x_to: usize,
) -> (usize, usize) {
    let mut top = height;
    let mut bottom = 0usize;
    for y in 0..height {
        let mut lit = 0usize;
        for x in x_from..x_to.min(width) {
            let at = (y * width + x) * 3;
            if at + 2 >= frame.len() {
                continue;
            }
            if frame[at] > 200 && frame[at + 1] > 200 && frame[at + 2] > 200 {
                lit += 1;
            }
        }
        if lit > 0 {
            top = top.min(y);
            bottom = bottom.max(y);
        }
    }
    (top, bottom)
}

fn episode(path: &Path) -> EpisodeInput<'_> {
    EpisodeInput {
        path: path.to_str().expect("路径是 UTF-8"),
        width: 1080,
        height: 1920,
        fps: 30.0,
        duration_sec: 4.0,
        has_audio: true,
        subtitle_path: None,
    }
}

#[test]
fn version_graph_runs_end_to_end_and_reports_quality() {
    let runner = SystemRunner::new();
    if !ffmpeg_available(&runner) {
        eprintln!("跳过：本机没有可用的 ffmpeg");
        return;
    }
    let missing = filters_available(&runner, &["ssim", "psnr", "noise", "eq"]);
    if !missing.is_empty() {
        eprintln!("跳过：本机 ffmpeg 缺少滤镜 {}", missing.join("、"));
        return;
    }

    let dir = test_dir("e2e");
    let source = dir.join("第01集.mp4");
    make_source(&runner, &source, 4.0);
    let probe = dossier_core::probe_episode(
        &dossier_core::EpisodeFile {
            path: source.clone(),
            file_name: "第01集.mp4".into(),
            size_bytes: 0,
            episode_number: None,
        },
        &dossier_core::FfprobeConfig { bin: None },
    )
    .expect("探测素材");
    assert!(probe.spec.duration_sec > 3.0, "素材时长应该接近 4 秒");

    // 不写字、只做细微处理：先验证主链路和画质统计。
    let episode_input = EpisodeInput {
        duration_sec: probe.spec.duration_sec,
        ..episode(&source)
    };
    let graph = build_version_graph(
        &VersionTemplate::default(),
        &spec(),
        &subtle(2),
        &episode_input,
        &GraphExtras::default(),
        42,
    )
    .expect("生成滤镜图");

    let (output, ssim_path, psnr_path) = run_graph(&runner, &dir, "plain", &graph);

    // 输出规格：分辨率、帧率、编码、时长。
    let checked = dossier_core::probe_episode(
        &dossier_core::EpisodeFile {
            path: output.clone(),
            file_name: "out.mp4".into(),
            size_bytes: 0,
            episode_number: None,
        },
        &dossier_core::FfprobeConfig { bin: None },
    )
    .expect("探测输出");
    assert_eq!(checked.spec.width, spec().width, "宽度要统一到输出规格");
    assert_eq!(checked.spec.height, spec().height, "高度要统一到输出规格");
    assert_eq!(checked.spec.video_codec.as_deref(), Some("h264"));
    assert_eq!(checked.spec.audio_codec.as_deref(), Some("aac"));
    let delta = (checked.spec.duration_sec - graph.output_duration_sec).abs();
    assert!(
        delta < 0.5,
        "实际时长 {} 和算出来的 {} 差太多",
        checked.spec.duration_sec,
        graph.output_duration_sec
    );
    assert_eq!(graph.output_name, "第01集.mp4");

    // 画质统计：两个文件都要有内容，且能解析出分数。
    let ssim_text = std::fs::read_to_string(&ssim_path).expect("SSIM 统计文件");
    let psnr_text = std::fs::read_to_string(&psnr_path).expect("PSNR 统计文件");
    let quality = QualityResult::from_stats(&ssim_text, &psnr_text).expect("解析画质统计");
    // 只加轻微颗粒：分数应当远高于下限（light 档 0.97 / 38）。
    let light_floor =
        QualityFloor::from_params(&json!({"min_ssim": 0.97, "min_psnr": 38.0})).unwrap();
    assert!(
        quality.passes(&light_floor),
        "轻微颗粒不该低于 light 档下限：SSIM {} / PSNR {}",
        quality.ssim,
        quality.psnr
    );
    assert!(
        quality.ssim <= 1.0 && quality.ssim > 0.0,
        "SSIM 要在 (0,1]：{}",
        quality.ssim
    );
    assert!(quality.psnr > 0.0, "PSNR 要是正数：{}", quality.psnr);

    // 同一个滤镜图跑第二遍：结果必须一致（配方可复现，续跑才产得出同一份结果）。
    let (output_again, ssim_again, psnr_again) = run_graph(&runner, &dir, "again", &graph);
    let quality_again = QualityResult::from_stats(
        &std::fs::read_to_string(&ssim_again).unwrap(),
        &std::fs::read_to_string(&psnr_again).unwrap(),
    )
    .unwrap();
    assert_eq!(
        quality, quality_again,
        "同样的参数重跑一遍，画质分数必须完全一样"
    );
    assert_eq!(
        std::fs::metadata(&output).unwrap().len(),
        std::fs::metadata(&output_again).unwrap().len(),
        "重跑产出的文件大小要一致"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn heavy_grain_drops_quality_below_the_floor() {
    let runner = SystemRunner::new();
    if !ffmpeg_available(&runner) {
        eprintln!("跳过：本机没有可用的 ffmpeg");
        return;
    }
    let missing = filters_available(&runner, &["ssim", "psnr", "noise"]);
    if !missing.is_empty() {
        eprintln!("跳过：本机 ffmpeg 缺少滤镜 {}", missing.join("、"));
        return;
    }

    let dir = test_dir("grain");
    let source = dir.join("第01集.mp4");
    make_source(&runner, &source, 3.0);
    let probe = dossier_core::probe_episode(
        &dossier_core::EpisodeFile {
            path: source.clone(),
            file_name: "第01集.mp4".into(),
            size_bytes: 0,
            episode_number: None,
        },
        &dossier_core::FfprobeConfig { bin: None },
    )
    .expect("探测素材");
    let episode_input = EpisodeInput {
        duration_sec: probe.spec.duration_sec,
        ..episode(&source)
    };

    // 颗粒拉满（ffmpeg 的 noise 上限是 100）：画质必须掉到 heaviest 下限之下，
    // 否则「画质下限」这道闸门就是摆设。
    let graph = build_version_graph(
        &VersionTemplate::default(),
        &spec(),
        &subtle(100),
        &episode_input,
        &GraphExtras::default(),
        9,
    )
    .expect("生成滤镜图");
    let (_, ssim_path, psnr_path) = run_graph(&runner, &dir, "grain", &graph);
    let quality = QualityResult::from_stats(
        &std::fs::read_to_string(&ssim_path).unwrap(),
        &std::fs::read_to_string(&psnr_path).unwrap(),
    )
    .expect("解析画质统计");

    // 三档下限里最宽的是 heavy（0.84 / 32）。颗粒拉满应当掉到它之下。
    let heavy_floor =
        QualityFloor::from_params(&json!({"min_ssim": 0.84, "min_psnr": 32.0})).unwrap();
    assert!(
        !quality.passes(&heavy_floor),
        "颗粒拉满还高于最宽的下限，说明画质检测量不到损失：SSIM {} / PSNR {}",
        quality.ssim,
        quality.psnr
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ai_marks_are_visible_and_metadata_reads_back() {
    let runner = SystemRunner::new();
    if !ffmpeg_available(&runner) {
        eprintln!("跳过：本机没有可用的 ffmpeg");
        return;
    }
    let Some(font) = system_font() else {
        eprintln!("跳过：本机没有找到中文字体（AI 标识要写中文）");
        return;
    };
    let missing = filters_available(&runner, &["ssim", "psnr", "drawtext", "noise"]);
    if !missing.is_empty() {
        eprintln!("跳过：本机 ffmpeg 缺少滤镜 {}", missing.join("、"));
        return;
    }

    let dir = test_dir("aimark");
    let source = dir.join("第01集.mp4");
    make_source(&runner, &source, 5.0);
    let probe = dossier_core::probe_episode(
        &dossier_core::EpisodeFile {
            path: source.clone(),
            file_name: "第01集.mp4".into(),
            size_bytes: 0,
            episode_number: None,
        },
        &dossier_core::FfprobeConfig { bin: None },
    )
    .expect("探测素材");
    let episode_input = EpisodeInput {
        duration_sec: probe.spec.duration_sec,
        ..episode(&source)
    };
    let template = VersionTemplate {
        ai_marked: true,
        ai_content_producer: "drama-dossier".into(),
        ..VersionTemplate::default()
    };

    // 对照组：同一段素材、同样的处理，但不写 AI 标识。
    let plain = build_version_graph(
        &VersionTemplate::default(),
        &spec(),
        &subtle(0),
        &episode_input,
        &GraphExtras::default(),
        3,
    )
    .expect("对照组滤镜图");
    let (plain_out, _, _) = run_graph(&runner, &dir, "noai", &plain);

    let marked = build_version_graph(
        &template,
        &spec(),
        &subtle(0),
        &episode_input,
        &GraphExtras {
            font: Some(font),
            ..GraphExtras::default()
        },
        3,
    )
    .expect("带标识的滤镜图");
    let (marked_out, _, _) = run_graph(&runner, &dir, "ai", &marked);

    // 隐式标识：写进了 MP4 元数据，ffprobe 能读回来。
    let metadata = format_metadata(&runner, &marked_out);
    assert!(
        metadata.contains("AIGC"),
        "输出文件里应该能读到 AIGC 元数据：{metadata}"
    );
    assert!(
        metadata.contains("drama-dossier"),
        "元数据里要有服务提供者：{metadata}"
    );
    let plain_metadata = format_metadata(&runner, &plain_out);
    assert!(
        !plain_metadata.contains("AIGC"),
        "没勾 AI 的文件不该有 AIGC 元数据"
    );

    // 显式标识：开头（起始画面，3 秒内）和之后都画在右下角。
    let width = spec().width as usize;
    let height = spec().height as usize;
    for (at, label) in [(1.0, "起始 3 秒内"), (4.0, "3 秒之后")] {
        let before = bright_by_region(&grab_frame(&runner, &plain_out, at), width, height);
        let after = bright_by_region(&grab_frame(&runner, &marked_out, at), width, height);
        let gain = region_gain(&before, &after);
        // 右下角 = 九宫格的第 9 格（下标 8）。
        assert!(
            gain[8] > 100,
            "{label}：右下角应该多出标识文字，实际增量 {}（对照 {before:?}，带标识 {after:?}）",
            gain[8]
        );
        // 标识只画在右下角：左上角不该凭空多出亮像素。
        assert!(
            gain[0] < 50,
            "{label}：标识不该出现在左上角，实际增量 {}",
            gain[0]
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// 字幕字号必须真的是「输出分辨率的像素」。
///
/// 这里钉的是一个真实踩过的坑:`subtitles` 滤镜里的 `Fontsize` 是 ASS 脚本单位,
/// libass 会按「画面高 / PlayResY」缩放,而 srt 转 ass 用的是默认 PlayRes(384×288)。
/// 不声明 `PlayResX` / `PlayResY` 时,1920 高的画面会把字号放大 6.7 倍:
/// 默认的 56 会渲染成两百多像素高、还换行,盖住画面中部。
#[test]
fn color_preset_does_not_count_as_quality_loss() {
    let runner = SystemRunner::new();
    if !ffmpeg_available(&runner) {
        eprintln!("跳过:本机没有可用的 ffmpeg");
        return;
    }
    let missing = filters_available(&runner, &["ssim", "psnr", "colorbalance", "noise"]);
    if !missing.is_empty() {
        eprintln!("跳过:本机 ffmpeg 缺少滤镜 {}", missing.join("、"));
        return;
    }

    let dir = test_dir("preset");
    let source = dir.join("第01集.mp4");
    make_source(&runner, &source, 3.0);
    let probe = dossier_core::probe_episode(
        &dossier_core::EpisodeFile {
            path: source.clone(),
            file_name: "第01集.mp4".into(),
            size_bytes: 0,
            episode_number: None,
        },
        &dossier_core::FfprobeConfig { bin: None },
    )
    .expect("探测素材");
    let episode_input = EpisodeInput {
        duration_sec: probe.spec.duration_sec,
        ..episode(&source)
    };

    let quality_of = |preset: ColorPreset, tag: &str| -> QualityResult {
        let template = VersionTemplate {
            color_preset: preset,
            ..VersionTemplate::default()
        };
        let graph = build_version_graph(
            &template,
            &spec(),
            &subtle(2),
            &episode_input,
            &GraphExtras::default(),
            42,
        )
        .expect("生成滤镜图");
        let (_, ssim_path, psnr_path) = run_graph(&runner, &dir, tag, &graph);
        QualityResult::from_stats(
            &std::fs::read_to_string(&ssim_path).unwrap(),
            &std::fs::read_to_string(&psnr_path).unwrap(),
        )
        .expect("解析画质统计")
    };

    let original = quality_of(ColorPreset::Original, "original");
    let warm = quality_of(ColorPreset::Warm, "warm");
    let contrast = quality_of(ColorPreset::HighContrast, "contrast");

    // 调色预设是用户明确选的「看得见的内容」,不该被当成画质损失:
    // 套了「暖色」之后的分数应当和原色基本一致(差异远小于档位下限之间的间隔),
    // 而且三个预设都要过 light 档的下限。
    let light_floor =
        QualityFloor::from_params(&json!({"min_ssim": 0.97, "min_psnr": 38.0})).unwrap();
    for (label, result) in [("原色", &original), ("暖色", &warm), ("高对比", &contrast)] {
        assert!(
            result.passes(&light_floor),
            "{label}预设不该把画质压到下限之下:SSIM {} / PSNR {}",
            result.ssim,
            result.psnr
        );
    }
    let gap = (original.ssim - warm.ssim).abs();
    assert!(
        gap < 0.02,
        "调色预设不该算进画质损失(原色 SSIM {}、暖色 SSIM {},差了 {gap:.4});\
         差得多说明预设还在两路分开之前",
        original.ssim,
        warm.ssim
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn subtitle_size_is_measured_in_output_pixels() {
    let runner = SystemRunner::new();
    if !ffmpeg_available(&runner) {
        eprintln!("跳过:本机没有可用的 ffmpeg");
        return;
    }
    let Some(font) = system_font() else {
        eprintln!("跳过:本机没有找到中文字体");
        return;
    };
    let missing = filters_available(&runner, &["ssim", "psnr", "subtitles"]);
    if !missing.is_empty() {
        eprintln!("跳过:本机 ffmpeg 缺少滤镜 {}", missing.join("、"));
        return;
    }

    let dir = test_dir("subsize");
    let source = dir.join("第01集.mp4");
    make_black_source(&runner, &source, 2.0);
    let probe = dossier_core::probe_episode(
        &dossier_core::EpisodeFile {
            path: source.clone(),
            file_name: "第01集.mp4".into(),
            size_bytes: 0,
            episode_number: None,
        },
        &dossier_core::FfprobeConfig { bin: None },
    )
    .expect("探测素材");
    let episode_input = EpisodeInput {
        duration_sec: probe.spec.duration_sec,
        ..episode(&source)
    };

    // 只放两个字,字号大到一定程度会换行,换行会让「量字高」失去意义。
    let subtitle = dir.join("第01集.srt");
    std::fs::write(&subtitle, "1\n00:00:00,000 --> 00:00:02,000\n字幕\n").expect("写字幕");

    let height = spec().height as usize;
    let width = spec().width as usize;
    let measure = |size: u32| -> (usize, usize) {
        let template = VersionTemplate {
            subtitle_size: size,
            ..VersionTemplate::default()
        };
        let graph = build_version_graph(
            &template,
            &spec(),
            &subtle(0),
            &EpisodeInput {
                subtitle_path: subtitle.to_str(),
                ..episode_input.clone()
            },
            &GraphExtras {
                font: Some(font),
                ..GraphExtras::default()
            },
            3,
        )
        .expect("生成滤镜图");
        // 滤镜图里要显式声明 PlayRes,否则字号单位就不是像素。
        assert!(
            graph.filter_complex.contains(&format!(
                "PlayResX={},PlayResY={}",
                spec().width,
                spec().height
            )),
            "字幕滤镜要声明 PlayRes:{}",
            graph.filter_complex
        );
        let (output, _, _) = run_graph(&runner, &dir, &format!("size{size}"), &graph);
        let frame = grab_frame(&runner, &output, 1.0);
        let (top, bottom) = lit_rows(&frame, width, height);
        assert!(bottom > top, "字号 {size}:黑底上应该看得见字幕");
        (top, bottom - top + 1)
    };

    let (_, small) = measure(28);
    let (_, large) = measure(56);
    // 字号翻倍,字高应该大致翻倍(允许抗锯齿和编码带来的误差)。
    let ratio = large as f64 / small as f64;
    assert!(
        (1.6..=2.4).contains(&ratio),
        "字号 28→56 的字高比应该是 2 倍左右,实际 {ratio:.2}(28 号高 {small} 像素,56 号高 {large} 像素);\
         比例接近 1 说明字号还被 libass 的默认缩放放大着"
    );
    // 56 像素的字:字身高度不该超过字号本身太多,更不该铺满画面中部。
    assert!(
        large <= 90,
        "56 像素的字不该渲染成 {large} 像素高(默认 PlayRes 没被抵消时会变成两百多像素)"
    );

    // 字幕离底边的距离同样是像素:边距加大 200,字幕就该上移 200 像素。
    // （`subtitle_margin_v` 不接受 0,所以基准取 20。）
    let with_margin = |margin: u32| -> usize {
        let template = VersionTemplate {
            subtitle_size: 56,
            subtitle_margin_v: Some(margin),
            ..VersionTemplate::default()
        };
        let graph = build_version_graph(
            &template,
            &spec(),
            &subtle(0),
            &EpisodeInput {
                subtitle_path: subtitle.to_str(),
                ..episode_input.clone()
            },
            &GraphExtras {
                font: Some(font),
                ..GraphExtras::default()
            },
            3,
        )
        .expect("生成滤镜图");
        assert!(
            graph.filter_complex.contains(&format!("MarginV={margin}")),
            "边距要进 force_style:{}",
            graph.filter_complex
        );
        let (output, _, _) = run_graph(&runner, &dir, &format!("margin{margin}"), &graph);
        let frame = grab_frame(&runner, &output, 1.0);
        lit_rows(&frame, width, height).1
    };
    let default_bottom = with_margin(20);
    let raised_bottom = with_margin(220);
    let moved = default_bottom as i64 - raised_bottom as i64;
    assert!(
        (150..=250).contains(&moved),
        "边距 20→220(差 200 像素)应该把字幕上移约 200 像素,实际 {moved}(底边 {default_bottom} → {raised_bottom})"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
#[test]
fn subtitle_and_watermark_reach_the_output() {
    let runner = SystemRunner::new();
    if !ffmpeg_available(&runner) {
        eprintln!("跳过：本机没有可用的 ffmpeg");
        return;
    }
    let Some(font) = system_font() else {
        eprintln!("跳过：本机没有找到中文字体（水印要写中文）");
        return;
    };
    let missing = filters_available(&runner, &["ssim", "psnr", "drawtext", "subtitles"]);
    if !missing.is_empty() {
        eprintln!("跳过：本机 ffmpeg 缺少滤镜 {}", missing.join("、"));
        return;
    }

    let dir = test_dir("text");
    let source = dir.join("第01集.mp4");
    // 黑底：这样九宫格里任何一个亮像素都只可能来自字幕或水印。
    make_black_source(&runner, &source, 3.0);
    let subtitle = dir.join("第01集.srt");
    std::fs::write(
        &subtitle,
        "1\n00:00:00,200 --> 00:00:02,500\n这是一句测试字幕\n",
    )
    .expect("写字幕文件");
    let probe = dossier_core::probe_episode(
        &dossier_core::EpisodeFile {
            path: source.clone(),
            file_name: "第01集.mp4".into(),
            size_bytes: 0,
            episode_number: None,
        },
        &dossier_core::FfprobeConfig { bin: None },
    )
    .expect("探测素材");

    let template = VersionTemplate {
        watermark_text: "测试剧名".into(),
        // 透明度拉满，让「水印有没有画上去」这件事在像素上看得清。
        watermark_opacity: 1.0,
        ..VersionTemplate::default()
    };
    let episode_input = EpisodeInput {
        duration_sec: probe.spec.duration_sec,
        subtitle_path: subtitle.to_str(),
        ..episode(&source)
    };
    // 对照组：同一段素材、同样处理，但不烧字幕、不放水印。
    let control = build_version_graph(
        &VersionTemplate::default(),
        &spec(),
        &subtle(0),
        &EpisodeInput {
            subtitle_path: None,
            ..episode_input.clone()
        },
        &GraphExtras::default(),
        3,
    )
    .expect("对照组滤镜图");
    let (control_out, _, _) = run_graph(&runner, &dir, "control", &control);

    let graph = build_version_graph(
        &template,
        &spec(),
        &subtle(0),
        &episode_input,
        &GraphExtras {
            font: Some(font),
            ..GraphExtras::default()
        },
        3,
    )
    .expect("生成滤镜图");
    let (output, _, _) = run_graph(&runner, &dir, "text", &graph);

    let width = spec().width as usize;
    let height = spec().height as usize;
    let before = bright_by_region(&grab_frame(&runner, &control_out, 1.0), width, height);
    let after = bright_by_region(&grab_frame(&runner, &output, 1.0), width, height);
    let gain = region_gain(&before, &after);

    // 对照组是黑底、不烧字幕、不放水印:九宫格里一个亮像素都不该有。
    assert_eq!(
        before, [0usize; 9],
        "对照组是纯黑画面,不该有亮像素:{before:?}"
    );
    // 带上字幕和水印之后,亮像素只可能来自它们:
    // 右上角(下标 2)是剧名水印,下排(6/7/8)是字幕。上排左侧和中排左侧不该有东西。
    assert!(gain[2] > 100, "右上角应该有剧名水印,实际 {gain:?}");
    assert!(
        gain[6] > 100 || gain[7] > 100 || gain[8] > 100,
        "下排应该有字幕,实际 {gain:?}"
    );
    assert_eq!(gain[0], 0, "左上角不该有字:{gain:?}");
    assert_eq!(gain[1], 0, "上排中间不该有字:{gain:?}");

    // 字幕应该贴着底边、高度合理(字号 56,字高应当在几十像素量级)。
    // 量的时候只扫左边三分之二:右上角是水印,一起扫会把两个元素连成一片。
    let frame = grab_frame(&runner, &output, 1.0);
    let (subtitle_top, subtitle_bottom) = lit_rows_in(&frame, width, height, 0, width * 2 / 3);
    let subtitle_height = subtitle_bottom - subtitle_top + 1;
    assert!(
        subtitle_height <= 120,
        "56 像素的字幕不该有 {subtitle_height} 像素高(默认 PlayRes 没被抵消时会变成两百多像素)"
    );
    assert!(
        subtitle_bottom > height - 100,
        "字幕要贴着底边,实际底部在 {subtitle_bottom}(画面高 {height})"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
