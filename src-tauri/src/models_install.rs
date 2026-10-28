//! 内容模型运行时一键安装：ASR（openai-whisper）与 OCR（tesseract）。
//!
//! 与 `install_runtime_blocking`（ffmpeg/ffprobe）同一套交互：后台阻塞执行、
//! `install-progress` 事件报进度、装完写回设置。安装目标都在应用数据目录，
//! 不碰系统 PATH。国内网络直连 PyPI / GitHub 基本不可用，所有外网下载都配
//! 了「镜像优先、官方兜底」的多级源：
//! - ASR：`<app_data>/asr-env` 虚拟环境装 openai-whisper（pip 清华镜像优先），
//!   base 模型下到 `<app_data>/models/whisper/base.pt`（CLI 用绝对路径加载，
//!   不落用户缓存）；
//! - OCR（macOS）：Homebrew 装 `tesseract` + `tesseract-lang`（含 chi_sim）；
//! - OCR（Windows）：UB-Mannheim 静默安装器（ghfast.top 镜像优先）装进
//!   `<app_data>/tesseract`（安装器忽略 /DIR 时从默认落位搬运回来），
//!   再补下 chi_sim 语言包（安装器默认组件不含中文）。
//!
//! 每个组件独立软失败：一边失败不影响另一边的成果，错误进报告 messages。

use std::path::{Path, PathBuf};
#[cfg(target_os = "windows")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dossier_core::AppSettings;
use dossier_runtime::CommandRunner;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::toolbox::read_settings;

/// CUDA 版 whisper 的后台下载取消标志（仅 Windows 使用）。
///
/// 单独一个标志而不是复用转码的取消开关：两者互不相干，
/// 用户在转码页点「停止」不应该顺手掐掉正在下载的 GPU 加速包。
/// 退出程序或离开设置页时置位，下载线程在下一次轮询时退出并清理半成品。
#[cfg(target_os = "windows")]
static CUDA_DOWNLOAD_CANCEL: AtomicBool = AtomicBool::new(false);

/// 请求取消 CUDA 包下载（幂等）。
#[cfg(target_os = "windows")]
pub(crate) fn cancel_cuda_download() {
    CUDA_DOWNLOAD_CANCEL.store(true, Ordering::SeqCst);
}

#[cfg(target_os = "windows")]
fn cuda_download_cancelled() -> bool {
    CUDA_DOWNLOAD_CANCEL.load(Ordering::SeqCst)
}

/// 退出程序时调用：把后台下载停掉，避免进程退出时留下半成品。
pub(crate) fn cancel_background_downloads() {
    #[cfg(target_os = "windows")]
    cancel_cuda_download();
}

/// Windows 用 UB-Mannheim 的 tesseract 安装器（Inno Setup，静默参数官方支持）。
/// 钉住已知可用版本；升级时换这里的版本号。
/// 多级下载源：GitHub 直连在国内基本拉不动，ghfast.top 加速代理优先，官方源兜底。
#[cfg(target_os = "windows")]
const TESSERACT_SETUP_SOURCES: &[&str] = &[
    "https://ghfast.top/https://github.com/UB-Mannheim/tesseract/releases/download/v5.4.0.20240606/tesseract-ocr-w64-setup-5.4.0.20240606.exe",
    "https://github.com/UB-Mannheim/tesseract/releases/download/v5.4.0.20240606/tesseract-ocr-w64-setup-5.4.0.20240606.exe",
];

/// chi_sim 语言包的多级下载源（GitHub raw 主源，CDN 镜像兜底国内直连）。
/// 仅 Windows 安装器使用；macOS 走 brew 自带的 chi_sim。
#[cfg(target_os = "windows")]
const CHI_SIM_SOURCES: &[&str] = &[
    "https://raw.githubusercontent.com/tesseract-ocr/tessdata_fast/main/chi_sim.traineddata",
    "https://cdn.jsdelivr.net/gh/tesseract-ocr/tessdata_fast@main/chi_sim.traineddata",
    "https://ghfast.top/https://raw.githubusercontent.com/tesseract-ocr/tessdata_fast/main/chi_sim.traineddata",
];

/// pip 安装要拉 torch（mac 约 60MB / win 约 200MB）再装 whisper 本体，给足超时。
const PIP_INSTALL_TIMEOUT_SECS: u64 = 1800;

/// PyPI 主源：清华镜像。镜像不可用时 pip 会自动落到官方源，不会因此装不上。
const PYPI_MIRROR: &str = "https://pypi.tuna.tsinghua.edu.cn/simple";
/// PyTorch 的 CUDA 轮子源。CUDA 版 torch **不在 PyPI**，必须单独指定；
/// 选 cu130 是因为内置 Python 是 3.13，老索引（cu121 及更早）没有 3.13 的轮子，
/// pip 会报「versions: none」。CUDA 13 要求驱动 580+；驱动过旧的机器装完
/// `torch.cuda.is_available()` 仍为 False，运行时会走 CPU 重试兜底（见 content.rs）。
/// 升级时同步改这里，并更新 `installed_torch_variant` 的后缀判断依据。
const PYTORCH_CUDA_INDEX: &str = "https://download.pytorch.org/whl/cu130";

/// 读出当前 venv 里 torch 的版本串（例如 `2.3.1+cu121` 或 `2.3.1`）。
/// 读不到返回 `None`——调用方按"没有 CUDA"处理，不阻塞安装。
fn installed_torch_variant(python: &Path) -> Option<String> {
    let spec = dossier_runtime::CommandSpec::new(python.display().to_string())
        .args(["-c", "import torch; print(torch.__version__)"])
        .timeout(Duration::from_secs(60));
    let output = dossier_runtime::SystemRunner::new().run(&spec).ok()?;
    if !output.success() {
        return None;
    }
    let line = output.stdout.lines().next()?.trim().to_string();
    if line.is_empty() {
        None
    } else {
        Some(line)
    }
}

/// base 模型约 140MB，网络慢时可能要几分钟。
const MODEL_DOWNLOAD_TIMEOUT_SECS: u64 = 1800;
/// Homebrew 装 tesseract + 全语言包约几十 MB，brew 首次可能更新索引。
#[cfg(target_os = "macos")]
const BREW_INSTALL_TIMEOUT_SECS: u64 = 900;

/// 一键安装结果。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentModelsReport {
    pub asr_path: Option<String>,
    pub ocr_path: Option<String>,
    /// 逐组件的软失败信息（报告里展示，不作为整体错误抛出）。
    pub messages: Vec<String>,
    pub note: String,
}

/// 发安装进度（与 toolbox 的 InstallProgress 事件同构，前端同一弹窗渲染）。
fn emit_progress(
    app: &AppHandle,
    component: &str,
    stage: &str,
    stage_label: &str,
    total_bytes: Option<u64>,
    downloaded_bytes: u64,
) {
    let _ = app.emit(
        "install-progress",
        serde_json::json!({
            "component": component,
            "stage": stage,
            "stageLabel": stage_label,
            "totalBytes": total_bytes,
            "downloadedBytes": downloaded_bytes,
        }),
    );
}

/// 应用数据根目录。
fn app_data(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map_err(|error| format!("无法定位应用数据目录：{error}"))
}

/// 取 stderr 末尾最多 3 行非空内容，拼成可展示的失败原因。
fn last_stderr(output: &dossier_runtime::CommandOutput) -> String {
    let mut tail: Vec<&str> = output
        .stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(3)
        .collect();
    tail.reverse();
    if tail.is_empty() {
        "（无输出）".to_string()
    } else {
        tail.join("\n")
    }
}

/// 把设置落盘（只改传入的闭包里动的字段）。
fn persist_settings(app: &AppHandle, update: impl FnOnce(&mut AppSettings)) -> Result<(), String> {
    let path = crate::settings_path(app)?;
    dossier_core::update_settings(&path, |settings| {
        update(settings);
        Ok(())
    })
    .map(|_| ())
    .map_err(|error| format!("写设置失败：{error}"))
}

/// 探测一个候选解释器能否执行（--version 秒级返回）。
fn python_works(program: &Path) -> bool {
    let spec = dossier_runtime::CommandSpec::new(program.display().to_string())
        .arg("--version")
        .timeout(Duration::from_secs(10));
    dossier_runtime::SystemRunner::new()
        .run(&spec)
        .map(|output| output.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Windows：检测到 NVIDIA 显卡时，后台把 CPU 版换成 CUDA 版
// ---------------------------------------------------------------------------

/// CUDA 版 whisper 的资源名。版本与 CI 内置的 CPU 版同源（同一份 whisper.cpp 源码），
/// 但 CUDA 版需要显卡驱动配合，所以走"检测到再下载"而不是内置。
#[cfg(target_os = "windows")]
const WHISPER_CUDA_ASSET: &str = "whisper-cli-cuda";

/// 后台尝试把 CPU 版 whisper-cli 换成 CUDA 版。
///
/// 触发条件：Windows + 装的是内置 CPU 版 + `nvidia-smi` 报有可用显卡。
/// 行为：下载到临时文件 → 校验 → 原子替换；期间 CPU 版照常可用，失败只记日志。
/// 取消：用户离开设置页或退出程序时置 `CUDA_DOWNLOAD_CANCEL`，下次轮询退出并删掉半成品。
#[cfg(target_os = "windows")]
fn upgrade_whisper_to_cuda(app: &AppHandle) {
    if !crate::toolbox::nvidia_gpu_present() {
        return;
    }
    let Ok(root) = app_data(app) else { return };
    let bin_dir = root.join("whisper");
    let target = bin_dir.join("whisper-cli.exe");
    if !target.is_file() {
        return;
    }
    // 已经换过就不再重复下载：用同目录的标记文件判断
    let stamp = bin_dir.join("cuda.ready");
    if stamp.is_file() {
        return;
    }
    let tmp = bin_dir.join("whisper-cli.exe.cuda-tmp");
    CUDA_DOWNLOAD_CANCEL.store(false, Ordering::SeqCst);
    emit_progress(
        app,
        "asr",
        "downloading",
        "检测到 NVIDIA 显卡，正在下载 GPU 加速包（约 285 MB）",
        None,
        0,
    );
    let asset = crate::downloads::whisper_cuda();
    let result = crate::downloads::fetch_with_fallback(&asset, &tmp, |url, path| {
        if cuda_download_cancelled() {
            return Err(crate::downloads::Failure::Network);
        }
        crate::toolbox::download_file(app, WHISPER_CUDA_ASSET, url, path)
            .map_err(|_| crate::downloads::Failure::Network)
    });
    match result {
        Ok(_) if !cuda_download_cancelled() => {
            // 原子替换：先备份 CPU 版，替换失败能回滚
            let backup = bin_dir.join("whisper-cli.exe.cpu");
            let _ = std::fs::rename(&target, &backup);
            match std::fs::rename(&tmp, &target) {
                Ok(()) => {
                    let _ = std::fs::write(&stamp, "1");
                    let _ = std::fs::remove_file(&backup);
                    crate::logging::log_event("环境", "ASR 已升级为 CUDA 版（GPU 加速）");
                    emit_progress(app, "asr", "finished", "GPU 加速包已安装", Some(0), 0);
                }
                Err(error) => {
                    let _ = std::fs::rename(&backup, &target);
                    crate::logging::log_event(
                        "环境",
                        format!("替换 CUDA 版失败（已保留 CPU 版）：{error}"),
                    );
                }
            }
        }
        _ => {
            let _ = std::fs::remove_file(&tmp);
            crate::logging::log_event(
                "环境",
                if cuda_download_cancelled() {
                    "GPU 加速包下载已取消（继续使用 CPU 版）".into()
                } else {
                    "GPU 加速包下载失败（继续使用 CPU 版）".to_string()
                },
            );
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn upgrade_whisper_to_cuda(_app: &AppHandle) {}

// ---------------------------------------------------------------------------
// 内置 whisper.cpp：零下载、无 Python
// ---------------------------------------------------------------------------

/// 安装包内置的 whisper 资源目录（CI 在打包时预置 whisper-cli 与 base 模型）。
/// 返回 `None` 表示这个构建里没有内置资源，调用方退回 pip 流程。
fn bundled_whisper_dir(app: &AppHandle) -> Option<PathBuf> {
    use tauri::Manager;
    let root = app.path().resource_dir().ok()?;
    let exe = if cfg!(target_os = "windows") {
        "whisper-cli.exe"
    } else {
        "whisper-cli"
    };
    // Tauri v2 把 `resources/whisper/**/*` 落在 `<resource_dir>/resources/whisper`；
    // 兼容再查一层 `<resource_dir>/whisper`，防打包器行为差异。
    let dir = [root.join("resources").join("whisper"), root.join("whisper")]
        .into_iter()
        .find(|dir| dir.join(exe).is_file());
    if dir.is_none() {
        crate::logging::log_event(
            "环境",
            "安装包未内置 whisper.cpp，退回 Python + pip 流程（本地开发或旧安装包属正常）",
        );
    }
    dir
}

/// 用内置资源完成 ASR 安装；返回 `Ok(Some(路径))` 表示已装好，
/// `Ok(None)` 表示没有内置资源、应退回 pip，`Err` 表示内置资源存在但安装失败。
fn install_bundled_whisper(app: &AppHandle, models_dir: &Path) -> Result<Option<String>, String> {
    let Some(bundled) = bundled_whisper_dir(app) else {
        return Ok(None);
    };
    let exe = if cfg!(target_os = "windows") {
        "whisper-cli.exe"
    } else {
        "whisper-cli"
    };
    let bin_src = bundled.join(exe);
    let model_src = bundled.join("models").join("ggml-base.bin");
    if !model_src.is_file() {
        // 二进制在、模型不在：宁可退回 pip，也不要装出一半
        crate::logging::log_event(
            "环境",
            format!(
                "内置 whisper 缺少模型文件 {}，退回 pip 流程",
                model_src.display()
            ),
        );
        return Ok(None);
    }

    let root = app_data(app)?;
    let bin_dir = root.join("whisper");
    std::fs::create_dir_all(&bin_dir).map_err(|error| format!("创建目录失败：{error}"))?;
    let bin = bin_dir.join(exe);
    let model = models_dir.join("ggml-base.bin");

    emit_progress(
        app,
        "asr",
        "installing",
        "正在安装语音识别（安装包内置）",
        None,
        0,
    );
    // 复制二进制；失败（被占用等）就如实报错，让用户重试，不静默降级到 pip
    std::fs::copy(&bin_src, &bin)
        .map_err(|error| format!("复制 whisper-cli 失败（可能被占用，请关闭后重试）：{error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| format!("设置可执行权限失败：{error}"))?;
    }
    // 模型体积大（约 141 MB），已存在且大小一致就跳过复制
    let same = std::fs::metadata(&model)
        .and_then(|meta| std::fs::metadata(&model_src).map(|src| meta.len() == src.len()))
        .unwrap_or(false);
    if !same {
        std::fs::copy(&model_src, &model)
            .map_err(|error| format!("复制 base 模型失败：{error}"))?;
    }

    // whisper.cpp 的模型是文件路径，不是模型名
    persist_settings(app, |settings| {
        settings.asr_executable = Some(bin.clone());
        settings.asr_model = Some(model.display().to_string());
        settings.asr_engine = Some("whisper_cpp".into());
    })?;
    emit_progress(
        app,
        "asr",
        "finished",
        "语音识别已就绪（whisper.cpp）",
        Some(0),
        0,
    );
    crate::logging::log_event(
        "环境",
        format!("ASR 安装完成（内置 whisper.cpp）：{}", bin.display()),
    );
    // Windows 上如果检测到 NVIDIA 显卡，接着把 CPU 版换成 CUDA 版：
    // 先保证有可用结果，再后台提升；失败或取消都不影响已经能用的 CPU 版。
    upgrade_whisper_to_cuda(app);
    Ok(Some(bin.display().to_string()))
}

// ---------------------------------------------------------------------------
// ASR：venv + openai-whisper + base 模型（内置资源缺失时的兜底路径）
// ---------------------------------------------------------------------------

/// ASR 虚拟环境解释器路径（两平台布局不同）。
fn asr_venv_python(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app_data(app)?;
    let rel = if cfg!(target_os = "windows") {
        "asr-env/Scripts/python.exe"
    } else {
        "asr-env/bin/python"
    };
    Ok(dir.join(rel))
}

/// 创建 ASR venv 的基础解释器：Windows 优先应用内 Python（一键运行环境装的 3.11），
/// macOS 依次试 python3 / /usr/bin/python3 / python。
fn asr_base_python(app: &AppHandle) -> Result<PathBuf, String> {
    #[cfg(target_os = "windows")]
    {
        let bundled = app_bin_python(app)?;
        if python_works(&bundled) {
            return Ok(bundled);
        }
    }
    // macOS 不需要应用目录，仅 Windows 用应用内 Python；显式消费避免 unused 告警。
    #[cfg(not(target_os = "windows"))]
    let _ = app;
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &["python", "py"]
    } else {
        &["python3", "/usr/bin/python3", "python"]
    };
    for name in candidates {
        let path = PathBuf::from(name);
        if python_works(&path) {
            return Ok(path);
        }
    }
    Err("未找到可用的 Python 3 解释器：请先在「设置 → 本机环境」完成一键配置运行环境。".into())
}

/// Windows 上应用内 Python 解释器（一键配置运行环境安装到 bin 目录）。
#[cfg(target_os = "windows")]
fn app_bin_python(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(crate::toolbox::app_bin_dir(app)?.join("python.exe"))
}

fn install_asr(app: &AppHandle) -> Result<String, String> {
    let python = asr_venv_python(app)?;
    let root = app_data(app)?;
    let models_dir = root.join("models").join("whisper");
    std::fs::create_dir_all(&models_dir).map_err(|error| format!("创建模型目录失败：{error}"))?;

    // 0) 优先用安装包内置的 whisper.cpp：零下载、无 Python 依赖、Windows 走 CPU、
    //    Apple Silicon 走 Metal。内置缺失（本地开发、旧版安装包）才退回下面的 pip 流程。
    if let Some(done) = install_bundled_whisper(app, &models_dir)? {
        return Ok(done);
    }

    // 1) venv（已存在则跳过创建，直接进入依赖检查）。
    if !python.is_file() {
        let base = asr_base_python(app)?;
        emit_progress(app, "asr", "working", "创建 ASR Python 环境", None, 0);
        let venv_dir = python
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .ok_or_else(|| "无法推断 ASR 虚拟环境目录".to_string())?;
        let spec = dossier_runtime::CommandSpec::new(base.display().to_string())
            .args(["-m", "venv", &venv_dir.display().to_string()])
            .timeout(Duration::from_secs(300));
        dossier_runtime::SystemRunner::new()
            .run(&spec)
            .map_err(|error| format!("创建 ASR 虚拟环境失败：{error}"))?;
        if !python.is_file() {
            return Err(format!(
                "ASR 虚拟环境创建后未找到解释器：{}",
                python.display()
            ));
        }
    }

    // 2) openai-whisper（重跑幂等：已装则 pip 秒回）。
    //
    // 关键：PyPI 默认装的是 **CPU 版 torch**。要用上 NVIDIA 显卡必须显式指定
    // PyTorch 的 CUDA 源（--extra-index-url），否则"有显卡也跑在 CPU 上"。
    // 国内环境再加 PyPI 镜像作为主源，两者是不同域名，任一不通都会失败，
    // 所以源地址集中在这里，失败信息里带上用的是哪个源。
    let wants_cuda = cfg!(target_os = "windows") && crate::toolbox::nvidia_gpu_present();
    let stage = if wants_cuda {
        "下载 ASR 运行时（whisper + CUDA 版 torch，约 1~2GB）"
    } else {
        "下载 ASR 运行时（whisper，含 CPU 版 torch 约 60~200MB）"
    };
    emit_progress(app, "asr", "downloading", stage, None, 0);
    let pip = python
        .parent()
        .map(|dir| {
            dir.join(if cfg!(target_os = "windows") {
                "pip.exe"
            } else {
                "pip"
            })
        })
        .ok_or_else(|| "无法推断 pip 路径".to_string())?;
    let mut spec = dossier_runtime::CommandSpec::new(pip.display().to_string())
        .args([
            "install",
            "--quiet",
            "--disable-pip-version-check",
            // 国内镜像作为主源；镜像不可用时 pip 会自动回落到 PyPI 默认源
            "--index-url",
            PYPI_MIRROR,
        ])
        .timeout(Duration::from_secs(PIP_INSTALL_TIMEOUT_SECS));
    if wants_cuda {
        // CUDA 轮子不在 PyPI 镜像里，必须显式指向 PyTorch 官方源
        spec = spec.args([
            "--extra-index-url",
            PYTORCH_CUDA_INDEX,
            "torch",
            "torchaudio",
        ]);
    }
    spec = spec.arg("openai-whisper");
    let output = dossier_runtime::SystemRunner::new()
        .run(&spec)
        .map_err(|error| format!("安装 openai-whisper 失败（需要网络）：{error}"))?;
    if !output.success() {
        let detail = last_stderr(&output);
        return Err(format!(
            "openai-whisper 安装失败（退出码 {}）：{detail}",
            output.code_label()
        ));
    }
    // 装完核验：装了 CUDA 版就检查 torch 是否真的带 CUDA 标记，不能只看 pip 退出码
    if wants_cuda {
        match installed_torch_variant(&python) {
            Some(variant) if variant.contains("+cu") => {
                crate::logging::log_event("环境", format!("ASR 已启用 GPU：torch {variant}"));
            }
            other => {
                // 不致命：CPU 版仍能跑，只是慢。但要如实告知，不让用户以为在吃显卡
                crate::logging::log_event(
                    "环境",
                    format!(
                        "ASR 未装上 CUDA 版 torch（实际：{:?}），将使用 CPU 模式",
                        other.unwrap_or_else(|| "未知".into())
                    ),
                );
            }
        }
    }

    // 3) base 模型：load_model 下到应用目录，设置里用绝对路径加载（不占用户缓存）。
    let model_path = models_dir.join("base.pt");
    if !model_path.is_file() {
        emit_progress(
            app,
            "asr",
            "downloading",
            "下载 base 模型（约 140MB）",
            None,
            0,
        );
        let snippet = format!(
            "import whisper; whisper.load_model('base', download_root=r'{}')",
            models_dir.display()
        );
        let spec = dossier_runtime::CommandSpec::new(python.display().to_string())
            .args(["-c", &snippet])
            .timeout(Duration::from_secs(MODEL_DOWNLOAD_TIMEOUT_SECS));
        let output = dossier_runtime::SystemRunner::new()
            .run(&spec)
            .map_err(|error| format!("下载 base 模型失败：{error}"))?;
        if !output.success() || !model_path.is_file() {
            let detail = last_stderr(&output);
            return Err(format!(
                "base 模型下载失败（退出码 {}）：{detail}",
                output.code_label()
            ));
        }
    }

    // 4) 写设置：CLI 可执行文件 + 模型绝对路径 + 引擎 openai。
    let whisper_cli = python
        .parent()
        .map(|dir| {
            dir.join(if cfg!(target_os = "windows") {
                "whisper.exe"
            } else {
                "whisper"
            })
        })
        .ok_or_else(|| "无法推断 whisper 命令行路径".to_string())?;
    if !whisper_cli.is_file() {
        return Err(format!("whisper 命令行未找到：{}", whisper_cli.display()));
    }
    persist_settings(app, |settings| {
        settings.asr_executable = Some(whisper_cli.clone());
        settings.asr_model = Some(model_path.display().to_string());
        settings.asr_engine = Some("openai".into());
    })?;
    crate::logging::log_event("环境", "ASR 安装完成（openai-whisper + base 模型）");
    Ok(whisper_cli.display().to_string())
}

// ---------------------------------------------------------------------------
// OCR：Homebrew（macOS）/ UB-Mannheim 静默安装（Windows）
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn install_ocr(app: &AppHandle) -> Result<String, String> {
    // GUI 进程的 PATH 很干净，brew 用绝对路径调，并补上 brew 依赖的系统目录。
    let brew = ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"]
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .ok_or_else(|| {
            "未找到 Homebrew。请先安装 Homebrew（https://brew.sh），\
             或手动执行 brew install tesseract tesseract-lang。"
                .to_string()
        })?;
    emit_progress(
        app,
        "ocr",
        "downloading",
        "Homebrew 安装 tesseract 与中文语言包（首次约 1~3 分钟）",
        None,
        0,
    );
    let mut spec = dossier_runtime::CommandSpec::new(brew.display().to_string())
        .args(["install", "--quiet", "tesseract", "tesseract-lang"])
        .timeout(Duration::from_secs(BREW_INSTALL_TIMEOUT_SECS));
    spec.env = vec![
        (
            "PATH".into(),
            "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".into(),
        ),
        ("HOMEBREW_NO_AUTO_UPDATE".into(), "1".into()),
        ("HOMEBREW_NO_INSTALL_CLEANUP".into(), "1".into()),
        // 国内网络直连 raw.githubusercontent.com 常被重置，formula 拉不下来；
        // 走清华镜像（bottle 在 ghcr.io 反而可达，但 API 源必须镜像）。
        (
            "HOMEBREW_API_DOMAIN".into(),
            "https://mirrors.tuna.tsinghua.edu.cn/homebrew-bottles/api".into(),
        ),
        (
            "HOMEBREW_BOTTLE_DOMAIN".into(),
            "https://mirrors.tuna.tsinghua.edu.cn/homebrew-bottles".into(),
        ),
    ];
    let output = dossier_runtime::SystemRunner::new()
        .run(&spec)
        .map_err(|error| format!("Homebrew 执行失败：{error}"))?;
    if !output.success() {
        let detail = last_stderr(&output);
        return Err(format!(
            "Homebrew 安装 tesseract 失败（退出码 {}）：{detail}",
            output.code_label()
        ));
    }
    let tesseract = ["/opt/homebrew/bin/tesseract", "/usr/local/bin/tesseract"]
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .ok_or_else(|| "安装完成但未找到 tesseract 可执行文件".to_string())?;
    persist_settings(app, |settings| {
        settings.ocr_executable = Some(tesseract.clone());
    })?;
    crate::logging::log_event(
        "环境",
        "OCR 安装完成（Homebrew tesseract + tesseract-lang）",
    );
    Ok(tesseract.display().to_string())
}

#[cfg(target_os = "windows")]
fn install_ocr(app: &AppHandle) -> Result<String, String> {
    let dir = app_data(app)?.join("tesseract");
    std::fs::create_dir_all(&dir).map_err(|error| format!("创建 OCR 目录失败：{error}"))?;
    let tesseract = dir.join("tesseract.exe");

    if !tesseract.is_file() {
        let setup = dir.join("tesseract-setup.exe");
        let mut last_error = String::from("（未尝试）");
        for source in TESSERACT_SETUP_SOURCES {
            match crate::toolbox::download_file(app, "ocr", source, &setup) {
                Ok(()) => break,
                Err(error) => last_error = error,
            }
        }
        if !setup.is_file() {
            return Err(format!("tesseract 安装器下载失败：{last_error}"));
        }
        emit_progress(
            app,
            "ocr",
            "extracting",
            "静默安装 tesseract（约 1~2 分钟）",
            None,
            0,
        );
        use std::os::windows::process::CommandExt;
        // /CURRENTUSER 走每用户安装：不需要管理员授权，UAC 被拒也能装完。
        let mut command = std::process::Command::new(&setup);
        command
            .args([
                "/VERYSILENT",
                "/SUPPRESSMSGBOXES",
                "/NORESTART",
                "/CURRENTUSER",
                &format!("/DIR={}", dir.display()),
            ])
            .creation_flags(0x0800_0000);
        let status = command
            .status()
            .map_err(|error| format!("启动 tesseract 安装器失败：{error}"))?;
        let _ = std::fs::remove_file(&setup);
        if !status.success() {
            return Err(format!(
                "tesseract 静默安装未完成（退出码 {:?}）",
                status.code()
            ));
        }
        // 5.4.0 安装器会忽略 /DIR 落到默认目录：把文件搬回应用目录，
        // 保持「安装目标都在应用数据目录」的布局（含管理员安装落 Program Files 的情形）。
        if !tesseract.is_file() {
            if let Some(root) = locate_installed_tesseract() {
                copy_dir_contents(&root, &dir)
                    .map_err(|error| format!("搬运 tesseract 文件失败：{error}"))?;
            }
        }
        if !tesseract.is_file() {
            return Err("tesseract 静默安装后未找到可执行文件".to_string());
        }
    }

    // 安装器默认组件不含中文：补下 chi_sim 语言包到安装目录的 tessdata。
    let tessdata = dir.join("tessdata");
    std::fs::create_dir_all(&tessdata).map_err(|error| format!("创建语言包目录失败：{error}"))?;
    let chi_sim = tessdata.join("chi_sim.traineddata");
    if !chi_sim.is_file() {
        let mut last_error = String::from("（未尝试）");
        for source in CHI_SIM_SOURCES {
            match crate::toolbox::download_file(app, "ocr", source, &chi_sim) {
                Ok(()) => break,
                Err(error) => last_error = error,
            }
        }
        if !chi_sim.is_file() {
            return Err(format!("chi_sim 语言包下载失败：{last_error}"));
        }
    }

    persist_settings(app, |settings| {
        settings.ocr_executable = Some(tesseract.clone());
    })?;
    crate::logging::log_event("环境", "OCR 安装完成（UB-Mannheim tesseract + chi_sim）");
    Ok(tesseract.display().to_string())
}

/// 安装器忽略 /DIR 时可能落位的目录（每用户默认 / 管理员默认）。
#[cfg(target_os = "windows")]
fn locate_installed_tesseract() -> Option<PathBuf> {
    let local = std::env::var("LOCALAPPDATA").ok()?;
    [
        PathBuf::from(local).join("Programs").join("Tesseract-OCR"),
        PathBuf::from(r"C:\Program Files\Tesseract-OCR"),
    ]
    .into_iter()
    .find(|root| root.join("tesseract.exe").is_file())
}

/// 递归复制目录内容到目标目录（跳过卸载器文件，源目录本身不复制）。
#[cfg(target_os = "windows")]
fn copy_dir_contents(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with("unins") {
            continue;
        }
        let target = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_contents(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn install_ocr(_app: &AppHandle) -> Result<String, String> {
    Err("OCR 自动安装目前仅支持 macOS / Windows，请手动安装 tesseract（含 chi_sim）。".into())
}

// ---------------------------------------------------------------------------
// 主入口
// ---------------------------------------------------------------------------

fn install_content_models_blocking(app: &AppHandle) -> Result<ContentModelsReport, String> {
    let _ = read_settings(app)?; // 设置损坏时提前失败，避免装完写不回。
    let mut report = ContentModelsReport {
        asr_path: None,
        ocr_path: None,
        messages: Vec::new(),
        note: String::new(),
    };
    emit_progress(
        app,
        "asr",
        "connecting",
        "开始安装 ASR（openai-whisper）",
        None,
        0,
    );
    match install_asr(app) {
        Ok(path) => report.asr_path = Some(path),
        Err(error) => {
            crate::logging::log_event("环境", format!("ASR 一键安装失败：{error}"));
            report.messages.push(format!("ASR 安装失败：{error}"));
        }
    }
    emit_progress(
        app,
        "ocr",
        "connecting",
        "开始安装 OCR（tesseract）",
        None,
        0,
    );
    match install_ocr(app) {
        Ok(path) => report.ocr_path = Some(path),
        Err(error) => {
            crate::logging::log_event("环境", format!("OCR 一键安装失败：{error}"));
            report.messages.push(format!("OCR 安装失败：{error}"));
        }
    }
    emit_progress(app, "ocr", "finished", "内容模型安装流程结束", Some(0), 0);
    if report.asr_path.is_some() && report.ocr_path.is_some() {
        report.note =
            "ASR 与 OCR 均已就绪：到「设置 → 本机环境」点「重新检测」后即可启用材料线程。".into();
    } else if !report.messages.is_empty() {
        report.note = "部分组件安装失败，请按提示处理或手动安装。".into();
    }
    Ok(report)
}

/// IPC：一键安装内容模型运行时（ASR + OCR）。
#[tauri::command]
pub async fn install_content_models(app: AppHandle) -> Result<ContentModelsReport, String> {
    crate::logging::log_event("环境", "开始一键安装内容模型（ASR/OCR）");
    let result =
        tauri::async_runtime::spawn_blocking(move || install_content_models_blocking(&app))
            .await
            .map_err(|error| format!("内容模型安装任务失败：{error}"))?;
    crate::logging::log_event(
        "环境",
        crate::logging::result_text(&result, |report| {
            format!(
                "内容模型安装完成：asr={:?} ocr={:?} messages={:?}",
                report.asr_path, report.ocr_path, report.messages
            )
        }),
    );
    result
}
