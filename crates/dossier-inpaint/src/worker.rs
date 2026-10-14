//! 常驻 worker 会话（stdio 行协议）。
//!
//! 一次性模式下，每帧检测 / 每片段修复都要重新启动解释器并重新加载模型
//! （DBNet 会话约 0.2s，E2FGVI 权重可达数秒）。常驻会话把进程与权重
//! 摊到整段视频上：`dossier-inpaint` 起一个 worker，之后每个请求只写一行
//! `{"workDir": "..."}`、读一行结果。
//!
//! 像素数据仍然走原来的目录交换协议（`request.json` / `in/*.raw` / `boxes.json` /
//! `out/*.raw`），因此常驻模式不改变任何单次请求的语义，也不增加内存峰值。

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use thiserror::Error;

use crate::detect::WorkerConfig;

#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("无法启动常驻 worker {program}：{source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("常驻 worker 协议错误：{0}")]
    Protocol(String),
    #[error("常驻 worker 后端错误：{0}")]
    Backend(String),
    #[error("常驻 worker I/O 失败：{0}")]
    Io(#[from] std::io::Error),
}

/// 一个常驻 worker 子进程；进程随对象析构而终止。
pub struct WorkerSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl WorkerSession {
    /// 以 `--serve` 启动 worker，并等待它的 `{"ready": true}` 握手。
    pub fn spawn(config: &WorkerConfig) -> Result<Self, WorkerError> {
        let mut child = Command::new(&config.program)
            .args(&config.args)
            .arg("--serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|source| WorkerError::Spawn {
                program: config.program.display().to_string(),
                source,
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| WorkerError::Protocol("无法取得 worker stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| WorkerError::Protocol("无法取得 worker stdout".into()))?;
        let mut session = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        };
        let hello = session.read_line()?;
        let value: serde_json::Value = serde_json::from_str(&hello)
            .map_err(|error| WorkerError::Protocol(format!("握手响应不是 JSON：{error}")))?;
        if value.get("ready").and_then(|flag| flag.as_bool()) != Some(true) {
            return Err(WorkerError::Protocol(format!(
                "worker 未返回 ready 握手：{hello}"
            )));
        }
        Ok(session)
    }

    /// 让 worker 处理 `work_dir` 里的请求，阻塞到该请求完成。
    pub fn run(&mut self, work_dir: &Path) -> Result<(), WorkerError> {
        let request = serde_json::json!({ "workDir": work_dir.display().to_string() });
        // 管道断开说明 worker 已经死了（例如加载模型后崩溃），这是一个「worker 不可用」
        // 的协议级问题，不应把它和普通的 I/O 错误混在一起。
        if let Err(error) = writeln!(self.stdin, "{request}").and_then(|_| self.stdin.flush()) {
            return Err(self.pipe_error(error));
        }
        let line = self.read_line()?;
        let value: serde_json::Value = serde_json::from_str(&line)
            .map_err(|error| WorkerError::Protocol(format!("响应不是 JSON：{error}（{line}）")))?;
        if value.get("ok").and_then(|flag| flag.as_bool()) == Some(true) {
            return Ok(());
        }
        let message = value
            .get("error")
            .and_then(|text| text.as_str())
            .unwrap_or("未知错误")
            .to_string();
        match value.get("kind").and_then(|kind| kind.as_str()) {
            Some("protocol") => Err(WorkerError::Protocol(message)),
            _ => Err(WorkerError::Backend(message)),
        }
    }

    /// worker 进程号（日志/排查用）。
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// 把「管道断开」翻译成带退出状态的协议错误。
    fn pipe_error(&mut self, error: std::io::Error) -> WorkerError {
        if error.kind() == std::io::ErrorKind::BrokenPipe {
            return match self.child.try_wait().ok().flatten() {
                Some(status) => {
                    WorkerError::Protocol(format!("worker 已退出（{status}），请求未能送达"))
                }
                None => WorkerError::Protocol("worker 关闭了输入流，请求未能送达".into()),
            };
        }
        WorkerError::Io(error)
    }

    fn read_line(&mut self) -> Result<String, WorkerError> {
        let mut line = String::new();
        let read = self.stdout.read_line(&mut line)?;
        if read == 0 {
            let status = self.child.try_wait().ok().flatten();
            return Err(WorkerError::Protocol(match status {
                Some(status) => format!("worker 已退出（{status}），且没有返回结果"),
                None => "worker 关闭了输出流，且没有返回结果".into(),
            }));
        }
        Ok(line.trim().to_string())
    }
}

impl Drop for WorkerSession {
    fn drop(&mut self) {
        // 关闭 stdin 让 worker 自然退出；仍存活则强杀，避免留下孤儿进程。
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// 用 `/bin/sh -c <脚本>` 充当假 worker：不落地可执行文件，
    /// 因此不会撞上「并行 fork 时 exec 到正被写入的文件」的 ETXTBSY。
    fn fake_worker(name: &str, script: &str) -> (PathBuf, WorkerConfig) {
        let dir =
            std::env::temp_dir().join(format!("dossier-worker-{name}-{}", std::process::id()));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).unwrap();
        let config = WorkerConfig {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".to_string(), script.to_string()],
        };
        (dir, config)
    }

    const OK_SCRIPT: &str = r#"echo '{"ready":true,"pid":1}'
while read -r line; do echo '{"ok":true}'; done
"#;

    #[test]
    fn session_handshakes_and_serves_multiple_requests_in_one_process() {
        let (dir, config) = fake_worker("ok", OK_SCRIPT);
        let mut session = WorkerSession::spawn(&config).unwrap();
        for _ in 0..3 {
            session.run(&dir).unwrap();
        }
    }

    #[test]
    fn backend_errors_are_surfaced_with_their_kind() {
        let (dir, config) = fake_worker(
            "backend-error",
            "echo '{\"ready\":true,\"pid\":1}'\nread -r line\necho '{\"ok\":false,\"kind\":\"backend\",\"error\":\"boom\"}'
\n",
        );
        let mut session = WorkerSession::spawn(&config).unwrap();
        match session.run(&dir) {
            Err(WorkerError::Backend(message)) => assert_eq!(message, "boom"),
            Err(other) => panic!("期望 backend 错误，实际 {other:?}"),
            Ok(()) => panic!("期望 backend 错误，实际成功"),
        }
    }

    #[test]
    fn protocol_errors_keep_their_kind() {
        let (dir, config) = fake_worker(
            "protocol-error",
            "echo '{\"ready\":true,\"pid\":1}'\nread -r line\necho '{\"ok\":false,\"kind\":\"protocol\",\"error\":\"bad request\"}'
\n",
        );
        let mut session = WorkerSession::spawn(&config).unwrap();
        match session.run(&dir) {
            Err(WorkerError::Protocol(message)) => assert_eq!(message, "bad request"),
            Err(other) => panic!("期望协议错误，实际 {other:?}"),
            Ok(()) => panic!("期望协议错误，实际成功"),
        }
    }

    #[test]
    fn missing_ready_handshake_is_rejected() {
        let (_dir, config) = fake_worker("no-handshake", "echo '{\"ok\":true}'\nsleep 1\n");
        match WorkerSession::spawn(&config) {
            Err(WorkerError::Protocol(message)) => assert!(message.contains("ready"), "{message}"),
            Err(other) => panic!("期望握手错误，实际 {other:?}"),
            Ok(_) => panic!("期望握手错误，实际成功"),
        }
    }

    #[test]
    fn worker_exit_without_result_reports_the_status() {
        let (dir, config) =
            fake_worker("early-exit", "echo '{\"ready\":true,\"pid\":1}'\nexit 3\n");
        let mut session = WorkerSession::spawn(&config).unwrap();
        match session.run(&dir) {
            Err(WorkerError::Protocol(message)) => {
                // 时序上可能是「写请求时管道已断开」或「读结果时读到 EOF」，
                // 两条路径都必须落到协议错误，并指明 worker 已不可用。
                assert!(message.contains("worker"), "{message}");
            }
            Err(other) => panic!("期望协议错误，实际 {other:?}"),
            Ok(()) => panic!("期望协议错误，实际成功"),
        }
    }
}
