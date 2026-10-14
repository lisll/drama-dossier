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
    /// 默认输出目录；`None` 表示生成到「剧目目录/报审包」。
    pub output_dir: Option<PathBuf>,
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
            output_dir: Some(PathBuf::from("/tmp/报审输出")),
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
