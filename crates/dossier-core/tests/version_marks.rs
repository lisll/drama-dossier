//! AI 显式 / 隐式标识与模板门槛（M4b-1，计划第 5 节「AI 标识」「门槛」）。
//!
//! AI 标识是**法规要求**的内容（《人工智能生成合成内容标识办法》第四条、GB 45438-2025），
//! 所以这里逐条钉住：显式标识的文字、位置、字高、持续时间，隐式标识的七个字段，
//! 以及「勾了 AI 却生成不出标识」的情况必须报错而不是静默出片。
//!
//! 模板门槛（背景音乐授权、AI 与水印抢位、取值范围）同样在这里钉住：它们决定了
//! 什么样的模板不能开工。

use dossier_core::version::{
    build_cover_args, build_version_graph, Corner, EpisodeInput, GraphExtras, OutputSpec,
    SubtleParams, VersionTemplate,
};
use serde_json::{json, Value};

const FONT: &str = "/字体/NotoSansCJK-Regular.ttc";

fn spec() -> OutputSpec {
    OutputSpec::from_params(&json!({
        "width": 1080, "height": 1920, "fps": 30,
        "video_bitrate_kbps": 5000, "audio_bitrate_kbps": 128,
        "max_duration_sec": 900, "container": "mp4",
        "video_codec": "h264", "audio_codec": "aac",
    }))
    .expect("通用规格")
}

fn subtle() -> SubtleParams {
    SubtleParams::from_params(&json!({
        "grain": 4, "zoom_percent": 2.0, "brightness": 0.01, "saturation": 1.02,
        "hue_degrees": 1.5, "tempo": 1.0, "pitch_semitones": 0.2,
        "trim_head_frames": 2, "trim_tail_frames": 2,
    }))
    .expect("中档参数")
}

fn episode() -> EpisodeInput<'static> {
    EpisodeInput {
        path: "/素材/剧A/第01集.mp4",
        width: 1080,
        height: 1920,
        fps: 30.0,
        duration_sec: 60.0,
        has_audio: true,
        subtitle_path: None,
    }
}

fn with_font() -> GraphExtras<'static> {
    GraphExtras {
        font: Some(FONT),
        ..GraphExtras::default()
    }
}

fn ai_template() -> VersionTemplate {
    VersionTemplate {
        ai_marked: true,
        ai_content_producer: "drama-dossier".into(),
        ..VersionTemplate::default()
    }
}

fn graph_of(
    template: &VersionTemplate,
    extras: &GraphExtras,
    seed: u64,
) -> dossier_core::version::VersionGraph {
    build_version_graph(template, &spec(), &subtle(), &episode(), extras, seed)
        .expect("应该能生成滤镜图")
}

#[test]
fn explicit_ai_mark_meets_the_standard() {
    let graph = graph_of(&ai_template(), &with_font(), 7);
    let filter = &graph.filter_complex;

    // 办法第四条：起始画面要有显著标识；GB 45438 5.4 要求持续不少于 2 秒，这里取 3 秒。
    assert!(filter.contains("text=人工智能生成合成"), "缺少起始画面标识");
    assert!(
        filter.contains("enable='between(t,0,3)'"),
        "起始标识要持续 3 秒：{filter}"
    );
    // 之后常驻「AI生成」，位置相同。
    assert!(filter.contains("text=AI生成"), "缺少常驻标识");
    assert!(
        filter.contains("enable='gt(t,3)'"),
        "常驻标识要在 3 秒之后接上"
    );

    // 位置：右下角（GB 45438 5.4 要求位于边或角）。
    let (x, y) = Corner::BottomRight.expressions();
    assert!(
        filter.contains(&format!("x={x}:y={y}")),
        "AI 标识要在右下角：{filter}"
    );

    // 字高不低于短边 5%：短边 1080 × 0.055 = 59.4，取偶数 60，达标。
    assert!(filter.contains("fontsize=60"), "字高不达标：{filter}");

    // 两条 drawtext 都要带字体，否则中文出方块。
    let marks: Vec<&str> = filter
        .split(',')
        .filter(|part| part.contains("人工智能生成合成") || part.contains("AI生成"))
        .collect();
    assert_eq!(marks.len(), 2, "应该正好两条 AI 标识：{marks:?}");
    assert_eq!(
        filter.matches(&format!("fontfile={FONT}")).count(),
        2,
        "两条 AI 标识都要指定字体"
    );
}

#[test]
fn implicit_ai_metadata_has_all_seven_fields() {
    let graph = graph_of(&ai_template(), &with_font(), 7);
    assert_eq!(
        graph.metadata.len(),
        1,
        "只写一份隐式标识（GB 45438 6.1 c）"
    );
    let (key, raw) = &graph.metadata[0];
    assert_eq!(key, "AIGC");

    let value: Value = serde_json::from_str(raw).expect("隐式标识必须是合法 JSON");
    let aigc = value.get("AIGC").expect("顶层键是 AIGC");
    let object = aigc.as_object().expect("AIGC 是对象");
    // 附录 E 是规范性附录：七个键一个都不能少、不能多。
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "ContentProducer",
            "ContentPropagator",
            "Label",
            "ProduceID",
            "PropagateID",
            "ReservedCode1",
            "ReservedCode2",
        ]
    );
    assert_eq!(aigc["Label"], json!("1"), "Label=1 表示含生成合成内容");
    assert_eq!(aigc["ContentProducer"], json!("drama-dossier"));
    // 附录 E 注 1：首次写入时传播方与制作方、传播编号与制作编号相同。
    assert_eq!(aigc["ContentPropagator"], aigc["ContentProducer"]);
    assert_eq!(aigc["PropagateID"], aigc["ProduceID"]);
    assert_eq!(aigc["ReservedCode1"], json!(""));
    assert_eq!(aigc["ReservedCode2"], json!(""));
    assert!(
        aigc["ProduceID"].as_str().is_some_and(|id| !id.is_empty()),
        "制作编号不能是空串"
    );
}

#[test]
fn produce_id_is_reproducible_and_per_episode() {
    let template = ai_template();
    // 同一集、同一个种子：续跑要能产出完全相同的文件，所以制作编号必须可复现。
    let a = graph_of(&template, &with_font(), 7);
    let b = graph_of(&template, &with_font(), 7);
    assert_eq!(a.metadata, b.metadata, "同种子同集的制作编号必须一致");

    // 换种子（换一次配方）要换编号。
    let other_seed = graph_of(&template, &with_font(), 8);
    assert_ne!(
        a.metadata, other_seed.metadata,
        "不同配方的制作编号应该不同"
    );

    // 同一个配方里，每集各有一个编号（附录 E 的 ProduceID 是针对一份内容的）。
    let second = build_version_graph(
        &template,
        &spec(),
        &subtle(),
        &EpisodeInput {
            path: "/素材/剧A/第02集.mp4",
            ..episode()
        },
        &with_font(),
        7,
    )
    .expect("第 2 集");
    assert_ne!(
        a.metadata, second.metadata,
        "同一配方下每集的制作编号应该不同"
    );
}

#[test]
fn no_ai_mark_means_no_metadata() {
    // 没勾 AI：既不画标识，也不写元数据——不能给没有 AI 内容的片子贴上标识。
    let graph = graph_of(&VersionTemplate::default(), &GraphExtras::default(), 7);
    assert!(graph.metadata.is_empty(), "没勾 AI 就不该有隐式标识");
    assert!(!graph.filter_complex.contains("人工智能生成合成"));
    assert!(!graph.filter_complex.contains("AI生成"));
}

#[test]
fn ai_mark_without_font_is_refused_not_silently_dropped() {
    // 勾了 AI 但本机没有中文字体：必须报错。要是静默跳过标识，出来的片子就不合规。
    let error = build_version_graph(
        &ai_template(),
        &spec(),
        &subtle(),
        &episode(),
        &GraphExtras::default(),
        7,
    )
    .expect_err("没有字体时必须拒绝生成");
    assert_eq!(error.code(), "bad_template");
    assert!(
        error.to_string().contains("中文字体"),
        "错误信息要告诉用户去指定字体：{error}"
    );
}

#[test]
fn ai_producer_charset_follows_appendix_e() {
    // 附录 E j)：可见 ASCII，且本程序不写入 `"` 和 `\`。
    for good in [
        "drama-dossier",
        "Tool_1.0",
        "a",
        "~!#$%&'()*+,-./:;<=>?@[]^_`{|}",
    ] {
        let template = VersionTemplate {
            ai_content_producer: good.into(),
            ..ai_template()
        };
        template
            .validate()
            .unwrap_or_else(|error| panic!("{good} 应该合法：{error}"));
    }
    for bad in [
        "",            // 空串
        "剧备",        // 中文
        "my tool",     // 空格
        "say\"hi\"",   // 半角引号
        "back\\slash", // 反斜杠
        "tab\there",   // 控制字符
        "emoji🎬",     // 非 ASCII
    ] {
        let template = VersionTemplate {
            ai_content_producer: bad.into(),
            ..ai_template()
        };
        let error = template
            .validate()
            .expect_err(&format!("{bad:?} 应该被拒绝"));
        assert_eq!(error.code(), "bad_template", "{bad:?}");
    }
    // 没勾 AI 时不校验这个字段（用户可能填了又取消勾选）。
    let template = VersionTemplate {
        ai_marked: false,
        ai_content_producer: "中文也行".into(),
        ..VersionTemplate::default()
    };
    template.validate().expect("没勾 AI 时不校验服务提供者");
}

#[test]
fn watermark_cannot_squat_the_ai_corner() {
    // 勾了 AI 且有水印时，水印不能放右下角（会和 AI 标识叠在一起看不清）。
    let blocked = VersionTemplate {
        watermark_text: "剧名".into(),
        watermark_corner: Corner::BottomRight,
        ..ai_template()
    };
    assert_eq!(
        blocked.validate().expect_err("应该被拒绝").code(),
        "bad_template"
    );
    // 其余三个角都可以，而且水印和 AI 标识会同时出现在滤镜图里。
    for corner in [Corner::TopLeft, Corner::TopRight, Corner::BottomLeft] {
        let template = VersionTemplate {
            watermark_corner: corner,
            ..blocked.clone()
        };
        template
            .validate()
            .unwrap_or_else(|error| panic!("{corner:?} 应该可用：{error}"));
        let graph = graph_of(&template, &with_font(), 1);
        assert!(graph.filter_complex.contains("text=剧名"));
        assert!(graph.filter_complex.contains("text=AI生成"));
    }
    // 没有水印文字时，水印位置设成右下角也无所谓（那条 drawtext 根本不会生成）。
    let no_text = VersionTemplate {
        watermark_text: String::new(),
        watermark_corner: Corner::BottomRight,
        ..ai_template()
    };
    no_text.validate().expect("没有水印文字就不冲突");
}

#[test]
fn music_requires_an_explicit_license_checkbox() {
    // 选了背景音乐却没勾授权：不能开工（总方案 2.2 节「只用有授权的素材」）。
    let template = VersionTemplate {
        music_path: Some("/音乐/bgm.mp3".into()),
        music_licensed: false,
        ..VersionTemplate::default()
    };
    let error = template.validate().expect_err("没勾授权要被拒绝");
    assert_eq!(error.code(), "bad_template");
    assert!(
        error.to_string().contains("使用授权"),
        "错误信息要点明是授权问题：{error}"
    );

    let licensed = VersionTemplate {
        music_licensed: true,
        ..template
    };
    licensed.validate().expect("勾了授权就能过");

    // 没选音乐时，授权勾选框是什么状态都无所谓。
    let none = VersionTemplate {
        music_path: None,
        music_licensed: false,
        ..VersionTemplate::default()
    };
    none.validate().expect("没选音乐就不用勾授权");
}

#[test]
fn template_value_ranges_are_enforced() {
    let base = VersionTemplate::default();
    let cases: Vec<(&str, VersionTemplate)> = vec![
        (
            "封面秒数为负",
            VersionTemplate {
                cover_at_sec: -1.0,
                ..base.clone()
            },
        ),
        (
            "封面秒数不是有限数",
            VersionTemplate {
                cover_at_sec: f64::INFINITY,
                ..base.clone()
            },
        ),
        (
            "字幕字号为 0",
            VersionTemplate {
                subtitle_size: 0,
                ..base.clone()
            },
        ),
        (
            "水印透明度超过 1",
            VersionTemplate {
                watermark_opacity: 1.5,
                ..base.clone()
            },
        ),
        (
            "水印透明度为负",
            VersionTemplate {
                watermark_opacity: -0.1,
                ..base.clone()
            },
        ),
        (
            "音量超过 1",
            VersionTemplate {
                music_volume: 2.0,
                ..base.clone()
            },
        ),
        (
            "字幕边距为 0",
            VersionTemplate {
                subtitle_margin_v: Some(0),
                ..base.clone()
            },
        ),
        (
            "重新分集 0 集",
            VersionTemplate {
                recut: true,
                recut_count: Some(0),
                ..base.clone()
            },
        ),
    ];
    for (name, template) in cases {
        let error = template
            .validate()
            .expect_err(&format!("{name} 应该被拒绝，却通过了"));
        assert_eq!(error.code(), "bad_template", "{name}");
    }
    // 边界值合法：0 和 1 都在允许范围内。
    let edge = VersionTemplate {
        cover_at_sec: 0.0,
        watermark_opacity: 0.0,
        music_volume: 1.0,
        subtitle_size: 1,
        ..base
    };
    edge.validate().expect("边界值应该合法");
}

#[test]
fn needs_font_covers_every_text_source() {
    let base = VersionTemplate::default();
    assert!(!base.needs_font(), "默认模板不写字");
    assert!(VersionTemplate {
        watermark_text: "剧名".into(),
        ..base.clone()
    }
    .needs_font());
    assert!(ai_template().needs_font());
    assert!(VersionTemplate {
        cover_text: Some("封面字".into()),
        ..base.clone()
    }
    .needs_font());
    // 空的封面文字不算要写字。
    assert!(!VersionTemplate {
        cover_text: Some(String::new()),
        ..base
    }
    .needs_font());
}

#[test]
fn cover_needs_font_only_when_it_has_text() {
    let spec = spec();
    // 有封面文字就要字体，没有就不要——和 needs_font 保持一致。
    let with_text = VersionTemplate {
        cover_text: Some("第一季".into()),
        ..VersionTemplate::default()
    };
    let error = build_cover_args(
        &with_text,
        &spec,
        "/产物/第01集.mp4",
        None,
        "/产物/封面.jpg",
    )
    .expect_err("有封面文字却没字体要报错");
    assert_eq!(error.code(), "bad_template");

    let args = build_cover_args(
        &with_text,
        &spec,
        "/产物/第01集.mp4",
        Some(FONT),
        "/产物/封面.jpg",
    )
    .expect("给了字体就能生成");
    let joined = args.join(" ");
    assert!(joined.contains("第一季"), "封面文字要进参数：{joined}");
    assert!(joined.contains(FONT));
    assert!(joined.ends_with("/产物/封面.jpg"), "输出路径在最后");
    assert!(joined.contains("-frames:v 1"), "封面只取一帧：{joined}");

    let plain = build_cover_args(
        &VersionTemplate::default(),
        &spec,
        "/产物/第01集.mp4",
        None,
        "/产物/封面.jpg",
    )
    .expect("没有封面文字时不需要字体");
    assert!(!plain.join(" ").contains("drawtext"));
}
