//! 后期工具盘点与能力标签。
//!
//! 这一层的价值在于把「这台机器现在能做哪些后期能力」变成**数据**，而不是散落在
//! 各处的 `if tool_missing { println!("请安装 …") }`。云端模块注册表可以直接消费这些
//! 能力标签做准入判断（见 `cloud/src/drama_cloud/modules`）。
//!
//! 与 [`dossier_runtime::probe`] 的分工：那边探测通用工具（ffmpeg/python/nvidia-smi），
//! 这里只补**后期侧**的专用工具，并负责把「工具 → 能力」的映射写清楚。

use dossier_runtime::probe::{probe_ffmpeg_capabilities, probe_optional_tool, ToolReport};
use dossier_runtime::{CommandRunner, FfmpegCapabilities};
use serde::{Deserialize, Serialize};

/// 能力标签：`media.render.compose` 这种「点分路径」形式，与云端模块注册表一致。
pub mod tags {
    /// 读媒体信息（时长/分辨率/流）。
    pub const PROBE: &str = "media.probe";
    /// 合成渲染：文字叠加、图片叠加。
    pub const COMPOSE: &str = "media.render.compose";
    /// 字幕烧录（libass）。
    pub const SUBTITLE_BURN: &str = "media.render.subtitle_burn";
    /// 镜头切点检测。
    pub const SHOT_DETECT: &str = "media.shot.detect";
    /// 变速**不变调**（需要 Rubber Band 这类专门的时长伸缩器）。
    pub const TIME_STRETCH: &str = "media.audio.timestretch";
    /// 只有变速（`atempo`），音调会跟着变——能力降级，标签也单独一个。
    pub const TEMPO_ONLY: &str = "media.audio.tempo_only";
    /// 音源分离（人声/伴奏）。
    pub const SEPARATE: &str = "media.audio.separate";
    /// 语音识别。
    pub const ASR: &str = "media.asr";
    /// 图像文字识别。
    pub const OCR: &str = "media.ocr";
}

/// 后期工具配置（都可用绝对路径覆盖，便于非标准安装）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaToolConfig {
    pub ffmpeg: String,
    pub ffprobe: String,
    pub rubberband: String,
    pub demucs: String,
    pub whisper: String,
    pub tesseract: String,
}

impl Default for MediaToolConfig {
    fn default() -> Self {
        Self {
            ffmpeg: "ffmpeg".to_string(),
            ffprobe: "ffprobe".to_string(),
            rubberband: "rubberband".to_string(),
            demucs: "demucs".to_string(),
            whisper: "whisper".to_string(),
            tesseract: "tesseract".to_string(),
        }
    }
}

/// 一个后期工具的探测结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaToolReport {
    pub name: String,
    pub purpose: String,
    #[serde(flatten)]
    pub tool: ToolReport,
}

/// 后期能力探测总表。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaCapabilityReport {
    pub tools: Vec<MediaToolReport>,
    /// 本机具备的能力标签。
    pub capabilities: Vec<String>,
    /// 缺失的能力标签 → 缺哪个工具。
    pub missing: Vec<MissingCapability>,
    /// 明确的限制说明（例如「本机 ffmpeg 没有 drawtext，所以不能烧字幕」）。
    pub limitations: Vec<String>,
}

/// 缺一项能力的原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MissingCapability {
    pub capability: String,
    pub reason: String,
}

impl MediaCapabilityReport {
    pub fn supports(&self, capability: &str) -> bool {
        self.capabilities.iter().any(|tag| tag == capability)
    }
}

/// 探测后期工具链并推导能力标签。
pub fn probe_media_tools(
    runner: &dyn CommandRunner,
    config: &MediaToolConfig,
) -> MediaCapabilityReport {
    // ffmpeg 家族要看**滤镜清单**，不能只看可执行文件在不在：本机实测就出现过
    // 「ffmpeg 在、但某个滤镜没编译进来」的情况。
    let ffmpeg = probe_ffmpeg_capabilities(runner, Some(std::path::Path::new(&config.ffmpeg)));
    let ffprobe = probe_optional_tool(
        runner,
        "ffprobe",
        &config.ffprobe,
        &["-hide_banner", "-version"],
    );
    let rubberband = probe_optional_tool(runner, "rubberband", &config.rubberband, &["--version"]);
    let demucs = probe_optional_tool(runner, "demucs", &config.demucs, &["--version"]);
    let whisper = probe_optional_tool(runner, "whisper", &config.whisper, &["--help"]);
    let tesseract = probe_optional_tool(runner, "tesseract", &config.tesseract, &["--version"]);

    let tools = vec![
        report("ffmpeg", "滤镜/编码/合成渲染的底座", &ffmpeg.ffmpeg),
        report("ffprobe", "媒体信息读取", &ffprobe),
        report("rubberband", "变速不变调（时长伸缩）", &rubberband),
        report("demucs", "音源分离（人声/伴奏）", &demucs),
        report("whisper", "语音识别转字幕", &whisper),
        report("tesseract", "OCR 文字识别", &tesseract),
    ];

    let mut capabilities = Vec::new();
    let mut missing = Vec::new();
    let mut limitations = Vec::new();

    // ffprobe：媒体信息。
    judge(
        &mut capabilities,
        &mut missing,
        &mut limitations,
        tags::PROBE,
        ffprobe.found,
        "缺少 ffprobe（来自 FFmpeg 套件）",
        None,
    );

    judge_ffmpeg_filter(
        &ffmpeg,
        &mut capabilities,
        &mut missing,
        &mut limitations,
        &["drawtext", "overlay"],
        tags::COMPOSE,
        "合成渲染需要 ffmpeg 的 drawtext 与 overlay 滤镜",
    );
    judge_ffmpeg_filter(
        &ffmpeg,
        &mut capabilities,
        &mut missing,
        &mut limitations,
        &["subtitles"],
        tags::SUBTITLE_BURN,
        "字幕烧录需要 ffmpeg 的 subtitles 滤镜（libass）",
    );
    judge_ffmpeg_filter(
        &ffmpeg,
        &mut capabilities,
        &mut missing,
        &mut limitations,
        &["select"],
        tags::SHOT_DETECT,
        "镜头切点检测需要 ffmpeg 的 select 滤镜",
    );

    // 变速：两条路分开报，避免把「只有 atempo」当成「能变速不变调」。
    if rubberband.found {
        capabilities.push(tags::TIME_STRETCH.to_string());
    } else {
        missing.push(MissingCapability {
            capability: tags::TIME_STRETCH.to_string(),
            reason: "缺少 rubberband；只有 ffmpeg atempo 可用（变速会同时变调）".to_string(),
        });
        limitations.push("变速不变调（pitch 保持）在本机不可用：请安装 rubberband".to_string());
    }
    judge(
        &mut capabilities,
        &mut missing,
        &mut limitations,
        tags::TEMPO_ONLY,
        ffmpeg.usable(),
        "缺少 ffmpeg，连 atempo 变速都做不到",
        None,
    );

    judge(
        &mut capabilities,
        &mut missing,
        &mut limitations,
        tags::SEPARATE,
        demucs.found,
        "缺少 demucs（Python 包，命令名 `demucs`）",
        None,
    );
    judge(
        &mut capabilities,
        &mut missing,
        &mut limitations,
        tags::ASR,
        whisper.found,
        "缺少 whisper（openai-whisper 命令行，或 whisper.cpp 的 whisper-cli）",
        None,
    );
    judge(
        &mut capabilities,
        &mut missing,
        &mut limitations,
        tags::OCR,
        tesseract.found,
        "缺少 tesseract（含 chi_sim 语言包才能识别中文）",
        None,
    );

    if let Some(error) = &ffmpeg.list_error {
        limitations.push(format!("FFmpeg 能力列举不完整：{error}"));
    }
    capabilities.sort();
    capabilities.dedup();

    MediaCapabilityReport {
        tools,
        capabilities,
        missing,
        limitations,
    }
}

fn report(name: &str, purpose: &str, tool: &ToolReport) -> MediaToolReport {
    MediaToolReport {
        name: name.to_string(),
        purpose: purpose.to_string(),
        tool: tool.clone(),
    }
}

fn judge(
    capabilities: &mut Vec<String>,
    missing: &mut Vec<MissingCapability>,
    _limitations: &mut Vec<String>,
    capability: &str,
    ok: bool,
    reason: &str,
    _note: Option<&str>,
) {
    if ok {
        capabilities.push(capability.to_string());
    } else {
        missing.push(MissingCapability {
            capability: capability.to_string(),
            reason: reason.to_string(),
        });
    }
}

fn judge_ffmpeg_filter(
    ffmpeg: &FfmpegCapabilities,
    capabilities: &mut Vec<String>,
    missing: &mut Vec<MissingCapability>,
    _limitations: &mut Vec<String>,
    filters: &[&str],
    capability: &str,
    reason: &str,
) {
    if !ffmpeg.usable() {
        missing.push(MissingCapability {
            capability: capability.to_string(),
            reason: "缺少 ffmpeg".to_string(),
        });
        return;
    }
    let absent: Vec<&str> = filters
        .iter()
        .copied()
        .filter(|filter| !ffmpeg.filters.supports(filter))
        .collect();
    if absent.is_empty() {
        capabilities.push(capability.to_string());
    } else {
        missing.push(MissingCapability {
            capability: capability.to_string(),
            reason: format!("{reason}；缺滤镜：{}", absent.join(" / ")),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dossier_runtime::FakeRunner;
    use dossier_runtime::{CommandOutput, CommandSpec};

    /// 造一份「本机实测」的 ffmpeg 滤镜/编码器清单。
    const FILTERS: &str = "\
Filters:
  T.. = Timeline support
  ... ass               V->V       Render ASS subtitles onto input video using libass
  T.C drawtext          V->V       Draw text on top of video frames using libfreetype
  ... null              V->V       Pass the source unchanged
  TSC overlay           VV->V      Overlay a video source on top of the input
  ..C scale             V->V       Scale the input video size
  ... select            V->V       Select video frames
  ... subtitles         V->V       Render text subtitles onto input video using libass
";
    const ENCODERS: &str = "\
Encoders:
 V..... = Video
 ------
 V....D libx264              libx264 H.264 (codec h264)
";

    fn full_runner() -> FakeRunner {
        FakeRunner::new()
            .on_match("ffmpeg", "-filters", CommandOutput::ok(FILTERS))
            .on_match("ffmpeg", "-encoders", CommandOutput::ok(ENCODERS))
            .on("ffmpeg", CommandOutput::ok("ffmpeg version 4.2.4"))
            .on("ffprobe", CommandOutput::ok("ffprobe version 4.2.4"))
            .on("rubberband", CommandOutput::ok("Rubber Band 3.1.2"))
            .on("demucs", CommandOutput::ok("demucs 4.0.1"))
            .on("whisper", CommandOutput::ok("usage: whisper"))
            .on("tesseract", CommandOutput::ok("tesseract 4.1.0"))
    }

    #[test]
    fn everything_present_yields_all_capability_tags() {
        let report = probe_media_tools(&full_runner(), &MediaToolConfig::default());
        for tag in [
            tags::PROBE,
            tags::COMPOSE,
            tags::SUBTITLE_BURN,
            tags::SHOT_DETECT,
            tags::TIME_STRETCH,
            tags::TEMPO_ONLY,
            tags::SEPARATE,
            tags::ASR,
            tags::OCR,
        ] {
            assert!(report.supports(tag), "应具备 {tag}：{report:?}");
        }
        assert!(report.missing.is_empty());
        assert_eq!(report.tools.len(), 6);
    }

    #[test]
    fn missing_tools_report_what_to_install_instead_of_silently_degrading() {
        // 只有 ffmpeg+ffprobe 的机器：能渲染、能检测镜头，但不能分离/识别。
        let runner = FakeRunner::new()
            .on_match("ffmpeg", "-filters", CommandOutput::ok(FILTERS))
            .on_match("ffmpeg", "-encoders", CommandOutput::ok(ENCODERS))
            .on("ffmpeg", CommandOutput::ok("ffmpeg version 4.2.4"))
            .on("ffprobe", CommandOutput::ok("ffprobe version 4.2.4"));
        let report = probe_media_tools(&runner, &MediaToolConfig::default());
        assert!(report.supports(tags::COMPOSE));
        assert!(report.supports(tags::TEMPO_ONLY));
        assert!(!report.supports(tags::TIME_STRETCH));
        assert!(!report.supports(tags::SEPARATE));
        assert!(!report.supports(tags::ASR));
        assert!(!report.supports(tags::OCR));
        let time_stretch = report
            .missing
            .iter()
            .find(|item| item.capability == tags::TIME_STRETCH)
            .unwrap();
        assert!(time_stretch.reason.contains("rubberband"));
        assert!(report
            .limitations
            .iter()
            .any(|text| text.contains("变速不变调")));
    }

    /// ffmpeg 在、但滤镜没编译进来：必须按「缺能力」处理。
    #[test]
    fn ffmpeg_without_a_filter_loses_the_capability() {
        let no_drawtext = FILTERS.replace("  T.C drawtext", "  ... notdrawtext");
        let runner = FakeRunner::new()
            .on_match("ffmpeg", "-filters", CommandOutput::ok(&no_drawtext))
            .on_match("ffmpeg", "-encoders", CommandOutput::ok(ENCODERS))
            .on("ffmpeg", CommandOutput::ok("ffmpeg version 4.2.4"))
            .on("ffprobe", CommandOutput::ok("ffprobe version 4.2.4"));
        let report = probe_media_tools(&runner, &MediaToolConfig::default());
        assert!(!report.supports(tags::COMPOSE));
        assert!(report.supports(tags::SUBTITLE_BURN));
        let missing = report
            .missing
            .iter()
            .find(|item| item.capability == tags::COMPOSE)
            .unwrap();
        assert!(missing.reason.contains("drawtext"), "{}", missing.reason);
    }

    /// 列举滤镜失败（老版本/裁剪版）时不能假装能力齐全。
    #[test]
    fn a_failed_filter_listing_reports_a_limitation() {
        let runner = FakeRunner::new()
            .on_match("ffmpeg", "-filters", CommandOutput::new(1, "", "boom"))
            .on_match("ffmpeg", "-encoders", CommandOutput::new(1, "", "boom"))
            .on("ffmpeg", CommandOutput::ok("ffmpeg version 1.0"));
        let report = probe_media_tools(&runner, &MediaToolConfig::default());
        assert!(!report.supports(tags::COMPOSE));
        assert!(report
            .limitations
            .iter()
            .any(|text| text.contains("列举不完整")));
    }

    /// 真机冒烟：把真实环境的能力标签打出来，确认调用链能跑通。
    #[test]
    fn real_environment_capability_smoke_test() {
        let runner = dossier_runtime::SystemRunner::new();
        let report = probe_media_tools(&runner, &MediaToolConfig::default());
        eprintln!(
            "本机后期能力：{}；缺失 {} 项；限制 {} 条",
            report.capabilities.join(", "),
            report.missing.len(),
            report.limitations.len()
        );
        for tool in &report.tools {
            eprintln!(
                "  {:<11} {} {}",
                tool.name,
                if tool.tool.found { "OK" } else { "缺失" },
                tool.tool.display_version()
            );
        }
        assert!(
            runner
                .run(&CommandSpec::new("ffmpeg").arg("-version"))
                .is_ok(),
            "本机应当装有 ffmpeg（否则这条冒烟测试没有意义）"
        );
    }
}
