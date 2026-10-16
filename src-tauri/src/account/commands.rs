//! 账号相关的 Tauri 命令。每个改变状态的命令结束后都发一次 `account-changed`。
//!
//! 失败时返回 [`CommandError`]：`{error, message, retryAfter?, ...附加字段}`，前端按 `error` 分支，`message` 直接展示。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, State};

use super::api::{ApiError, DeviceList};
use super::session::StatusPayload;
use super::{emit, AccountState};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandError {
    error: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after: Option<u64>,
    /// 服务端附加的字段（如 `device_limit` 带的设备列表、`unbind_quota`），原样转给前端。
    #[serde(skip_serializing_if = "Value::is_null")]
    extra: Value,
}

impl From<ApiError> for CommandError {
    fn from(error: ApiError) -> Self {
        let message = error.message();
        match error {
            ApiError::Server {
                code,
                retry_after,
                extra,
                ..
            } => {
                // 参数校验失败时，把各字段的中文说明拼起来，比笼统的「请求参数不正确」好懂。
                let message = field_messages(&extra).unwrap_or(message);
                CommandError {
                    error: code,
                    message,
                    retry_after,
                    extra,
                }
            }
            other => CommandError {
                error: other.code().to_owned(),
                message,
                retry_after: None,
                extra: Value::Null,
            },
        }
    }
}

fn field_messages(extra: &Value) -> Option<String> {
    let fields = extra.get("fields")?.as_array()?;
    let messages: Vec<&str> = fields
        .iter()
        .filter_map(|field| field.get("message").and_then(Value::as_str))
        .collect();
    (!messages.is_empty()).then(|| messages.join("；"))
}

pub(crate) fn local_error(code: &str, message: impl Into<String>) -> CommandError {
    CommandError {
        error: code.to_owned(),
        message: message.into(),
        retry_after: None,
        extra: Value::Null,
    }
}

type Reply<T> = Result<T, CommandError>;

#[tauri::command]
pub async fn account_status(account: State<'_, AccountState>) -> Reply<StatusPayload> {
    Ok(account.status())
}

/// 「重试」：钥匙串不可用时重新自检；已登录时重新联网刷新（名额满了解绑后也走这里重新绑定）。
#[tauri::command]
pub async fn account_retry(
    app: AppHandle,
    account: State<'_, AccountState>,
) -> Reply<StatusPayload> {
    if account.is_logged_in() {
        account.sync().await;
    } else {
        account.retry_restore().await;
    }
    Ok(emit(&app, &account))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendCodeArgs {
    email: String,
    /// `register` / `reset_password`。
    purpose: String,
}

#[tauri::command]
pub async fn account_send_code(
    account: State<'_, AccountState>,
    args: SendCodeArgs,
) -> Reply<Value> {
    Ok(account.send_code(&args.email, &args.purpose).await?)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterArgs {
    email: String,
    code: String,
    password: String,
    accept_terms: bool,
}

#[tauri::command]
pub async fn account_register(
    app: AppHandle,
    account: State<'_, AccountState>,
    args: RegisterArgs,
) -> Reply<StatusPayload> {
    account
        .register(&args.email, &args.code, &args.password, args.accept_terms)
        .await?;
    Ok(emit(&app, &account))
}

#[tauri::command]
pub async fn account_login(
    app: AppHandle,
    account: State<'_, AccountState>,
    email: String,
    password: String,
) -> Reply<StatusPayload> {
    account.login(&email, &password).await?;
    Ok(emit(&app, &account))
}

#[tauri::command]
pub async fn account_logout(
    app: AppHandle,
    account: State<'_, AccountState>,
) -> Reply<StatusPayload> {
    account.logout().await;
    Ok(emit(&app, &account))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetPasswordArgs {
    email: String,
    code: String,
    new_password: String,
}

#[tauri::command]
pub async fn account_reset_password(
    account: State<'_, AccountState>,
    args: ResetPasswordArgs,
) -> Reply<()> {
    Ok(account
        .reset_password(&args.email, &args.code, &args.new_password)
        .await?)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangePasswordArgs {
    current_password: String,
    new_password: String,
}

#[tauri::command]
pub async fn account_change_password(
    app: AppHandle,
    account: State<'_, AccountState>,
    args: ChangePasswordArgs,
) -> Reply<StatusPayload> {
    let result = account
        .change_password(&args.current_password, &args.new_password)
        .await;
    // 会话失效时 authed 已经清掉了登录状态，界面也要跟着变。
    let status = emit(&app, &account);
    result?;
    Ok(status)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RedeemPayload {
    /// 同一张卡之前已经兑换过（没有重复加时间）。
    already_redeemed: bool,
    status: StatusPayload,
}

#[tauri::command]
pub async fn license_redeem(
    app: AppHandle,
    account: State<'_, AccountState>,
    code: String,
) -> Reply<RedeemPayload> {
    let result = account.redeem(&code).await;
    let status = emit(&app, &account);
    let response = result?;
    Ok(RedeemPayload {
        already_redeemed: response.already_redeemed,
        status,
    })
}

#[tauri::command]
pub async fn license_refresh(
    app: AppHandle,
    account: State<'_, AccountState>,
) -> Reply<StatusPayload> {
    account.sync().await;
    Ok(emit(&app, &account))
}

#[tauri::command]
pub async fn device_list(app: AppHandle, account: State<'_, AccountState>) -> Reply<DeviceList> {
    let result = account.list_devices().await;
    emit(&app, &account);
    Ok(result?)
}

#[tauri::command]
pub async fn device_unbind(
    app: AppHandle,
    account: State<'_, AccountState>,
    id: String,
) -> Reply<StatusPayload> {
    let result = account.unbind(&id).await;
    let status = emit(&app, &account);
    result?;
    Ok(status)
}

/// 名额满了、解绑一台之后，重新绑定本机。
#[tauri::command]
pub async fn device_activate(
    app: AppHandle,
    account: State<'_, AccountState>,
) -> Reply<StatusPayload> {
    account.activate().await;
    Ok(emit(&app, &account))
}

/// 导出个人数据：先让用户选保存位置（取消就不请求），再拉数据，先写临时文件再改名。
/// 返回保存的路径；用户取消时返回 `None`。
#[tauri::command]
pub async fn account_export(
    app: AppHandle,
    account: State<'_, AccountState>,
) -> Reply<Option<String>> {
    let picked = tauri::async_runtime::spawn_blocking(|| {
        rfd::FileDialog::new()
            .set_title("导出个人数据")
            .set_file_name("剧备-个人数据.json")
            .add_filter("JSON", &["json"])
            .save_file()
    })
    .await
    .map_err(|error| local_error("dialog_failed", format!("保存对话框打开失败：{error}")))?;
    let Some(path) = picked else {
        return Ok(None);
    };
    let result = account.export().await;
    emit(&app, &account);
    let data = result?;
    let bytes = serde_json::to_vec_pretty(&data)
        .map_err(|error| local_error("write_failed", format!("整理导出数据失败：{error}")))?;
    write_atomic(&path, &bytes)
        .map_err(|error| local_error("write_failed", format!("保存文件失败：{error}")))?;
    Ok(Some(path.display().to_string()))
}

fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(".tmp");
    let temp = std::path::PathBuf::from(temp);
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

#[tauri::command]
pub async fn account_delete(
    app: AppHandle,
    account: State<'_, AccountState>,
    password: String,
) -> Reply<StatusPayload> {
    let result = account.delete_account(&password).await;
    let status = emit(&app, &account);
    result?;
    Ok(status)
}
