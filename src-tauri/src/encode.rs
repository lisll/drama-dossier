//! 编码执行层（M4b-1）：统一的编码参数、可中断的 ffmpeg 子进程、硬件失败退回软件编码。
//!
//! 之前 `toolbox.rs` 里写死 `ENCODE_ARGS`（libx264 / veryfast / crf 23），三个转码功能各自复制一份。
//! 现在「智能微调」「分集重组」「码率修复」「处理版本」都从这里拿编码参数：
//! - 硬件编码：按目标码率设 `-b:v` / `-maxrate` / `-bufsize`；
//! - 软件编码：`-crf` 起步，同样用 `-maxrate` / `-bufsize` 封顶，保证不管哪个分支都超不过规格上限。
//!
//! 可中断子进程：`spawn` 之后循环 `try_wait`，每 200 毫秒看一次控制标志，收到终止就 `kill`
//! 并删半成品。处理版本（M4b-1）和批量擦除（M4b-2）的长任务都走这一套。
//!
//! 硬件失败退软件：`verify_encoder_plan` 单帧实测不过，或真实编码中途失败，就用软件编码重跑
//! 一次，并把「已改用软件编码」写进结果——不静默降级。

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dossier_runtime::caps::{
    plan_encoder, verify_encoder_plan, EncoderPlan, EncoderVerificationConfig, HardwareSignal,
};
use dossier_runtime::probe::{parse_encoder_listing, EncoderList};
use dossier_runtime::CommandRunner;

/// 谁在用什么编码器：结果里如实记录，界面和版本说明都要展示。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EncoderChoice {
    /// 选中的编码器名（如 `h264_qsv` / `libx264`）。
    pub encoder: String,
    /// 是否硬件编码。
    pub hardware: bool,
    /// 决策理由（含被挡掉的候选，可读）。
    pub reason: String,
    /// 这一次编码是否是「硬件失败后改用软件」。
    pub fell_back_to_software: bool,
}

/// 编码请求：一次 ffmpeg 调用的输出侧参数。
#[derive(Debug, Clone)]
pub struct EncodeRequest<'a> {
    /// 输出文件。
    pub output: &'a Path,
    /// 视频码率上限（kbps）。处理版本用输出规格；`None` 表示不封顶（旧的转码工具没有规格）。
    pub max_video_kbps: Option<u64>,
    /// 音频码率（kbps）。
    pub audio_kbps: u64,
    /// 有自定义元数据（AI 隐式标识）时为真：MP4 要加 `use_metadata_tags`，否则自定义键会被静默丢掉。
    pub metadata_tags: bool,
}

/// 硬件编码不封顶时的目标码率（kbps）：硬件编码器没有 crf，必须给一个码率。
const HARDWARE_DEFAULT_KBPS: u64 = 8000;

/// 按输出规格上限生成 `-c:v` 之后的编码参数（不含输出路径本身）。
///
/// 硬件编码：`-b:v` 直接对准上限，`-maxrate` / `-bufsize` 同值（ffmpeg 多数硬件编码器
/// 忽略 crf；给足码率预算即可）。软件编码：`-crf` 保画质，`-maxrate` / `-bufsize`
/// 封顶防尖峰，两项组合保证「平均和瞬时都超不过规格」。
pub fn encode_output_args(encoder: &str, hardware: bool, request: &EncodeRequest) -> Vec<String> {
    let mut args = vec!["-c:v".to_string(), encoder.to_string()];
    if hardware {
        let kbps = request.max_video_kbps.unwrap_or(HARDWARE_DEFAULT_KBPS);
        args.push("-b:v".into());
        args.push(format!("{kbps}k"));
        args.push("-maxrate".into());
        args.push(format!("{kbps}k"));
        args.push("-bufsize".into());
        args.push(format!("{}k", kbps.saturating_mul(2)));
    } else {
        args.push("-preset".into());
        args.push("veryfast".into());
        args.push("-crf".into());
        args.push("23".into());
        if let Some(kbps) = request.max_video_kbps {
            args.push("-maxrate".into());
            args.push(format!("{kbps}k"));
            args.push("-bufsize".into());
            args.push(format!("{}k", kbps.saturating_mul(2)));
        }
    }
    args.push("-c:a".into());
    args.push("aac".into());
    args.push("-b:a".into());
    args.push(format!("{}k", request.audio_kbps));
    // `-movflags` 只认最后一次出现：两个标志必须写在同一个参数里。
    args.push("-movflags".into());
    args.push(if request.metadata_tags {
        "+faststart+use_metadata_tags".into()
    } else {
        "+faststart".into()
    });
    args
}

/// 探测本机编码器清单：跑一次 `ffmpeg -encoders`。
pub fn list_encoders(ffmpeg: &Path) -> Result<EncoderList, String> {
    let runner = dossier_runtime::SystemRunner::new();
    let spec = dossier_runtime::CommandSpec::new(ffmpeg.display().to_string())
        .args(["-hide_banner", "-encoders"])
        .timeout(Duration::from_secs(30));
    let output = runner
        .run(&spec)
        .map_err(|error| format!("ffmpeg 启动失败（{}）：{error}", ffmpeg.display()))?;
    if !output.success() {
        return Err(format!("ffmpeg -encoders 退出码 {}", output.code_label()));
    }
    Ok(parse_encoder_listing(&output.combined()))
}

/// 一个任务里共用的编码器选择：首选计划（已单帧实测）+ 纯软件兜底计划。
/// 一个任务只探测一次，不必每集都重新跑 `-encoders` 和单帧实测。
#[derive(Debug, Clone)]
pub struct EncoderPlans {
    pub preferred: EncoderPlan,
    pub software: EncoderPlan,
}

/// 选编码器：候选链 + 平台信号 + 单帧实测。
pub fn prepare_encoders(ffmpeg: &Path, prefer_hardware: bool) -> Result<EncoderPlans, String> {
    let encoders = list_encoders(ffmpeg)?;
    let signal = HardwareSignal::detect().platform_defaults();
    let plan = plan_encoder(&encoders, "h264", prefer_hardware, &signal)
        .map_err(|error| error.to_string())?;
    let runner = dossier_runtime::SystemRunner::new();
    let config = EncoderVerificationConfig::new(ffmpeg.display().to_string());
    let preferred =
        verify_encoder_plan(&runner, &config, &plan).map_err(|error| error.to_string())?;
    // 兜底：不管首选是硬件还是软件，都准备一份「纯软件」计划（用户要求软件时两者相同）。
    let software =
        plan_encoder(&encoders, "h264", false, &signal).map_err(|error| error.to_string())?;
    Ok(EncoderPlans {
        preferred,
        software,
    })
}

/// 终止标志：任务队列「终止」按钮置位，正在跑的子进程循环里看到就 kill。
#[derive(Debug, Clone, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// 可中断的 ffmpeg 运行：`spawn` 后轮询 `try_wait`，每 200 毫秒检查一次终止标志。
///
/// 收到终止：`kill` 子进程、等他退出、删掉半成品，返回 `Err(Cancelled)`。
/// 超时（默认 6 小时，长剧单集按最高规格算绰绰有余）同样杀进程删半成品。
pub fn run_ffmpeg_cancellable(
    ffmpeg: &Path,
    args: &[String],
    output: &Path,
    cancel: &CancelFlag,
) -> Result<(), EncodeRunError> {
    run_child(ffmpeg, args, output, cancel, MAX_RUNTIME)
}

/// 一次可中断子进程的错误。手写 Display：src-tauri 不依赖 thiserror，
/// 而这里的展示文案就是给界面用的，自己写最直接。
#[derive(Debug, Clone, PartialEq)]
pub enum EncodeRunError {
    Cancelled,
    TimedOut { secs: u64 },
    SpawnFailed { program: String, message: String },
    Failed { code: String, first_line: String },
}

impl std::fmt::Display for EncodeRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeRunError::Cancelled => write!(f, "已终止"),
            EncodeRunError::TimedOut { secs } => {
                write!(f, "运行超时（{secs} 秒），已停止并删除半成品")
            }
            EncodeRunError::SpawnFailed { program, message } => {
                write!(f, "ffmpeg 启动失败（{program}）：{message}")
            }
            EncodeRunError::Failed { code, first_line } => {
                write!(f, "ffmpeg 退出码 {code}：{first_line}")
            }
        }
    }
}

/// 单集编码的最长运行时间：6 小时。最高规格（5000kbps × 15 分钟）远用不了这么久；
/// 到点还在跑说明多半是解码器卡死，杀掉比无限等强。
const MAX_RUNTIME: Duration = Duration::from_secs(6 * 3600);

fn run_child(
    program: &Path,
    args: &[String],
    output: &Path,
    cancel: &CancelFlag,
    timeout: Duration,
) -> Result<(), EncodeRunError> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| EncodeRunError::SpawnFailed {
            program: program.display().to_string(),
            message: error.to_string(),
        })?;
    // 边跑边读 stderr：不读的话输出一多会塞满管道缓冲区，子进程卡死在写日志上。
    let stderr_reader = child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            let mut tail: Vec<u8> = Vec::new();
            let mut buffer = [0u8; 8192];
            while let Ok(read) = stderr.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                tail.extend_from_slice(&buffer[..read]);
                if tail.len() > STDERR_TAIL_BYTES * 2 {
                    tail.drain(..tail.len() - STDERR_TAIL_BYTES);
                }
            }
            String::from_utf8_lossy(&tail).into_owned()
        })
    });
    let collect_stderr = |reader: Option<std::thread::JoinHandle<String>>| {
        reader
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default()
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // 先看终止标志：用户点「终止」不该等这一集跑完。
        if cancel.is_cancelled() {
            kill_child(&mut child);
            collect_stderr(stderr_reader);
            let _ = std::fs::remove_file(output);
            return Err(EncodeRunError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let combined = collect_stderr(stderr_reader);
                if status.success() {
                    return Ok(());
                }
                let _ = std::fs::remove_file(output);
                if cancel.is_cancelled() {
                    return Err(EncodeRunError::Cancelled);
                }
                let first_line = combined
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .unwrap_or("（无输出）")
                    .to_string();
                return Err(EncodeRunError::Failed {
                    code: code_label(&status),
                    first_line,
                });
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    kill_child(&mut child);
                    collect_stderr(stderr_reader);
                    let _ = std::fs::remove_file(output);
                    return Err(EncodeRunError::TimedOut {
                        secs: timeout.as_secs(),
                    });
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(error) => {
                kill_child(&mut child);
                collect_stderr(stderr_reader);
                let _ = std::fs::remove_file(output);
                return Err(EncodeRunError::SpawnFailed {
                    program: program.display().to_string(),
                    message: error.to_string(),
                });
            }
        }
    }
}

/// stderr 只留最后这么多字节（报错信息在末尾）。
const STDERR_TAIL_BYTES: usize = 16 * 1024;

fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn code_label(status: &std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => code.to_string(),
        None => "信号终止".to_string(),
    }
}

/// 一次「带退软件兜底」的编码：先按首选计划跑，失败且首选是硬件时用软件计划重跑一次。
///
/// `pre_output_args` 是编码器之前的全部参数（输入、`-filter_complex`、`-map`、`-metadata`），
/// 由调用方拼好。返回实际用的编码器（含「已改用软件编码」标记）。
/// 用户点了终止就直接返回，不再用软件重试。
pub fn encode_with_fallback(
    ffmpeg: &Path,
    pre_output_args: &[String],
    request: &EncodeRequest,
    plans: &EncoderPlans,
    cancel: &CancelFlag,
) -> Result<EncoderChoice, EncodeRunError> {
    let attempt = |encoder: &str, hardware: bool| -> Result<(), EncodeRunError> {
        let mut args: Vec<String> = vec![
            "-y".into(),
            "-hide_banner".into(),
            "-nostdin".into(),
            "-loglevel".into(),
            "error".into(),
        ];
        args.extend(pre_output_args.iter().cloned());
        args.extend(encode_output_args(encoder, hardware, request));
        args.push(request.output.display().to_string());
        run_ffmpeg_cancellable(ffmpeg, &args, request.output, cancel)
    };

    let plan = &plans.preferred;
    let first_error = match attempt(&plan.selected, plan.hardware) {
        Ok(()) => {
            return Ok(EncoderChoice {
                encoder: plan.selected.clone(),
                hardware: plan.hardware,
                reason: plan.reason.clone(),
                fell_back_to_software: false,
            });
        }
        Err(EncodeRunError::Cancelled) => return Err(EncodeRunError::Cancelled),
        Err(error) => error,
    };
    // 首选是硬件时才有「退软件」一说；首选本来就是软件，失败就是失败。
    if !plan.hardware {
        return Err(first_error);
    }
    let software = &plans.software;
    attempt(&software.selected, false)?;
    Ok(EncoderChoice {
        encoder: software.selected.clone(),
        hardware: false,
        reason: format!(
            "{}；硬件编码（{}）失败，已改用软件编码（原因：{first_error}）",
            software.reason, plan.selected
        ),
        fell_back_to_software: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(kbps: u64) -> EncodeRequest<'static> {
        EncodeRequest {
            output: Path::new("/tmp/x.mp4"),
            max_video_kbps: Some(kbps),
            audio_kbps: 128,
            metadata_tags: false,
        }
    }

    #[test]
    fn software_args_use_crf_with_cap() {
        let args = encode_output_args("libx264", false, &request(5000));
        let joined = args.join(" ");
        assert!(joined.contains("-c:v libx264"));
        assert!(joined.contains("-crf 23"));
        assert!(joined.contains("-maxrate 5000k"));
        assert!(joined.contains("-bufsize 10000k"));
        assert!(joined.contains("-c:a aac"));
        assert!(joined.contains("-b:a 128k"));
        assert!(joined.contains("+faststart"));
    }

    #[test]
    fn hardware_args_target_bitrate() {
        let args = encode_output_args("h264_qsv", true, &request(5000));
        let joined = args.join(" ");
        assert!(joined.contains("-b:v 5000k"));
        assert!(joined.contains("-maxrate 5000k"));
        assert!(!joined.contains("-crf"));
    }

    #[test]
    fn uncapped_software_matches_legacy_args_and_metadata_flag() {
        let request = EncodeRequest {
            output: Path::new("/tmp/x.mp4"),
            max_video_kbps: None,
            audio_kbps: 128,
            metadata_tags: true,
        };
        let joined = encode_output_args("libx264", false, &request).join(" ");
        assert_eq!(
            joined,
            "-c:v libx264 -preset veryfast -crf 23 -c:a aac -b:a 128k -movflags +faststart+use_metadata_tags"
        );
    }

    #[test]
    fn cancel_flag_round_trip() {
        let flag = CancelFlag::new();
        assert!(!flag.is_cancelled());
        flag.cancel();
        assert!(flag.is_cancelled());
        // 克隆共享同一个原子（任务队列持有克隆，子进程循环看原件）。
        let clone = flag.clone();
        assert!(clone.is_cancelled());
    }

    #[test]
    fn cancelled_run_kills_child_and_removes_output() {
        use std::io::Write;
        let dir = std::env::temp_dir().join("dossier-encode-test");
        std::fs::create_dir_all(&dir).unwrap();
        let output = dir.join("half.mp4");
        // 用 sleep 当假 ffmpeg：够慢，能验证「先取消再收尾」的路径。
        let fake = dir.join("fake-ffmpeg.sh");
        let mut script = std::fs::File::create(&fake).unwrap();
        writeln!(script, "#!/bin/sh\nsleep 30").unwrap();
        drop(script);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(&output, b"half").unwrap();
        let flag = CancelFlag::new();
        let worker = {
            let flag = flag.clone();
            let output = output.clone();
            let fake = fake.clone();
            std::thread::spawn(move || {
                run_ffmpeg_cancellable(&fake, &[], &output, &flag).unwrap_err()
            })
        };
        std::thread::sleep(Duration::from_millis(300));
        flag.cancel();
        let error = worker.join().unwrap();
        assert_eq!(error, EncodeRunError::Cancelled);
        assert!(!output.exists(), "半成品应该被删掉");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
