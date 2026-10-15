//! 剪映桌面入口：探测安装目录 / 版本 / 草稿目录。
//!
//! 与设置页「剪映」子页配套：自动检测按钮 → [`jianying_detect`]；
//! 用户手动指定的目录保存在 `AppSettings.jianying_dir` / `jianying_draft_dir`。

use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::AppHandle;

use crate::toolbox::read_settings;

/// 探测结果（`version` / `draftDir` 等字段可能为空，界面按缺省文案展示）。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JianyingDetect {
    pub installed: bool,
    pub install_dir: String,
    pub version: Option<String>,
    pub draft_dir: String,
    pub draft_dir_exists: bool,
}

/// 剪映默认安装位置（macOS）。
const DEFAULT_APP: &str = "/Applications/JianyingPro.app";
/// 剪映默认草稿目录（国内版剪映的标准位置）。
const DEFAULT_DRAFT_DIR: &str = "Movies/JianyingPro/User Data/Projects/com.lveditor.draft";

/// 从 Info.plist 文本中提取 `CFBundleShortVersionString` 的值（std only，不引 plist 库）。
///
/// 形如 `<key>CFBundleShortVersionString</key>\n<string>5.9.0</string>`：
/// 找到键名后要**依次跳过两对标签**——`</key>` 与 `<string>`——才能取到值。
fn version_from_plist(text: &str) -> Option<String> {
    let key = "CFBundleShortVersionString";
    let start = text.find(key)? + key.len();
    let rest = &text[start..];
    let after_key = skip_tag(rest)?;
    let after_value_open = skip_tag(after_key)?;
    let end = after_value_open.find('<')?;
    let value = after_value_open[..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// 跳过文本中下一对 `<...>` 标签，返回其后剩余部分。
fn skip_tag(text: &str) -> Option<&str> {
    let open = text.find('<')?;
    let close = text[open..].find('>')? + open + 1;
    Some(&text[close..])
}

/// 探测剪映：设置里手动指定的目录优先，其次默认安装位置。
#[tauri::command]
pub fn jianying_detect(app: AppHandle) -> Result<JianyingDetect, String> {
    let result = jianying_detect_inner(&app);
    // 测试期关键日志：剪映检测影响「设置-剪映」与挂载流程提示。
    crate::logging::log_event(
        "剪映",
        crate::logging::result_text(&result, |detect| {
            if detect.installed {
                format!(
                    "已安装：{}（版本 {:?}，草稿目录 {}）",
                    detect.install_dir,
                    detect.version,
                    detect.draft_dir
                )
            } else {
                format!("未安装（探测路径 {}）", detect.install_dir)
            }
        }),
    );
    result
}

fn jianying_detect_inner(app: &AppHandle) -> Result<JianyingDetect, String> {
    let settings = read_settings(app)?;
    let configured = settings
        .jianying_dir
        .unwrap_or_else(|| PathBuf::from(DEFAULT_APP));
    let install_dir = if configured.exists() {
        configured
    } else {
        PathBuf::from(DEFAULT_APP)
    };

    let plist = install_dir.join("Contents").join("Info.plist");
    let installed = plist.is_file();
    let version = installed
        .then(|| std::fs::read_to_string(&plist).ok())
        .flatten()
        .and_then(|text| version_from_plist(&text));

    let draft_dir = settings
        .jianying_draft_dir
        .unwrap_or_else(default_draft_dir);
    let draft_dir_exists = draft_dir.is_dir();

    Ok(JianyingDetect {
        installed,
        install_dir: install_dir.display().to_string(),
        version,
        draft_dir: draft_dir.display().to_string(),
        draft_dir_exists,
    })
}

/// 默认草稿目录：`~/Movies/JianyingPro/...`。
fn default_draft_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    Path::new(&home).join(DEFAULT_DRAFT_DIR)
}

#[cfg(test)]
mod tests {
    use super::version_from_plist;

    #[test]
    fn extracts_short_version_from_plist_text() {
        let plist = r#"
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>com.lemon.lvpro</string>
    <key>CFBundleShortVersionString</key>
    <string>5.9.0</string>
    <key>CFBundleVersion</key>
    <string>12345</string>
</dict>
</plist>
"#;
        assert_eq!(version_from_plist(plist).as_deref(), Some("5.9.0"));
        assert_eq!(version_from_plist("no version here"), None);
    }
}
