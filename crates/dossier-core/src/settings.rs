//! 用户设置：本地 JSON 持久化。
//!
//! 设计：
//! - 设置文件损坏或缺失时回退到内置默认值，并在 [`LoadedSettings::warning`] 中带回可展示的提示；
//! - 写入采用「临时文件 + 替换」，避免中途失败留下半个配置文件；
//! - 本模块只处理文件读写，不依赖 GUI 或 Tauri。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::checklist::ChecklistRules;

/// 用户可配置项。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSettings {
    /// 检查规则覆盖；`None` 表示使用内置默认阈值。
    pub rules: Option<ChecklistRules>,
    /// ffprobe 可执行文件路径；`None` 表示从 `PATH` 查找。
    pub ffprobe_path: Option<PathBuf>,
    /// ffmpeg 可执行文件路径；`None` 表示从 `PATH` 查找（一键安装的运行环境写在这里）。
    pub ffmpeg_path: Option<PathBuf>,
    /// 默认输出目录（**产物根目录**）；`None` 表示产物写在剧目目录内。
    ///
    /// 非空时按 `<该目录>/<剧名>/<产物名>` 落盘，见 [`resolve_product_dir`]。
    pub output_dir: Option<PathBuf>,
    /// 上次使用的素材根目录；`None` 表示还没有扫描过，界面需要重新选择。
    pub library_root: Option<PathBuf>,
    /// AI 提示词模板（设置-AI 子页编辑；后续云端拉取后也缓存在这里）。
    pub ai_template: Option<String>,
    /// 剪映安装目录；`None` 表示使用自动探测结果。
    pub jianying_dir: Option<PathBuf>,
    /// 剪映草稿目录；`None` 表示使用剪映默认路径。
    pub jianying_draft_dir: Option<PathBuf>,
}

impl AppSettings {
    /// 生效的检查规则：用户覆盖优先，否则使用内置默认。
    pub fn effective_rules(&self) -> ChecklistRules {
        self.rules.clone().unwrap_or_default()
    }
}

/// 读取结果：即使回退到默认值也保留原因，便于界面提示。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadedSettings {
    pub settings: AppSettings,
    /// 读取或解析失败时的可展示提示；正常读取为 `None`。
    pub warning: Option<String>,
}

/// 从 `path` 读取设置；文件不存在时静默返回默认值。
pub fn load_settings(path: &Path) -> LoadedSettings {
    let fallback = |warning: Option<String>| LoadedSettings {
        settings: AppSettings::default(),
        warning,
    };

    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return fallback(None),
        Err(error) => {
            return fallback(Some(format!(
                "读取设置文件失败（{}）：{error}。已使用默认设置。",
                path.display()
            )))
        }
    };

    match serde_json::from_slice::<AppSettings>(&bytes) {
        Ok(settings) => LoadedSettings {
            settings,
            warning: None,
        },
        Err(error) => fallback(Some(format!(
            "设置文件解析失败（{}）：{error}。已回退到默认设置，保存后会覆盖该文件。",
            path.display()
        ))),
    }
}

/// 写入设置；父目录不存在时自动创建。
pub fn save_settings(path: &Path, settings: &AppSettings) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let data = serde_json::to_vec_pretty(settings).map_err(std::io::Error::other)?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, data)?;
    std::fs::rename(&temp, path)
}

/// 产物落点解析：本次覆盖 → 设置里的默认输出目录（产物根目录）→ 剧目目录内。
///
/// 三层语义（与界面文案一一对应，改文案前先读这段）：
/// 1. `override_dir`：生成页/工具箱里**本次运行**填的目录。它就是最终产物目录本身，
///    不再拼接任何东西——使用者的意图是「这次就写到这」。
/// 2. `output_root`：设置里的「默认输出目录」。它是**产物根目录**而不是某一部剧的目录，
///    所以按 `<根目录>/<剧名>/<叶子目录>` 落盘；这样多部剧各占一个子目录，不会互相顶掉
///    （报审包的落盘规则是「目录已存在就拒绝」，共用一个目录必然撞车）。
/// 3. 两者都没有：写在剧目目录内（`<剧目目录>/<叶子目录>`），也就是素材和产物同处一地。
///
/// `leaf` 是产物名（`报审包` / `微调输出` / `重组输出`）。
///
/// 剧名要参与拼路径，而剧名来自扫描结果、可能带路径分隔符，所以这里先做一次净化：
/// 只保留最后一个路径分量，并挡掉 `.` / `..` / 空名，避免「输出根目录」被剧名带偏到别处。
///
/// 桌面端（`src-tauri`）通过 `dossier_core::resolve_product_dir` 使用这个函数，
/// 下面这个例子顺带守住「这个名字确实是公开可用的」——本机编不了 `src-tauri`，
/// 少一个 re-export 会让它直接编译失败，而换个 crate 视角的 doctest 能立刻发现。
///
/// ```
/// use dossier_core::resolve_product_dir;
/// use std::path::Path;
///
/// // 设置里填了产物根目录：按「根目录 / 剧名 / 产物名」落盘。
/// assert_eq!(
///     resolve_product_dir(
///         None,
///         Some(Path::new("/产物")),
///         Path::new("/素材/剧A"),
///         "剧A",
///         "报审包",
///     ),
///     Path::new("/产物/剧A/报审包"),
/// );
///
/// // 没填则留在剧目目录内，与加这个设置之前的行为一致。
/// assert_eq!(
///     resolve_product_dir(None, None, Path::new("/素材/剧A"), "剧A", "报审包"),
///     Path::new("/素材/剧A/报审包"),
/// );
/// ```
pub fn resolve_product_dir(
    override_dir: Option<&Path>,
    output_root: Option<&Path>,
    drama_dir: &Path,
    drama_name: &str,
    leaf: &str,
) -> PathBuf {
    if let Some(explicit) = override_dir {
        if !explicit.as_os_str().is_empty() {
            return explicit.to_path_buf();
        }
    }
    match output_root {
        Some(root) if !root.as_os_str().is_empty() => {
            let safe_name = safe_path_component(drama_name);
            let root = match &safe_name {
                Some(name) => root.join(name),
                None => root.to_path_buf(),
            };
            root.join(leaf)
        }
        _ => drama_dir.join(leaf),
    }
}

/// 把剧名压成单个安全路径分量；压不出安全分量时返回 `None`（调用方退回「不拼剧名」）。
///
/// 只取最后一个路径分量是为了让「带前缀的剧名」仍然落在输出根目录之内，
/// 而不是把产物写到输出根目录之外的绝对路径上。
fn safe_path_component(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }
    let last = Path::new(trimmed)
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())?;
    let last = last.trim();
    if last.is_empty() || last == "." || last == ".." {
        return None;
    }
    Some(last.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("dossier-settings-{tag}-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn product_dir_prefers_run_override() {
        let dir = resolve_product_dir(
            Some(Path::new("/tmp/本次")),
            Some(Path::new("/tmp/根")),
            Path::new("/素材/剧A"),
            "剧A",
            "报审包",
        );
        assert_eq!(dir, PathBuf::from("/tmp/本次"));
    }

    #[test]
    fn product_dir_uses_output_root_with_drama_name() {
        let dir = resolve_product_dir(
            None,
            Some(Path::new("/tmp/产物根")),
            Path::new("/素材/剧A"),
            "剧A",
            "报审包",
        );
        assert_eq!(dir, PathBuf::from("/tmp/产物根/剧A/报审包"));
    }

    #[test]
    fn product_dir_falls_back_inside_drama_dir() {
        let dir = resolve_product_dir(None, None, Path::new("/素材/剧A"), "剧A", "微调输出");
        assert_eq!(dir, PathBuf::from("/素材/剧A/微调输出"));
    }

    /// 空白值按「没填」处理：界面清空输入框后不能把产物写到一个名为空的目录里。
    #[test]
    fn blank_override_and_root_are_treated_as_absent() {
        let dir = resolve_product_dir(
            Some(Path::new("")),
            Some(Path::new("")),
            Path::new("/素材/剧A"),
            "剧A",
            "报审包",
        );
        assert_eq!(dir, PathBuf::from("/素材/剧A/报审包"));
    }

    /// 剧名带分隔符或相对路径时不能被当作路径改写：产物必须留在输出根目录内。
    #[test]
    fn drama_name_cannot_escape_output_root() {
        for name in ["../别处", "/绝对/剧B", "剧C"] {
            let dir = resolve_product_dir(
                None,
                Some(Path::new("/tmp/根")),
                Path::new("/素材"),
                name,
                "报审包",
            );
            assert!(
                dir.starts_with("/tmp/根"),
                "{name} 逃出了输出根目录：{}",
                dir.display()
            );
        }
        assert_eq!(
            resolve_product_dir(
                None,
                Some(Path::new("/tmp/根")),
                Path::new("/素材"),
                "../别处",
                "报审包"
            ),
            PathBuf::from("/tmp/根/别处/报审包")
        );
    }

    /// 剧名净化后为空（`.` / `..` / 纯空白）时退回「不拼剧名」，不产生 `根/./报审包` 这类路径。
    #[test]
    fn unusable_drama_name_skips_the_name_component() {
        for name in ["", "   ", ".", ".."] {
            let dir = resolve_product_dir(
                None,
                Some(Path::new("/tmp/根")),
                Path::new("/素材"),
                name,
                "报审包",
            );
            assert_eq!(
                dir,
                PathBuf::from("/tmp/根/报审包"),
                "剧名 {name:?} 处理不对"
            );
        }
    }

    #[test]
    fn missing_file_falls_back_to_defaults() {
        let dir = temp_dir("missing");
        let loaded = load_settings(&dir.join("settings.json"));
        assert!(loaded.warning.is_none());
        assert!(loaded.settings.rules.is_none());
        assert_eq!(
            loaded.settings.effective_rules().min_episodes,
            ChecklistRules::default().min_episodes
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn round_trips_settings_and_creates_parent_dirs() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("nested").join("settings.json");
        let settings = AppSettings {
            rules: Some(ChecklistRules {
                min_episodes: 6,
                min_duration_sec: 45.0,
                max_duration_sec: 300.0,
                max_bitrate_bps: 6_000_000,
                prefer_portrait: false,
            }),
            ffprobe_path: Some(PathBuf::from("/opt/ffmpeg/bin/ffprobe")),
            ffmpeg_path: Some(PathBuf::from("/opt/ffmpeg/bin/ffmpeg")),
            output_dir: Some(PathBuf::from("/tmp/报审输出")),
            library_root: Some(PathBuf::from("/tmp/素材库")),
            ai_template: Some("你是短剧报审材料撰写助手…".to_string()),
            jianying_dir: Some(PathBuf::from("/Applications/JianyingPro.app")),
            jianying_draft_dir: None,
        };
        save_settings(&path, &settings).unwrap();

        let loaded = load_settings(&path);
        assert!(loaded.warning.is_none());
        let rules = loaded.settings.effective_rules();
        assert_eq!(rules.min_episodes, 6);
        assert!(!rules.prefer_portrait);
        assert_eq!(
            loaded.settings.ffprobe_path,
            Some(PathBuf::from("/opt/ffmpeg/bin/ffprobe"))
        );
        assert_eq!(
            loaded.settings.output_dir,
            Some(PathBuf::from("/tmp/报审输出"))
        );
        assert_eq!(
            loaded.settings.library_root,
            Some(PathBuf::from("/tmp/素材库"))
        );
        assert_eq!(
            loaded.settings.ai_template.as_deref(),
            Some("你是短剧报审材料撰写助手…")
        );
        assert_eq!(
            loaded.settings.jianying_dir,
            Some(PathBuf::from("/Applications/JianyingPro.app"))
        );
        assert!(loaded.settings.jianying_draft_dir.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_file_falls_back_with_warning() {
        let dir = temp_dir("corrupt");
        let path = dir.join("settings.json");
        std::fs::write(&path, b"{ not json").unwrap();

        let loaded = load_settings(&path);
        assert!(loaded.warning.is_some());
        assert!(loaded.settings.rules.is_none());

        // 覆盖写入后应恢复正常。
        save_settings(&path, &AppSettings::default()).unwrap();
        assert!(load_settings(&path).warning.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn partial_settings_use_defaults_for_missing_fields() {
        let dir = temp_dir("partial");
        let path = dir.join("settings.json");
        std::fs::write(&path, br#"{"ffprobePath":"/usr/bin/ffprobe"}"#).unwrap();

        let loaded = load_settings(&path);
        assert!(loaded.warning.is_none());
        assert!(loaded.settings.rules.is_none());
        assert_eq!(
            loaded.settings.ffprobe_path,
            Some(PathBuf::from("/usr/bin/ffprobe"))
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
