//! 语音识别（Whisper）→ SRT。
//!
//! 现状与缺口：项目原先只有「字幕**擦除**」，没有「字幕**生成**」。这条链路补上之后，
//! 才能做「识别原声 → 生成字幕 → 和擦除结果合成」，也就是完整的一轮译制/重配流程。
//!
//! 支持两种命令行形态，因为它们在真实机器上都很常见：
//! - **openai-whisper**：`whisper in.wav --model small --output_format srt --output_dir out`
//!   → 产物 `<out>/<basename>.srt`；
//! - **whisper.cpp**：`whisper-cli -m model.bin -f in.wav -osrt -of <out>/<basename>`
//!   → 产物 `<out>/<basename>.srt`。
//!
//! 两者产物路径规则不同（前者由 whisper 拼，后者由 `-of` 前缀拼），所以
//! [`AsrPlan::expected_subtitle_path`] 把规则写死在一处，避免调用方各猜一遍。

use std::path::{Path, PathBuf};

use dossier_runtime::{CommandRunner, CommandSpec};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::failure_message;
use crate::srt::{SubtitleCue, SubtitleError};

/// 语音识别错误。
#[derive(Debug, Error)]
pub enum AsrError {
    #[error("输入文件不存在：{0}")]
    InputMissing(PathBuf),
    #[error("本机没有可用的 Whisper：{0}")]
    ToolMissing(String),
    #[error("whisper.cpp 需要显式指定模型文件（--model-path），当前为空")]
    MissingModelPath,
    #[error("模型文件不存在：{0}")]
    ModelMissing(PathBuf),
    #[error("识别输出不存在：{0}（命令成功但没有产物）")]
    OutputMissing(PathBuf),
    #[error(transparent)]
    Subtitle(#[from] SubtitleError),
    #[error("识别失败（{command}）：{message}")]
    RunFailed { command: String, message: String },
}

/// 要调用的 Whisper 实现。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WhisperFlavor {
    /// `pip install openai-whisper` 得到的 `whisper` 命令。
    OpenAi,
    /// whisper.cpp 的 `whisper-cli`（更省内存，适合没有 GPU 的机器）。
    WhisperCpp,
}

/// 识别计划。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsrPlan {
    pub input: PathBuf,
    pub output_dir: PathBuf,
    pub flavor: WhisperFlavor,
    /// 模型名（openai 形态：`tiny`/`base`/`small`/`medium`/`large`）或
    /// 模型文件路径（whisper.cpp 形态）。
    pub model: String,
    /// 语言提示；`None` 让工具自检。
    #[serde(default)]
    pub language: Option<String>,
    /// CPU 线程数；`None` 用工具默认值。
    #[serde(default)]
    pub threads: Option<u32>,
}

impl AsrPlan {
    pub fn new(
        input: impl Into<PathBuf>,
        output_dir: impl Into<PathBuf>,
        flavor: WhisperFlavor,
        model: impl Into<String>,
    ) -> Self {
        Self {
            input: input.into(),
            output_dir: output_dir.into(),
            flavor,
            model: model.into(),
            language: None,
            threads: None,
        }
    }

    /// 音频基名（决定产物文件名）。
    pub fn basename(&self) -> String {
        self.input
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "audio".to_string())
    }

    /// 预期产出的 SRT 路径。
    pub fn expected_subtitle_path(&self) -> PathBuf {
        self.output_dir.join(format!("{}.srt", self.basename()))
    }

    pub fn validate(&self) -> Result<(), AsrError> {
        if !self.input.is_file() {
            return Err(AsrError::InputMissing(self.input.clone()));
        }
        if self.model.trim().is_empty() {
            return Err(AsrError::MissingModelPath);
        }
        // whisper.cpp 的 `-m` 是**文件路径**，指错了只会在跑了几分钟后才失败，
        // 所以在计划阶段就先确认。
        if self.flavor == WhisperFlavor::WhisperCpp {
            let path = Path::new(&self.model);
            if path.is_absolute() && !path.is_file() {
                return Err(AsrError::ModelMissing(path.to_path_buf()));
            }
        }
        Ok(())
    }

    /// 生成命令。`available` 为假时报「缺工具」。
    pub fn build_command(&self, program: &str, available: bool) -> Result<CommandSpec, AsrError> {
        self.validate()?;
        if !available {
            return Err(AsrError::ToolMissing(
                "请安装 openai-whisper（pip install openai-whisper）或 whisper.cpp".to_string(),
            ));
        }
        let output_dir = self.output_dir.display().to_string();
        match self.flavor {
            WhisperFlavor::OpenAi => {
                let mut spec = CommandSpec::new(program.to_string())
                    .arg(self.input.display().to_string())
                    .args(["--model", &self.model])
                    .args(["--output_format", "srt"])
                    .args(["--output_dir", &output_dir])
                    // 不给 `--task translate`：短剧原声是中文，识别即可；
                    // 需要翻译时那是另一个明确的步骤，不该藏在默认值里。
                    .args(["--task", "transcribe"])
                    .args(["--verbose", "False"]);
                if let Some(language) = &self.language {
                    spec = spec.args(["--language", language]);
                }
                if let Some(threads) = self.threads {
                    spec = spec.args(["--threads", &threads.to_string()]);
                }
                Ok(spec)
            }
            WhisperFlavor::WhisperCpp => {
                let mut spec = CommandSpec::new(program.to_string())
                    .args(["-m", &self.model, "-f"])
                    .arg(self.input.display().to_string())
                    .args(["-osrt", "-of"])
                    .arg(self.output_dir.join(self.basename()).display().to_string());
                if let Some(language) = &self.language {
                    spec = spec.args(["-l", language]);
                }
                if let Some(threads) = self.threads {
                    spec = spec.args(["-t", &threads.to_string()]);
                }
                Ok(spec)
            }
        }
    }
}

/// 读取识别产物。命令成功但文件不在时给出明确错误，而不是返回空字幕。
pub fn load_transcript(path: &Path) -> Result<Vec<SubtitleCue>, AsrError> {
    if !path.is_file() {
        return Err(AsrError::OutputMissing(path.to_path_buf()));
    }
    Ok(crate::srt::load_srt(path)?)
}

/// 一次识别的执行结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsrOutcome {
    pub command: String,
    pub subtitle: PathBuf,
    /// 解析出的字幕条数（用来区分「识别成功但没说话」和「字幕根本没生成」）。
    pub cues: usize,
}

impl AsrPlan {
    /// 真的跑一次 Whisper，并把生成的 SRT 读回来。
    ///
    /// 「命令成功但没有产物」在真实机器上很常见（whisper 把输出写到了别处、
    /// 或 `--output_dir` 被忽略），所以这里必须核对文件，而不是拿到退出码 0 就返回。
    pub fn execute(
        &self,
        runner: &dyn CommandRunner,
        program: &str,
    ) -> Result<AsrOutcome, AsrError> {
        let available = runner
            .run(&CommandSpec::new(program.to_string()).arg("--help"))
            .map(|output| output.success())
            .unwrap_or(false);
        let spec = self.build_command(program, available)?;
        let output = runner.run(&spec).map_err(|error| AsrError::RunFailed {
            command: spec.display(),
            message: error.to_string(),
        })?;
        if !output.success() {
            return Err(AsrError::RunFailed {
                command: spec.display(),
                message: failure_message(&output),
            });
        }
        let subtitle = self.expected_subtitle_path();
        let cues = load_transcript(&subtitle)?;
        Ok(AsrOutcome {
            command: spec.display(),
            subtitle,
            cues: cues.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(tag: &str, flavor: WhisperFlavor) -> AsrPlan {
        let dir = std::env::temp_dir().join(format!("dossier-asr-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("ep01.wav");
        std::fs::write(&input, b"fake").unwrap();
        AsrPlan::new(&input, dir.join("out"), flavor, "small")
    }

    #[test]
    fn openai_flavor_builds_the_documented_arguments() {
        let plan = plan("openai", WhisperFlavor::OpenAi);
        assert_eq!(
            plan.expected_subtitle_path(),
            plan.output_dir.join("ep01.srt")
        );
        let spec = plan.build_command("whisper", true).unwrap();
        assert_eq!(spec.program, "whisper");
        assert!(spec.args.contains(&"--model".to_string()));
        assert!(spec.args.contains(&"small".to_string()));
        assert!(spec.args.contains(&"srt".to_string()));
        assert!(spec.args.contains(&"transcribe".to_string()));
        assert!(
            !spec.args.contains(&"--task".to_string())
                || spec.args.contains(&"transcribe".to_string())
        );
    }

    #[test]
    fn whisper_cpp_flavor_uses_the_output_prefix_form() {
        let plan = plan("cpp", WhisperFlavor::WhisperCpp);
        let spec = plan.build_command("whisper-cli", true).unwrap();
        assert_eq!(spec.program, "whisper-cli");
        let position = spec.args.iter().position(|arg| arg == "-of").unwrap();
        // `-of` 是**不带扩展名**的前缀，产物实际是它 + ".srt"。
        assert_eq!(
            spec.args[position + 1],
            plan.output_dir.join("ep01").display().to_string()
        );
        assert!(spec.args.contains(&"-osrt".to_string()));
    }

    #[test]
    fn language_and_threads_are_only_passed_when_set() {
        let mut plan = plan("opts", WhisperFlavor::OpenAi);
        let bare = plan.build_command("whisper", true).unwrap();
        assert!(!bare.args.contains(&"--language".to_string()));
        plan.language = Some("zh".to_string());
        plan.threads = Some(8);
        let spec = plan.build_command("whisper", true).unwrap();
        assert!(spec.args.contains(&"--language".to_string()));
        assert!(spec.args.contains(&"zh".to_string()));
        assert!(spec.args.contains(&"8".to_string()));
    }

    #[test]
    fn missing_tool_or_model_is_reported_before_running() {
        let plan = plan("errors", WhisperFlavor::OpenAi);
        assert!(matches!(
            plan.build_command("whisper", false).unwrap_err(),
            AsrError::ToolMissing(_)
        ));
        let empty = AsrPlan::new(&plan.input, &plan.output_dir, WhisperFlavor::OpenAi, "  ");
        assert!(matches!(
            empty.validate().unwrap_err(),
            AsrError::MissingModelPath
        ));
        // whisper.cpp 指了一个不存在的模型文件 → 计划阶段就报错。
        let cpp = AsrPlan::new(
            &plan.input,
            &plan.output_dir,
            WhisperFlavor::WhisperCpp,
            "/definitely/not/here.bin",
        );
        assert!(matches!(
            cpp.validate().unwrap_err(),
            AsrError::ModelMissing(_)
        ));
    }

    #[test]
    fn missing_output_file_is_not_silently_an_empty_transcript() {
        let dir = std::env::temp_dir().join(format!("dossier-asr-out-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(matches!(
            load_transcript(&dir.join("nope.srt")).unwrap_err(),
            AsrError::OutputMissing(_)
        ));
        let real = dir.join("real.srt");
        crate::srt::save_srt(&real, &[SubtitleCue::new(0.0, 1.0, "你好")]).unwrap();
        let cues = load_transcript(&real).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "你好");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod execute_tests {
    use super::*;
    use dossier_runtime::{CommandOutput, FakeRunner};

    const SRT: &str = "1\n00:00:01,000 --> 00:00:02,000\n你好，世界\n\n2\n00:00:02,500 --> 00:00:03,500\n第二句\n";

    fn plan(tag: &str) -> AsrPlan {
        let dir =
            std::env::temp_dir().join(format!("dossier-asr-exec-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("ep01.wav");
        std::fs::write(&input, b"fake").unwrap();
        AsrPlan::new(&input, dir.join("out"), WhisperFlavor::OpenAi, "small")
    }

    #[test]
    fn execute_reads_back_the_generated_subtitle() {
        let plan = plan("ok");
        std::fs::create_dir_all(&plan.output_dir).unwrap();
        std::fs::write(plan.expected_subtitle_path(), SRT).unwrap();
        let runner = FakeRunner::new().on("whisper", CommandOutput::new(0, "", ""));
        let outcome = plan.execute(&runner, "whisper").unwrap();
        assert_eq!(outcome.cues, 2);
        assert_eq!(outcome.subtitle, plan.expected_subtitle_path());
    }

    #[test]
    fn execute_fails_when_the_tool_succeeds_without_a_subtitle() {
        let plan = plan("nosrt");
        std::fs::create_dir_all(&plan.output_dir).unwrap();
        let runner = FakeRunner::new().on("whisper", CommandOutput::new(0, "", ""));
        let error = plan.execute(&runner, "whisper").unwrap_err();
        assert!(matches!(error, AsrError::OutputMissing(_)), "{error}");
    }

    #[test]
    fn execute_refuses_when_whisper_is_absent() {
        let plan = plan("absent");
        let runner = FakeRunner::new();
        let error = plan.execute(&runner, "whisper").unwrap_err();
        assert!(matches!(error, AsrError::ToolMissing(_)), "{error}");
    }
}
