//! 内容模型运行时一键安装：ASR（openai-whisper）与 OCR（tesseract）。
//!
//! 与 `install_runtime_blocking`（ffmpeg/ffprobe）同一套交互：后台阻塞执行、
//! `install-progress` 事件报进度、装完写回设置。安装目标都在应用数据目录，
//! 不碰系统 PATH：
//! - ASR：`<app_data>/asr-env` 虚拟环境装 openai-whisper，base 模型下到
//!   `<app_data>/models/whisper/base.pt`（CLI 用绝对路径加载，不落用户缓存）；
//! - OCR（macOS）：Homebrew 装 `tesseract` + `tesseract-lang`（含 chi_sim）；
//! - OCR（Windows）：UB-Mannheim 静默安装器装进 `<app_data>/tesseract`，
//!   再补下 chi_sim 语言包（安装器默认组件不含中文）。
//!
//! 每个组件独立软失败：一边失败不影响另一边的成果，错误进报告 messages。

use std::path::{Path, PathBuf};
use std::time::Duration;

use dossier_core::{save_settings, AppSettings};
use dossier_runtime::CommandRunner;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::toolbox::read_settings;

/// Windows 用 UB-Mannheim 的 tesseract 安装器（Inno Setup，静默参数官方支持）。
/// 钉住已知可用版本；升级时换这里的版本号。
#[cfg(target_os = "windows")]
const TESSERACT_SETUP_URL: &str = "https://github.com/UB-Mannheim/tesseract/releases/download/v5.4.0.20240606/tesseract-ocr-w64-setup-5.4.0.20240606.exe";

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
    let mut loaded = dossier_core::load_settings(&path);
    update(&mut loaded.settings);
    save_settings(&path, &loaded.settings).map_err(|error| format!("写设置失败：{error}"))
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
// ASR：venv + openai-whisper + base 模型
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
    emit_progress(
        app,
        "asr",
        "downloading",
        "下载 ASR 运行时（whisper，含 torch 约 60~200MB）",
        None,
        0,
    );
    let pip = python
        .parent()
        .map(|dir| dir.join(if cfg!(target_os = "windows") { "pip.exe" } else { "pip" }))
        .ok_or_else(|| "无法推断 pip 路径".to_string())?;
    let spec = dossier_runtime::CommandSpec::new(pip.display().to_string())
        .args(["install", "--quiet", "--disable-pip-version-check", "openai-whisper"])
        .timeout(Duration::from_secs(PIP_INSTALL_TIMEOUT_SECS));
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

    // 3) base 模型：load_model 下到应用目录，设置里用绝对路径加载（不占用户缓存）。
    let model_path = models_dir.join("base.pt");
    if !model_path.is_file() {
        emit_progress(app, "asr", "downloading", "下载 base 模型（约 140MB）", None, 0);
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
        ("PATH".into(), "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".into()),
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
    crate::logging::log_event("环境", "OCR 安装完成（Homebrew tesseract + tesseract-lang）");
    Ok(tesseract.display().to_string())
}

#[cfg(target_os = "windows")]
fn install_ocr(app: &AppHandle) -> Result<String, String> {
    let dir = app_data(app)?.join("tesseract");
    std::fs::create_dir_all(&dir).map_err(|error| format!("创建 OCR 目录失败：{error}"))?;
    let tesseract = dir.join("tesseract.exe");

    if !tesseract.is_file() {
        let setup = dir.join("tesseract-setup.exe");
        crate::toolbox::download_file(app, "ocr", TESSERACT_SETUP_URL, &setup)?;
        emit_progress(app, "ocr", "extracting", "静默安装 tesseract（约 1~2 分钟）", None, 0);
        use std::os::windows::process::CommandExt;
        let mut command = std::process::Command::new(&setup);
        command
            .args([
                "/VERYSILENT",
                "/SUPPRESSMSGBOXES",
                "/NORESTART",
                &format!("/DIR={}", dir.display()),
            ])
            .creation_flags(0x0800_0000);
        let status = command
            .status()
            .map_err(|error| format!("启动 tesseract 安装器失败：{error}"))?;
        let _ = std::fs::remove_file(&setup);
        if !status.success() || !tesseract.is_file() {
            return Err(format!(
                "tesseract 静默安装未完成（退出码 {:?}）",
                status.code()
            ));
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
    emit_progress(app, "asr", "connecting", "开始安装 ASR（openai-whisper）", None, 0);
    match install_asr(app) {
        Ok(path) => report.asr_path = Some(path),
        Err(error) => {
            crate::logging::log_event("环境", format!("ASR 一键安装失败：{error}"));
            report.messages.push(format!("ASR 安装失败：{error}"));
        }
    }
    emit_progress(app, "ocr", "connecting", "开始安装 OCR（tesseract）", None, 0);
    match install_ocr(app) {
        Ok(path) => report.ocr_path = Some(path),
        Err(error) => {
            crate::logging::log_event("环境", format!("OCR 一键安装失败：{error}"));
            report.messages.push(format!("OCR 安装失败：{error}"));
        }
    }
    emit_progress(app, "ocr", "finished", "内容模型安装流程结束", Some(0), 0);
    if report.asr_path.is_some() && report.ocr_path.is_some() {
        report.note = "ASR 与 OCR 均已就绪：到「设置 → 本机环境」点「重新检测」后即可启用材料线程。".into();
    } else if !report.messages.is_empty() {
        report.note = "部分组件安装失败，请按提示处理或手动安装。".into();
    }
    Ok(report)
}

/// IPC：一键安装内容模型运行时（ASR + OCR）。
#[tauri::command]
pub async fn install_content_models(app: AppHandle) -> Result<ContentModelsReport, String> {
    crate::logging::log_event("环境", "开始一键安装内容模型（ASR/OCR）");
    let result = tauri::async_runtime::spawn_blocking(move || install_content_models_blocking(&app))
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
