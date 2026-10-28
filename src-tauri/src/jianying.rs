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

/// Windows 上剪映的可执行文件名。
const WINDOWS_EXE: &str = "JianyingPro.exe";
/// Windows 上剪映的候选安装目录（相对 `%LOCALAPPDATA%`），按顺序查找。
///
/// **待真机确认**：这几处是常见的默认安装位置，还没在装了剪映的 Windows 电脑上逐一核实；
/// 确认后应在这里写明核对日期与剪映版本。找不到时界面如实显示「未检测到」，
/// 用户可以在设置里手动指定安装目录与草稿目录（手动值优先）。
const WINDOWS_INSTALL_CANDIDATES: &[&str] = &["JianyingPro", "JianyingPro\\Apps"];
/// Windows 上剪映的默认草稿目录（相对 `%LOCALAPPDATA%`；**待真机确认**，同上）。
const WINDOWS_DRAFT_DIR: &str = "JianyingPro\\User Data\\Projects\\com.lveditor.draft";

/// 在候选目录里找剪映可执行文件（纯函数，按传入顺序检查 `目录/JianyingPro.exe`）。
fn find_windows_exe(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(|dir| dir.join(WINDOWS_EXE))
        .find(|exe| exe.is_file())
}

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

/// 探测剪映：界面本次传入的目录优先，其次设置里手动指定的，最后默认安装位置。
///
/// `dir` 是输入框的当前值——「自动检测」必须即时响应用户刚填的路径，
/// 而不是逼用户先点「保存到设置」再检测。
#[tauri::command]
pub fn jianying_detect(app: AppHandle, dir: Option<String>) -> Result<JianyingDetect, String> {
    let result = jianying_detect_inner(&app, dir);
    // 测试期关键日志：剪映检测影响「设置-剪映」与挂载流程提示。
    crate::logging::log_event(
        "剪映",
        crate::logging::result_text(&result, |detect| {
            if detect.installed {
                format!(
                    "已安装：{}（版本 {:?}，草稿目录 {}）",
                    detect.install_dir, detect.version, detect.draft_dir
                )
            } else {
                format!("未安装（探测路径 {}）", detect.install_dir)
            }
        }),
    );
    result
}

fn jianying_detect_inner(app: &AppHandle, dir: Option<String>) -> Result<JianyingDetect, String> {
    if cfg!(target_os = "windows") {
        return jianying_detect_windows(app, dir);
    }
    let settings = read_settings(app)?;
    // 优先级：界面本次传入 > 设置保存 > 默认路径。
    let configured = dir
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or(settings.jianying_dir)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_APP));
    let configured = if configured.exists() {
        configured
    } else {
        PathBuf::from(DEFAULT_APP)
    };
    // 用户可能填到 `X.app/Contents`（甚至更深）这一层；归一回 .app 根才能读到 Info.plist。
    let install_dir = normalize_app_dir(&configured);

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

/// Windows：以前只认 macOS 的 `.app` 路径，在 Windows 上永远显示「未安装」。
/// 优先级与 macOS 相同：界面本次传入 > 设置保存 > 默认候选目录。版本号暂不读取（显示为空）。
fn jianying_detect_windows(app: &AppHandle, dir: Option<String>) -> Result<JianyingDetect, String> {
    let settings = read_settings(app)?;
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let configured = dir
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or(settings.jianying_dir);
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = &configured {
        // 用户可能填安装根目录、exe 所在的 Apps 子目录，也可能直接选中 JianyingPro.exe 本身
        if dir.is_file() {
            if let Some(parent) = dir.parent() {
                candidates.push(parent.to_path_buf());
            }
        }
        candidates.push(dir.clone());
        candidates.push(dir.join("Apps"));
    }
    if let Some(local) = &local {
        candidates.extend(WINDOWS_INSTALL_CANDIDATES.iter().map(|rel| local.join(rel)));
    }
    let exe = find_windows_exe(&candidates);
    let install_dir = exe
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .or(configured)
        .or_else(|| {
            local
                .as_ref()
                .map(|local| local.join(WINDOWS_INSTALL_CANDIDATES[0]))
        })
        .unwrap_or_default();
    let draft_dir = settings.jianying_draft_dir.unwrap_or_else(|| {
        local
            .as_ref()
            .map(|local| local.join(WINDOWS_DRAFT_DIR))
            .unwrap_or_default()
    });
    let draft_dir_exists = draft_dir.is_dir();
    Ok(JianyingDetect {
        installed: exe.is_some(),
        install_dir: install_dir.display().to_string(),
        version: None,
        draft_dir: draft_dir.display().to_string(),
        draft_dir_exists,
    })
}

/// 把用户填的路径归一到 `.app` 包根目录。
///
/// 从深到浅找第一个「以 .app 结尾且含 Contents/Info.plist」的祖先——
/// 填了 `X.app/Contents` 或 `X.app/Contents/MacOS` 都能归到 `X.app`；
/// 路径里没有任何 .app 层时原样返回，保持原判定与报错语义。
fn normalize_app_dir(path: &Path) -> PathBuf {
    let mut current = Some(path);
    while let Some(dir) = current {
        if dir.extension().and_then(|ext| ext.to_str()) == Some("app")
            && dir.join("Contents").join("Info.plist").is_file()
        {
            return dir.to_path_buf();
        }
        current = dir.parent();
    }
    path.to_path_buf()
}

/// 默认草稿目录：`~/Movies/JianyingPro/...`。
fn default_draft_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    Path::new(&home).join(DEFAULT_DRAFT_DIR)
}

#[cfg(test)]
mod tests {
    use super::{find_windows_exe, normalize_app_dir, version_from_plist, WINDOWS_EXE};
    use std::path::{Path, PathBuf};

    #[test]
    fn finds_windows_exe_in_candidate_order() {
        let base = std::env::temp_dir().join(format!("dossier-jianying-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("JianyingPro");
        let apps = root.join("Apps");
        std::fs::create_dir_all(&apps).unwrap();
        assert_eq!(find_windows_exe(&[root.clone(), apps.clone()]), None);
        std::fs::write(apps.join(WINDOWS_EXE), b"MZ").unwrap();
        assert_eq!(
            find_windows_exe(&[root.clone(), apps.clone()]),
            Some(apps.join(WINDOWS_EXE)),
            "根目录没有 exe 时找到 Apps 子目录里的"
        );
        std::fs::write(root.join(WINDOWS_EXE), b"MZ").unwrap();
        assert_eq!(
            find_windows_exe(&[root.clone(), apps]),
            Some(root.join(WINDOWS_EXE))
        );
        let _ = std::fs::remove_dir_all(&base);
    }

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

    /// 用真实存在的目录结构验证归一化：本机的 VideoFusion（com.lemon.lvpro）
    /// 或任何 /Applications 下的 .app 都行；没有可用样本时跳过。
    #[test]
    fn normalizes_contents_level_path_to_app_root() {
        let sample = Path::new("/Applications/VideoFusion-macOS.app");
        if !sample.join("Contents").join("Info.plist").is_file() {
            return; // 环境无样本（如 Windows CI），跳过
        }
        // 填到 Contents 层 → 归到 .app 根
        let deep = sample.join("Contents");
        assert_eq!(normalize_app_dir(&deep), sample.to_path_buf());
        // 填到可执行文件所在层 → 一样归到 .app 根
        let deeper = sample.join("Contents").join("MacOS");
        assert_eq!(normalize_app_dir(&deeper), sample.to_path_buf());
        // 已是 .app 根 → 原样
        assert_eq!(normalize_app_dir(sample), sample.to_path_buf());
        // 与 .app 无关的路径 → 原样返回（保持报错语义）
        assert_eq!(normalize_app_dir(Path::new("/tmp")), PathBuf::from("/tmp"));
    }
}
