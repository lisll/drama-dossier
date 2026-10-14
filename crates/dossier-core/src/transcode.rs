//! 转码建议与批量规范化（Roadmap：转码建议与批量规范化）。
//!
//! 本模块**只产出建议，不执行任何命令**：给定探测好的分集规格与一个目标规格，
//! 逐集算出「是否需要转码」「为什么」以及可直接复制到终端的一条 `ffmpeg` 命令。
//! 真正的执行由使用者自己决定（学习原型刻意不做隐式转码，避免静默重编码素材）。
//!
//! 合规边界：只做格式/码率/分辨率的规范化建议，不做内容改写、不做指纹规避。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::types::ProbedEpisode;

/// 目标规格：规范化后希望分集长什么样。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TranscodeTarget {
    /// 期望容器（按扩展名判断，如 `mp4`）。
    pub container: String,
    /// 期望视频编码（`ffmpeg` 的编码器名，如 `libx264`）。
    pub video_codec: String,
    /// 期望音频编码（如 `aac`）。
    pub audio_codec: String,
    /// 码率上限（bps）；超过时用 `-maxrate/-bufsize` 压住。
    pub max_bitrate_bps: u64,
    /// 目标最大宽度（0 表示不限制）。
    pub max_width: u32,
    /// 目标最大高度（0 表示不限制）。
    pub max_height: u32,
    /// 期望帧率（None 表示保持原样）。
    pub fps: Option<f64>,
    /// x264 质量档（`-crf`）。
    pub crf: u32,
    /// 编码 preset。
    pub preset: String,
    /// 是否加 `-movflags +faststart`（mp4 网络播放友好）。
    pub faststart: bool,
    /// 音频码率。
    pub audio_bitrate_kbps: u32,
}

impl Default for TranscodeTarget {
    fn default() -> Self {
        Self {
            container: "mp4".into(),
            video_codec: "libx264".into(),
            audio_codec: "aac".into(),
            max_bitrate_bps: 8_000_000,
            max_width: 1080,
            max_height: 1920,
            fps: None,
            crf: 20,
            preset: "medium".into(),
            faststart: true,
            audio_bitrate_kbps: 128,
        }
    }
}

impl TranscodeTarget {
    /// 校验目标规格是否自洽，返回可直接展示的中文错误。
    pub fn validate(&self) -> Result<(), String> {
        if self.container.trim().is_empty() {
            return Err("目标容器不能为空（例如 mp4）。".into());
        }
        if self.video_codec.trim().is_empty() {
            return Err("目标视频编码不能为空（例如 libx264）。".into());
        }
        if self.audio_codec.trim().is_empty() {
            return Err("目标音频编码不能为空（例如 aac）。".into());
        }
        if self.max_bitrate_bps == 0 {
            return Err("目标码率上限必须大于 0。".into());
        }
        if self.crf > 51 {
            return Err("CRF 必须在 0–51 之间（0 为无损）。".into());
        }
        if self.audio_bitrate_kbps == 0 || self.audio_bitrate_kbps > 512 {
            return Err("音频码率必须在 1–512 kbps 之间。".into());
        }
        if let Some(fps) = self.fps {
            if !fps.is_finite() || fps <= 0.0 || fps > 240.0 {
                return Err("目标帧率必须是 0–240 之间的秒数。".into());
            }
        }
        if self.max_width == 0 && self.max_height == 0 {
            // 不缩放是合法选择，这里不报错，只保证不会写出空 filter。
        }
        Ok(())
    }

    /// 目标画幅描述，例如 `1080x1920`；两边都为 0 时显示「保持原分辨率」。
    pub fn size_label(&self) -> String {
        if self.max_width == 0 && self.max_height == 0 {
            return "保持原分辨率".into();
        }
        format!("{}x{} 以内", self.max_width, self.max_height)
    }
}

/// 单集建议。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpisodeAdvice {
    pub file_name: String,
    pub path: PathBuf,
    pub episode_number: Option<u32>,
    /// 是否需要转码（没有任何理由时为 false）。
    pub needs_transcode: bool,
    /// 触发转码的理由（中文，可直接展示）。
    pub reasons: Vec<String>,
    /// 可直接接在 `ffmpeg -i <输入>` 之后的参数。
    pub ffmpeg_args: Vec<String>,
    /// 完整命令（含输入与输出路径，shell 已转义）。
    pub command_line: String,
    /// 输出文件名（与输入同目录、带 `_normalized` 后缀）。
    pub output_name: String,
}

/// 一部剧的规范化计划。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizationPlan {
    pub drama_name: String,
    pub target: TranscodeTarget,
    pub episodes: Vec<EpisodeAdvice>,
    /// 需要转码的集数。
    pub transcode_count: usize,
    /// 已经符合目标的集数。
    pub compliant_count: usize,
    /// 输出目录（脚本里写文件的位置）。
    pub output_dir: PathBuf,
}

impl NormalizationPlan {
    /// 生成可人工审阅后执行的 shell 脚本。
    ///
    /// 刻意不使用 `--force` 之外的隐式覆盖逻辑：脚本里对已存在的输出会跳过。
    pub fn script(&self) -> String {
        let mut out = String::from("#!/usr/bin/env bash\n");
        out.push_str("# 由 DramaDossier 生成的转码建议脚本：执行前请自行审阅。\n");
        out.push_str("set -euo pipefail\n");
        out.push_str(&format!(
            "out_dir={}\n",
            shell_quote(&self.output_dir.display().to_string())
        ));
        out.push_str("mkdir -p \"$out_dir\"\n\n");
        if self.transcode_count == 0 {
            out.push_str("# 所有分集均已符合目标规格，无需转码。\n");
            return out;
        }
        for ep in self.episodes.iter().filter(|e| e.needs_transcode) {
            out.push_str(&format!(
                "# {}：{}\n",
                ep.output_name,
                ep.reasons.join("；")
            ));
            let label = ep
                .episode_number
                .map(|n| format!("第{n}集"))
                .unwrap_or_else(|| ep.file_name.clone());
            out.push_str(&format!(
                "if [ ! -f \"$out_dir\"/{out} ]; then\n  ffmpeg -y -i {src} {args} \"$out_dir\"/{out}\nfi\n\n",
                out = shell_quote(&ep.output_name),
                src = shell_quote(&ep.path.display().to_string()),
                args = ep.ffmpeg_args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" "),
            ));
            let _ = label;
        }
        out
    }
}

/// 逐集生成转码建议。
pub fn plan_normalization(
    drama_name: &str,
    drama_dir: &Path,
    episodes: &[ProbedEpisode],
    target: &TranscodeTarget,
) -> Result<NormalizationPlan, String> {
    target.validate()?;
    let mut advices = Vec::with_capacity(episodes.len());
    let mut needs = 0;
    for ep in episodes {
        let advice = advise_episode(ep, target);
        if advice.needs_transcode {
            needs += 1;
        }
        advices.push(advice);
    }
    Ok(NormalizationPlan {
        drama_name: drama_name.to_string(),
        target: target.clone(),
        transcode_count: needs,
        compliant_count: advices.len() - needs,
        episodes: advices,
        output_dir: drama_dir.join("规范化"),
    })
}

/// 单集建议（不含剧目录上下文）。
pub fn advise_episode(ep: &ProbedEpisode, target: &TranscodeTarget) -> EpisodeAdvice {
    let mut reasons = Vec::new();

    let ext = ep
        .file
        .path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if !target.container.is_empty() && ext != target.container.to_ascii_lowercase() {
        reasons.push(format!("容器为 {ext}，目标为 {}", target.container));
    }
    if let Some(codec) = ep.spec.video_codec.as_deref() {
        if !codec_matches(codec, &target.video_codec) {
            reasons.push(format!(
                "视频编码 {codec} 与目标 {} 不一致",
                target.video_codec
            ));
        }
    }
    if let Some(codec) = ep.spec.audio_codec.as_deref() {
        if !codec_matches(codec, &target.audio_codec) {
            reasons.push(format!(
                "音频编码 {codec} 与目标 {} 不一致",
                target.audio_codec
            ));
        }
    }
    if let Some(bps) = ep.spec.bitrate_bps {
        if bps > target.max_bitrate_bps {
            reasons.push(format!(
                "码率 {:.1} Mbps 超过上限 {:.1} Mbps",
                bps as f64 / 1e6,
                target.max_bitrate_bps as f64 / 1e6
            ));
        }
    }
    let oversize = exceeds_target_size(ep.spec.width, ep.spec.height, target);
    if oversize {
        reasons.push(format!(
            "分辨率 {}x{} 超过目标 {}",
            ep.spec.width,
            ep.spec.height,
            target.size_label()
        ));
    }
    if let (Some(fps), Some(want)) = (ep.spec.fps, target.fps) {
        if (fps - want).abs() > 0.01 {
            reasons.push(format!("帧率 {fps:.3} 与目标 {want:.3} 不一致"));
        }
    }

    let output_name = normalized_name(&ep.file.file_name, &target.container);
    let ffmpeg_args = if reasons.is_empty() {
        Vec::new()
    } else {
        build_args(ep, target, oversize)
    };
    let command_line = if reasons.is_empty() {
        String::new()
    } else {
        format!(
            "ffmpeg -y -i {} {} {}",
            shell_quote(&ep.file.path.display().to_string()),
            ffmpeg_args
                .iter()
                .map(|a| shell_quote(a))
                .collect::<Vec<_>>()
                .join(" "),
            shell_quote(&output_name)
        )
    };

    EpisodeAdvice {
        file_name: ep.file.file_name.clone(),
        path: ep.file.path.clone(),
        episode_number: ep.file.episode_number,
        needs_transcode: !reasons.is_empty(),
        reasons,
        ffmpeg_args,
        command_line,
        output_name,
    }
}

fn codec_matches(actual: &str, target: &str) -> bool {
    let actual = actual.to_ascii_lowercase();
    let target = target.to_ascii_lowercase();
    if actual == target {
        return true;
    }
    // ffprobe 报的是解码器名（h264/hevc/mpeg4），目标是编码器名（libx264/libx265）。
    let families = [
        ("libx264", "h264"),
        ("libx265", "hevc"),
        ("libx265", "h265"),
        ("libvpx-vp9", "vp9"),
        ("libaom-av1", "av1"),
        ("libsvtav1", "av1"),
        ("aac", "aac"),
        ("libopus", "opus"),
    ];
    families
        .iter()
        .any(|(encoder, decoder)| target == *encoder && actual == *decoder)
}

fn exceeds_target_size(width: u32, height: u32, target: &TranscodeTarget) -> bool {
    let over_w = target.max_width > 0 && width > target.max_width;
    let over_h = target.max_height > 0 && height > target.max_height;
    over_w || over_h
}

fn build_args(ep: &ProbedEpisode, target: &TranscodeTarget, scale: bool) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if scale {
        let w = if target.max_width > 0 {
            target.max_width.to_string()
        } else {
            "-2".into()
        };
        let h = if target.max_height > 0 {
            target.max_height.to_string()
        } else {
            "-2".into()
        };
        // force_original_aspect_ratio=decrease：等比缩到框内；再补偶数边避免 4:2:0 出错。
        args.push("-vf".into());
        args.push(format!(
            "scale={w}:{h}:force_original_aspect_ratio=decrease,scale=trunc(iw/2)*2:trunc(ih/2)*2"
        ));
    }
    args.push("-c:v".into());
    args.push(target.video_codec.clone());
    args.push("-preset".into());
    args.push(target.preset.clone());
    args.push("-crf".into());
    args.push(target.crf.to_string());
    args.push("-maxrate".into());
    args.push(format!("{}k", target.max_bitrate_bps / 1000));
    args.push("-bufsize".into());
    args.push(format!("{}k", target.max_bitrate_bps / 500));
    args.push("-pix_fmt".into());
    args.push("yuv420p".into());
    if let Some(fps) = target.fps {
        args.push("-r".into());
        args.push(format!("{fps:.6}"));
    }
    args.push("-c:a".into());
    args.push(target.audio_codec.clone());
    args.push("-b:a".into());
    args.push(format!("{}k", target.audio_bitrate_kbps));
    if target.faststart && target.container.eq_ignore_ascii_case("mp4") {
        args.push("-movflags".into());
        args.push("+faststart".into());
    }
    let _ = ep;
    args
}

/// 输出文件名：主干加 `_normalized`，扩展名换成目标容器。
fn normalized_name(file_name: &str, container: &str) -> String {
    let stem = file_name
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(file_name);
    format!("{stem}_normalized.{container}")
}

/// 单引号转义，供生成脚本与复制命令使用。
pub fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-/=:+,@%".contains(c))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EpisodeFile, EpisodeSpec};

    fn episode(name: &str, spec: EpisodeSpec) -> ProbedEpisode {
        ProbedEpisode {
            file: EpisodeFile {
                path: PathBuf::from(format!("/tmp/d/{name}")),
                file_name: name.to_string(),
                size_bytes: 1,
                episode_number: Some(1),
            },
            spec,
        }
    }

    fn spec(width: u32, height: u32, bitrate: u64, vcodec: &str) -> EpisodeSpec {
        EpisodeSpec {
            duration_sec: 60.0,
            width,
            height,
            video_codec: Some(vcodec.into()),
            audio_codec: Some("aac".into()),
            fps: Some(30.0),
            bitrate_bps: Some(bitrate),
        }
    }

    #[test]
    fn compliant_episode_needs_no_transcode() {
        let ep = episode("第01集.mp4", spec(1080, 1920, 4_000_000, "h264"));
        let advice = advise_episode(&ep, &TranscodeTarget::default());
        assert!(!advice.needs_transcode, "{:?}", advice.reasons);
        assert!(advice.ffmpeg_args.is_empty());
        assert!(advice.command_line.is_empty());
    }

    #[test]
    fn oversized_bitrate_and_resolution_produce_capped_command() {
        let ep = episode("第02集.mp4", spec(3840, 2160, 24_000_000, "h264"));
        let advice = advise_episode(&ep, &TranscodeTarget::default());
        assert!(advice.needs_transcode);
        assert!(advice.reasons.iter().any(|r| r.contains("码率")));
        assert!(advice.reasons.iter().any(|r| r.contains("分辨率")));
        let args = advice.ffmpeg_args.join(" ");
        assert!(args.contains("-maxrate 8000k"), "{args}");
        assert!(args.contains("scale=1080:1920"), "{args}");
        assert!(args.contains("-movflags +faststart"), "{args}");
        assert!(advice.command_line.contains("ffmpeg -y -i"));
        assert!(advice.output_name.ends_with("_normalized.mp4"));
    }

    #[test]
    fn codec_family_matching_avoids_pointless_reencode() {
        // ffprobe 报 `h264`，目标是 `libx264`：同一族，不该被判定为需要转码。
        assert!(codec_matches("h264", "libx264"));
        assert!(codec_matches("HEVC", "libx265"));
        assert!(!codec_matches("mpeg4", "libx264"));
    }

    #[test]
    fn plan_counts_and_script_skips_compliant_episodes() {
        let dir = Path::new("/tmp/d");
        let episodes = vec![
            episode("第01集.mp4", spec(1080, 1920, 4_000_000, "h264")),
            episode("第02集.mp4", spec(1920, 1080, 12_000_000, "h264")),
        ];
        let plan = plan_normalization("t", dir, &episodes, &TranscodeTarget::default()).unwrap();
        assert_eq!(plan.transcode_count, 1);
        assert_eq!(plan.compliant_count, 1);
        let script = plan.script();
        assert!(script.contains("set -euo pipefail"));
        assert_eq!(script.matches("ffmpeg -y -i").count(), 1);
        assert!(script.contains("if [ ! -f"));
    }

    #[test]
    fn rejects_inconsistent_target() {
        assert!(TranscodeTarget::default().validate().is_ok());
        assert!(TranscodeTarget {
            max_bitrate_bps: 0,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(TranscodeTarget {
            crf: 60,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(TranscodeTarget {
            fps: Some(0.0),
            ..Default::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn shell_quote_escapes_spaces_and_quotes() {
        assert_eq!(shell_quote("plain.mp4"), "plain.mp4");
        assert_eq!(shell_quote("/tmp/a b.mp4"), "'/tmp/a b.mp4'");
        assert_eq!(shell_quote("it's.mp4"), r"'it'\''s.mp4'");
    }
}
