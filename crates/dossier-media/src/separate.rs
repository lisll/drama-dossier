//! 音源分离（Demucs）。
//!
//! 短剧场景下的用途很具体：**换掉原声只留配乐**（做译制/口播重配），或者
//! **只取人声**（做字幕对齐、做粗剪参考）。两轨模式（`--two-stems=vocals`）就够，
//! 比四轨（鼓/贝斯/其他）更快、更省内存，也更符合短剧「有配乐没人声」的常态。
//!
//! 这里不实现模型，只做三件事：**规划参数 → 生成命令 → 说清输出落在哪**。
//! 最后一点尤其重要：Demucs 的输出路径规则（`<out>/<model>/<track>/vocals.wav`）
//! 不写下来，调用方只能靠猜，于是「跑完了但找不到文件」。

use std::path::{Path, PathBuf};

use dossier_runtime::{CommandRunner, CommandSpec};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::failure_message;

/// 音源分离错误。
#[derive(Debug, Error)]
pub enum SeparateError {
    #[error("输入文件不存在：{0}")]
    InputMissing(PathBuf),
    #[error("分离模型名不能为空")]
    EmptyModel,
    #[error("本机没有 demucs：{0}")]
    ToolMissing(String),
    #[error("分离失败（{command}）：{message}")]
    RunFailed { command: String, message: String },
    #[error("分离命令成功但预期音轨不存在：{0}")]
    OutputMissing(String),
}

/// 分离模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeparateMode {
    /// 两轨：人声 / 伴奏（默认，短剧最常用）。
    TwoStems,
    /// 四轨：人声 / 鼓 / 贝斯 / 其他。
    FourStems,
}

impl SeparateMode {
    pub fn stems(&self) -> &'static [&'static str] {
        match self {
            Self::TwoStems => &["vocals", "no_vocals"],
            Self::FourStems => &["vocals", "drums", "bass", "other"],
        }
    }
}

/// 分离计划。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeparatePlan {
    pub input: PathBuf,
    /// 输出根目录。
    pub output_dir: PathBuf,
    pub mode: SeparateMode,
    pub model: String,
    /// GPU 可用时用 CUDA（`-d cuda`），否则 CPU（`-d cpu`）。
    pub device: String,
    /// 混音设备名（Demucs 的 `--shifts` 之类的增强项保持默认，不在这里暴露）。
    pub jobs: u32,
}

impl Default for SeparatePlan {
    fn default() -> Self {
        Self {
            input: PathBuf::new(),
            output_dir: PathBuf::new(),
            mode: SeparateMode::TwoStems,
            model: "htdemucs".to_string(),
            device: "cpu".to_string(),
            jobs: 1,
        }
    }
}

/// Demucs 的输入文件名（去掉扩展名后就是它建出来的子目录名）。
pub fn track_name(input: &Path) -> String {
    input
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "track".to_string())
}

impl SeparatePlan {
    pub fn new(input: impl Into<PathBuf>, output_dir: impl Into<PathBuf>) -> Self {
        Self {
            input: input.into(),
            output_dir: output_dir.into(),
            ..Self::default()
        }
    }

    /// Demucs 会把每个音轨写到这里（`<out>/<model>/<track>/<stem>.wav`）。
    pub fn stem_path(&self, stem: &str) -> PathBuf {
        self.output_dir
            .join(&self.model)
            .join(track_name(&self.input))
            .join(format!("{stem}.wav"))
    }

    /// 计划里所有 stem 的预期路径。
    pub fn expected_outputs(&self) -> Vec<PathBuf> {
        self.mode
            .stems()
            .iter()
            .map(|stem| self.stem_path(stem))
            .collect()
    }

    pub fn validate(&self) -> Result<(), SeparateError> {
        if !self.input.is_file() {
            return Err(SeparateError::InputMissing(self.input.clone()));
        }
        if self.model.trim().is_empty() {
            return Err(SeparateError::EmptyModel);
        }
        Ok(())
    }

    /// 生成命令。`available` 为假时直接报「缺工具」，不生成一条注定失败的命令。
    pub fn build_command(
        &self,
        demucs: &str,
        available: bool,
    ) -> Result<CommandSpec, SeparateError> {
        self.validate()?;
        if !available {
            return Err(SeparateError::ToolMissing(
                "请安装 demucs（pip install demucs）".to_string(),
            ));
        }
        let mut spec = CommandSpec::new(demucs.to_string())
            .arg("-n")
            .arg(self.model.clone())
            .arg("-d")
            .arg(self.device.clone())
            .arg("-j")
            .arg(self.jobs.to_string());
        if self.mode == SeparateMode::TwoStems {
            spec = spec.arg("--two-stems=vocals");
        }
        Ok(spec
            .arg("-o")
            .arg(self.output_dir.display().to_string())
            .arg(self.input.display().to_string()))
    }
}

/// 一次分离的执行结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeparateOutcome {
    pub command: String,
    /// 实际存在于磁盘上的音轨文件（顺序与 [`SeparatePlan::expected_outputs`] 一致）。
    pub outputs: Vec<PathBuf>,
}

impl SeparatePlan {
    /// 真的跑一次 demucs。
    ///
    /// 缺工具/参数不成立时在**执行前**报错（[`SeparatePlan::build_command`] 已经把这两类
    /// 判掉了）；命令成功但预期音轨不在时也报错——「跑完了但找不到文件」必须显式暴露，
    /// 否则调用方会拿到一个空的音轨列表，然后在更远的地方以更难查的方式失败。
    pub fn execute(
        &self,
        runner: &dyn CommandRunner,
        demucs: &str,
    ) -> Result<SeparateOutcome, SeparateError> {
        let available = runner
            .run(&CommandSpec::new(demucs.to_string()).arg("--version"))
            .map(|output| output.success())
            .unwrap_or(false);
        let spec = self.build_command(demucs, available)?;
        let output = runner
            .run(&spec)
            .map_err(|error| SeparateError::RunFailed {
                command: spec.display(),
                message: error.to_string(),
            })?;
        if !output.success() {
            return Err(SeparateError::RunFailed {
                command: spec.display(),
                message: failure_message(&output),
            });
        }
        let expected = self.expected_outputs();
        let missing: Vec<String> = expected
            .iter()
            .filter(|path| !path.is_file())
            .map(|path| path.display().to_string())
            .collect();
        if !missing.is_empty() {
            return Err(SeparateError::OutputMissing(missing.join(", ")));
        }
        Ok(SeparateOutcome {
            command: spec.display(),
            outputs: expected,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(tag: &str) -> SeparatePlan {
        let dir = std::env::temp_dir().join(format!("dossier-sep-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("episode01.wav");
        std::fs::write(&input, b"fake").unwrap();
        SeparatePlan::new(&input, dir.join("stems"))
    }

    #[test]
    fn two_stem_plan_predicts_the_output_layout() {
        let plan = plan("two");
        assert_eq!(
            plan.stem_path("vocals"),
            plan.output_dir.join("htdemucs/episode01/vocals.wav")
        );
        assert_eq!(plan.expected_outputs().len(), 2);
        let spec = plan.build_command("demucs", true).unwrap();
        assert_eq!(spec.program, "demucs");
        assert!(spec.args.contains(&"--two-stems=vocals".to_string()));
        assert_eq!(
            spec.args[spec.args.len() - 1],
            plan.input.display().to_string()
        );
        assert_eq!(
            spec.args[spec.args.len() - 3..spec.args.len() - 1].to_vec(),
            vec!["-o".to_string(), plan.output_dir.display().to_string()]
        );
    }

    #[test]
    fn four_stem_mode_lists_every_stem() {
        let mut plan = plan("four");
        plan.mode = SeparateMode::FourStems;
        assert_eq!(plan.expected_outputs().len(), 4);
        let spec = plan.build_command("demucs", true).unwrap();
        assert!(!spec.args.iter().any(|arg| arg.starts_with("--two-stems")));
    }

    #[test]
    fn missing_tool_is_an_error_not_a_silent_noop() {
        let plan = plan("missing");
        let error = plan.build_command("demucs", false).unwrap_err();
        assert!(matches!(error, SeparateError::ToolMissing(_)));
        assert!(error.to_string().contains("demucs"));
    }

    #[test]
    fn rejects_a_missing_input() {
        let mut plan = plan("noinput");
        plan.input = PathBuf::from("/definitely/not/here.wav");
        assert!(matches!(
            plan.validate(),
            Err(SeparateError::InputMissing(_))
        ));
    }
}

#[cfg(test)]
mod execute_tests {
    use super::*;
    use dossier_runtime::{CommandOutput, FakeRunner};

    fn plan(tag: &str) -> SeparatePlan {
        let dir =
            std::env::temp_dir().join(format!("dossier-sep-exec-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("episode01.wav");
        std::fs::write(&input, b"fake").unwrap();
        SeparatePlan::new(&input, dir.join("stems"))
    }

    fn materialize(plan: &SeparatePlan) {
        for path in plan.expected_outputs() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"wav").unwrap();
        }
    }

    #[test]
    fn execute_returns_the_stems_that_really_exist() {
        let plan = plan("ok");
        materialize(&plan);
        let runner = FakeRunner::new().on("demucs", CommandOutput::new(0, "", ""));
        let outcome = plan.execute(&runner, "demucs").unwrap();
        assert_eq!(outcome.outputs.len(), 2);
        assert!(outcome.command.contains("--two-stems=vocals"));
    }

    #[test]
    fn execute_reports_missing_stems_instead_of_returning_an_empty_list() {
        let plan = plan("missing");
        std::fs::create_dir_all(&plan.output_dir).unwrap();
        let runner = FakeRunner::new().on("demucs", CommandOutput::new(0, "", ""));
        let error = plan.execute(&runner, "demucs").unwrap_err();
        assert!(matches!(error, SeparateError::OutputMissing(_)), "{error}");
    }

    #[test]
    fn execute_refuses_when_demucs_is_absent() {
        let plan = plan("absent");
        let runner = FakeRunner::new();
        let error = plan.execute(&runner, "demucs").unwrap_err();
        assert!(matches!(error, SeparateError::ToolMissing(_)), "{error}");
    }

    #[test]
    fn a_nonzero_exit_becomes_a_run_failure_with_the_tool_output() {
        let plan = plan("fail");
        let runner = FakeRunner::new()
            .on_match(
                "demucs",
                "--two-stems=vocals",
                CommandOutput::new(1, "", "CUDA out of memory"),
            )
            .on("demucs", CommandOutput::new(0, "", ""));
        let error = plan.execute(&runner, "demucs").unwrap_err();
        match error {
            SeparateError::RunFailed { message, .. } => assert!(message.contains("CUDA")),
            other => panic!("应报 RunFailed，实际 {other}"),
        }
    }
}
