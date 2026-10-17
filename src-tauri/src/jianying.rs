//! 剪映桌面入口：探测安装目录 / 版本 / 草稿目录。
//!
//! 与设置页「剪映」子页配套：自动检测按钮 → [`jianying_detect`]；
//! 用户手动指定的目录保存在 `AppSettings.jianying_dir` / `jianying_draft_dir`。
//!
//! 两个平台的默认路径完全不同，但「先看设置、再找默认位置、都找不到就报未安装」
//! 这套判断是一样的。所以把判断抽成不依赖平台的 [`resolve`]，平台只提供候选路径，
//! 这样在 Linux 上也能用假目录把两个平台的分支都测到。
//!
//! Windows 的两个默认路径是**推断值**（网上只有间接说法），必须真机核实；
//! 所以找不到时界面要引导用户手动选，而不是断言「没装剪映」。

use std::path::{Path, PathBuf};

use serde::Serialize;
use tauri::AppHandle;

use crate::toolbox::read_settings;

/// 探测结果（`version` / `draftDir` 等字段可能为空，界面按缺省文案展示）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JianyingDetect {
    pub installed: bool,
    pub install_dir: String,
    pub version: Option<String>,
    pub draft_dir: String,
    pub draft_dir_exists: bool,
    /// 这些默认路径是推断的、没见过官方说明（Windows 上为 true）。
    ///
    /// 界面据此把文案从「未安装剪映」改成「没找到，请手动选择安装目录」——
    /// 断言一个可能不成立的结论，比让用户多点一次更糟。
    pub default_is_inferred: bool,
}

/// 剪映默认安装位置（macOS）。
#[cfg(target_os = "macos")]
const DEFAULT_APP: &str = "/Applications/JianyingPro.app";
/// 剪映默认草稿目录（macOS，国内版剪映的标准位置）。
#[cfg(target_os = "macos")]
const DEFAULT_DRAFT_DIR: &str = "Movies/JianyingPro/User Data/Projects/com.lveditor.draft";
/// Windows 默认安装目录（推断：`%LOCALAPPDATA%\JianyingPro`）。
#[cfg(any(
    target_os = "windows",
    not(any(target_os = "macos", target_os = "windows"))
))]
const WINDOWS_INSTALL_DIR: &str = "JianyingPro";
/// Windows 默认草稿目录（推断：`%LOCALAPPDATA%\JianyingPro\User Data\Projects\com.lveditor.draft`）。
#[cfg(any(
    target_os = "windows",
    not(any(target_os = "macos", target_os = "windows"))
))]
const WINDOWS_DRAFT_DIR: &str = "JianyingPro/User Data/Projects/com.lveditor.draft";
/// 剪映主程序名（Windows）。
const WINDOWS_EXE: &str = "JianyingPro.exe";
/// 版本号文件（Windows）：有的版本会在安装目录里放一个纯文本 `version`。
const WINDOWS_VERSION_FILE: &str = "version";

/// 平台相关的探测输入。
///
/// 拆出来是为了能在 Linux 上测两个平台的逻辑：测试直接构造这个结构，
/// 指向临时目录里的假安装位置。
#[derive(Debug, Clone)]
struct Layout {
    /// 默认安装目录。
    default_install: PathBuf,
    /// 默认草稿目录。
    default_draft: PathBuf,
    /// 判断「装了」的标志文件（相对安装目录）。
    marker: PathBuf,
    /// 版本号文本文件（相对安装目录）；`None` 表示该平台从别处读版本。
    version_file: Option<PathBuf>,
    /// 默认路径是否为推断值。
    inferred: bool,
}

#[cfg(target_os = "macos")]
fn layout() -> Layout {
    Layout {
        default_install: PathBuf::from(DEFAULT_APP),
        default_draft: {
            let home = std::env::var("HOME").unwrap_or_default();
            Path::new(&home).join(DEFAULT_DRAFT_DIR)
        },
        marker: PathBuf::from("Contents/Info.plist"),
        // macOS 的版本在 Info.plist 里，单独处理。
        version_file: None,
        inferred: false,
    }
}

#[cfg(target_os = "windows")]
fn layout() -> Layout {
    let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
    let base = Path::new(&local);
    Layout {
        default_install: base.join(WINDOWS_INSTALL_DIR),
        default_draft: base.join(WINDOWS_DRAFT_DIR),
        marker: PathBuf::from(WINDOWS_EXE),
        version_file: Some(PathBuf::from(WINDOWS_VERSION_FILE)),
        // Windows 这两个默认路径网上只有间接说法，没有官方文档。
        inferred: true,
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn layout() -> Layout {
    // Linux 上没有剪映桌面版；这里沿用 Windows 的相对结构，让代码能编译、逻辑能测。
    let base = std::env::temp_dir();
    Layout {
        default_install: base.join(WINDOWS_INSTALL_DIR),
        default_draft: base.join(WINDOWS_DRAFT_DIR),
        marker: PathBuf::from(WINDOWS_EXE),
        version_file: Some(PathBuf::from(WINDOWS_VERSION_FILE)),
        inferred: true,
    }
}

/// 纯函数：给定设置与平台布局，算出探测结果。
///
/// 顺序：设置里手动指定的 → 平台默认位置 → 未安装。
/// 手动指定的目录不存在时**不回退**到默认位置：使用者显式指定过，说明默认位置不对，
/// 悄悄换成默认位置会让人以为设置生效了。
fn resolve(
    layout: &Layout,
    configured_install: Option<&Path>,
    configured_draft: Option<&Path>,
) -> JianyingDetect {
    let (install_dir, from_settings) = match configured_install {
        Some(path) => (path.to_path_buf(), true),
        None => (layout.default_install.clone(), false),
    };
    let installed = install_dir.join(&layout.marker).is_file();
    let version = if installed {
        read_version(&install_dir, layout)
    } else {
        None
    };
    // 显式指定了安装目录但里面没有主程序：也算未安装，但保留目录让用户看清指到了哪。
    let draft_dir = configured_draft
        .map(Path::to_path_buf)
        .unwrap_or_else(|| layout.default_draft.clone());
    JianyingDetect {
        installed,
        install_dir: install_dir.display().to_string(),
        version,
        draft_dir: draft_dir.display().to_string(),
        draft_dir_exists: draft_dir.is_dir(),
        // 用户自己指定过就不算「推断」：那是他确认过的位置。
        default_is_inferred: layout.inferred && !from_settings,
    }
}

/// 读版本号：macOS 读 Info.plist，其余平台读安装目录下的 `version` 文本文件。
fn read_version(install_dir: &Path, layout: &Layout) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let plist = install_dir.join("Contents").join("Info.plist");
        let text = std::fs::read_to_string(plist).ok()?;
        return version_from_plist(&text);
    }
    #[cfg(not(target_os = "macos"))]
    {
        let file = layout.version_file.as_ref()?;
        let text = std::fs::read_to_string(install_dir.join(file)).ok()?;
        let trimmed = text.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }
}

/// 探测剪映：设置里手动指定的目录优先，其次默认安装位置。
#[tauri::command]
pub fn jianying_detect(app: AppHandle) -> Result<JianyingDetect, String> {
    let settings = read_settings(&app)?;
    Ok(resolve(
        &layout(),
        settings.jianying_dir.as_deref(),
        settings.jianying_draft_dir.as_deref(),
    ))
}

/// 从 Info.plist 文本中提取 `CFBundleShortVersionString` 的值（std only，不引 plist 库）。
///
/// 形如 `<key>CFBundleShortVersionString</key>\n<string>5.9.0</string>`：
/// 找到键名后要**依次跳过两对标签**——`</key>` 与 `<string>`——才能取到值。
#[cfg(target_os = "macos")]
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
#[cfg(target_os = "macos")]
fn skip_tag(text: &str) -> Option<&str> {
    let open = text.find('<')?;
    let close = text[open..].find('>')? + open + 1;
    Some(&text[close..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("dossier-jianying-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 造一个「Windows 式」的安装目录：一个 JianyingPro.exe + 一个 version 文本。
    fn windows_install(root: &Path, version: Option<&str>) -> PathBuf {
        let install = root.join("JianyingPro");
        std::fs::create_dir_all(&install).unwrap();
        std::fs::write(install.join(WINDOWS_EXE), b"MZ fake exe").unwrap();
        if let Some(version) = version {
            std::fs::write(install.join(WINDOWS_VERSION_FILE), version).unwrap();
        }
        install
    }

    fn windows_layout(root: &Path) -> Layout {
        Layout {
            default_install: root.join("JianyingPro"),
            default_draft: root.join("JianyingPro/User Data/Projects/com.lveditor.draft"),
            marker: PathBuf::from(WINDOWS_EXE),
            version_file: Some(PathBuf::from(WINDOWS_VERSION_FILE)),
            inferred: true,
        }
    }

    #[test]
    fn finds_the_install_and_reads_the_version_file() {
        let root = temp("win-found");
        windows_install(&root, Some("5.9.0\n"));
        let layout = windows_layout(&root);
        let result = resolve(&layout, None, None);
        assert!(result.installed, "有 JianyingPro.exe 就该算已安装");
        assert_eq!(
            result.version.as_deref(),
            Some("5.9.0"),
            "版本要 trim 掉换行"
        );
        assert!(
            result.default_is_inferred,
            "用的是推断出来的默认路径，要如实标注"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_version_file_reports_unknown_instead_of_failing() {
        let root = temp("win-noversion");
        windows_install(&root, None);
        let layout = windows_layout(&root);
        let result = resolve(&layout, None, None);
        assert!(result.installed);
        assert_eq!(result.version, None, "读不到版本就显示未知，不编造");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_exe_means_not_installed() {
        let root = temp("win-missing");
        // 目录在、但里面没有主程序。
        std::fs::create_dir_all(root.join("JianyingPro")).unwrap();
        let layout = windows_layout(&root);
        let result = resolve(&layout, None, None);
        assert!(!result.installed, "没有 JianyingPro.exe 就不算装好");
        assert_eq!(result.version, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn configured_install_wins_and_is_not_marked_inferred() {
        let root = temp("win-configured");
        let install = windows_install(&root, Some("6.0.1"));
        let layout = windows_layout(&root);
        let result = resolve(&layout, Some(&install), None);
        assert!(result.installed);
        assert_eq!(result.version.as_deref(), Some("6.0.1"));
        assert!(!result.default_is_inferred, "用户自己指定的位置不是推断值");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn configured_install_is_not_silently_replaced_by_the_default() {
        let root = temp("win-bad-configured");
        // 默认位置有个能用的安装。
        windows_install(&root, Some("5.9.0"));
        let layout = windows_layout(&root);
        // 但用户显式指定了一个不存在的目录。
        let missing = root.join("用户指定的目录");
        let result = resolve(&layout, Some(&missing), None);
        assert!(
            !result.installed,
            "显式指定了却回退到默认位置，会让人以为设置生效了"
        );
        assert_eq!(result.install_dir, missing.display().to_string());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn draft_dir_existence_is_reported() {
        let root = temp("win-draft");
        windows_install(&root, Some("5.9.0"));
        let layout = windows_layout(&root);
        // 默认草稿目录不存在。
        let result = resolve(&layout, None, None);
        assert!(!result.draft_dir_exists);
        // 造出来之后就存在了。
        std::fs::create_dir_all(&layout.default_draft).unwrap();
        let result = resolve(&layout, None, None);
        assert!(result.draft_dir_exists);
        // 用户指定了草稿目录：用他的。
        let custom = root.join("我的草稿");
        std::fs::create_dir_all(&custom).unwrap();
        let result = resolve(&layout, None, Some(&custom));
        assert!(result.draft_dir_exists);
        assert_eq!(result.draft_dir, custom.display().to_string());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(target_os = "macos")]
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
