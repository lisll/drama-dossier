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
    pub(crate) message: String,
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

impl CommandError {
    /// 错误码。**只给测试用**：前端拿的是序列化后的 `error` 字段，
    /// 生产代码里没有第二个消费者，加 `cfg(test)` 免得变成死代码告警。
    #[cfg(test)]
    pub(crate) fn code(&self) -> &str {
        &self.error
    }
}

pub(crate) type Reply<T> = Result<T, CommandError>;

#[tauri::command]
pub async fn account_status(account: State<'_, AccountState>) -> Reply<StatusPayload> {
    Ok(account.status())
}

// ---------- 积分与支付（M5a） ----------

/// 余额与最近流水。
#[tauri::command]
pub async fn credits_overview(account: State<'_, AccountState>) -> Reply<Value> {
    Ok(account.credits().await?)
}

/// 更长的流水。
#[tauri::command]
pub async fn credit_history(account: State<'_, AccountState>, limit: Option<u32>) -> Reply<Value> {
    Ok(account.credit_history(limit.unwrap_or(50)).await?)
}

/// 充值档位。
#[tauri::command]
pub async fn recharge_tiers(account: State<'_, AccountState>) -> Reply<Value> {
    Ok(account.recharge_tiers().await?)
}

/// 下单，返回怎么去付。
#[tauri::command]
pub async fn create_recharge_order(
    account: State<'_, AccountState>,
    tier_id: String,
    channel: String,
) -> Reply<Value> {
    Ok(account.create_order(&tier_id, &channel).await?)
}

/// 查订单状态。
#[tauri::command]
pub async fn recharge_order_status(
    account: State<'_, AccountState>,
    out_trade_no: String,
) -> Reply<Value> {
    Ok(account.order_status(&out_trade_no).await?)
}

/// 主动查单（「我已完成支付」）。
#[tauri::command]
pub async fn reconcile_recharge(
    account: State<'_, AccountState>,
    out_trade_no: String,
) -> Reply<Value> {
    Ok(account.reconcile_order(&out_trade_no).await?)
}

/// 我的邀请码与邀请战绩。
#[tauri::command]
pub async fn invite_status(account: State<'_, AccountState>) -> Reply<Value> {
    Ok(account.invites().await?)
}

/// 客户端配置（公告 / 开关 / 版本策略，M7-3）。
///
/// **失败不抛错**：这个接口是公开的、每次启动都会拉。网络不通、服务端还没升级
/// 到有该接口的版本，都不该让界面报错或卡住——前端有安全默认值
/// （空公告、开关全开、不强制更新）。所以这里把错误吞成 `{unavailable: true}`，
/// 只带一句原因供界面显示在设置页里，不影响任何功能。
#[tauri::command]
pub async fn client_config(account: State<'_, AccountState>) -> Reply<Value> {
    match account.client_config().await {
        Ok(value) => Ok(value),
        Err(error) => {
            // 只记错误类型，不记地址与响应内容。
            eprintln!("拉取客户端配置失败：{}", error.message());
            Ok(serde_json::json!({ "unavailable": true, "reason": error.message() }))
        }
    }
}

/// 上报一条客户端错误（M7-6）。
///
/// **由前端决定要不要报**：前端读设置里的开关（默认关），关着就不发这个命令。
/// 这里只做「字段白名单」的最后一道把关——即使前端传了多余的东西，也不会发出去。
///
/// 只发四个字段：错误码、应用版本、平台、出错位置。**不发**错误消息：
/// 消息里可能带文件路径或剧名。
#[tauri::command]
pub async fn report_client_error(
    account: State<'_, AccountState>,
    code: String,
    location: String,
) -> Reply<Value> {
    // 错误码与位置只做长度与字符集约束，不做内容解释。
    let code = sanitize_short(&code, 64);
    let location = sanitize_short(&location, 64);
    if code.is_empty() {
        return Ok(serde_json::json!({ "accepted": false }));
    }
    match account.report_client_error(&code, &location).await {
        Ok(value) => Ok(value),
        Err(error) => {
            // 上报失败不打扰用户：它本来就是个后台动作。
            eprintln!("错误上报失败：{}", error.message());
            Ok(serde_json::json!({ "accepted": false }))
        }
    }
}

/// 只保留字母数字、下划线与短横线，并截断到 `max`。
///
/// 这样即使调用方传了带路径或空格的字符串，也发不出去——
/// 上报白名单不只在服务端做，客户端也做一层。
fn sanitize_short(value: &str, max: usize) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(max)
        .collect()
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
    /// 邀请码（可选）。填错不会让注册失败——邀请是附加动作，不是注册的前提。
    #[serde(default)]
    invite_code: Option<String>,
}

#[tauri::command]
pub async fn account_register(
    app: AppHandle,
    account: State<'_, AccountState>,
    args: RegisterArgs,
) -> Reply<StatusPayload> {
    account
        .register(
            &args.email,
            &args.code,
            &args.password,
            args.accept_terms,
            args.invite_code.as_deref(),
        )
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
