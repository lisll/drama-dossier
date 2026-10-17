//! 区域擦除的端到端链路（M4b-2 验证第 3 步）：造样例剧 → 自动识别 → 真擦一段 → 检查产物。
//!
//! 桌面端「批量擦除」走任务队列，队列本身在 `src-tauri/src/jobs_tests.rs` 里用假执行器测过；
//! 这里验证的是**真正被队列调用的那条命令链**：`--detect-only` 认出区域 → 带区域跑擦除 →
//! 产物存在、能读、报告里记了结论。队列与命令之间的装配差异（路径、参数）由
//! `dossier-inpaint` 的交叉校验测试守着。
//!
//! 缺 ffmpeg/ffprobe 的环境直接跳过。

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
    let dir = std::env::temp_dir().join(format!("dossier-erase-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 造一集：**渐变**背景 + 底部一段固定暗色字幕（模拟硬字幕 / 固定水印）。
///
/// 素材形态是刻意选的，两个坑都踩过：
/// 1. 「动态画面上贴一块纯色」：`column` 按同一列上下文填色，对与背景毫无渐变关系的
///    硬色块天然填不好，残留检查会判没过；
/// 2. 「纯色背景上贴一条硬边白色色带」：擦完虽然残留为 0，但色带边界被抹平后，
///    抹平后的区域与参考画面的边界梯度差太多，**接缝分数** 3.70 超过失败阈值 3.00。
///
/// 渐变背景 + 暗色文字是 `column` 基线能自然处理的形态：擦掉之后边界过渡平滑，
/// 自检的三项（残留 / 接缝 / 一致性）都能真实反映「链路跑通了」。
fn episode_with_watermark(path: &Path) {
    // 纵向渐变：每行的亮度随 y 变化，column 上下文的像素值连续可插值。
    let background = "gradients=s=320x240:c0=0x203040:c1=0x90a0b0:x0=0:y0=0:x1=0:y1=240:d=2:r=25";
    let filter = "drawtext=text='FIXED 2026':fontsize=28:\
                  fontcolor=black@0.85:x=(w-text_w)/2:y=205";
    let status = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            background,
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=2",
            "-vf",
            filter,
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
        ])
        .arg(path)
        .status()
        .expect("ffmpeg 启动失败");
    assert!(status.success(), "生成测试素材失败");
}

/// 跑一次只检测，返回候选框。
fn detect(dir: &Path, input: &Path) -> serde_json::Value {
    let report = dir.join("candidates.json");
    let output = Command::new(env!("CARGO_BIN_EXE_dossier-inpaint"))
        .args(["--input"])
        .arg(input)
        .args(["--detect-only", "--report-json"])
        .arg(&report)
        .arg("--still-samples")
        .arg("10")
        .output()
        .expect("dossier-inpaint 启动失败");
    assert_eq!(
        output.status.code(),
        Some(0),
        "只检测应当成功：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_str(&std::fs::read_to_string(&report).expect("检测报告")).expect("报告是 JSON")
}

#[test]
fn detect_then_erase_against_a_real_clip() {
    if !ready() {
        eprintln!("跳过：本机没有 ffmpeg/ffprobe");
        return;
    }
    let dir = temp_dir("full");
    let input = dir.join("第01集.mp4");
    episode_with_watermark(&input);

    // 1. 自动识别：应当认出底部那条固定字幕带。
    let report = detect(&dir, &input);
    let candidates = report["candidates"].as_array().expect("候选是数组");
    assert!(
        !candidates.is_empty(),
        "底部有固定字幕，应该检出来：{report}"
    );
    let mark = candidates
        .iter()
        .find(|candidate| {
            let y = candidate["y"].as_u64().unwrap_or(0);
            let h = candidate["height"].as_u64().unwrap_or(0);
            // 字幕带在 y≈200..240；候选应当和它有交叠。
            y < 240 && y + h > 200
        })
        .unwrap_or_else(|| panic!("没有框住底部字幕带的候选：{candidates:?}"));
    let (x, y) = (mark["x"].as_u64().unwrap(), mark["y"].as_u64().unwrap());
    let (w, h) = (
        mark["width"].as_u64().unwrap(),
        mark["height"].as_u64().unwrap(),
    );

    // 2. 用识别出来的区域真擦一遍（走和桌面端批量完全相同的参数形态）。
    let output = dir.join("擦除输出").join("第01集.mp4");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    let run = Command::new(env!("CARGO_BIN_EXE_dossier-inpaint"))
        .args(["--input"])
        .arg(&input)
        .args(["--output"])
        .arg(&output)
        .arg("--roi")
        .arg(format!("{x},{y},{w},{h}"))
        .args(["--detector", "fixed", "--inpainter", "column"])
        // 只处理很短一段，端到端重点是链路通不通，不是修复质量。
        .args(["--start", "0", "--duration", "1"])
        .args(["--probe-frames", "2", "--clip-frames", "6"])
        .output()
        .expect("dossier-inpaint 启动失败");

    // 退出码语义：0=通过，3=需人工抽查（两者都表示「跑完了、自检有结论」）；
    // 2=自检判定没擦干净，1=执行错误。真机的擦除质量要在真实素材上评估（真机阶段做）。
    let code = run.status.code();
    // 报告路径由 CLI 用 `with_extension("inpaint-report.json")` 派生：
    // `第01集.mp4` → `第01集.inpaint-report.json`（扩展名被**替换**，不是追加）。
    let report_path = output.with_extension("inpaint-report.json");
    if !matches!(code, Some(0) | Some(3)) {
        let detail =
            std::fs::read_to_string(&report_path).unwrap_or_else(|_| "（没有报告）".into());
        eprintln!("擦除退出码 {code:?}，自检报告：{detail}");
        eprintln!("stderr：{}", String::from_utf8_lossy(&run.stderr));
    }
    assert!(
        matches!(code, Some(0) | Some(3)),
        "擦除应当跑完（通过或需人工抽查），实际 {code:?}：{}",
        String::from_utf8_lossy(&run.stderr)
    );

    // 3. 产物存在、能读、报告在。
    assert!(output.is_file(), "没有产出擦除后的视频");
    let probe = Command::new("ffprobe")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-show_entries",
            "stream=codec_type,width,height",
            "-of",
            "csv=p=0",
        ])
        .arg(&output)
        .output()
        .expect("ffprobe 启动失败");
    assert!(probe.status.success(), "产物读不出来");
    let text = String::from_utf8_lossy(&probe.stdout);
    assert!(text.contains("video"), "产物里应当有视频流：{text}");

    assert!(report_path.is_file(), "没有写处理报告：{report_path:?}");
    let report_text = std::fs::read_to_string(&report_path).unwrap();
    assert!(
        report_text.contains("verification"),
        "处理报告里应当有自检结论：{report_text}"
    );
}

#[test]
fn detect_only_does_not_touch_the_source_episode() {
    if !ready() {
        eprintln!("跳过：本机没有 ffmpeg/ffprobe");
        return;
    }
    let dir = temp_dir("readonly");
    let input = dir.join("第01集.mp4");
    episode_with_watermark(&input);
    let before = std::fs::metadata(&input).unwrap();
    let before_bytes = std::fs::read(&input).unwrap();

    // 只检测跑几次都不该改动源文件（界面上「自动识别」可以反复点）。
    for _ in 0..2 {
        let _ = detect(&dir, &input);
    }
    let after = std::fs::metadata(&input).unwrap();
    assert_eq!(before.len(), after.len(), "源文件大小被改了");
    assert_eq!(
        before_bytes,
        std::fs::read(&input).unwrap(),
        "源文件内容被改了"
    );
    // 目录里也不该多出视频。
    let videos: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "mp4"))
        .collect();
    assert_eq!(videos.len(), 1, "只检测不该产出新视频：{videos:?}");
}
