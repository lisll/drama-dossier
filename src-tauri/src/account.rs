//! 账号桌面入口：本地演示版账号的读写与命令封装。
//!
//! 与 `dossier_core::account` 的分工：纯逻辑（校验 / 哈希 / 状态机）在 core，
//! 这里只负责账号 JSON 文件的读写（`app_config_dir/account.json`）与 Tauri 命令适配。
//! 云端版落地时：替换本文件内的读写为 HTTP 调用，core 的函数签名保持不变。

use dossier_core::{
    change_password, login, logout, register, status, AccountError, AccountStore, AccountStatus,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

/// 账号文件位置：应用配置目录下的 `account.json`。
fn account_path(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    app.path()
        .app_config_dir()
        .map(|dir| dir.join("account.json"))
        .map_err(|error| format!("无法定位应用配置目录：{error}。请检查系统权限。"))
}

/// 读取账号文件；不存在或损坏时返回空白状态（未注册）。
fn read_store(app: &AppHandle) -> Result<AccountStore, String> {
    let path = account_path(app)?;
    if !path.exists() {
        return Ok(AccountStore::default());
    }
    let bytes = std::fs::read(&path)
        .map_err(|error| format!("读取账号文件失败：{error}"))?;
    serde_json::from_slice::<AccountStore>(&bytes)
        .map_err(|error| format!("账号文件损坏（{}）：{error}", path.display()))
}

/// 原子写入账号文件（临时文件 + 替换，与 settings 写入一致）。
fn write_store(app: &AppHandle, store: &AccountStore) -> Result<(), String> {
    let path = account_path(app)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("创建配置目录失败：{error}"))?;
    }
    let data =
        serde_json::to_vec_pretty(store).map_err(|error| format!("序列化账号失败：{error}"))?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, data).map_err(|error| format!("写入账号文件失败：{error}"))?;
    std::fs::rename(&temp, &path).map_err(|error| format!("替换账号文件失败：{error}"))
}

/// 把 `AccountError` 转成中文消息（core 的 Display 已是中文，直接透传）。
fn flatten<T>(result: Result<T, AccountError>) -> Result<T, String> {
    result.map_err(|error| error.to_string())
}

/// 注册入参（含本地演示验证码；验证码由前端生成回显，本层只透传不做校验）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterArgs {
    pub email: String,
    pub password: String,
    /// 本地演示验证码；云端版会改为服务端校验。
    #[allow(dead_code)]
    pub code: String,
}

/// 修改密码入参。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangePasswordArgs {
    pub old_password: String,
    pub new_password: String,
}

/// 注册 + 登录状态统一回包（带账号文件位置便于界面提示）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountPayload {
    #[serde(flatten)]
    pub status: AccountStatus,
    pub path: String,
}

fn payload(app: &AppHandle, store: &AccountStore) -> Result<AccountPayload, String> {
    Ok(AccountPayload {
        status: status(store),
        path: account_path(app)?.display().to_string(),
    })
}

#[tauri::command]
pub fn account_register(app: AppHandle, args: RegisterArgs) -> Result<AccountPayload, String> {
    let mut store = read_store(&app)?;
    flatten(register(&mut store, &args.email, &args.password))?;
    write_store(&app, &store)?;
    payload(&app, &store)
}

#[tauri::command]
pub fn account_login(app: AppHandle, email: String, password: String) -> Result<AccountPayload, String> {
    let mut store = read_store(&app)?;
    flatten(login(&mut store, &email, &password))?;
    write_store(&app, &store)?;
    payload(&app, &store)
}

#[tauri::command]
pub fn account_logout(app: AppHandle) -> Result<AccountPayload, String> {
    let mut store = read_store(&app)?;
    logout(&mut store);
    write_store(&app, &store)?;
    payload(&app, &store)
}

#[tauri::command]
pub fn account_status(app: AppHandle) -> Result<AccountPayload, String> {
    let store = read_store(&app)?;
    payload(&app, &store)
}

#[tauri::command]
pub fn account_change_password(
    app: AppHandle,
    args: ChangePasswordArgs,
) -> Result<AccountPayload, String> {
    let mut store = read_store(&app)?;
    flatten(change_password(
        &mut store,
        &args.old_password,
        &args.new_password,
    ))?;
    write_store(&app, &store)?;
    payload(&app, &store)
}
