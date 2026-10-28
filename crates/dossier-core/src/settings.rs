//! 用户设置：本地 JSON 持久化。
//!
//! 设计：
//! - 设置文件损坏或缺失时回退到内置默认值，并在 [`LoadedSettings::warning`] 中带回可展示的提示；
//! - 写入采用「临时文件 + 替换」，避免中途失败留下半个配置文件；
//! - 本模块只处理文件读写，不依赖 GUI 或 Tauri。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::checklist::ChecklistRules;

/// 一部剧的报审文字材料；空字符串视同未填写。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DramaMaterials {
    pub title: Option<String>,
    pub synopsis: Option<String>,
}

impl DramaMaterials {
    fn is_empty(&self) -> bool {
        let blank =
            |value: &Option<String>| value.as_deref().is_none_or(|text| text.trim().is_empty());
        blank(&self.title) && blank(&self.synopsis)
    }
}

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
    /// Python 解释器路径（AI 擦除的 worker 用）；`None` 表示从 `PATH` 查找。
    /// Windows 一键配置会静默安装官方 Python 到应用目录并写在这里。
    pub python_path: Option<PathBuf>,
    /// 默认输出目录（**产物根目录**）；`None` 表示产物写在剧目目录内。
    ///
    /// 非空时按 `<该目录>/<剧名>/<产物名>` 落盘，见 [`resolve_product_dir`]。
    pub output_dir: Option<PathBuf>,
    /// 上次使用的素材根目录；`None` 表示还没有扫描过，界面需要重新选择。
    pub library_root: Option<PathBuf>,
    /// AI 提示词模板（设置-AI 子页编辑；后续云端拉取后也缓存在这里）。
    pub ai_template: Option<String>,
    /// 剧映安装目录；`None` 表示使用自动探测结果。
    pub jianying_dir: Option<PathBuf>,
    /// 剪映草稿目录；`None` 表示使用剪映默认路径。
    pub jianying_draft_dir: Option<PathBuf>,
    /// 报审材料（剧名 / 简介），**按剧存**：键是规范化后的剧目目录绝对路径。
    ///
    /// 以前是三个全局字段（`dramaTitle` / `dramaSynopsis` / `posterPath`），哪部剧都读同一份，
    /// 导致剧与剧之间串用材料。旧字段无法判断属于哪部剧，读取时直接忽略（serde 默认忽略未知字段）。
    /// 海报不在这里存路径：海报只认本次输出目录里的「剧目海报.*」，天然按剧隔离。
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub drama_materials: BTreeMap<String, DramaMaterials>,
    /// LLM API 基础地址（OpenAI 兼容，如 https://api.deepseek.com/v1）；空表示未配置。
    pub llm_base_url: Option<String>,
    /// LLM API 密钥；仅落本机 settings.json，不打印日志。
    pub llm_api_key: Option<String>,
    /// LLM 模型名（如 deepseek-chat）。
    pub llm_model: Option<String>,
    /// 生图 API 基础地址（OpenAI images 兼容）；空表示未配置。
    pub img_base_url: Option<String>,
    /// 生图 API 密钥；仅落本机 settings.json，不打印日志。
    pub img_api_key: Option<String>,
    /// 生图模型名（如 some-text2img-v2）。
    pub img_model: Option<String>,
    /// 深度处理输出目录；`None` 表示写在剧目目录内（同 `resolve_product_dir` 语义）。
    pub deep_output_dir: Option<PathBuf>,
    /// 深度处理重组的单集最短时长（秒）；`None` 表示默认 60。
    pub recut_min_sec: Option<u32>,
    /// 深度处理重组的单集最长时长（秒）；`None` 表示默认 70。
    pub recut_max_sec: Option<u32>,
    /// 剧名来源开关：`None`/`true` 表示 AI 生成，`false` 表示本地手动填写。
    pub ai_title_enabled: Option<bool>,
    /// 简介来源开关：`None`/`true` 表示 AI 生成，`false` 表示本地手动填写。
    pub ai_synopsis_enabled: Option<bool>,
    /// 海报来源开关：`None`/`true` 表示 AI 生成，`false` 表示本地图片。
    pub ai_poster_enabled: Option<bool>,
    /// ASR 引擎：`openai`（openai-whisper 兼容 CLI）或 `whisper_cpp`。
    pub asr_engine: Option<String>,
    /// ASR 可执行文件；空/None 表示从 PATH 查找 `whisper` / `whisper-cli`。
    pub asr_executable: Option<PathBuf>,
    /// openai-whisper 模型名，或 whisper.cpp 的模型文件路径。
    pub asr_model: Option<String>,
    /// OCR 可执行文件；空/None 表示从 PATH 查找 `tesseract`。
    pub ocr_executable: Option<PathBuf>,
    /// 深度处理前用于理解剧情的分集数；默认前 4 集。
    pub content_sample_count: Option<u32>,
    /// 深度处理时是否自动执行“内容提取 + 报审材料生成”编排。
    pub material_pipeline_enabled: Option<bool>,
    /// 深度处理去重配方开关：`None`/`true` 表示启用（剧名 seed 派生确定性扰动
    /// + 机型元数据 + 配方存证），`false` 关闭。
    pub dedup_recipe_enabled: Option<bool>,
    /// 深度处理 AI 合规角标烧录开关：`None`/`true` 表示把
    /// 「内容由AI生成·纯属虚构」烧进成片画面，`false` 关闭。
    pub ai_badge_enabled: Option<bool>,
}

impl AppSettings {
    /// 生效的检查规则：用户覆盖优先，否则使用内置默认。
    pub fn effective_rules(&self) -> ChecklistRules {
        self.rules.clone().unwrap_or_default()
    }

    /// 深度处理重组单集时长区间的生效值：未配置时默认 60~70 秒。
    /// 集数不再直接配置，由「素材总时长 ÷ 单集目标时长」反推。
    pub fn effective_recut_range(&self) -> (u32, u32) {
        let min = self.recut_min_sec.unwrap_or(60).clamp(5, 3600);
        let max = self.recut_max_sec.unwrap_or(70).clamp(min + 5, 3600);
        (min, max)
    }

    /// 参与剧情理解的分集数：默认前 4 集，最多 16 集。
    pub fn effective_content_sample_count(&self) -> u32 {
        self.content_sample_count.unwrap_or(4).clamp(1, 16)
    }

    /// 深度处理默认开启材料流水线；如用户显式关闭则不再自动请求模型。
    pub fn effective_material_pipeline_enabled(&self) -> bool {
        self.material_pipeline_enabled.unwrap_or(true)
    }

    /// 深度处理默认启用去重配方（seed 确定性扰动）；显式关闭后成片只做重组 + 微帧。
    pub fn effective_dedup_recipe_enabled(&self) -> bool {
        self.dedup_recipe_enabled.unwrap_or(true)
    }

    /// 深度处理默认烧录 AI 合规角标；显式关闭后成片画面不带角标。
    pub fn effective_ai_badge_enabled(&self) -> bool {
        self.ai_badge_enabled.unwrap_or(true)
    }

    /// 读一部剧的材料；没存过返回空白。`key` 由调用方规范化（桌面层用规范化后的剧目目录）。
    pub fn materials_for(&self, key: &str) -> DramaMaterials {
        self.drama_materials.get(key).cloned().unwrap_or_default()
    }

    /// 写一部剧的材料；全空时删掉该条，不留空壳。只动这一条，不影响其他剧。
    pub fn set_materials_for(&mut self, key: &str, materials: DramaMaterials) {
        if materials.is_empty() {
            self.drama_materials.remove(key);
        } else {
            self.drama_materials.insert(key.to_string(), materials);
        }
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

/// 严格读取失败的两种情况：写入方据此决定拒绝写入还是先备份再覆盖。
#[derive(Debug)]
enum SettingsReadError {
    /// 读不出来（权限、被其他程序占用等）：文件内容未知，任何覆盖都可能丢数据。
    Unreadable(String),
    /// 读出来了但解析失败（文件损坏）：内容已知，可以先备份再覆盖。
    Corrupt(String),
}

/// 严格读取：文件不存在视为默认设置（首次运行），读不出或解析失败都返回错误、不回退。
fn read_settings_strict(path: &Path) -> Result<AppSettings, SettingsReadError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AppSettings::default())
        }
        Err(error) => {
            return Err(SettingsReadError::Unreadable(format!(
                "读取设置文件失败（{}）：{error}",
                path.display()
            )))
        }
    };
    serde_json::from_slice::<AppSettings>(&bytes).map_err(|error| {
        SettingsReadError::Corrupt(format!("设置文件解析失败（{}）：{error}", path.display()))
    })
}

/// 严格读取失败时给用户的说明：损坏可以到「设置」页保存恢复；读不出来要先查权限或占用。
fn strict_read_message(error: &SettingsReadError) -> String {
    match error {
        SettingsReadError::Corrupt(reason) => format!(
            "{reason}。为避免覆盖已拒绝本次操作；请到「设置」页点「保存设置」恢复（损坏的旧文件会先备份）。"
        ),
        SettingsReadError::Unreadable(reason) => format!(
            "{reason}。为避免覆盖已拒绝本次操作；请检查设置文件的权限，或是否被其他程序占用。"
        ),
    }
}

/// 严格读取设置：文件不存在视为默认设置；损坏或读不出时返回可展示的错误（不回退默认值）。
/// 用于结果取决于真实设置、不能拿默认值凑数的场合（例如重做海报前检查生图配置）。
pub fn try_load_settings(path: &Path) -> Result<AppSettings, String> {
    read_settings_strict(path).map_err(|error| strict_read_message(&error))
}

/// 从 `path` 读取设置；文件不存在时静默返回默认值，读取或解析失败时回退默认值并带提示。
pub fn load_settings(path: &Path) -> LoadedSettings {
    match read_settings_strict(path) {
        Ok(settings) => LoadedSettings {
            settings,
            warning: None,
        },
        Err(SettingsReadError::Unreadable(reason)) => LoadedSettings {
            settings: AppSettings::default(),
            warning: Some(format!(
                "{reason}。已暂用默认设置；文件无法读取时不会被覆盖，请检查权限或是否被其他程序占用。"
            )),
        },
        Err(SettingsReadError::Corrupt(reason)) => LoadedSettings {
            settings: AppSettings::default(),
            warning: Some(format!(
                "{reason}。已暂用默认设置；在「设置」页保存时会先把旧文件备份为 settings.json.broken-<时间>，再覆盖。"
            )),
        },
    }
}

/// 写入设置；父目录不存在时自动创建。
///
/// 临时文件名带进程号与序号：多个写入方同时落盘时各写各的临时文件，
/// 不会把别人写了一半的内容 rename 成正式配置。
pub fn save_settings(path: &Path, settings: &AppSettings) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    let data = serde_json::to_vec_pretty(settings).map_err(std::io::Error::other)?;
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = path.with_extension(format!("json.{}.{seq}.tmp", std::process::id()));
    std::fs::write(&temp, data).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })?;
    std::fs::rename(&temp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
}

/// 进程内所有设置写入共用的锁：读、改、写在锁内一次完成。
fn settings_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // 某个写入方 panic 后锁会中毒；锁里没有需要保护的内存状态（数据在磁盘上），继续使用即可。
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 串行化的「读 → 改 → 写」：进程内所有**局部**设置写入方（材料回写、剧名、工具路径等）都走这里，
/// 避免并发时互相覆盖更新。
///
/// - `update` 拿到的是磁盘上的最新设置；它返回 `Err` 时不写盘、原样返回错误。
/// - 设置文件读不出或已损坏时**拒绝写入**、文件原样不动：此时手里只有默认值，写回去会把
///   API 密钥、工具路径和所有剧的材料一起抹掉。恢复路径是到「设置」页保存（见 [`replace_settings`]）。
/// - 锁只在本进程内有效（应用没有多实例同时写设置的场景）。
pub fn update_settings(
    path: &Path,
    update: impl FnOnce(&mut AppSettings) -> Result<(), String>,
) -> Result<AppSettings, String> {
    let _guard = settings_lock();
    let mut settings = read_settings_strict(path).map_err(|error| strict_read_message(&error))?;
    update(&mut settings)?;
    save_settings(path, &settings)
        .map_err(|error| format!("写入设置失败（{}）：{error}", path.display()))?;
    Ok(settings)
}

/// 「设置」页整份保存时的合并规则：用界面传来的设置替换整份，但保留只由后端写入的字段——
/// 界面从不回传它们，原样覆盖会把它们抹掉：
/// - `drama_materials`：按剧材料，由材料线程、`save_drama_title`、`clear_drama_materials` 维护；
/// - `python_path`：一键配置装好 Python 后写入，界面没有对应输入框，也没有清空入口。
pub fn merge_saved_settings(current: &mut AppSettings, incoming: AppSettings) {
    let materials = std::mem::take(&mut current.drama_materials);
    let python_path = current.python_path.take();
    *current = incoming;
    current.drama_materials = materials;
    current.python_path = python_path;
}

/// [`replace_settings`] 的结果：写入后的设置，以及（文件损坏时）旧文件的备份路径。
#[derive(Debug)]
pub struct ReplaceOutcome {
    pub settings: AppSettings,
    pub backup: Option<PathBuf>,
}

/// 「设置」页整份保存专用：与 [`update_settings`] 同一把锁，区别只在文件损坏时——
/// 先把旧文件复制成 `settings.json.broken-<YYYYMMDD-HHMMSS>`，复制成功才覆盖（`update` 拿到的是默认值）。
/// 文件读不出来（权限、占用）时复制也会失败，直接拒绝，不覆盖内容未知的文件。
pub fn replace_settings(
    path: &Path,
    update: impl FnOnce(&mut AppSettings) -> Result<(), String>,
) -> Result<ReplaceOutcome, String> {
    let _guard = settings_lock();
    let (mut settings, backup) = match read_settings_strict(path) {
        Ok(settings) => (settings, None),
        Err(SettingsReadError::Corrupt(reason)) => {
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
            let backup = path.with_extension(format!("json.broken-{stamp}"));
            std::fs::copy(path, &backup).map_err(|error| {
                format!(
                    "{reason}；备份旧文件到 {} 失败：{error}。为避免丢失已拒绝覆盖。",
                    backup.display()
                )
            })?;
            (AppSettings::default(), Some(backup))
        }
        Err(SettingsReadError::Unreadable(reason)) => {
            return Err(format!(
                "{reason}。文件内容无法读取，已拒绝覆盖；请检查权限或是否被其他程序占用。"
            ))
        }
    };
    update(&mut settings)?;
    save_settings(path, &settings)
        .map_err(|error| format!("写入设置失败（{}）：{error}", path.display()))?;
    Ok(ReplaceOutcome { settings, backup })
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
/// 桌面端（`src-tauri`）通过 `dossier_core::resolve_product_dir` 使用这个函数；这条公开路径
/// 由 `tests/settings_product_dir.rs` 换一个 crate 视角编译，少一个 re-export 就会失败。
///
// 示例刻意写成 ```text 而不是 doctest：`tauri-build` 在 MSVC 下会往自己的 `OUT_DIR`
// 写一个空壳 `msvcrt.lib` 并加进原生库搜索路径，而配套的 `/NODEFAULTLIB:msvcrt.lib`
// 只作用于 `src-tauri` 自己的产物，doctest 的链接会先搜到这个空壳而失败（只影响
// Windows CI）。不要改回可执行代码块——可执行版本就是上面那个集成测试。
/// ```text
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
    fn legacy_global_material_fields_are_ignored() {
        // 旧版设置文件里的三个全局字段：读得进来、其余设置不丢，但不再串到任何剧上。
        let dir = temp_dir("legacy");
        let path = dir.join("settings.json");
        std::fs::write(
            &path,
            r#"{"outputDir":"/产物","dramaTitle":"旧剧名","dramaSynopsis":"旧简介","posterPath":"/x/剧目海报.png"}"#,
        )
        .unwrap();
        let loaded = load_settings(&path);
        assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
        assert_eq!(loaded.settings.output_dir, Some(PathBuf::from("/产物")));
        assert!(loaded.settings.drama_materials.is_empty());
        // 保存后旧字段消失，不会再被读回。
        save_settings(&path, &loaded.settings).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("dramaTitle"), "{raw}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn drama_materials_are_isolated_per_drama() {
        let mut settings = AppSettings::default();
        settings.set_materials_for(
            "/素材/剧A",
            DramaMaterials {
                title: Some("剧A名".into()),
                synopsis: Some("剧A简介".into()),
            },
        );
        settings.set_materials_for(
            "/素材/剧B",
            DramaMaterials {
                title: Some("剧B名".into()),
                synopsis: None,
            },
        );
        assert_eq!(
            settings.materials_for("/素材/剧A").title.as_deref(),
            Some("剧A名")
        );
        assert_eq!(settings.materials_for("/素材/剧B").synopsis, None);
        assert_eq!(
            settings.materials_for("/素材/剧C"),
            DramaMaterials::default()
        );

        // 只改剧 B，剧 A 不受影响；写全空则删除该条。
        settings.set_materials_for("/素材/剧B", DramaMaterials::default());
        assert!(!settings.drama_materials.contains_key("/素材/剧B"));
        assert_eq!(
            settings.materials_for("/素材/剧A").synopsis.as_deref(),
            Some("剧A简介")
        );
        // 只有空白字符也算空。
        settings.set_materials_for(
            "/素材/剧A",
            DramaMaterials {
                title: Some("  ".into()),
                synopsis: None,
            },
        );
        assert!(settings.drama_materials.is_empty());
    }

    #[test]
    fn drama_materials_round_trip_through_disk() {
        let dir = temp_dir("materials");
        let path = dir.join("settings.json");
        let mut settings = AppSettings::default();
        settings.set_materials_for(
            "/素材/剧A",
            DramaMaterials {
                title: Some("剧A名".into()),
                synopsis: None,
            },
        );
        save_settings(&path, &settings).unwrap();
        let loaded = load_settings(&path).settings;
        assert_eq!(
            loaded.materials_for("/素材/剧A").title.as_deref(),
            Some("剧A名")
        );
        let _ = std::fs::remove_dir_all(dir);
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
    fn content_pipeline_defaults_and_clamps_are_stable() {
        let mut settings = AppSettings::default();
        assert_eq!(settings.effective_content_sample_count(), 4);
        assert!(settings.effective_material_pipeline_enabled());

        settings.content_sample_count = Some(0);
        assert_eq!(settings.effective_content_sample_count(), 1);
        settings.content_sample_count = Some(99);
        assert_eq!(settings.effective_content_sample_count(), 16);

        settings.material_pipeline_enabled = Some(false);
        assert!(!settings.effective_material_pipeline_enabled());
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
            python_path: Some(PathBuf::from("C:/Apps/runtime/python/python.exe")),
            output_dir: Some(PathBuf::from("/tmp/报审输出")),
            library_root: Some(PathBuf::from("/tmp/素材库")),
            ai_template: Some("你是短剧报审材料撰写助手…".to_string()),
            jianying_dir: Some(PathBuf::from("/Applications/JianyingPro.app")),
            jianying_draft_dir: None,
            // 演示用例只关心旧字段；新增配置项走默认值即可。
            ..AppSettings::default()
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

    #[test]
    fn concurrent_updates_do_not_lose_writes_or_leave_temp_files() {
        let dir = temp_dir("concurrent");
        let path = dir.join("settings.json");
        let handles: Vec<_> = (0..8)
            .map(|worker| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for round in 0..50 {
                        update_settings(&path, |settings| {
                            settings.set_materials_for(
                                &format!("/剧目/{worker}"),
                                DramaMaterials {
                                    title: Some(format!("剧{worker}-{round}")),
                                    synopsis: None,
                                },
                            );
                            Ok(())
                        })
                        .unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let loaded = load_settings(&path);
        assert!(loaded.warning.is_none());
        for worker in 0..8 {
            assert_eq!(
                loaded
                    .settings
                    .materials_for(&format!("/剧目/{worker}"))
                    .title,
                Some(format!("剧{worker}-49"))
            );
        }
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "残留临时文件：{leftovers:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // Windows 上多个线程同时 rename 到同一目标时，目标正被替换或临时文件被杀毒软件占用都可能
    // 返回 ACCESS_DENIED，测试会偶发失败；那边由加锁版本的并发测试覆盖。
    #[cfg(unix)]
    #[test]
    fn unlocked_concurrent_saves_never_leave_partial_file_or_temp_files() {
        // 不经过锁、直接并发调 save_settings：验证的是「临时文件名唯一 + rename 替换」本身。
        let dir = temp_dir("unlocked-save");
        let path = dir.join("settings.json");
        let handles: Vec<_> = (0..8)
            .map(|worker| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for round in 0..50 {
                        let settings = AppSettings {
                            recut_min_sec: Some(worker * 100 + round),
                            ..Default::default()
                        };
                        save_settings(&path, &settings).unwrap();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let loaded = load_settings(&path);
        assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
        assert!(loaded.settings.recut_min_sec.is_some());
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_file_rejects_partial_updates_and_is_backed_up_on_replace() {
        let dir = temp_dir("corrupt-write");
        let path = dir.join("settings.json");
        std::fs::write(&path, b"{ not json").unwrap();
        // 局部写入：拒绝，文件字节不变，闭包不被调用。
        let mut called = false;
        let error = update_settings(&path, |_| {
            called = true;
            Ok(())
        })
        .unwrap_err();
        assert!(!called);
        assert!(error.contains("拒绝"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), b"{ not json");
        // 整份保存：先备份再覆盖。
        let outcome = replace_settings(&path, |settings| {
            settings.recut_min_sec = Some(42);
            Ok(())
        })
        .unwrap();
        let backup = outcome.backup.expect("应有备份");
        assert!(backup
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("settings.json.broken-"));
        assert_eq!(std::fs::read(&backup).unwrap(), b"{ not json");
        let loaded = load_settings(&path);
        assert!(loaded.warning.is_none());
        assert_eq!(loaded.settings.recut_min_sec, Some(42));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn merge_keeps_backend_only_fields_from_disk() {
        let mut current = AppSettings {
            python_path: Some(PathBuf::from("C:/app/python/python.exe")),
            recut_min_sec: Some(10),
            ..Default::default()
        };
        current.set_materials_for(
            "/剧目/甲",
            DramaMaterials {
                title: Some("甲".into()),
                synopsis: None,
            },
        );
        // 界面从不回传 python_path / drama_materials；其他字段以界面为准。
        let incoming = AppSettings {
            recut_min_sec: Some(20),
            ..Default::default()
        };
        merge_saved_settings(&mut current, incoming);
        assert_eq!(current.recut_min_sec, Some(20));
        assert_eq!(
            current.python_path,
            Some(PathBuf::from("C:/app/python/python.exe"))
        );
        assert_eq!(
            current.materials_for("/剧目/甲").title.as_deref(),
            Some("甲")
        );
    }

    #[test]
    fn strict_load_reports_corruption_instead_of_defaults() {
        let dir = temp_dir("strict-load");
        let path = dir.join("settings.json");
        assert!(try_load_settings(&path).is_ok(), "文件不存在视为默认设置");
        std::fs::write(&path, b"{ broken").unwrap();
        let error = try_load_settings(&path).unwrap_err();
        assert!(
            error.contains("保存设置"),
            "损坏时应指向设置页恢复：{error}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_is_written_by_both_writers_without_backup() {
        let dir = temp_dir("missing-write");
        let path = dir.join("settings.json");
        update_settings(&path, |settings| {
            settings.recut_min_sec = Some(7);
            Ok(())
        })
        .unwrap();
        assert_eq!(load_settings(&path).settings.recut_min_sec, Some(7));
        std::fs::remove_file(&path).unwrap();
        let outcome = replace_settings(&path, |_| Ok(())).unwrap();
        assert!(outcome.backup.is_none());
        assert!(path.is_file());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_file_is_never_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("unreadable");
        let path = dir.join("settings.json");
        save_settings(&path, &AppSettings::default()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        // root 不受权限位约束：读得出来就说明本环境测不了这一条，跳过。
        if std::fs::read(&path).is_ok() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            std::fs::remove_dir_all(&dir).ok();
            eprintln!("以 root 运行，跳过不可读文件用例");
            return;
        }
        assert!(update_settings(&path, |_| Ok(())).is_err());
        assert!(replace_settings(&path, |_| Ok(())).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_settings(&path).warning.is_none(), "文件内容应未被改动");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn update_settings_error_leaves_file_untouched() {
        let dir = temp_dir("update-err");
        let path = dir.join("settings.json");
        save_settings(&path, &AppSettings::default()).unwrap();
        let before = std::fs::read(&path).unwrap();
        let result = update_settings(&path, |settings| {
            settings.recut_min_sec = Some(1);
            Err("拒绝".into())
        });
        assert_eq!(result.unwrap_err(), "拒绝");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::remove_dir_all(&dir).ok();
    }
}
