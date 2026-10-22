//! 「服务端处理」的 Tauri 命令（M5b-8）。
//!
//! 与 M4b 的「本机处理」是两条并存的路：
//!
//! * 本机处理：视频不出这台电脑，`jobs.rs` 逐集跑 ffmpeg；
//! * 服务端处理：视频分片上传到媒体控制面，那边排队跑，产物在那边，再下载回来。
//!
//! 这里只做「把文件切成片、算哈希、跟服务端来回」这几件事，
//! 不判断业务该不该做（余额够不够、能不能建任务都由服务端决定）。
//!
//! **分片大小不在这里定**：服务端在开上传会话时给出 `chunk_size`，
//! 客户端照着切。两边各定一个值，一旦不一致就会拼出错的文件。

use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, State};

use super::commands::{local_error, CommandError, Reply};
use super::AccountState;

/// 上传进度事件名。前端 `listen` 这个名字。
pub const PROGRESS_EVENT: &str = "server-upload-progress";

/// 一次分片上传的进度。前端据此画进度条。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadProgress {
    pub upload_id: String,
    pub sent_chunks: u32,
    pub total_chunks: u32,
    /// 已经跳过（之前就传过）的块数，断点续传时用来说明「没白等」。
    pub skipped_chunks: u32,
}

/// 计算整个文件的 SHA-256。上传前必须算：服务端用它校验拼接结果。
fn sha256_of(path: &Path) -> Result<(u64, String), CommandError> {
    let mut file =
        std::fs::File::open(path).map_err(|error| io_error("读取文件失败", path, &error))?;
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        use std::io::Read;
        let read = file
            .read(&mut buffer)
            .map_err(|error| io_error("读取文件失败", path, &error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
    Ok((total, format!("{:x}", hasher.finalize())))
}

/// 读一段字节。分片按 `offset` + `length` 定位。
///
/// **允许读到文件末尾时少于 `length`**：最后一块本来就可能不满一个分片
/// （文件 10 字节、分片 8 字节时，第二块只有 2 字节）。用 `read_to_end`
/// 而不是 `read_exact`——后者会把「最后一块偏短」当成错误，
/// 结果是非整块大小的文件永远传不完。
fn read_range(path: &Path, offset: u64, length: u64) -> Result<Vec<u8>, CommandError> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file =
        std::fs::File::open(path).map_err(|error| io_error("读取文件失败", path, &error))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| io_error("定位文件失败", path, &error))?;
    // 先按请求长度读；读不满说明到底了，正常。
    let mut buffer = vec![0u8; length as usize];
    let mut filled = 0usize;
    while filled < buffer.len() {
        let read = file
            .read(&mut buffer[filled..])
            .map_err(|error| io_error("读取分片失败", path, &error))?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    buffer.truncate(filled);
    if filled == 0 && length > 0 {
        // 一块都读不到：说明 offset 就已经越过文件末尾了，这是真的出错
        // （不是「最后一块偏短」）。如实报出来。
        return Err(io_error(
            "读取分片失败",
            path,
            &std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "偏移已越过文件末尾"),
        ));
    }
    Ok(buffer)
}

fn io_error(action: &str, path: &Path, error: &std::io::Error) -> CommandError {
    // 带上文件名：用户要知道是哪一集出的问题。
    local_error(
        "io",
        format!(
            "{action}：{}（{}）",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("文件"),
            error
        ),
    )
}

fn path_error(message: &str) -> CommandError {
    local_error("io", message)
}

/// 把字符串里可能拼进 URL 路径的部分检查一遍。
///
/// `upload_id` 与 `task_id` 都来自服务端，理论上安全；但它们会被直接拼进请求路径，
/// 所以按不可信处理：只允许字母数字、下划线和短横线。
fn safe_segment(value: &str, label: &str) -> Result<(), CommandError> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(path_error(&format!("{label}格式不正确：{value}")));
    }
    Ok(())
}

/// 开上传会话：算哈希 → 报给服务端 → 拿 upload_id 与 chunk_size。
#[tauri::command]
pub async fn server_upload_start(
    account: State<'_, AccountState>,
    path: String,
    name: String,
    filename: String,
) -> Reply<serde_json::Value> {
    let path = PathBuf::from(path);
    // 算哈希与读文件是阻塞动作，放到阻塞线程里，别卡住异步运行时。
    let (size_bytes, sha256) = tauri::async_runtime::spawn_blocking({
        let path = path.clone();
        move || sha256_of(&path)
    })
    .await
    .map_err(|error| path_error(&format!("计算校验和失败：{error}")))??;

    let value = account
        .server_upload_start(&name, &filename, size_bytes, &sha256)
        .await?;
    // 项目里没有日志框架，统一用 eprintln（与 jobs.rs 一致）。
    // 只打 upload_id 与大小，不打文件名（文件名可能含剧名，属于内容信息）。
    eprintln!(
        "服务端上传会话已开：{}，{} 字节",
        value
            .get("upload_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?"),
        size_bytes
    );
    Ok(value)
}

/// 查已收分片（断点续传前调用）。
#[tauri::command]
pub async fn server_upload_status(
    account: State<'_, AccountState>,
    upload_id: String,
) -> Reply<serde_json::Value> {
    safe_segment(&upload_id, "上传编号")?;
    Ok(account.server_upload_status(&upload_id).await?)
}

/// 传完剩下的分片。
///
/// 一次命令传完整个文件的所有缺失分片，中途发 `server-upload-progress` 事件。
/// 为什么不做「一块一个命令」：前端要对每块发一次 IPC，几千次的往返比这里循环慢得多，
/// 而且进度更新会更碎。失败时抛错，前端重试时用 `server_upload_status` 接着传。
#[tauri::command]
pub async fn server_upload_chunks(
    app: AppHandle,
    account: State<'_, AccountState>,
    upload_id: String,
    path: String,
    chunk_size: u64,
) -> Reply<UploadProgress> {
    safe_segment(&upload_id, "上传编号")?;
    if chunk_size == 0 || chunk_size > 64 * 1024 * 1024 {
        return Err(path_error("分片大小不合理"));
    }

    // 先问服务端哪些块已经有了：断点续传就是「只补缺的」。
    let status = account.server_upload_status(&upload_id).await?;
    let missing: Vec<u64> = status
        .get("missing")
        .and_then(|v| v.as_array())
        .map(|items| items.iter().filter_map(|v| v.as_u64()).collect())
        .ok_or_else(|| path_error("服务端没有返回缺失分片列表"))?;
    let total: u32 = status
        .get("total_chunks")
        .and_then(|v| v.as_u64())
        .unwrap_or(missing.len() as u64) as u32;
    let skipped = total.saturating_sub(missing.len() as u32);

    let source = PathBuf::from(path);
    let mut sent = 0u32;
    for index in missing {
        let offset = index * chunk_size;
        let payload = tauri::async_runtime::spawn_blocking({
            let source = source.clone();
            move || read_range(&source, offset, chunk_size)
        })
        .await
        .map_err(|error| path_error(&format!("读取分片失败：{error}")))??;

        account
            .server_upload_chunk(&upload_id, index as u32, payload)
            .await?;
        sent += 1;
        // 进度事件发不出去不影响上传本身：本地界面收不到只是进度条不动，
        // 上传该继续。所以这里不 `?`，忽略错误。
        let _ = app.emit(
            PROGRESS_EVENT,
            &UploadProgress {
                upload_id: upload_id.clone(),
                sent_chunks: sent + skipped,
                total_chunks: total,
                skipped_chunks: skipped,
            },
        );
    }

    Ok(UploadProgress {
        upload_id,
        sent_chunks: total,
        total_chunks: total,
        skipped_chunks: skipped,
    })
}

/// 完成上传：服务端拼接并校验整文件哈希，返回新建的素材。
#[tauri::command]
pub async fn server_upload_complete(
    account: State<'_, AccountState>,
    upload_id: String,
) -> Reply<serde_json::Value> {
    safe_segment(&upload_id, "上传编号")?;
    Ok(account.server_upload_complete(&upload_id).await?)
}

/// 放弃上传：删掉已传的分片。
#[tauri::command]
pub async fn server_upload_abort(
    account: State<'_, AccountState>,
    upload_id: String,
) -> Reply<serde_json::Value> {
    safe_segment(&upload_id, "上传编号")?;
    Ok(account.server_upload_abort(&upload_id).await?)
}

/// 建服务端处理任务（服务端会先预扣积分）。
#[tauri::command]
pub async fn server_submit_task(
    account: State<'_, AccountState>,
    asset_ids: Vec<String>,
    recipe_id: String,
) -> Reply<serde_json::Value> {
    if asset_ids.is_empty() {
        return Err(path_error("至少要有 1 集素材"));
    }
    Ok(account.server_submit_task(&asset_ids, &recipe_id).await?)
}

/// 查任务状态。
#[tauri::command]
pub async fn server_task(
    account: State<'_, AccountState>,
    task_id: String,
) -> Reply<serde_json::Value> {
    safe_segment(&task_id, "任务编号")?;
    Ok(account.server_task(&task_id).await?)
}

/// 取消任务（服务端会把预扣的积分退回来）。
#[tauri::command]
pub async fn server_cancel_task(
    account: State<'_, AccountState>,
    task_id: String,
) -> Reply<serde_json::Value> {
    safe_segment(&task_id, "任务编号")?;
    Ok(account.server_cancel_task(&task_id).await?)
}

/// 取产物的短时下载链接。
///
/// 媒体控制面回的 `download_url` 是站内路径（`/api/v1/artifacts/…/download?token=…`），
/// 界面只把 http/https 完整地址做成链接，所以这里补上媒体控制面的地址。
#[tauri::command]
pub async fn server_artifact(
    account: State<'_, AccountState>,
    artifact_id: String,
) -> Reply<serde_json::Value> {
    safe_segment(&artifact_id, "产物编号")?;
    let mut artifact = account.server_artifact(&artifact_id).await?;
    let base =
        super::api::resolve_media_base().map_err(|problem| local_error("config", problem))?;
    absolutize_download_url(&mut artifact, &base);
    Ok(artifact)
}

/// 把产物信息里的 `download_url` 补成完整地址；没有这个字段（或不是字符串）就不动。
fn absolutize_download_url(artifact: &mut serde_json::Value, base: &reqwest::Url) {
    if let Some(url) = artifact
        .get_mut("download_url")
        .and_then(|value| value.as_str().map(str::to_owned))
    {
        artifact["download_url"] = super::api::absolute_media_url(base, &url).into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一份已知内容的 SHA-256：用 `sha256sum` 对同一串字节核过。
    #[test]
    fn sha256_matches_a_known_digest() {
        let dir = tempdir();
        let path = dir.join("a.bin");
        std::fs::write(&path, b"abc").unwrap();
        let (size, digest) = sha256_of(&path).unwrap();
        assert_eq!(size, 3);
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_panic() {
        let dir = tempdir();
        let error = sha256_of(&dir.join("nope.bin")).unwrap_err();
        assert_eq!(error.code(), "io");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_range_takes_exactly_the_bytes_asked_for() {
        let dir = tempdir();
        let path = dir.join("b.bin");
        std::fs::write(&path, b"0123456789").unwrap();
        assert_eq!(read_range(&path, 0, 4).unwrap(), b"0123");
        assert_eq!(read_range(&path, 4, 4).unwrap(), b"4567");
        assert_eq!(read_range(&path, 8, 2).unwrap(), b"89");
        // 越过末尾：报错而不是返回空数据。
        assert!(read_range(&path, 12, 4).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 最后一块偏短是**正常情况**，不能报错。
    ///
    /// 曾经用 `read_exact`：文件 10 字节、分片 8 字节时，第二块只该读 2 字节，
    /// `read_exact` 却要 8 字节 → 直接失败，于是「非整块大小的文件永远传不完」。
    #[test]
    fn the_last_chunk_may_be_shorter_than_the_chunk_size() {
        let dir = tempdir();
        let path = dir.join("c.bin");
        std::fs::write(&path, b"0123456789").unwrap(); // 10 字节，分片 8
        assert_eq!(read_range(&path, 0, 8).unwrap(), b"01234567");
        // 第二块只要到 2 字节，正常返回而不是报错。
        assert_eq!(read_range(&path, 8, 8).unwrap(), b"89");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 编号会被拼进请求路径，所以只放行字母数字、下划线与短横线。
    #[test]
    fn safe_segment_rejects_anything_that_could_escape_the_path() {
        for good in ["up_abc", "tsk-123", "a1", "AF_09-zz"] {
            assert!(safe_segment(good, "编号").is_ok(), "{good} 应当放行");
        }
        for bad in ["", "a/b", "../etc", "a?x=1", "a b", "a#b", "中文"] {
            assert!(safe_segment(bad, "编号").is_err(), "{bad} 应当被拒");
        }
    }

    /// 媒体控制面回的是站内路径；补成完整地址后界面才能做成链接。其余字段原样保留。
    #[test]
    fn artifact_download_url_is_made_absolute() {
        let base = reqwest::Url::parse("http://127.0.0.1:8000").unwrap();
        let mut artifact = serde_json::json!({
            "id": "af_1",
            "download_url": "/api/v1/artifacts/af_1/download?token=t",
        });
        absolutize_download_url(&mut artifact, &base);
        assert_eq!(
            artifact,
            serde_json::json!({
                "id": "af_1",
                "download_url": "http://127.0.0.1:8000/api/v1/artifacts/af_1/download?token=t",
            })
        );

        // 没有下载链接（例如产物还没生成）：不凭空加字段。
        let mut pending = serde_json::json!({ "id": "af_2", "download_url": null });
        absolutize_download_url(&mut pending, &base);
        assert_eq!(
            pending,
            serde_json::json!({ "id": "af_2", "download_url": null })
        );
    }

    /// 测试用的临时目录：不引第三方 crate，用系统临时目录 + 唯一名。
    fn tempdir() -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("dossier-server-jobs-{nanos}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}
