//! 产物戳记的端到端用例：真的跑一次擦除，再检查 `<输出>.stamp.json`。
//!
//! 手工跑一次只能证明「当时那台机器可以」；这里把「默认必须写戳记、`--no-stamp` 必须不写」
//! 钉成回归用例。缺 ffmpeg/ffprobe 的环境直接跳过（本仓库的其余单测同样按能力跳过）。

use std::path::{Path, PathBuf};
use std::process::Command;

use dossier_core::pipeline_version::{pipeline_version, ArtifactStamp, ERASE_PIPELINE};

fn tool_available(program: &str) -> bool {
    Command::new(program)
        .arg("-version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dossier-inpaint-stamp-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn synthetic_clip(dir: &Path) -> PathBuf {
    let path = dir.join("in.mp4");
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg("testsrc=size=320x240:rate=12")
        .args(["-frames:v", "24", "-pix_fmt", "yuv420p"])
        .arg(&path)
        .status()
        .expect("ffmpeg 启动失败");
    assert!(status.success(), "生成测试素材失败");
    path
}

fn run_inpaint(dir: &Path, input: &Path, extra: &[&str]) -> (PathBuf, std::process::Output) {
    let output = dir.join("fixed.mp4");
    let mut command = Command::new(env!("CARGO_BIN_EXE_dossier-inpaint"));
    command
        .args(["--input"])
        .arg(input)
        .args(["--output"])
        .arg(&output)
        .args(["--roi", "0,200,320,40", "--probe-frames", "1"])
        .args(extra);
    let result = command.output().expect("dossier-inpaint 启动失败");
    (output, result)
}

#[test]
fn erase_run_writes_a_valid_stamp_by_default() {
    if !tool_available("ffmpeg") || !tool_available("ffprobe") {
        eprintln!("跳过：本机没有 ffmpeg/ffprobe");
        return;
    }
    let dir = temp_dir("default");
    let input = synthetic_clip(&dir);
    let (output, result) = run_inpaint(&dir, &input, &[]);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(result.status.code().is_some(), "进程被信号终止：{stderr}");
    assert!(output.is_file(), "没有产出擦除结果：{stderr}");

    let stamp_path = PathBuf::from(format!("{}.stamp.json", output.display()));
    assert!(stamp_path.is_file(), "默认没有写产物戳记：{stderr}");
    let stamp = ArtifactStamp::load(&stamp_path).expect("戳记无法解析");
    stamp.validate().expect("戳记不自洽");
    assert_eq!(stamp.pipeline, ERASE_PIPELINE);
    assert_eq!(
        stamp.pipeline_version,
        pipeline_version(ERASE_PIPELINE).unwrap()
    );
    let tool = stamp.tool.expect("戳记没有记录工具");
    assert_eq!(tool.name, "dossier-inpaint");
    assert!(tool.version.is_some());
    assert!(
        stamp
            .outputs
            .iter()
            .any(|artifact| artifact.path.ends_with("fixed.mp4")),
        "戳记没有登记输出视频：{:?}",
        stamp.outputs
    );
    assert!(!stamp.inputs.is_empty(), "戳记没有记录输入摘要");
}

#[test]
fn no_stamp_skips_the_stamp_file() {
    if !tool_available("ffmpeg") || !tool_available("ffprobe") {
        eprintln!("跳过：本机没有 ffmpeg/ffprobe");
        return;
    }
    let dir = temp_dir("nostamp");
    let input = synthetic_clip(&dir);
    let (output, _result) = run_inpaint(&dir, &input, &["--no-stamp"]);
    let stamp_path = PathBuf::from(format!("{}.stamp.json", output.display()));
    assert!(!stamp_path.exists(), "--no-stamp 仍然写了戳记");
}
