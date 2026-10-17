//! 滤镜图在各档参数**上下限两端**的快照，以及画质下限的判定（M4b-1，计划第 5 节「滤镜图」「画质」）。
//!
//! 模块内的单测用的是中间档的一组典型值；这里专门走极端值：服务端 `recipes.py` 里
//! light / medium / heavy 三档每个参数的上下限。极端值是最容易出事的地方——
//! 裁帧数大到吃掉整集、变速到 0.97、放大 3% 之后尺寸算成奇数，都在这里钉住。
//!
//! 画质下限的数值和服务端 `QUALITY_FLOORS` 对齐（COMMERCE.md 3.6）：
//! light (0.97, 38)、medium (0.92, 35)、heavy (0.84, 32)。

use dossier_core::version::{
    build_version_graph, fill_stats_paths, ColorPreset, EpisodeInput, GraphExtras, OutputSpec,
    QualityFloor, QualityResult, SubtleParams, VersionTemplate, PSNR_STATS_PLACEHOLDER,
    SSIM_STATS_PLACEHOLDER,
};
use serde_json::json;

fn spec() -> OutputSpec {
    OutputSpec::from_params(&json!({
        "width": 1080, "height": 1920, "fps": 30,
        "video_bitrate_kbps": 5000, "audio_bitrate_kbps": 128,
        "max_duration_sec": 900, "container": "mp4",
        "video_codec": "h264", "audio_codec": "aac",
    }))
    .expect("通用规格")
}

/// 服务端各档参数的上下限（`cloud/src/drama_cloud/commerce/recipes.py` 的 `SUBTLE_RANGES`）。
/// `at_low = true` 取每项下限，否则取上限。
fn subtle_at(strength: &str, at_low: bool) -> SubtleParams {
    // (下限, 上限)
    let (grain, zoom, brightness, saturation_span, hue, tempo_span, pitch, trim) = match strength {
        "light" => (
            (1, 3),
            (1.0, 1.5),
            (0.0, 0.01),
            0.02,
            (0.0, 1.0),
            0.01,
            (0.0, 0.1),
            (1, 2),
        ),
        "medium" => (
            (3, 6),
            (1.5, 2.2),
            (0.0, 0.02),
            0.04,
            (0.0, 2.0),
            0.02,
            (0.0, 0.2),
            (2, 3),
        ),
        "heavy" => (
            (6, 10),
            (2.2, 3.0),
            (0.0, 0.03),
            0.06,
            (0.0, 3.0),
            0.03,
            (0.0, 0.3),
            (3, 5),
        ),
        other => panic!("没有这个档位：{other}"),
    };
    let pick = |pair: (f64, f64)| if at_low { pair.0 } else { pair.1 };
    let pick_u = |pair: (u64, u64)| if at_low { pair.0 } else { pair.1 };
    SubtleParams::from_params(&json!({
        "grain": pick_u(grain),
        "zoom_percent": pick(zoom),
        // 偏移量取负号方向的极值，顺带覆盖「负偏移」这一侧。
        "brightness": -pick(brightness),
        "saturation": if at_low { 1.0 - saturation_span } else { 1.0 + saturation_span },
        "hue_degrees": -pick(hue),
        "tempo": if at_low { 1.0 - tempo_span } else { 1.0 + tempo_span },
        "pitch_semitones": -pick(pitch),
        "trim_head_frames": pick_u(trim),
        "trim_tail_frames": pick_u(trim),
    }))
    .expect("档位参数")
}

fn episode(duration_sec: f64, width: u32, height: u32) -> EpisodeInput<'static> {
    EpisodeInput {
        path: "/素材/剧A/第01集.mp4",
        width,
        height,
        fps: 30.0,
        duration_sec,
        has_audio: true,
        subtitle_path: None,
    }
}

/// 把滤镜图里每个 `scale=W:H` / `crop=W:H` 的宽高取出来。
fn sizes(filter: &str, name: &str) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let needle = format!("{name}=");
    let mut rest = filter;
    while let Some(index) = rest.find(&needle) {
        rest = &rest[index + needle.len()..];
        let tail: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == ':')
            .collect();
        let mut parts = tail.split(':');
        if let (Some(w), Some(h)) = (parts.next(), parts.next()) {
            if let (Ok(w), Ok(h)) = (w.parse(), h.parse()) {
                out.push((w, h));
            }
        }
    }
    out
}

#[test]
fn every_strength_bound_produces_a_usable_graph() {
    let template = VersionTemplate::default();
    for strength in ["light", "medium", "heavy"] {
        for at_low in [true, false] {
            let subtle = subtle_at(strength, at_low);
            let graph = build_version_graph(
                &template,
                &spec(),
                &subtle,
                &episode(600.0, 1080, 1920),
                &GraphExtras::default(),
                42,
            )
            .unwrap_or_else(|error| panic!("{strength} {at_low} 应该能生成：{error}"));

            let bound = if at_low { "下限" } else { "上限" };
            // 不受参数取值影响的几项必须一直在：裁帧、颗粒、变速、音频重采样。
            for needle in ["trim=start=", "noise=alls=", "setpts=", "atempo="] {
                assert!(
                    graph.filter_complex.contains(needle),
                    "{strength} {bound}：滤镜图里缺少 {needle}"
                );
            }
            // 色相偏移在下限档是 0（服务端 SUBTLE_RANGES 的下限就是 0），
            // 这时按设计不加 hue 滤镜——加一个 h=0 的空操作只会白费一遍解码。
            // 取上限时必须出现。
            assert_eq!(
                graph.filter_complex.contains("hue=h="),
                !at_low,
                "{strength} {bound}：色相滤镜该不该出现判断错了"
            );
            // 所有尺寸都必须是偶数：H.264 的 yuv420p 要求宽高能被 2 整除，
            // 放大 1%–3% 之后很容易算出奇数。
            for name in ["scale", "crop", "pad"] {
                for (w, h) in sizes(&graph.filter_complex, name) {
                    assert_eq!(w % 2, 0, "{strength} {bound}：{name} 宽 {w} 不是偶数");
                    assert_eq!(h % 2, 0, "{strength} {bound}：{name} 高 {h} 不是偶数");
                }
            }
            // 输出时长要为正，且不超过规格上限（否则交付出去就是超时长的废片）。
            assert!(
                graph.output_duration_sec > 0.0,
                "{strength} {bound}：输出时长不该是 0 或负数"
            );
            assert!(
                graph.output_duration_sec <= spec().max_duration_sec as f64,
                "{strength} {bound}：输出时长 {} 超过上限",
                graph.output_duration_sec
            );
            assert_eq!(graph.output_name, "第01集.mp4");
            // 画质两路统计的占位符都在，调用方会替换成临时文件路径。
            assert!(graph.filter_complex.contains(SSIM_STATS_PLACEHOLDER));
            assert!(graph.filter_complex.contains(PSNR_STATS_PLACEHOLDER));
        }
    }
}

#[test]
fn same_seed_and_params_give_byte_identical_graphs() {
    let template = VersionTemplate::default();
    for strength in ["light", "medium", "heavy"] {
        let subtle = subtle_at(strength, false);
        let once = build_version_graph(
            &template,
            &spec(),
            &subtle,
            &episode(60.0, 1080, 1920),
            &GraphExtras::default(),
            7,
        )
        .unwrap();
        let twice = build_version_graph(
            &template,
            &spec(),
            &subtle,
            &episode(60.0, 1080, 1920),
            &GraphExtras::default(),
            7,
        )
        .unwrap();
        assert_eq!(once, twice, "{strength}：同参数同种子必须完全一样");
    }
    // 换种子会换出不同的噪点种子（续跑用同一个配方，所以这里只验证「种子确实进了滤镜图」）。
    let a = build_version_graph(
        &template,
        &spec(),
        &subtle_at("medium", false),
        &episode(60.0, 1080, 1920),
        &GraphExtras::default(),
        1,
    )
    .unwrap();
    let b = build_version_graph(
        &template,
        &spec(),
        &subtle_at("medium", false),
        &episode(60.0, 1080, 1920),
        &GraphExtras::default(),
        2,
    )
    .unwrap();
    assert_ne!(a.filter_complex, b.filter_complex, "种子要影响滤镜图");
    assert!(a.filter_complex.contains("all_seed=1"));
    assert!(b.filter_complex.contains("all_seed=2"));
}

#[test]
fn episode_shorter_than_the_trimmed_frames_is_refused() {
    // heavy 档首尾各裁 5 帧，30fps 下是 1/3 秒。比这还短的一集不能硬裁成负时长。
    let error = build_version_graph(
        &VersionTemplate::default(),
        &spec(),
        &subtle_at("heavy", false),
        &episode(0.2, 1080, 1920),
        &GraphExtras::default(),
        1,
    )
    .expect_err("太短的一集应该被拒绝，而不是产出负时长");
    assert_eq!(error.code(), "bad_spec");
    assert!(
        error.to_string().contains("分集太短"),
        "错误信息要说清是哪里不对：{error}"
    );

    // 另一头：处理后超过规格的单集时长上限（15 分钟）同样要拦住，不能交付超长片。
    let error = build_version_graph(
        &VersionTemplate::default(),
        &spec(),
        &subtle_at("light", true),
        &episode(1000.0, 1080, 1920),
        &GraphExtras::default(),
        1,
    )
    .expect_err("超过时长上限应该被拒绝");
    assert_eq!(error.code(), "bad_spec");
    assert!(
        error.to_string().contains("超过输出规格上限"),
        "错误信息要说清超了什么：{error}"
    );
}

#[test]
fn landscape_source_is_letterboxed_not_stretched() {
    let graph = build_version_graph(
        &VersionTemplate::default(),
        &spec(),
        &subtle_at("light", true),
        &episode(60.0, 1920, 1080),
        &GraphExtras::default(),
        1,
    )
    .unwrap();
    // 横屏进竖屏：等比缩到 1080×608 再上下补边到 1080×1920，不能直接拉伸。
    assert!(
        graph.filter_complex.contains("scale=1080:608"),
        "横屏素材应该等比缩放：{}",
        graph.filter_complex
    );
    assert!(
        graph.filter_complex.contains("pad=1080:1920"),
        "缩放之后要补边到目标尺寸：{}",
        graph.filter_complex
    );
}

#[test]
fn color_preset_is_applied_after_the_quality_split() {
    // 调色预设是第一层「看得见的内容」,用户明确选的,不该算成画质损失。
    // 它必须加在输出这一路,而画质对比的两路里不能有它。
    for (preset, filter) in [
        (ColorPreset::Warm, "colorbalance=rs=0.06:gs=0.02:bs=-0.06"),
        (ColorPreset::HighContrast, "eq=contrast=1.12"),
        (ColorPreset::LowSaturation, "eq=saturation=0.85"),
    ] {
        let template = VersionTemplate {
            color_preset: preset,
            ..VersionTemplate::default()
        };
        let graph = build_version_graph(
            &template,
            &spec(),
            &subtle_at("medium", false),
            &episode(60.0, 1080, 1920),
            &GraphExtras::default(),
            42,
        )
        .unwrap_or_else(|error| panic!("{preset:?} 应该能生成：{error}"));

        // 对比用的两路（q_a / q_b）由 split 直接产出，链上不能挂调色预设。
        let chains: Vec<&str> = graph.filter_complex.split(';').collect();
        let split_line = chains
            .iter()
            .find(|chain| chain.contains("split=3[q_main][q_a][q_b]"))
            .unwrap_or_else(|| panic!("{preset:?}：找不到分流那一行"));
        assert!(
            !split_line.contains(filter),
            "{preset:?}：调色预设不该在画质对比之前，实际链是 {split_line}"
        );

        // 预设要真的出现在滤镜图里（加在分流之后、输出那一路）。
        assert!(
            graph.filter_complex.contains(filter),
            "{preset:?}：预设没进滤镜图：{}",
            graph.filter_complex
        );
        let graded_line = chains
            .iter()
            .find(|chain| chain.starts_with("[q_main]"))
            .unwrap_or_else(|| panic!("{preset:?}：找不到套预设的那一行"));
        assert!(
            graded_line.contains(filter),
            "{preset:?}：预设应该加在 [q_main] 之后，实际是 {graded_line}"
        );
    }

    // 原色（不套预设）时不该凭空多出一段滤镜。
    let plain = build_version_graph(
        &VersionTemplate::default(),
        &spec(),
        &subtle_at("medium", false),
        &episode(60.0, 1080, 1920),
        &GraphExtras::default(),
        42,
    )
    .unwrap();
    assert!(
        !plain.filter_complex.contains("colorbalance"),
        "原色不该有 colorbalance：{}",
        plain.filter_complex
    );
    assert!(
        !plain.filter_complex.contains("[q_graded]"),
        "原色不该多出一段套预设的链：{}",
        plain.filter_complex
    );
}

#[test]
fn quality_floors_match_the_server_table() {
    // 服务端 QUALITY_FLOORS（COMMERCE.md 3.6）：三档的下限各不相同。
    for (strength, min_ssim, min_psnr) in [
        ("light", 0.97, 38.0),
        ("medium", 0.92, 35.0),
        ("heavy", 0.84, 32.0),
    ] {
        let floor = QualityFloor::from_params(&json!({
            "min_ssim": min_ssim, "min_psnr": min_psnr
        }))
        .unwrap_or_else(|error| panic!("{strength} 的下限应该能解析：{error}"));
        assert_eq!(floor.min_ssim, min_ssim);
        assert_eq!(floor.min_psnr, min_psnr);

        // 刚好等于下限算通过（下限是「不得低于」）。
        let exactly = QualityResult {
            ssim: min_ssim,
            psnr: min_psnr,
        };
        assert!(exactly.passes(&floor), "{strength}：等于下限应该通过");
        // 任何一项低于下限就不通过——两项是「且」的关系，不能一项好就放行。
        assert!(
            !QualityResult {
                ssim: min_ssim - 0.001,
                psnr: min_psnr + 10.0,
            }
            .passes(&floor),
            "{strength}：SSIM 不达标就不能通过"
        );
        assert!(
            !QualityResult {
                ssim: 1.0,
                psnr: min_psnr - 0.1,
            }
            .passes(&floor),
            "{strength}：PSNR 不达标就不能通过"
        );
    }
}

#[test]
fn quality_floor_rejects_impossible_values() {
    // 服务端不该签出这些值；万一签出来，解析阶段就要拦住，不能当成「没有下限」。
    for bad in [
        json!({"min_ssim": 1.5, "min_psnr": 35.0}),
        json!({"min_ssim": -0.1, "min_psnr": 35.0}),
        json!({"min_ssim": 0.92, "min_psnr": -1.0}),
        json!({"min_ssim": "高", "min_psnr": 35.0}),
        json!({"min_psnr": 35.0}),
        json!({"min_ssim": 0.92}),
    ] {
        assert!(
            QualityFloor::from_params(&bad).is_err(),
            "不合理的画质下限应该被拒：{bad}"
        );
    }
}

#[test]
fn quality_stats_reject_missing_or_broken_files() {
    // 文件读不出来（ffmpeg 没写、被删了）时必须报错，不能当成 0 分或满分。
    assert!(QualityResult::from_stats("", "").is_err(), "两个文件都空");
    assert!(
        QualityResult::from_stats("n:1 All:0.9900\n", "").is_err(),
        "只有 SSIM 没有 PSNR"
    );
    assert!(
        QualityResult::from_stats("", "n:1 psnr_avg:40.00\n").is_err(),
        "只有 PSNR 没有 SSIM"
    );
    assert!(
        QualityResult::from_stats("乱码乱码\n", "也是乱码\n").is_err(),
        "内容解析不出来就要报错"
    );

    // 正常情况：取最后一行（ffmpeg 逐帧追加，最后一行是全片汇总）。
    let result = QualityResult::from_stats(
        "n:1 All:0.9000\nn:2 All:0.8000\nn:3 All:0.9500\n",
        "n:1 psnr_avg:40.00\nn:2 psnr_avg:33.50\nn:3 psnr_avg:36.25\n",
    )
    .expect("能解析");
    assert_eq!(result.ssim, 0.95);
    assert_eq!(result.psnr, 36.25);
}

#[test]
fn stats_placeholders_are_replaced_with_real_paths() {
    let graph = build_version_graph(
        &VersionTemplate::default(),
        &spec(),
        &subtle_at("medium", false),
        &episode(60.0, 1080, 1920),
        &GraphExtras::default(),
        1,
    )
    .unwrap();
    let filled = fill_stats_paths(
        &graph.filter_complex,
        "/tmp/任务/stats/第01集.ssim.log",
        "/tmp/任务/stats/第01集.psnr.log",
    );
    assert!(!filled.contains(SSIM_STATS_PLACEHOLDER), "占位符要被换掉");
    assert!(!filled.contains(PSNR_STATS_PLACEHOLDER), "占位符要被换掉");
    // 路径里的中文和斜杠原样保留，只有 ffmpeg 要求转义的字符会被转义。
    assert!(filled.contains("第01集.ssim.log"));
    assert!(filled.contains("第01集.psnr.log"));
}
