//! 应用升级：检查 GitHub Release 上的新版本并提示；用户点「立即升级」后才下载、验签、安装、重启。
//!
//! 信任链：
//! - 版本清单 `latest.json` 与安装包都在 GitHub Release 上，地址写在 `tauri.conf.json` 的
//!   `plugins.updater.endpoints`，插件强制 HTTPS。
//! - 安装包由 CI 用升级私钥签名；客户端内置公钥（`plugins.updater.pubkey`），验签不过就拒绝安装，
//!   这一步由插件强制执行、无法关闭。
//! - `requireSignedVersion`：签名里记录了它是哪个版本的包，清单宣称的版本必须与之一致。
//!   清单本身没有签名，没有这一条时，篡改清单可以把旧版本的合法包冒充成新版本推给用户。
//! - 插件默认只接受比当前更高的版本，不会被诱导降级。
//!
//! 前端不直接调用插件（capabilities 里没有 updater 权限），只能调用本模块的两个命令：
//! 检查（只提示，不下载）与安装（用户确认后才下载）。
//!
//! 安装会关闭软件（Windows 由安装器接管并退出本进程），所以有长任务在跑（转码、擦除、
//! 生成报审包等）时不能装。下载可能要几分钟，期间用户仍可能开始新任务，因此检查做两次：
//! 下载前一次（免得白下），下载完、真正安装前再一次。第二次被拦下时，已验签的安装包留在
//! 内存里，等任务结束后重试直接安装，不用重新下载。

use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::logging::log_event;
use crate::tasks;

/// 升级状态：最近一次检查到的更新，以及已下载并验签过的安装包。
#[derive(Default)]
pub struct PendingUpdate {
    /// 安装时用它，避免「检查时是 A 版、安装时清单已变成 B 版」。
    update: Mutex<Option<Update>>,
    /// 已下载且通过签名校验的安装包（版本号, 字节）。只在同一版本重试安装时复用。
    /// 用 `Arc` 共享：安装包可能一两百 MB，缓存与本次安装用的是同一份内存，不复制。
    downloaded: Mutex<Option<(String, Arc<Vec<u8>>)>>,
    /// 安装进行中：防止重复点击并发下载两份。
    installing: Mutex<bool>,
}

/// 返回给前端的更新信息（只含展示需要的字段）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub current_version: String,
    pub version: String,
    pub notes: Option<String>,
    pub date: Option<String>,
}

/// 升级进度事件 `app-update-progress` 的载荷。
///
/// `stage`：`downloading`（下载中）→ `verifying`（下载完，插件正在验签）→ `installing`（启动安装）；
/// 复用已下载的安装包时直接从 `installing` 开始。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct UpdateProgress {
    stage: &'static str,
    downloaded: u64,
    total: Option<u64>,
}

fn info_of(update: &Update) -> UpdateInfo {
    UpdateInfo {
        current_version: update.current_version.clone(),
        version: update.version.clone(),
        notes: update
            .body
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string),
        date: update.date.map(|date| date.date().to_string()),
    }
}

async fn fetch_update(app: &AppHandle) -> Result<Option<Update>, String> {
    let updater = app
        .updater()
        .map_err(|error| format!("升级组件初始化失败：{error}"))?;
    updater
        .check()
        .await
        .map_err(|error| format!("检查更新失败：{error}"))
}

/// IPC：检查是否有新版本（只查清单，不下载）。没有新版本返回 `null`。
///
/// 失败（断网、GitHub 访问不了、清单格式不对）返回错误文本，由前端决定是否打扰用户：
/// 自动检查时只记日志，手动点「检查更新」时显示原因。
#[tauri::command]
pub async fn app_update_check(
    app: AppHandle,
    pending: State<'_, PendingUpdate>,
) -> Result<Option<UpdateInfo>, String> {
    let current = app.package_info().version.to_string();
    let result = fetch_update(&app).await;
    match &result {
        Ok(Some(update)) => log_event(
            "升级",
            format!("发现新版本：{current} → {}", update.version),
        ),
        Ok(None) => log_event("升级", format!("已是最新版本（{current}）")),
        Err(error) => log_event("升级", format!("检查失败（当前 {current}）：{error}")),
    }
    let update = result?;
    let info = update.as_ref().map(info_of);
    let version = update.as_ref().map(|update| update.version.clone());
    *lock(&pending.update) = update;
    // 清单换了版本（或已无更新）：之前下载的安装包不再对应，丢掉
    let mut downloaded = lock(&pending.downloaded);
    let still_valid = matches!(
        (downloaded.as_ref(), version.as_ref()),
        (Some((cached, _)), Some(latest)) if cached == latest
    );
    if !still_valid {
        *downloaded = None;
    }
    Ok(info)
}

/// 正在运行、会被升级退出打断的任务；为空才允许安装。
fn blocking_tasks() -> Vec<&'static str> {
    tasks::running_kinds()
        .into_iter()
        .map(tasks::TaskKind::label)
        .collect()
}

fn busy_message(running: &[&str], downloaded: bool) -> String {
    let head = if downloaded {
        "新版本已下载并通过签名校验，但"
    } else {
        ""
    };
    let tail = if downloaded {
        "请等它完成（或先停止）后点「重试」，会直接安装，不用重新下载。"
    } else {
        "请等它完成（或先停止）后再升级。"
    };
    format!(
        "{head}{} 正在进行，升级会关闭软件并打断它。{tail}",
        running.join("、")
    )
}

/// 有长任务在跑就返回错误。
fn ensure_idle(downloaded: bool) -> Result<(), String> {
    let running = blocking_tasks();
    if running.is_empty() {
        Ok(())
    } else {
        Err(busy_message(&running, downloaded))
    }
}

/// IPC：下载（或复用已下载的）安装包，验签后安装并重启。用户在弹窗里点「立即升级」才会调用。
///
/// 进度通过事件 `app-update-progress` 推送。成功时不会返回：Windows 上安装器启动后插件直接
/// 退出本进程；macOS 装完后由这里重启。
#[tauri::command]
pub async fn app_update_install(
    app: AppHandle,
    pending: State<'_, PendingUpdate>,
) -> Result<(), String> {
    if cfg!(debug_assertions) {
        // 开发构建覆盖安装会把开发机上的正式版换掉，且开发构建本身就不是要分发的东西
        return Err("开发构建不执行升级安装；请用正式安装包验证升级。".to_string());
    }
    {
        let mut installing = lock(&pending.installing);
        if *installing {
            return Err("升级已在进行中，请稍候。".to_string());
        }
        *installing = true;
    }
    let result = install(&app, &pending).await;
    *lock(&pending.installing) = false;
    if let Err(error) = &result {
        log_event("升级", format!("安装未完成：{error}"));
    }
    result
}

async fn install(app: &AppHandle, pending: &PendingUpdate) -> Result<(), String> {
    let cached = lock(&pending.update).clone();
    let update = match cached {
        Some(update) => update,
        // 没先检查（或检查结果已被清空）：现在查一次
        None => fetch_update(app)
            .await?
            .ok_or_else(|| "已是最新版本，无需升级。".to_string())?,
    };

    let reused = {
        let downloaded = lock(&pending.downloaded);
        match downloaded.as_ref() {
            Some((version, bytes)) if *version == update.version => Some(Arc::clone(bytes)),
            _ => None,
        }
    };
    let bytes = match reused {
        Some(bytes) => {
            log_event("升级", format!("复用已下载的 {} 安装包", update.version));
            bytes
        }
        None => {
            // 第一次检查：有任务在跑就别开始下载
            ensure_idle(false)?;
            log_event(
                "升级",
                format!("开始下载：{} → {}", update.current_version, update.version),
            );
            let bytes = Arc::new(download(app, &update).await?);
            *lock(&pending.downloaded) = Some((update.version.clone(), Arc::clone(&bytes)));
            bytes
        }
    };

    // 第二次检查：下载期间用户可能开始了新任务。安装包已缓存，等任务结束重试即可。
    ensure_idle(true)?;
    emit(
        app,
        "installing",
        bytes.len() as u64,
        Some(bytes.len() as u64),
    );
    log_event("升级", format!("开始安装 {}，安装后重启", update.version));
    // install 内部会再验一次签名；Windows 上它启动安装器后直接退出进程，不会返回
    update
        .install(bytes.as_slice())
        .map_err(|error| format!("安装新版本失败：{error}"))?;
    app.restart();
}

/// 下载安装包。插件在 `download` 返回前已用内置公钥验签，验不过直接报错。
async fn download(app: &AppHandle, update: &Update) -> Result<Vec<u8>, String> {
    let mut downloaded: u64 = 0;
    let progress_app = app.clone();
    let finish_app = app.clone();
    update
        .download(
            move |chunk, total| {
                downloaded += chunk as u64;
                emit(&progress_app, "downloading", downloaded, total);
            },
            move || emit(&finish_app, "verifying", 0, None),
        )
        .await
        .map_err(|error| format!("下载或校验新版本失败：{error}"))
}

fn emit(app: &AppHandle, stage: &'static str, downloaded: u64, total: Option<u64>) {
    let _ = app.emit(
        "app-update-progress",
        UpdateProgress {
            stage,
            downloaded,
            total,
        },
    );
}
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // 锁中毒只说明持锁线程 panic 了；这里的状态是整体替换，不会半写。
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// 注册插件与状态。插件读取 `tauri.conf.json` 的 `plugins.updater` 配置。
pub fn init<R: tauri::Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(PendingUpdate::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_message_names_the_running_tasks() {
        let before = busy_message(&["转码", "擦除"], false);
        assert!(before.contains("转码、擦除"));
        assert!(before.contains("再升级"));
        assert!(!before.contains("已下载"));
        // 下载完才发现有任务：要告诉用户不用重新下载
        let after = busy_message(&["转码"], true);
        assert!(after.starts_with("新版本已下载并通过签名校验"));
        assert!(after.contains("不用重新下载"));
    }

    #[test]
    fn running_task_blocks_install() {
        let guard = tasks::register(tasks::TaskKind::Package);
        assert!(blocking_tasks().contains(&"生成报审包"));
        assert!(ensure_idle(true).unwrap_err().contains("生成报审包"));
        drop(guard);
    }

    #[test]
    fn progress_payload_shape() {
        let value = serde_json::to_value(UpdateProgress {
            stage: "downloading",
            downloaded: 10,
            total: Some(100),
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "stage": "downloading", "downloaded": 10, "total": 100 })
        );
    }
}

#[cfg(test)]
mod config_tests {
    /// tauri.conf.json 的 `plugins.updater` 必须能被插件自己的 Config 解析：
    /// 写错字段名或类型，插件初始化失败，应用启动即崩。顺带锁住几条安全相关的取值。
    #[test]
    fn updater_config_parses_with_plugin_schema() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let raw = conf["plugins"]["updater"].clone();
        let config: tauri_plugin_updater::Config = serde_json::from_value(raw).unwrap();
        assert!(!config.pubkey.trim().is_empty());
        assert!(
            config.require_signed_version,
            "必须开启 requireSignedVersion"
        );
        assert!(!config.allow_downgrades);
        assert!(!config.dangerous_insecure_transport_protocol);
        assert!(!config.dangerous_accept_invalid_certs);
        assert!(!config.endpoints.is_empty());
        assert!(config.endpoints.iter().all(|url| url.scheme() == "https"));
        assert_eq!(conf["bundle"]["createUpdaterArtifacts"], true);
    }
}
