//! 功能闸门：在 Rust 端按命令名放行或拒绝，不只靠界面。前端被改了也绕不过去。
//!
//! 包在 `invoke_handler` 外面，所有应用命令都先经过这里（插件命令走另一条路，不经过这里）。
//! 没登记的命令一律按「本地功能」处理：以后新加命令忘了登记，默认是收紧而不是放开。

use std::sync::Arc;

use serde::Serialize;
use tauri::ipc::Invoke;
use tauri::{Manager, Wry};

use super::session::Evaluation;
use super::AccountState;

/// 命令需要的档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    /// 任何时候都能用：账号、设置读写、选目录、打开目录，以及只读的本机信息。
    Open,
    /// 联网或离线档能用。只在开始时检查一次，所以断网前已经开始的任务会继续跑完。
    Local,
    /// 必须联网：执行前联网刷新一次授权。
    Paid,
}

pub fn need(command: &str) -> Need {
    match command {
        "account_status"
        | "account_retry"
        | "account_send_code"
        | "account_register"
        | "account_login"
        | "account_logout"
        | "account_reset_password"
        | "account_change_password"
        | "license_redeem"
        | "license_refresh"
        | "device_list"
        | "device_unbind"
        | "device_activate"
        | "account_export"
        | "account_delete"
        | "load_app_settings"
        | "save_app_settings"
        | "open_path"
        | "pick_folder"
        | "ledger_list"
        | "runtime_status"
        | "platform_profiles" => Need::Open,
        "generate_package" => Need::Paid,
        _ => Need::Local,
    }
}

/// 被闸门拒绝时返回给前端的错误。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LockedError {
    /// 固定为 `license_locked`，前端据此弹出对应的提示。
    error: &'static str,
    /// 锁定原因（见 `LockReason::code`）；付费步骤遇到离线档时是 `needs_online`。
    reason: &'static str,
    message: String,
}

fn locked_error(evaluation: &Evaluation) -> LockedError {
    match evaluation.tier {
        dossier_license::Tier::Locked(reason) => LockedError {
            error: "license_locked",
            reason: reason.code(),
            message: evaluation
                .message
                .clone()
                .unwrap_or_else(|| reason.message().to_owned()),
        },
        _ => LockedError {
            error: "license_locked",
            reason: "needs_online",
            message: "这一步需要联网：连不上服务端，请联网后重试".into(),
        },
    }
}

/// 给 `tauri::generate_handler!` 生成的处理函数套上闸门。
pub fn gate<H>(handler: H) -> impl Fn(Invoke<Wry>) -> bool + Send + Sync + 'static
where
    H: Fn(Invoke<Wry>) -> bool + Send + Sync + 'static,
{
    let handler = Arc::new(handler);
    move |invoke: Invoke<Wry>| match need(invoke.message.command()) {
        Need::Open => handler(invoke),
        Need::Local => {
            let webview = invoke.message.webview();
            let verdict = webview.state::<AccountState>().check_local();
            match verdict {
                Ok(()) => handler(invoke),
                Err(evaluation) => {
                    invoke.resolver.reject(locked_error(&evaluation));
                    true
                }
            }
        }
        Need::Paid => {
            let handler = Arc::clone(&handler);
            let app = invoke.message.webview().app_handle().clone();
            tauri::async_runtime::spawn(async move {
                let account = app.state::<AccountState>();
                match account.check_paid().await {
                    // 付费步骤本身是同步命令（会跑很久），放到阻塞线程上执行，不占异步线程。
                    Ok(()) => {
                        let _ = tauri::async_runtime::spawn_blocking(move || handler(invoke)).await;
                    }
                    Err(evaluation) => invoke.resolver.reject(locked_error(&evaluation)),
                }
            });
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dossier_license::{LockReason, Tier};

    /// 生成 `generate_handler!` 列表里每个应用命令的名字，和 `lib.rs` 对齐。
    /// 新加命令时这里不改也会被归到「本地」（默认收紧），这个测试只钉住已知命令的档位。
    #[test]
    fn known_commands_are_classified() {
        for open in [
            "account_status",
            "account_login",
            "account_logout",
            "account_delete",
            "account_export",
            "license_redeem",
            "license_refresh",
            "device_list",
            "device_unbind",
            "device_activate",
            "load_app_settings",
            "save_app_settings",
            "open_path",
            "pick_folder",
            "ledger_list",
            "runtime_status",
            "platform_profiles",
        ] {
            assert_eq!(need(open), Need::Open, "{open}");
        }
        assert_eq!(need("generate_package"), Need::Paid);
        for local in [
            "scan_library",
            "analyze_drama",
            "run_erase",
            "transcode_execute",
            "jianying_detect",
            "import_draft",
        ] {
            assert_eq!(need(local), Need::Local, "{local}");
        }
    }

    #[test]
    fn unknown_commands_default_to_local() {
        assert_eq!(need("some_future_command"), Need::Local);
        assert_eq!(need(""), Need::Local);
    }

    #[test]
    fn locked_error_shape() {
        let locked = Evaluation {
            tier: Tier::Locked(LockReason::NoEntitlement),
            renew_soon: false,
            message: None,
        };
        let value = serde_json::to_value(locked_error(&locked)).unwrap();
        assert_eq!(value["error"], "license_locked");
        assert_eq!(value["reason"], "no_entitlement");
        assert_eq!(value["message"], LockReason::NoEntitlement.message());

        let offline = Evaluation {
            tier: Tier::Offline { until: 0 },
            renew_soon: false,
            message: None,
        };
        let value = serde_json::to_value(locked_error(&offline)).unwrap();
        assert_eq!(value["reason"], "needs_online");
    }
}
