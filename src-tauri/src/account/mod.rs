//! 账号与授权：连服务端的真实账号，替换掉原来的本机演示账号。
//!
//! - [`api`]：HTTP 客户端；
//! - [`store`]：登录状态的本地存储（系统钥匙串 / Linux 文件）；
//! - [`session`]：会话、刷新令牌的单飞、授权凭证、三档判定；
//! - [`gate`]：命令闸门；
//! - [`commands`]：Tauri 命令。
//!
//! 纯逻辑（验签、防时钟倒退、使用权判定、机器码）在 `dossier-license`。

use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};

pub mod api;
pub mod commands;
pub mod gate;
pub mod server_jobs;
pub mod session;
pub mod store;

use api::HttpTransport;
use session::{Session, Setup};
use store::PlatformStore;

pub use gate::gate;

#[cfg(test)]
mod e2e_tests;

pub type AccountState = Session<HttpTransport, PlatformStore>;

/// 账号状态变化时发给前端的事件。
pub const EVENT: &str = "account-changed";

/// 在 Tauri 的 `setup` 里调用：建好会话、清理旧版演示账号文件、启动恢复和后台刷新。
pub fn init(app: &AppHandle) {
    let transport = HttpTransport::new();
    let data_dir = app.path().app_data_dir();
    let setup = build_setup(app, &transport, data_dir.is_ok());
    // 数据目录定位不了时 setup 是错误，会话不会碰存储，这里的占位路径不会被写入。
    let root = data_dir.map_or_else(|_| std::env::temp_dir(), |dir| dir.join("credentials"));
    app.manage(Session::new(
        transport,
        PlatformStore::open(root),
        setup,
        dossier_license::unix_now,
    ));
    remove_legacy_account(app);
    spawn_loop(app.clone());
}

fn build_setup(
    app: &AppHandle,
    transport: &HttpTransport,
    data_dir_ok: bool,
) -> Result<Setup, String> {
    if let Some(problem) = transport.problem() {
        return Err(problem.to_owned());
    }
    if !data_dir_ok {
        return Err("无法定位应用数据目录，请检查系统权限".into());
    }
    let identity = dossier_license::read_device_id().map_err(|error| error.to_string())?;
    let keys = dossier_license::trusted_keys()
        .map_err(|error| error.to_string())?
        .to_vec();
    Ok(Setup {
        identity,
        keys,
        app_version: app.package_info().version.to_string(),
    })
}

/// 旧版本地演示账号文件存的是弱哈希的密码，留着没用还有风险，新版首次启动时删除。
fn remove_legacy_account(app: &AppHandle) {
    let Ok(dir) = app.path().app_config_dir() else {
        return;
    };
    let path = dir.join("account.json");
    if path.exists() {
        match std::fs::remove_file(&path) {
            Ok(()) => eprintln!("[account] 已清理旧版本地演示账号文件"),
            Err(error) => eprintln!("[account] 清理旧版本地演示账号文件失败：{error}"),
        }
    }
}

/// 发一次 `account-changed`，返回发出去的状态。
pub fn emit(app: &AppHandle, account: &AccountState) -> session::StatusPayload {
    let status = account.status();
    let _ = app.emit(EVENT, &status);
    status
}

/// 启动恢复，然后定时刷新授权；最长 5 分钟醒一次，让离线宽限期到点时界面及时变成锁定。
fn spawn_loop(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let account = app.state::<AccountState>();
        account.restore().await;
        emit(&app, &account);
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(account.sleep_secs())) => {}
                _ = account.wake.notified() => {}
            }
            if account.sync_due() {
                account.sync().await;
            }
            emit(&app, &account);
        }
    });
}
