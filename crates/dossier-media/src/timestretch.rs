//! 变速与变调。
//!
//! 短剧后期最常见的两个诉求是**反的**：
//! - 「把这段拖慢一点，但音调别变（别变成慢放怪）」→ 需要时长伸缩（time-stretch）；
//! - 「把这段加快一点，音调也一起上去」→ 只改采样率/播放速度就够了。
//!
//! 本机没有 Rubber Band，所以这里把两条路分开、并在计划阶段就**显式声明**用哪条：
//! 一旦请求里含「保持音调」而本机只有 `ffmpeg atempo`，就报错而不是悄悄给出变调的结果。
//! `atempo` 本身在 (0.5, 2.0) 之外也不接受单级参数，需要级联——这个细节放在库里，
//! 免得每个调用方都踩一遍。

use std::path::{Path, PathBuf};

use dossier_runtime::{CommandRunner, CommandSpec};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::failure_message;

/// 变速/变调错误。
#[derive(Debug, Error)]
pub enum TimeStretchError {
    #[error("输入文件不存在：{0}")]
    InputMissing(PathBuf),
    #[error("速度倍率必须大于 0，实际 {0}")]
    InvalidSpeed(f64),
    #[error("音调偏移必须是 -24~24 之间的半音数，实际 {0}")]
    InvalidPitch(f64),
    #[error(
        "要求保持音调（pitch={pitch_semitones} 半音）但本机没有 rubberband，\
ffmpeg atempo 会改变音调；请安装 rubberband 或用 --allow-pitch-shift 显式接受变调"
    )]
    PitchPreservationUnavailable { pitch_semitones: f64 },
    #[error("变速失败（{command}）：{message}")]
    RunFailed { command: String, message: String },
    #[error("变速命令成功但输出文件不存在：{0}")]
    OutputMissing(PathBuf),
}

/// 变速/变调计划。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeStretchPlan {
    pub input: PathBuf,
    pub output: PathBuf,
    /// 速度倍率：2.0 = 快一倍（时长减半）。
    pub speed: f64,
    /// 音调偏移（半音）；0 表示不变调。
    pub pitch_semitones: f64,
    /// 是否要求保持音调（`speed != 1` 且不想连带变调）。
    pub preserve_pitch: bool,
    /// 计划使用的后端。
    pub backend: TimeStretchBackend,
    /// 降级说明（用了 atempo 时会写清「音调会变」）。
    #[serde(default)]
    pub degradation: Option<String>,
}

/// 变速后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeStretchBackend {
    /// Rubber Band CLI：真正做时长伸缩，音调可保持也可单独调整。
    RubberBand,
    /// ffmpeg `atempo`：只改速度，音调跟着变。
    FfmpegAtempo,
}

/// 规划变速。
pub fn plan_time_stretch(
    input: &Path,
    output: &Path,
    speed: f64,
    pitch_semitones: f64,
    rubberband_available: bool,
    allow_pitch_shift: bool,
) -> Result<TimeStretchPlan, TimeStretchError> {
    if !input.is_file() {
        return Err(TimeStretchError::InputMissing(input.to_path_buf()));
    }
    if !(speed.is_finite() && speed > 0.0) {
        return Err(TimeStretchError::InvalidSpeed(speed));
    }
    if !(pitch_semitones.is_finite() && (-24.0..=24.0).contains(&pitch_semitones)) {
        return Err(TimeStretchError::InvalidPitch(pitch_semitones));
    }

    let wants_pitch_control = pitch_semitones != 0.0;
    let wants_preserved_pitch = (speed - 1.0).abs() > f64::EPSILON && !wants_pitch_control;

    if rubberband_available {
        return Ok(TimeStretchPlan {
            input: input.to_path_buf(),
            output: output.to_path_buf(),
            speed,
            pitch_semitones,
            preserve_pitch: wants_preserved_pitch,
            backend: TimeStretchBackend::RubberBand,
            degradation: None,
        });
    }

    // 没有 rubberband：只有在「不要求保持音调」时才允许退到 atempo。
    if wants_pitch_control || (wants_preserved_pitch && !allow_pitch_shift) {
        return Err(TimeStretchError::PitchPreservationUnavailable { pitch_semitones });
    }
    Ok(TimeStretchPlan {
        input: input.to_path_buf(),
        output: output.to_path_buf(),
        speed,
        pitch_semitones: 0.0,
        preserve_pitch: false,
        backend: TimeStretchBackend::FfmpegAtempo,
        degradation: Some(format!(
            "用 ffmpeg atempo 变速 {speed}x：音调会一起变化（本机没有 rubberband）"
        )),
    })
}

impl TimeStretchPlan {
    /// 把 `atempo` 拆成多级（单级只接受 0.5~2.0）。
    ///
    /// 例如 4x → `atempo=2.0,atempo=2.0`；0.25x → `atempo=0.5,atempo=0.5`。
    pub fn atempo_chain(&self) -> Vec<f64> {
        let mut remaining = self.speed;
        let mut stages = Vec::new();
        while remaining > 2.0 {
            stages.push(2.0);
            remaining /= 2.0;
        }
        while remaining < 0.5 {
            stages.push(0.5);
            remaining /= 0.5;
        }
        stages.push(remaining);
        stages
    }

    /// 构造命令。
    pub fn build_command(
        &self,
        ffmpeg: &str,
        rubberband: &str,
    ) -> Result<CommandSpec, TimeStretchError> {
        match self.backend {
            TimeStretchBackend::RubberBand => {
                // `-t` 是时长因子（速度的倒数），`-p` 是音调偏移（半音，可为小数）。
                let mut spec = CommandSpec::new(rubberband.to_string());
                if (self.speed - 1.0).abs() > f64::EPSILON {
                    spec = spec.arg("-t").arg(format_float(1.0 / self.speed));
                }
                if self.pitch_semitones != 0.0 {
                    spec = spec.arg("-p").arg(format_float(self.pitch_semitones));
                }
                Ok(spec
                    .arg(self.input.display().to_string())
                    .arg(self.output.display().to_string()))
            }
            TimeStretchBackend::FfmpegAtempo => {
                let stages: Vec<String> = self
                    .atempo_chain()
                    .into_iter()
                    .map(|stage| format!("atempo={}", format_float(stage)))
                    .collect();
                let filter = stages.join(",");
                Ok(CommandSpec::new(ffmpeg.to_string())
                    .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
                    .arg(self.input.display().to_string())
                    .args(["-filter:a", &filter, "-vn"])
                    .arg(self.output.display().to_string()))
            }
        }
    }
}

fn format_float(value: f64) -> String {
    let text = format!("{value:.4}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 一次变速/变调的执行结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeStretchOutcome {
    pub command: String,
    pub output: PathBuf,
    /// 实际使用的后端（便于日志回答「这次到底变调了没有」）。
    pub backend: TimeStretchBackend,
    #[serde(default)]
    pub degradation: Option<String>,
}

impl TimeStretchPlan {
    /// 真的跑一次变速/变调。
    ///
    /// `rubberband_available` 由调用方探测后再规划（见 [`plan_time_stretch`]），
    /// 所以这里不再二次探测：规划结论与执行后端必须一致，否则会出现
    /// 「计划说保持音调、执行用了 atempo」这种最难查的偏差。
    pub fn execute(
        &self,
        runner: &dyn CommandRunner,
        ffmpeg: &str,
        rubberband: &str,
    ) -> Result<TimeStretchOutcome, TimeStretchError> {
        let spec = self.build_command(ffmpeg, rubberband)?;
        let output = runner
            .run(&spec)
            .map_err(|error| TimeStretchError::RunFailed {
                command: spec.display(),
                message: error.to_string(),
            })?;
        if !output.success() {
            return Err(TimeStretchError::RunFailed {
                command: spec.display(),
                message: failure_message(&output),
            });
        }
        if !self.output.is_file() {
            return Err(TimeStretchError::OutputMissing(self.output.clone()));
        }
        Ok(TimeStretchOutcome {
            command: spec.display(),
            output: self.output.clone(),
            backend: self.backend,
            degradation: self.degradation.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_input(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("dossier-ts-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.wav");
        std::fs::write(&input, b"fake").unwrap();
        (input, dir.join("out.wav"))
    }

    #[test]
    fn rubberband_handles_both_speed_and_pitch() {
        let (input, output) = temp_input("rb");
        let plan = plan_time_stretch(&input, &output, 1.5, -2.0, true, false).unwrap();
        assert_eq!(plan.backend, TimeStretchBackend::RubberBand);
        assert!(plan.degradation.is_none());
        let spec = plan.build_command("ffmpeg", "rubberband").unwrap();
        assert_eq!(spec.program, "rubberband");
        // 时长因子是速度的倒数，音调是半音。
        assert_eq!(spec.args[0], "-t");
        assert!((spec.args[1].parse::<f64>().unwrap() - 1.0 / 1.5).abs() < 1e-4);
        assert_eq!(spec.args[2], "-p");
        assert_eq!(spec.args[3], "-2");
    }

    /// 没有 rubberband 时，「变速但保持音调」必须直接报错，不能悄悄变调。
    #[test]
    fn refuses_silent_pitch_shift_when_rubberband_is_missing() {
        let (input, output) = temp_input("norb");
        let error = plan_time_stretch(&input, &output, 0.8, 0.0, false, false).unwrap_err();
        assert!(matches!(
            error,
            TimeStretchError::PitchPreservationUnavailable { .. }
        ));
        assert!(error.to_string().contains("rubberband"));
        // 显式接受变调后可以继续，但计划里必须带降级说明。
        let plan = plan_time_stretch(&input, &output, 0.8, 0.0, false, true).unwrap();
        assert_eq!(plan.backend, TimeStretchBackend::FfmpegAtempo);
        assert!(plan.degradation.as_deref().unwrap().contains("音调"));
    }

    #[test]
    fn atempo_chain_stays_within_the_supported_range() {
        let (input, output) = temp_input("chain");
        for (speed, expected) in [
            (4.0, vec![2.0, 2.0]),
            (1.0, vec![1.0]),
            (0.25, vec![0.5, 0.5]),
            (3.0, vec![2.0, 1.5]),
        ] {
            let plan = plan_time_stretch(&input, &output, speed, 0.0, false, true).unwrap();
            let chain = plan.atempo_chain();
            assert_eq!(chain, expected, "speed={speed}");
            let product: f64 = chain.iter().product();
            assert!(
                (product - speed).abs() < 1e-9,
                "speed={speed} 乘积 {product}"
            );
        }
    }

    #[test]
    fn atempo_command_uses_a_single_filter_argument() {
        let (input, output) = temp_input("cmd");
        let plan = plan_time_stretch(&input, &output, 4.0, 0.0, false, true).unwrap();
        let spec = plan.build_command("ffmpeg", "rubberband").unwrap();
        assert_eq!(spec.program, "ffmpeg");
        let position = spec.args.iter().position(|arg| arg == "-filter:a").unwrap();
        assert_eq!(spec.args[position + 1], "atempo=2,atempo=2");
    }

    /// 真跑一次 atempo：2 秒的正弦波加速 2x 后应当只剩约 1 秒。
    ///
    /// 这条用例能抓住「滤镜参数写对了但链到了错误的流上」这类错误——只看退出码看不出来。
    #[test]
    fn real_atempo_halves_the_duration() {
        use dossier_runtime::{CommandRunner, SystemRunner};
        let runner = SystemRunner::new();
        if !runner
            .run(&CommandSpec::new("ffmpeg").arg("-version"))
            .map(|output| output.success())
            .unwrap_or(false)
        {
            eprintln!("跳过：本机没有可用的 ffmpeg");
            return;
        }
        let dir = std::env::temp_dir().join(format!("dossier-ts-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("tone.wav");
        let render = runner
            .run(
                &CommandSpec::new("ffmpeg")
                    .args([
                        "-hide_banner",
                        "-loglevel",
                        "error",
                        "-y",
                        "-f",
                        "lavfi",
                        "-i",
                        "sine=frequency=440:duration=2",
                    ])
                    .arg(input.display().to_string()),
            )
            .unwrap();
        assert!(render.success(), "造音频失败：{}", render.combined());

        let output = dir.join("fast.wav");
        let plan = plan_time_stretch(&input, &output, 2.0, 0.0, false, true).unwrap();
        let spec = plan.build_command("ffmpeg", "rubberband").unwrap();
        let result = runner.run(&spec).unwrap();
        assert!(result.success(), "atempo 失败：{}", result.combined());

        // 用 ffprobe 读时长：应当接近 1 秒（±0.1）。
        let probe = runner
            .run(
                &CommandSpec::new("ffprobe")
                    .args([
                        "-v",
                        "quiet",
                        "-show_entries",
                        "format=duration",
                        "-of",
                        "csv=p=0",
                    ])
                    .arg(output.display().to_string()),
            )
            .unwrap();
        let duration: f64 = probe.stdout.trim().parse().unwrap_or_default();
        assert!(
            (duration - 1.0).abs() < 0.1,
            "加速 2x 后时长应约 1 秒，实际 {duration}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_out_of_range_parameters() {
        let (input, output) = temp_input("bad");
        assert!(matches!(
            plan_time_stretch(&input, &output, 0.0, 0.0, true, false).unwrap_err(),
            TimeStretchError::InvalidSpeed(_)
        ));
        assert!(matches!(
            plan_time_stretch(&input, &output, 1.0, 30.0, true, false).unwrap_err(),
            TimeStretchError::InvalidPitch(_)
        ));
        assert!(matches!(
            plan_time_stretch(
                Path::new("/definitely/not/here.wav"),
                &output,
                1.0,
                0.0,
                true,
                false
            )
            .unwrap_err(),
            TimeStretchError::InputMissing(_)
        ));
    }
}

#[cfg(test)]
mod execute_tests {
    use super::*;
    use dossier_runtime::{CommandOutput, FakeRunner};

    fn plan(speed: f64) -> TimeStretchPlan {
        // 每个用例一个独占目录：`execute_fails_when_the_output_is_missing` 断言的是
        // 「输出不存在」，共用同一路径时会被别的用例（或上一次运行）写下的文件带跑。
        // 同进程内用例是并行跑的，所以不能只靠 `remove_dir_all`，必须路径本身唯一。
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("dossier-ts-exec-{}-{}", std::process::id(), seq));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("voice.wav");
        std::fs::write(&input, b"fake").unwrap();
        plan_time_stretch(&input, &dir.join("fixed.wav"), speed, 0.0, false, true).unwrap()
    }

    #[test]
    fn execute_returns_the_output_file() {
        let plan = plan(1.5);
        std::fs::write(&plan.output, b"wav").unwrap();
        let runner = FakeRunner::new().on("ffmpeg", CommandOutput::new(0, "", ""));
        let outcome = plan.execute(&runner, "ffmpeg", "rubberband").unwrap();
        assert_eq!(outcome.output, plan.output);
        assert_eq!(outcome.backend, TimeStretchBackend::FfmpegAtempo);
        assert!(outcome.degradation.is_some());
    }

    #[test]
    fn execute_fails_when_the_output_is_missing() {
        let plan = plan(1.5);
        let runner = FakeRunner::new().on("ffmpeg", CommandOutput::new(0, "", ""));
        let error = plan.execute(&runner, "ffmpeg", "rubberband").unwrap_err();
        assert!(
            matches!(error, TimeStretchError::OutputMissing(_)),
            "{error}"
        );
    }

    #[test]
    fn a_nonzero_exit_becomes_a_run_failure() {
        let plan = plan(1.5);
        let runner = FakeRunner::new().on("ffmpeg", CommandOutput::new(1, "", "Invalid argument"));
        let error = plan.execute(&runner, "ffmpeg", "rubberband").unwrap_err();
        match error {
            TimeStretchError::RunFailed { message, .. } => {
                assert!(message.contains("Invalid argument"))
            }
            other => panic!("应报 RunFailed，实际 {other}"),
        }
    }
}
