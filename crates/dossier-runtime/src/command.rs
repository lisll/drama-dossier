//! 外部命令执行抽象。
//!
//! 为什么要有这一层：运行时探测（`ffmpeg` / `nvidia-smi` / `python` / `openssl`）在本机
//! 未必都有对应的可执行文件，而「有 NVIDIA GPU 才允许选 NVENC」「没有就回退 CPU」
//! 「签名校验失败必须拒绝安装」这类分支又必须被测试守住。把「执行动作」抽成
//! [`CommandRunner`]，生产代码用 [`SystemRunner`] 真跑进程，单测用 [`FakeRunner`]
//! 回放固定输出——分支就能在没有硬件、甚至没有装对应工具的机器上被覆盖。
//!
//! 与项目既有约定一致：不捆绑外部工具，只调用用户环境里已有的程序。

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use thiserror::Error;

/// 一次命令调用的完整描述。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    /// 写入子进程 stdin 的字节；`None` 表示立即关闭 stdin。
    pub stdin: Option<Vec<u8>>,
    pub cwd: Option<PathBuf>,
    /// 超时上限；`None` 表示不限时（不推荐用于可能挂起的图形/网络程序）。
    pub timeout: Option<Duration>,
    pub env: Vec<(String, String)>,
}

impl CommandSpec {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            stdin: None,
            cwd: None,
            timeout: None,
            env: Vec::new(),
        }
    }

    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    #[must_use]
    pub fn stdin_bytes(mut self, data: Vec<u8>) -> Self {
        self.stdin = Some(data);
        self
    }

    #[must_use]
    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// 用于错误信息与「给用户看的可复制命令」的渲染形式。
    ///
    /// 只是展示用途，**不**代表经过 shell 转义，不要拿它去 `sh -c`。
    pub fn display(&self) -> String {
        let mut out = self.program.clone();
        for arg in &self.args {
            out.push(' ');
            if arg.contains(' ') {
                out.push('"');
                out.push_str(arg);
                out.push('"');
            } else {
                out.push_str(arg);
            }
        }
        out
    }
}

/// 命令执行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// 退出码；被信号杀死时为 `None`。
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// stdout 的原始字节。
    ///
    /// `stdout` 是 lossy 解码后的文本，够用来解析日志；但 ffmpeg 这类工具会把
    /// **二进制**（rawvideo、png）写到 stdout，逐像素校验必须拿到没被 UTF-8 解码
    /// 破坏的字节，所以两者都留着。
    pub stdout_raw: Vec<u8>,
}

impl CommandOutput {
    pub fn new(code: i32, stdout: &str, stderr: &str) -> Self {
        Self {
            code: Some(code),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            stdout_raw: stdout.as_bytes().to_vec(),
        }
    }

    /// 带原始字节的构造（用于 stdout 是二进制而非文本的场景）。
    pub fn with_raw(code: i32, stdout: String, stderr: &str, stdout_raw: Vec<u8>) -> Self {
        Self {
            code: Some(code),
            stdout,
            stderr: stderr.to_string(),
            stdout_raw,
        }
    }

    pub fn ok(stdout: &str) -> Self {
        Self::new(0, stdout, "")
    }

    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// 退出码的展示形式（含「信号终止」）。
    pub fn code_label(&self) -> String {
        match self.code {
            Some(code) => code.to_string(),
            None => "signal".to_string(),
        }
    }

    /// 合并 stdout/stderr，供解析带告警输出的工具结果时兜底。
    pub fn combined(&self) -> String {
        let mut out = self.stdout.clone();
        if !self.stderr.is_empty() {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&self.stderr);
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CommandError {
    #[error("未找到可执行文件 {program}；请安装后重试，或用参数指定其路径")]
    NotFound { program: String },
    #[error("启动 {program} 失败：{message}")]
    Spawn { program: String, message: String },
    #[error("命令 {program} 超时（{} 秒）", .after.as_secs_f64())]
    Timeout { program: String, after: Duration },
}

/// 「执行一条命令」的能力。生产实现是 [`SystemRunner`]，测试实现是 [`FakeRunner`]。
pub trait CommandRunner {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, CommandError>;
}

/// 真跑进程的实现。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

impl SystemRunner {
    pub fn new() -> Self {
        Self
    }
}

impl CommandRunner for SystemRunner {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, CommandError> {
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        for (key, value) in &spec.env {
            command.env(key, value);
        }
        // 让子进程脱离我们自己的信号处理，避免终端 Ctrl-C 时留下僵尸。
        let mut child = command.spawn().map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                CommandError::NotFound {
                    program: spec.program.clone(),
                }
            } else {
                CommandError::Spawn {
                    program: spec.program.clone(),
                    message: source.to_string(),
                }
            }
        })?;

        match &spec.stdin {
            Some(data) => {
                if let Some(mut pipe) = child.stdin.take() {
                    let _ = pipe.write_all(data);
                }
            }
            None => drop(child.stdin.take()),
        }

        // 必须在 wait 之前把两个管道挂到线程上排空，否则子进程写满缓冲区就会死等。
        let stdout_reader = child.stdout.take().map(spawn_drain);
        let stderr_reader = child.stderr.take().map(spawn_drain);

        let deadline = spec.timeout.map(|timeout| Instant::now() + timeout);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(source) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(CommandError::Spawn {
                        program: spec.program.clone(),
                        message: source.to_string(),
                    });
                }
            }
            if let Some(deadline) = deadline {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(CommandError::Timeout {
                        program: spec.program.clone(),
                        after: spec.timeout.unwrap_or_default(),
                    });
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        };

        let stdout = join_drain(stdout_reader);
        let stderr = join_drain(stderr_reader);
        Ok(CommandOutput {
            code: status.code(),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            stdout_raw: stdout,
        })
    }
}

fn spawn_drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = pipe.read_to_end(&mut buffer);
        buffer
    })
}

fn join_drain(handle: Option<std::thread::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    match handle {
        Some(handle) => handle.join().unwrap_or_default(),
        None => Vec::new(),
    }
}

/// 单测用的假执行器：按「程序名 + 参数包含」匹配预置输出，并记录调用序列。
///
/// **规则按注册顺序取第一条命中的**，所以必须先注册具体规则（[`FakeRunner::on_match`]），
/// 再注册「这个程序干什么都返回同一结果」的通用规则（[`FakeRunner::on`]）；
/// 顺序反了的话，具体规则永远轮不到，测试会以一种很难查的方式变绿/变红。
#[derive(Debug, Default)]
pub struct FakeRunner {
    inner: Mutex<FakeInner>,
}

#[derive(Debug, Default)]
struct FakeInner {
    rules: Vec<Rule>,
    calls: Vec<CommandSpec>,
    /// 没有规则命中时的行为；缺省是「未找到」。
    fallback: Option<Result<CommandOutput, CommandError>>,
}

#[derive(Debug)]
struct Rule {
    program: String,
    args_contains: Option<String>,
    result: Result<CommandOutput, CommandError>,
}

impl FakeRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// 按程序名匹配，任何参数都返回同一输出。
    #[must_use]
    pub fn on(self, program: &str, output: CommandOutput) -> Self {
        self.push_rule(program, None, Ok(output))
    }

    /// 按程序名匹配，任何参数都返回同一错误。
    #[must_use]
    pub fn on_err(self, program: &str, error: CommandError) -> Self {
        self.push_rule(program, None, Err(error))
    }

    /// 按「程序名 + 参数包含某段文本」匹配；用于同一个程序的不同子命令。
    #[must_use]
    pub fn on_match(self, program: &str, needle: &str, output: CommandOutput) -> Self {
        self.push_rule(program, Some(needle), Ok(output))
    }

    /// 设置未命中规则时的兜底结果。
    #[must_use]
    pub fn with_fallback(self, result: Result<CommandOutput, CommandError>) -> Self {
        self.lock().fallback = Some(result);
        self
    }

    fn push_rule(
        self,
        program: &str,
        args_contains: Option<&str>,
        result: Result<CommandOutput, CommandError>,
    ) -> Self {
        self.lock().rules.push(Rule {
            program: program.to_string(),
            args_contains: args_contains.map(str::to_string),
            result,
        });
        self
    }

    /// 已发生的调用（按顺序）。
    pub fn calls(&self) -> Vec<CommandSpec> {
        self.lock().calls.clone()
    }

    /// 已调用过的程序名（按顺序，含重复）。
    pub fn programs(&self) -> Vec<String> {
        self.lock()
            .calls
            .iter()
            .map(|call| call.program.clone())
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeInner> {
        self.inner.lock().unwrap_or_else(|err| err.into_inner())
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, CommandError> {
        let mut inner = self.lock();
        inner.calls.push(spec.clone());
        let joined = spec.args.join(" ");
        for rule in &inner.rules {
            if rule.program != spec.program {
                continue;
            }
            match &rule.args_contains {
                Some(needle) if !joined.contains(needle.as_str()) => continue,
                _ => return rule.result.clone(),
            }
        }
        match &inner.fallback {
            Some(result) => result.clone(),
            None => Err(CommandError::NotFound {
                program: spec.program.clone(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_display_quotes_spaces() {
        let spec = CommandSpec::new("ffmpeg").arg("-i").arg("my clip.mp4");
        assert_eq!(spec.display(), "ffmpeg -i \"my clip.mp4\"");
    }

    #[test]
    fn system_runner_reports_missing_program() {
        let runner = SystemRunner::new();
        let err = runner
            .run(&CommandSpec::new("definitely-not-a-real-binary-xyz"))
            .unwrap_err();
        assert!(matches!(err, CommandError::NotFound { .. }));
    }

    #[test]
    fn system_runner_captures_stdout_and_stdin() {
        let runner = SystemRunner::new();
        let output = runner
            .run(
                &CommandSpec::new("cat")
                    .stdin_bytes(b"hello runtime".to_vec())
                    .timeout(Duration::from_secs(10)),
            )
            .unwrap();
        assert!(output.success());
        assert_eq!(output.stdout, "hello runtime");
    }

    #[test]
    fn system_runner_times_out_and_kills() {
        let runner = SystemRunner::new();
        let started = Instant::now();
        let err = runner
            .run(
                &CommandSpec::new("sleep")
                    .arg("30")
                    .timeout(Duration::from_millis(150)),
            )
            .unwrap_err();
        assert!(matches!(err, CommandError::Timeout { .. }));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn system_runner_reports_nonzero_exit() {
        let runner = SystemRunner::new();
        let output = runner
            .run(&CommandSpec::new("sh").args(["-c", "echo boom >&2; exit 3"]))
            .unwrap();
        assert!(!output.success());
        assert_eq!(output.code, Some(3));
        assert_eq!(output.stderr.trim(), "boom");
        assert_eq!(output.code_label(), "3");
    }

    #[test]
    fn fake_runner_matches_program_and_args() {
        // 先具体、后通用：`on` 不带参数条件，放在前面会把后面两条都吃掉。
        let runner = FakeRunner::new()
            .on_match("ffmpeg", "-encoders", CommandOutput::ok("libx264"))
            .on_match("ffmpeg", "-filters", CommandOutput::ok("drawtext"))
            .on("ffmpeg", CommandOutput::ok("version"));
        assert!(runner
            .run(&CommandSpec::new("ffmpeg").arg("-encoders"))
            .unwrap()
            .stdout
            .contains("libx264"));
        assert!(runner
            .run(&CommandSpec::new("ffmpeg").arg("-filters"))
            .unwrap()
            .stdout
            .contains("drawtext"));
        assert_eq!(runner.programs(), vec!["ffmpeg", "ffmpeg"]);
    }

    #[test]
    fn fake_runner_defaults_to_not_found() {
        let runner = FakeRunner::new();
        let err = runner.run(&CommandSpec::new("nvidia-smi")).unwrap_err();
        assert!(matches!(err, CommandError::NotFound { .. }));
    }

    #[test]
    fn fake_runner_fallback_applies() {
        let runner = FakeRunner::new().with_fallback(Ok(CommandOutput::new(1, "", "nope")));
        let output = runner.run(&CommandSpec::new("anything")).unwrap();
        assert_eq!(output.code, Some(1));
    }
}
