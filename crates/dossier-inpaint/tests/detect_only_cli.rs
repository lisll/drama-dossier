//! `--detect-only` 的端到端用例：真的造一段带固定角标的视频，再检查只检测入口。
//!
//! 这里钉的是「入口本身」的行为：不产出视频、退出码 0、JSON 结构对、能检出固定角标。
//! 判据算法本身的单测在 `src/still.rs` 里（合成帧，不依赖 ffmpeg）。
//!
//! 缺 ffmpeg/ffprobe 的环境直接跳过（本仓库其余用例同样按能力跳过）。

use std::path::{Path, PathBuf};
use std::process::Command;

fn tool_available(program: &str) -> bool {
    Command::new(program)
        .arg("-version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn ready() -> bool {
    tool_available("ffmpeg") && tool_available("ffprobe")
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dossier-inpaint-detect-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 造一段 2 秒、320×240 的视频：背景是平移的纹理（一直在动）+ 左上角一块固定不动的角标。
///
/// 用两条 lavfi 源叠起来做：`testsrc2` 提供一直在动的背景，`color` 画不动的方块再 overlay。
fn clip_with_fixed_corner(dir: &Path) -> PathBuf {
    let path = dir.join("in.mp4");
    let filter = "color=c=red:s=60x40:d=2[mark];\
                  [0:v][mark]overlay=x=10:y=10:shortest=1[out]";
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=25:duration=2",
            "-filter_complex",
            filter,
            "-map",
            "[out]",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&path)
        .status()
        .expect("ffmpeg 启动失败");
    assert!(status.success(), "生成测试素材失败");
    path
}

fn run_detect(dir: &Path, input: &Path, extra: &[&str]) -> (PathBuf, std::process::Output) {
    let report = dir.join("candidates.json");
    let mut command = Command::new(env!("CARGO_BIN_EXE_dossier-inpaint"));
    command
        .args(["--input"])
        .arg(input)
        .args(["--detect-only", "--report-json"])
        .arg(&report)
        .args(extra);
    let output = command.output().expect("dossier-inpaint 启动失败");
    (report, output)
}

#[test]
fn detect_only_reports_why_nothing_was_found() {
    // 诊断：把判据的中间量打出来。真实素材检不出东西时，先看这一步的数值，
    // 而不是在外面反复试参数（那样永远不知道为什么）。
    if !ready() {
        eprintln!("跳过：本机没有 ffmpeg/ffprobe");
        return;
    }
    let dir = temp_dir("diag");
    let input = clip_with_fixed_corner(&dir);
    // 直接在这里抽几帧给诊断用，和 CLI 走同一条 ffmpeg 路径。
    let mut frames: Vec<Vec<u8>> = Vec::new();
    for index in 0..8u32 {
        let seconds = f64::from(index) * 0.25;
        let output = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-ss"])
            .arg(format!("{seconds:.3}"))
            .arg("-i")
            .arg(&input)
            .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "bgr24", "-"])
            .output()
            .expect("ffmpeg 启动失败");
        assert!(output.status.success(), "抽帧失败");
        frames.push(output.stdout);
    }
    let size = dossier_inpaint::mask::FrameSize::new(320, 240);
    let config = dossier_inpaint::still::StillConfig::default();
    let diagnostics = dossier_inpaint::still::diagnose(&frames, size, &config).expect("诊断");
    eprintln!("诊断：{diagnostics:?}");
    // 不断言具体数值（素材由 ffmpeg 现生成，不同版本会有差异），
    // 只要求「诊断本身跑得通、字段之间有基本的因果关系」。
    assert!(diagnostics.variance_limit <= config.max_variance + f64::EPSILON);
    assert!(diagnostics.spread_limit <= config.max_spread + f64::EPSILON);
    assert!(
        diagnostics.masked_pixels <= diagnostics.still_pixels,
        "进入连通块分析的像素不可能多于「静止」像素：{diagnostics:?}"
    );
}

#[test]
fn detect_only_finds_the_fixed_corner_and_writes_no_video() {
    if !ready() {
        eprintln!("跳过：本机没有 ffmpeg/ffprobe");
        return;
    }
    let dir = temp_dir("corner");
    let input = clip_with_fixed_corner(&dir);
    let (report_path, output) = run_detect(&dir, &input, &["--still-samples", "12"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "只检测应当成功（没检出也算成功）：{stderr}"
    );

    // 不产出视频：目录里除输入和报告之外不该多出 mp4。
    let mp4_count = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "mp4"))
        .count();
    assert_eq!(mp4_count, 1, "只检测不该产出新视频");

    let text = std::fs::read_to_string(&report_path).expect("应当写出检测报告");
    let value: serde_json::Value = serde_json::from_str(&text).expect("报告是合法 JSON");
    assert_eq!(value["width"], 320);
    assert_eq!(value["height"], 240);
    assert!(
        value["video"]
            .as_str()
            .is_some_and(|v| v.contains("in.mp4")),
        "报告里要带素材路径：{value}"
    );
    assert_eq!(value["samples"], 12, "应当按 --still-samples 抽帧");

    let candidates = value["candidates"].as_array().expect("候选是数组");
    assert!(
        !candidates.is_empty(),
        "左上角有固定角标，应该检出候选：{value}"
    );
    // 左上角那块 (10,10,60x40) 应当被某个候选框住。
    let hit = candidates.iter().any(|candidate| {
        let x = candidate["x"].as_u64().unwrap_or(u64::MAX);
        let y = candidate["y"].as_u64().unwrap_or(u64::MAX);
        let w = candidate["width"].as_u64().unwrap_or(0);
        let h = candidate["height"].as_u64().unwrap_or(0);
        x <= 12 && y <= 12 && x + w >= 68 && y + h >= 48
    });
    assert!(hit, "候选没有框住固定角标：{candidates:?}");
}

#[test]
fn detect_only_refuses_to_erase() {
    if !ready() {
        eprintln!("跳过：本机没有 ffmpeg/ffprobe");
        return;
    }
    let dir = temp_dir("conflict");
    let input = clip_with_fixed_corner(&dir);
    let output_path = dir.join("out.mp4");
    let result = Command::new(env!("CARGO_BIN_EXE_dossier-inpaint"))
        .args(["--input"])
        .arg(&input)
        .args(["--output"])
        .arg(&output_path)
        .arg("--detect-only")
        .output()
        .expect("dossier-inpaint 启动失败");
    assert_ne!(result.status.code(), Some(0), "同时给 --output 应当失败");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("不能和 --output 一起用"),
        "错误信息要说清冲突：{stderr}"
    );
    assert!(!output_path.exists(), "失败时不该产出视频");
}

#[test]
fn detect_only_accepts_a_custom_report_path_or_prints_to_stdout() {
    if !ready() {
        eprintln!("跳过：本机没有 ffmpeg/ffprobe");
        return;
    }
    let dir = temp_dir("stdout");
    let input = clip_with_fixed_corner(&dir);
    // 不给 --report-json：结果打到 stdout。
    let output = Command::new(env!("CARGO_BIN_EXE_dossier-inpaint"))
        .args(["--input"])
        .arg(&input)
        .arg("--detect-only")
        .args(["--still-samples", "6"])
        .output()
        .expect("dossier-inpaint 启动失败");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\"candidates\""),
        "没给 --report-json 时应当把 JSON 打到 stdout：{stdout}"
    );
}
