//! 运行时组件的落盘管理：安装、校验、回滚。
//!
//! 磁盘布局（`<root>` 默认可由 CLI 用 `--root` 指定）：
//!
//! ```text
//! <root>/<component-id>/
//!   CURRENT                 # 文本文件：当前生效版本
//!   state.json              # 版本历史（时间、文件数、字节数）
//!   versions/<version>/...  # 每个版本一份完整内容（互不覆盖）
//!   staging/                # 安装暂存区（每次安装前清空）
//!     download/<file>       # 下载产物
//!     extract/              # 解压产物
//! ```
//!
//! 三条硬规则：
//! 1. **先校验再落地**：任何文件的 SHA-256/大小对不上，整次安装失败且不改变现状；
//! 2. **换版本不改历史**：新版本写进 `versions/<version>`，`CURRENT` 指向它，
//!    于是「回滚」只是改一个指针，不需要重新下载；
//! 3. **暂存区路径固定**（`staging/`），这样安装过程可以被测试完整驱动——
//!    测试只要预置暂存区内容并让假执行器返回成功，就能走完真实安装路径。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::command::{CommandError, CommandRunner, CommandSpec};
use crate::digest::{is_sha256_hex, sha256_bytes, sha256_file};
use crate::manifest::{is_safe_relative_path, Component, ManifestError, RuntimeManifest};

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// 生成清单时忽略的目录/文件名。
const IGNORED_NAMES: &[&str] = &[
    "__pycache__",
    ".git",
    ".DS_Store",
    ".staging",
    "staging",
    ".download",
];
/// 生成清单时忽略的后缀。
const IGNORED_SUFFIXES: &[&str] = &[".pyc", ".pyo", ".tmp", ".part", ".log"];
/// 单个组件的文件数上限（防止把整个数据集当组件打包进清单）。
const MAX_GENERATED_FILES: usize = 20_000;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("文件操作失败（{path}）：{source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error("清单里没有组件 `{0}`")]
    UnknownComponent(String),
    #[error("安装源不存在：{0}")]
    SourceMissing(PathBuf),
    #[error("安装源不是目录：{0}")]
    SourceNotDirectory(PathBuf),
    #[error("组件 {id} 的下载失败（{url}）：{message}")]
    Download {
        id: String,
        url: String,
        message: String,
    },
    #[error("组件 {id} 的下载产物校验失败：期望 {expected}，实际 {actual}")]
    ArtifactDigestMismatch {
        id: String,
        expected: String,
        actual: String,
    },
    #[error("组件 {id} 没有声明下载地址，无法在线安装")]
    NoDownloadUrl { id: String },
    #[error("组件 {id} 的源清单缺少产物 SHA-256，拒绝下载未校验的产物")]
    MissingArtifactDigest { id: String },
    #[error("解压失败（{tool}）：{message}")]
    Extract { tool: String, message: String },
    #[error("组件 {id} 的源目录缺少文件 {path}")]
    SourceFileMissing { id: String, path: String },
    #[error("组件 {id} 的文件 {path} 校验失败：期望 {expected}，实际 {actual}")]
    FileDigestMismatch {
        id: String,
        path: String,
        expected: String,
        /// 实际摘要；文件不存在时是「文件不存在」。
        actual: String,
    },
    #[error("组件 {id} 的文件 {path} 大小不符：期望 {expected} 字节，实际 {actual} 字节")]
    FileSizeMismatch {
        id: String,
        path: String,
        expected: u64,
        actual: u64,
    },
    #[error("组件 {id} 没有已安装的版本")]
    NotInstalled { id: String },
    #[error("组件 {id} 只有一个已安装版本，没有可回滚的目标")]
    NoPreviousVersion { id: String },
    #[error("组件 {id} 的状态文件损坏：{message}")]
    InvalidState { id: String, message: String },
    #[error(transparent)]
    Command(#[from] CommandError),
}

/// 下载工具。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Downloader {
    Curl(String),
    Wget(String),
}

impl Default for Downloader {
    fn default() -> Self {
        Self::Curl("curl".to_string())
    }
}

impl Downloader {
    pub fn program(&self) -> &str {
        match self {
            Self::Curl(program) | Self::Wget(program) => program,
        }
    }

    fn args(&self, url: &str, destination: &Path) -> Vec<String> {
        let dest = destination.display().to_string();
        match self {
            // `--fail` 让 4xx/5xx 变成非零退出码，避免把错误页当成功下载。
            Self::Curl(_) => vec![
                "-L".into(),
                "--fail".into(),
                "--silent".into(),
                "--show-error".into(),
                "-o".into(),
                dest,
                url.to_string(),
            ],
            Self::Wget(_) => vec!["-q".into(), "-O".into(), dest, url.to_string()],
        }
    }
}

/// 解压工具。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Archiver {
    SevenZip(String),
    Tar(String),
}

impl Default for Archiver {
    fn default() -> Self {
        Self::Tar("tar".to_string())
    }
}

impl Archiver {
    pub fn program(&self) -> &str {
        match self {
            Self::SevenZip(program) | Self::Tar(program) => program,
        }
    }

    fn build(&self, archive: &Path, destination: &Path) -> CommandSpec {
        match self {
            Self::SevenZip(program) => CommandSpec::new(program.clone())
                .arg("x")
                .arg("-y")
                .arg(format!("-o{}", destination.display()))
                .arg(archive.display().to_string())
                .timeout(EXTRACT_TIMEOUT),
            Self::Tar(program) => CommandSpec::new(program.clone())
                .arg("-xf")
                .arg(archive.display().to_string())
                .arg("-C")
                .arg(destination.display().to_string())
                .timeout(EXTRACT_TIMEOUT),
        }
    }
}

/// 存储配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreConfig {
    pub root: PathBuf,
    pub downloader: Downloader,
    pub archiver: Archiver,
}

impl StoreConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            downloader: Downloader::default(),
            archiver: Archiver::default(),
        }
    }
}

/// 一个文件的校验结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileCheck {
    pub path: String,
    pub expected_sha256: String,
    #[serde(default)]
    pub actual_sha256: Option<String>,
    #[serde(default)]
    pub expected_size: Option<u64>,
    #[serde(default)]
    pub actual_size: Option<u64>,
    pub ok: bool,
    /// 不通过时的简短说明。
    #[serde(default)]
    pub detail: Option<String>,
}

/// 一个组件的校验结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentVerification {
    pub id: String,
    #[serde(default)]
    pub version: Option<String>,
    pub dir: PathBuf,
    #[serde(default)]
    pub files: Vec<FileCheck>,
    /// 已安装但不在清单里的文件（不是错误，但要告知）。
    #[serde(default)]
    pub extra_files: Vec<String>,
    /// 硬性问题（缺失/摘要不符）。
    #[serde(default)]
    pub issues: Vec<String>,
    /// 每一条清单文件都通过校验。
    pub verified: bool,
}

/// 组件在磁盘上的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentState {
    /// 完全没装。
    Missing,
    /// 有目录但文件不完整/摘要不符。
    Incomplete,
    /// 已安装且校验通过，版本与清单一致。
    Installed,
    /// 已安装且校验通过，但版本与清单声明不同。
    VersionMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentStatus {
    pub id: String,
    pub role: String,
    pub optional: bool,
    pub declared_version: String,
    #[serde(default)]
    pub installed_version: Option<String>,
    pub state: ComponentState,
    pub detail: String,
    #[serde(default)]
    pub issues: Vec<String>,
}

impl ComponentStatus {
    /// 这份状态是否满足「清单要求」（可选组件缺失也算满足）。
    pub fn satisfied(&self) -> bool {
        match self.state {
            ComponentState::Installed => true,
            ComponentState::Missing => self.optional,
            ComponentState::Incomplete | ComponentState::VersionMismatch => false,
        }
    }
}

/// 安装结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstallReport {
    pub id: String,
    pub version: String,
    pub dir: PathBuf,
    pub file_count: usize,
    pub total_bytes: u64,
    #[serde(default)]
    pub replaced_version: Option<String>,
    /// 安装后重新校验的结果。
    pub verified: bool,
}

/// 回滚结果。
///
/// `files_present` 只说明「目标版本里清单声明的文件都在」——摘要校验只对清单声明的
/// **那一版**有意义，回滚到的旧版本内容本来就和当前清单不同，拿新清单的摘要去比会
/// 必然失败，所以这里不做摘要比对（要做也要拿旧版本的清单）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RollbackReport {
    pub id: String,
    pub from: String,
    pub to: String,
    pub dir: PathBuf,
    pub files_present: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoreState {
    #[serde(default)]
    current: Option<String>,
    #[serde(default)]
    versions: Vec<VersionRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionRecord {
    version: String,
    installed_at: String,
    file_count: usize,
    total_bytes: u64,
}

/// 组件存储。
#[derive(Debug, Clone)]
pub struct RuntimeStore {
    config: StoreConfig,
}

impl RuntimeStore {
    pub fn new(config: StoreConfig) -> Self {
        Self { config }
    }

    pub fn root(&self) -> &Path {
        &self.config.root
    }

    pub fn config(&self) -> &StoreConfig {
        &self.config
    }

    /// 组件目录：`<root>/<id>`。
    pub fn component_root(&self, id: &str) -> PathBuf {
        self.config.root.join(id)
    }

    /// 某个版本的目录。
    pub fn version_dir(&self, id: &str, version: &str) -> PathBuf {
        self.component_root(id).join("versions").join(version)
    }

    pub fn staging_dir(&self, id: &str) -> PathBuf {
        self.component_root(id).join("staging")
    }

    fn state_path(&self, id: &str) -> PathBuf {
        self.component_root(id).join("state.json")
    }

    fn current_path(&self, id: &str) -> PathBuf {
        self.component_root(id).join("CURRENT")
    }

    /// 当前生效版本。
    pub fn current_version(&self, id: &str) -> Result<Option<String>, StoreError> {
        let state = self.load_state(id)?;
        if let Some(current) = state.current {
            return Ok(Some(current));
        }
        Ok(state.versions.last().map(|record| record.version.clone()))
    }

    /// 已经安装过的版本（按安装顺序，最新在后）。
    pub fn installed_versions(&self, id: &str) -> Result<Vec<String>, StoreError> {
        Ok(self
            .load_state(id)?
            .versions
            .into_iter()
            .map(|record| record.version)
            .collect())
    }

    /// 当前版本对应的目录（未安装时返回 `None`）。
    pub fn resolve(&self, id: &str) -> Result<Option<PathBuf>, StoreError> {
        let Some(version) = self.current_version(id)? else {
            return Ok(None);
        };
        let dir = self.version_dir(id, &version);
        if dir.is_dir() {
            Ok(Some(dir))
        } else {
            Ok(None)
        }
    }

    fn load_state(&self, id: &str) -> Result<StoreState, StoreError> {
        let path = self.state_path(id);
        match fs::read(&path) {
            Ok(raw) => serde_json::from_slice(&raw).map_err(|error| StoreError::InvalidState {
                id: id.to_string(),
                message: error.to_string(),
            }),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                Ok(StoreState::default())
            }
            Err(source) => Err(StoreError::Io { path, source }),
        }
    }

    fn save_state(&self, id: &str, state: &StoreState) -> Result<(), StoreError> {
        let path = self.state_path(id);
        let raw = serde_json::to_vec_pretty(state).map_err(|error| StoreError::InvalidState {
            id: id.to_string(),
            message: error.to_string(),
        })?;
        fs::write(&path, raw).map_err(|source| StoreError::Io { path, source })?;
        if let Some(current) = &state.current {
            let current_path = self.current_path(id);
            fs::write(&current_path, format!("{current}\n")).map_err(|source| StoreError::Io {
                path: current_path,
                source,
            })?;
        }
        Ok(())
    }

    /// 校验一个已安装组件。
    pub fn verify_component(
        &self,
        component: &Component,
    ) -> Result<ComponentVerification, StoreError> {
        let version = self.current_version(&component.id)?;
        let dir = match &version {
            Some(version) => self.version_dir(&component.id, version),
            None => self.component_root(&component.id),
        };
        Ok(verify_dir(&component.id, version.clone(), &dir, component))
    }

    /// 校验清单里的全部组件。
    pub fn status_all(&self, manifest: &RuntimeManifest) -> Vec<ComponentStatus> {
        let mut out = Vec::new();
        for component in &manifest.components {
            let verification = match self.verify_component(component) {
                Ok(verification) => verification,
                Err(error) => {
                    out.push(ComponentStatus {
                        id: component.id.clone(),
                        role: component.role.clone(),
                        optional: component.optional,
                        declared_version: component.version.clone(),
                        installed_version: None,
                        state: ComponentState::Incomplete,
                        detail: error.to_string(),
                        issues: vec![error.to_string()],
                    });
                    continue;
                }
            };
            let state = if verification.version.is_none() {
                ComponentState::Missing
            } else if !verification.verified {
                ComponentState::Incomplete
            } else if verification.version.as_deref() == Some(component.version.as_str()) {
                ComponentState::Installed
            } else {
                ComponentState::VersionMismatch
            };
            let detail = match state {
                ComponentState::Missing => {
                    if component.optional {
                        "未安装（可选）".to_string()
                    } else {
                        "未安装".to_string()
                    }
                }
                ComponentState::Incomplete => verification
                    .issues
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "文件校验未通过".to_string()),
                ComponentState::Installed => format!(
                    "已安装 {}，{} 个文件校验通过",
                    verification.version.clone().unwrap_or_default(),
                    verification.files.len()
                ),
                ComponentState::VersionMismatch => format!(
                    "已安装 {}，清单要求 {}",
                    verification.version.clone().unwrap_or_default(),
                    component.version
                ),
            };
            out.push(ComponentStatus {
                id: component.id.clone(),
                role: component.role.clone(),
                optional: component.optional,
                declared_version: component.version.clone(),
                installed_version: verification.version.clone(),
                state,
                detail,
                issues: verification.issues.clone(),
            });
        }
        out
    }

    /// 从本地目录安装（目录里应含清单声明的相对路径）。
    pub fn install_local(
        &self,
        component: &Component,
        source_dir: &Path,
    ) -> Result<InstallReport, StoreError> {
        if !source_dir.exists() {
            return Err(StoreError::SourceMissing(source_dir.to_path_buf()));
        }
        if !source_dir.is_dir() {
            return Err(StoreError::SourceNotDirectory(source_dir.to_path_buf()));
        }

        let staging = self.staging_dir(&component.id);
        // 每次安装从干净暂存区开始，避免上一次的残留混进本次结果。
        let stage_root = staging.join(format!("stage-{}", std::process::id()));
        let _ = fs::remove_dir_all(&stage_root);
        create_dir_all(&stage_root)?;

        let mut total_bytes = 0_u64;
        for file in &component.files {
            let source = source_dir.join(&file.path);
            let expected_path = source.display().to_string();
            let metadata = fs::metadata(&source).map_err(|source_error| {
                if source_error.kind() == std::io::ErrorKind::NotFound {
                    StoreError::SourceFileMissing {
                        id: component.id.clone(),
                        path: file.path.clone(),
                    }
                } else {
                    StoreError::Io {
                        path: PathBuf::from(&expected_path),
                        source: source_error,
                    }
                }
            })?;
            if let Some(expected) = file.size {
                if metadata.len() != expected {
                    return Err(StoreError::FileSizeMismatch {
                        id: component.id.clone(),
                        path: file.path.clone(),
                        expected,
                        actual: metadata.len(),
                    });
                }
            }
            let actual = sha256_file(&source).map_err(|source_error| StoreError::Io {
                path: PathBuf::from(&expected_path),
                source: source_error,
            })?;
            if actual != file.sha256.to_lowercase() {
                return Err(StoreError::FileDigestMismatch {
                    id: component.id.clone(),
                    path: file.path.clone(),
                    expected: file.sha256.clone(),
                    actual,
                });
            }
            let destination = stage_root.join(&file.path);
            if let Some(parent) = destination.parent() {
                create_dir_all(parent)?;
            }
            fs::copy(&source, &destination).map_err(|source_error| StoreError::Io {
                path: destination.clone(),
                source: source_error,
            })?;
            if file.executable {
                set_executable(&destination)?;
            }
            total_bytes += metadata.len();
        }

        // 发布：先挪开旧目录，再把暂存目录改名就位；失败时不留半个版本。
        let version_dir = self.version_dir(&component.id, &component.version);
        let replaced_version = self.current_version(&component.id)?;
        if version_dir.exists() {
            fs::remove_dir_all(&version_dir).map_err(|source| StoreError::Io {
                path: version_dir.clone(),
                source,
            })?;
        }
        if let Some(parent) = version_dir.parent() {
            create_dir_all(parent)?;
        }
        fs::rename(&stage_root, &version_dir).map_err(|source| StoreError::Io {
            path: version_dir.clone(),
            source,
        })?;
        let _ = fs::remove_dir_all(&staging);

        let mut state = self.load_state(&component.id)?;
        state
            .versions
            .retain(|record| record.version != component.version);
        state.versions.push(VersionRecord {
            version: component.version.clone(),
            installed_at: chrono::Utc::now().to_rfc3339(),
            file_count: component.files.len(),
            total_bytes,
        });
        state.current = Some(component.version.clone());
        self.save_state(&component.id, &state)?;

        let verification = self.verify_component(component)?;
        Ok(InstallReport {
            id: component.id.clone(),
            version: component.version.clone(),
            dir: version_dir,
            file_count: component.files.len(),
            total_bytes,
            replaced_version: replaced_version.filter(|old| old != &component.version),
            verified: verification.verified,
        })
    }

    /// 在线安装：下载 → 校验产物摘要 → （必要时）解压 → 按本地安装流程落地。
    ///
    /// 下载产物先比对 `source.sha256` 再说其它，绝不把未校验的字节喂给解压器。
    pub fn install_from_url(
        &self,
        component: &Component,
        runner: &dyn CommandRunner,
    ) -> Result<InstallReport, StoreError> {
        let url = component
            .source
            .url
            .clone()
            .ok_or_else(|| StoreError::NoDownloadUrl {
                id: component.id.clone(),
            })?;
        let expected =
            component
                .source
                .sha256
                .clone()
                .ok_or_else(|| StoreError::MissingArtifactDigest {
                    id: component.id.clone(),
                })?;
        if !is_sha256_hex(&expected) {
            return Err(StoreError::MissingArtifactDigest {
                id: component.id.clone(),
            });
        }

        // 下载产物放在**持久**缓存目录，而不是随安装清理的暂存区：权重动辄上 GB，
        // 换版本重装时不该重复下载。缓存命中同样要过摘要校验——缓存坏了就删掉重下，
        // 而不是把坏字节当成功。
        let cache_dir = self.component_root(&component.id).join("downloads");
        create_dir_all(&cache_dir)?;
        let file_name = url_file_name(&url);
        let artifact = cache_dir.join(&file_name);
        if !artifact.exists() {
            // 先下到临时名再改名，避免半个文件被后来的安装当成有效缓存。
            let partial = cache_dir.join(format!("{file_name}.partial-{}", std::process::id()));
            let _ = fs::remove_file(&partial);
            let spec = CommandSpec::new(self.config.downloader.program().to_string())
                .args(self.config.downloader.args(&url, &partial))
                .timeout(DOWNLOAD_TIMEOUT);
            let output = runner.run(&spec).map_err(|error| StoreError::Download {
                id: component.id.clone(),
                url: url.clone(),
                message: error.to_string(),
            })?;
            if !output.success() {
                let _ = fs::remove_file(&partial);
                return Err(StoreError::Download {
                    id: component.id.clone(),
                    url: url.clone(),
                    message: format!(
                        "退出码 {}：{}",
                        output.code_label(),
                        first_line(&output.combined())
                    ),
                });
            }
            if !partial.exists() {
                return Err(StoreError::Download {
                    id: component.id.clone(),
                    url: url.clone(),
                    message: "命令返回成功但没有产生文件".to_string(),
                });
            }
            fs::rename(&partial, &artifact).map_err(|source| StoreError::Io {
                path: artifact.clone(),
                source,
            })?;
        }

        let actual = sha256_file(&artifact).map_err(|source| StoreError::Io {
            path: artifact.clone(),
            source,
        })?;
        if actual != expected.to_lowercase() {
            // 失败即封闭：坏产物/坏缓存都删掉，下次重跑会重新下载，而不是留下可疑字节。
            let _ = fs::remove_file(&artifact);
            return Err(StoreError::ArtifactDigestMismatch {
                id: component.id.clone(),
                expected,
                actual,
            });
        }

        let source_dir = match &component.source.archive {
            Some(format) => {
                let staging = self.staging_dir(&component.id);
                let _ = fs::remove_dir_all(&staging);
                let extract_dir = staging.join("extract");
                create_dir_all(&extract_dir)?;
                let _ = format;
                let spec = self.config.archiver.build(&artifact, &extract_dir);
                let output = runner.run(&spec).map_err(|error| StoreError::Extract {
                    tool: self.config.archiver.program().to_string(),
                    message: error.to_string(),
                })?;
                if !output.success() {
                    return Err(StoreError::Extract {
                        tool: self.config.archiver.program().to_string(),
                        message: format!(
                            "退出码 {}：{}",
                            output.code_label(),
                            first_line(&output.combined())
                        ),
                    });
                }
                match &component.source.layout {
                    Some(layout) => extract_dir.join(layout),
                    None => extract_dir,
                }
            }
            None => {
                // 单文件组件：把产物放到它声明的相对路径上。
                let single =
                    component
                        .files
                        .first()
                        .ok_or_else(|| StoreError::SourceFileMissing {
                            id: component.id.clone(),
                            path: "<未声明文件>".to_string(),
                        })?;
                if component.files.len() != 1 {
                    return Err(StoreError::Extract {
                        tool: "<无>".to_string(),
                        message: format!(
                            "组件 {} 声明了 {} 个文件却给了单文件下载源，无法判断如何摆放",
                            component.id,
                            component.files.len()
                        ),
                    });
                }
                let staging = self.staging_dir(&component.id);
                let _ = fs::remove_dir_all(&staging);
                let placed = staging.join("single").join(&single.path);
                if let Some(parent) = placed.parent() {
                    create_dir_all(parent)?;
                }
                // 用复制而不是改名：缓存要留着，旁边可能还有别的版本在装。
                fs::copy(&artifact, &placed).map_err(|source| StoreError::Io {
                    path: placed.clone(),
                    source,
                })?;
                staging.join("single")
            }
        };

        self.install_local(component, &source_dir)
    }

    /// 回滚到上一个已安装版本（只改指针，不重新下载）。
    pub fn rollback(&self, component: &Component) -> Result<RollbackReport, StoreError> {
        let mut state = self.load_state(&component.id)?;
        if state.versions.is_empty() {
            return Err(StoreError::NotInstalled {
                id: component.id.clone(),
            });
        }
        let current = state
            .current
            .clone()
            .unwrap_or_else(|| state.versions.last().unwrap().version.clone());
        let position = state
            .versions
            .iter()
            .position(|record| record.version == current)
            .ok_or_else(|| StoreError::InvalidState {
                id: component.id.clone(),
                message: format!("当前版本 {current} 不在历史记录里"),
            })?;
        if position == 0 {
            return Err(StoreError::NoPreviousVersion {
                id: component.id.clone(),
            });
        }
        let target = state.versions[position - 1].version.clone();
        let dir = self.version_dir(&component.id, &target);
        if !dir.is_dir() {
            return Err(StoreError::NotInstalled {
                id: format!("{}（版本 {} 的目录已丢失）", component.id, target),
            });
        }
        let files_present = component
            .files
            .iter()
            .all(|file| dir.join(&file.path).is_file());
        state.current = Some(target.clone());
        self.save_state(&component.id, &state)?;
        Ok(RollbackReport {
            id: component.id.clone(),
            from: current,
            to: target,
            dir,
            files_present,
        })
    }
}

/// 校验一个目录是否与组件清单一致。
pub fn verify_dir(
    id: &str,
    version: Option<String>,
    dir: &Path,
    component: &Component,
) -> ComponentVerification {
    let mut files = Vec::new();
    let mut issues = Vec::new();
    if !dir.is_dir() {
        return ComponentVerification {
            id: id.to_string(),
            version,
            dir: dir.to_path_buf(),
            files,
            extra_files: Vec::new(),
            issues: vec![format!("目录不存在：{}", dir.display())],
            verified: false,
        };
    }

    for file in &component.files {
        let path = dir.join(&file.path);
        let expected = file.sha256.to_lowercase();
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                let actual_size = metadata.len();
                let actual_sha256 = sha256_file(&path).ok();
                let size_ok = file.size.is_none_or(|size| size == actual_size);
                let digest_ok = actual_sha256
                    .as_deref()
                    .is_some_and(|actual| actual == expected);
                let ok = size_ok && digest_ok;
                let detail = if digest_ok && size_ok {
                    None
                } else if !digest_ok {
                    Some("内容摘要不符".to_string())
                } else {
                    Some("文件大小不符".to_string())
                };
                if let Some(detail) = &detail {
                    issues.push(format!("{}：{detail}", file.path));
                }
                files.push(FileCheck {
                    path: file.path.clone(),
                    expected_sha256: file.sha256.clone(),
                    actual_sha256,
                    expected_size: file.size,
                    actual_size: Some(actual_size),
                    ok,
                    detail,
                });
            }
            Ok(_) => {
                issues.push(format!("{}：不是普通文件", file.path));
                files.push(FileCheck {
                    path: file.path.clone(),
                    expected_sha256: file.sha256.clone(),
                    actual_sha256: None,
                    expected_size: file.size,
                    actual_size: None,
                    ok: false,
                    detail: Some("不是普通文件".to_string()),
                });
            }
            Err(_) => {
                issues.push(format!("{}：文件缺失", file.path));
                files.push(FileCheck {
                    path: file.path.clone(),
                    expected_sha256: file.sha256.clone(),
                    actual_sha256: None,
                    expected_size: file.size,
                    actual_size: None,
                    ok: false,
                    detail: Some("文件缺失".to_string()),
                });
            }
        }
    }

    let expected_paths: Vec<&str> = component
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    let extra_files = collect_relative_files(dir)
        .into_iter()
        .filter(|path| !expected_paths.contains(&path.as_str()))
        .collect::<Vec<String>>();

    ComponentVerification {
        id: id.to_string(),
        version,
        dir: dir.to_path_buf(),
        verified: issues.is_empty(),
        files,
        extra_files,
        issues,
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string()
}

fn create_dir_all(path: &Path) -> Result<(), StoreError> {
    fs::create_dir_all(path).map_err(|source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn set_executable(path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = fs::metadata(path).map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut permissions = metadata.permissions();
        permissions.set_mode(permissions.mode() | 0o755);
        fs::set_permissions(path, permissions).map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// 从 URL 里猜一个文件名（query 会去掉）。
fn url_file_name(url: &str) -> String {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let name = without_query
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("download");
    if name.is_empty() {
        "download".to_string()
    } else {
        name.to_string()
    }
}

/// 生成清单用的目录扫描结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerateOptions {
    pub id: String,
    pub role: String,
    pub version: String,
    pub title: Option<String>,
    pub optional: bool,
    pub requires: Vec<String>,
    pub url: Option<String>,
    /// 下载产物的 SHA-256（归档本身，不是解压后的文件）。给了 `url` 就必须给。
    pub artifact_sha256: Option<String>,
    pub archive: Option<String>,
    pub layout: Option<String>,
    pub license: Option<String>,
    pub backend: Option<String>,
    pub framework: Option<String>,
    pub entrypoint: Option<String>,
}

/// 扫描一个本地目录生成组件清单项（逐个文件算真实 SHA-256）。
///
/// 这是「模型/工具交付」的可执行入口：先把文件放好，再让工具生成带真实摘要的清单，
/// 而不是手抄摘要——手抄的摘要迟早会和实际文件不一致。
pub fn generate_component(
    directory: &Path,
    options: &GenerateOptions,
) -> Result<Component, StoreError> {
    if !directory.is_dir() {
        return Err(StoreError::SourceNotDirectory(directory.to_path_buf()));
    }
    let mut relative = collect_relative_files(directory);
    if relative.len() > MAX_GENERATED_FILES {
        relative.truncate(MAX_GENERATED_FILES);
    }
    let mut files = Vec::new();
    for path in relative {
        let absolute = directory.join(&path);
        let metadata = fs::metadata(&absolute).map_err(|source| StoreError::Io {
            path: absolute.clone(),
            source,
        })?;
        let digest = sha256_file(&absolute).map_err(|source| StoreError::Io {
            path: absolute.clone(),
            source,
        })?;
        files.push(crate::manifest::ComponentFile {
            path,
            sha256: digest,
            size: Some(metadata.len()),
            executable: is_executable(&metadata),
        });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));

    let model = match (&options.backend, &options.framework) {
        (Some(backend), Some(framework)) => Some(crate::manifest::ModelInfo {
            backend: backend.clone(),
            framework: framework.clone(),
            entrypoint: options.entrypoint.clone(),
        }),
        _ => None,
    };

    Ok(Component {
        id: options.id.clone(),
        role: options.role.clone(),
        title: options.title.clone(),
        version: options.version.clone(),
        optional: options.optional,
        requires: options.requires.clone(),
        source: crate::manifest::ComponentSource {
            url: options.url.clone(),
            sha256: options.artifact_sha256.clone(),
            archive: options.archive.clone(),
            layout: options.layout.clone(),
            license: options.license.clone(),
        },
        files,
        model,
        notes: None,
    })
}

/// 计算一个目录的「内容指纹」（用于比对两个目录是否是同一批文件）。
pub fn directory_digest(directory: &Path) -> Result<String, StoreError> {
    if !directory.is_dir() {
        return Err(StoreError::SourceNotDirectory(directory.to_path_buf()));
    }
    let mut parts = Vec::new();
    for path in collect_relative_files(directory) {
        let absolute = directory.join(&path);
        let digest = sha256_file(&absolute).map_err(|source| StoreError::Io {
            path: absolute.clone(),
            source,
        })?;
        parts.push(format!("{path}:{digest}"));
    }
    Ok(sha256_bytes(parts.join("\n").as_bytes()))
}

/// 递归收集目录下的相对文件路径（跳过忽略项，忽略符号链接）。
fn collect_relative_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if IGNORED_NAMES.contains(&name) {
                continue;
            }
            if IGNORED_SUFFIXES.iter().any(|suffix| name.ends_with(suffix)) {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(path);
            } else if metadata.is_file() {
                if let Ok(relative) = path.strip_prefix(root) {
                    let relative = relative.to_string_lossy().replace('\\', "/");
                    if is_safe_relative_path(&relative) {
                        out.push(relative);
                    }
                }
            }
        }
    }
    out.sort();
    out
}

fn is_executable(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{CommandOutput, FakeRunner};
    use crate::manifest::{ComponentFile, ComponentSource, RuntimeRoles};

    /// 测试用执行器装饰器：命中规则且执行成功时，把「这个工具本该写出的文件」
    /// 真的写到磁盘上——`curl -o` / `7z x` 的副作用正是这些文件，假执行器不写文件
    /// 就没法走完整安装路径。
    struct ProducingRunner {
        inner: FakeRunner,
        products: Vec<ProducedFiles>,
    }

    /// 一个「被模拟的工具本该写出的文件」：`(文件相对路径, 内容)`。
    type ProducedFile = (String, Vec<u8>);
    /// `(程序名, 落盘目录, 该程序产出的文件列表)`。
    type ProducedFiles = (String, PathBuf, Vec<ProducedFile>);

    impl ProducingRunner {
        fn new(inner: FakeRunner) -> Self {
            Self {
                inner,
                products: Vec::new(),
            }
        }

        #[must_use]
        fn produce(mut self, program: &str, dir: PathBuf, files: Vec<(&str, &[u8])>) -> Self {
            self.products.push((
                program.to_string(),
                dir,
                files
                    .into_iter()
                    .map(|(path, bytes)| (path.to_string(), bytes.to_vec()))
                    .collect(),
            ));
            self
        }

        fn programs(&self) -> Vec<String> {
            self.inner.programs()
        }

        fn calls(&self) -> Vec<crate::command::CommandSpec> {
            self.inner.calls()
        }
    }

    impl CommandRunner for ProducingRunner {
        fn run(
            &self,
            spec: &crate::command::CommandSpec,
        ) -> Result<crate::command::CommandOutput, crate::command::CommandError> {
            let produces_this_program = self
                .products
                .iter()
                .any(|(program, _, _)| program == &spec.program);
            // 没给这个程序配规则就当作成功：这个装饰器存在的前提就是「被模拟的工具
            // 会把文件写出来」，配规则只是为了覆盖退出码/stderr 这类场景。
            let output = match self.inner.run(spec) {
                Ok(output) => output,
                Err(crate::command::CommandError::NotFound { .. }) if produces_this_program => {
                    CommandOutput::ok("")
                }
                Err(error) => return Err(error),
            };
            if output.success() {
                for (program, dir, files) in &self.products {
                    if program != &spec.program {
                        continue;
                    }
                    // 下载类工具的目标是**参数里的那个文件**（`curl -o <path>`、
                    // `wget -O <path>`），不是我们提议的目录——store 会把下载写到
                    // `<name>.partial-<pid>`，只有跟着参数走才能复现真实副作用。
                    match (declared_output_path(spec), files.as_slice()) {
                        (Some(path), [(_, bytes)]) => write_bytes(&path, bytes),
                        _ => {
                            for (relative, bytes) in files {
                                write_bytes(&dir.join(relative), bytes);
                            }
                        }
                    }
                }
            }
            Ok(output)
        }
    }

    /// 从 `-o <path>` / `-O <path>` 里取出下载目标；解压类工具用 `-C`/`-o<dir>`，取不到。
    fn declared_output_path(spec: &crate::command::CommandSpec) -> Option<PathBuf> {
        let mut args = spec.args.iter();
        while let Some(arg) = args.next() {
            if arg == "-o" || arg == "-O" {
                return args.next().map(PathBuf::from);
            }
        }
        None
    }

    fn write_bytes(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("dossier-store-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn path(&self, relative: &str) -> PathBuf {
            self.dir.join(relative)
        }

        fn write(&self, relative: &str, content: &[u8]) -> PathBuf {
            let path = self.path(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, content).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// 造一个「源目录 + 对应清单项」的组合。
    fn make_component(
        fixture: &Fixture,
        id: &str,
        version: &str,
        files: &[(&str, &[u8])],
    ) -> Component {
        let mut declared = Vec::new();
        for (relative, content) in files {
            let path = fixture.write(&format!("src/{id}/{relative}"), content);
            let metadata = fs::metadata(&path).unwrap();
            declared.push(ComponentFile {
                path: (*relative).to_string(),
                sha256: sha256_file(&path).unwrap(),
                size: Some(metadata.len()),
                executable: false,
            });
        }
        Component {
            id: id.to_string(),
            role: RuntimeRoles::INPAINT_MODEL.to_string(),
            title: None,
            version: version.to_string(),
            optional: false,
            requires: Vec::new(),
            source: ComponentSource::default(),
            files: declared,
            model: Some(crate::manifest::ModelInfo {
                backend: "column".into(),
                framework: "pytorch".into(),
                entrypoint: Some("worker.py".into()),
            }),
            notes: None,
        }
    }

    fn open_store(root: &Path) -> RuntimeStore {
        RuntimeStore::new(StoreConfig::new(root))
    }

    #[test]
    fn install_verify_and_status() {
        let fixture = Fixture::new("install");
        let root = fixture.path("runtime");
        let component = make_component(
            &fixture,
            "column-model",
            "1.0",
            &[
                ("weights.bin", b"weights-v1"),
                ("worker.py", b"print('hi')"),
            ],
        );
        let store = open_store(&root);

        // 安装前：缺失。
        let status = store.status_all(&RuntimeManifest {
            schema_version: 1,
            generated_at: None,
            components: vec![component.clone()],
        });
        assert_eq!(status[0].state, ComponentState::Missing);
        assert!(!status[0].satisfied());

        let report = store
            .install_local(&component, &fixture.path("src/column-model"))
            .unwrap();
        assert_eq!(report.version, "1.0");
        assert_eq!(report.file_count, 2);
        assert!(report.verified);
        assert!(report.replaced_version.is_none());
        assert!(report.total_bytes > 0);

        // 安装后：CURRENT 指向 1.0，目录里文件齐全。
        assert_eq!(
            store.current_version("column-model").unwrap().as_deref(),
            Some("1.0")
        );
        let dir = store.resolve("column-model").unwrap().unwrap();
        assert_eq!(dir, root.join("column-model/versions/1.0"));
        assert_eq!(
            fs::read_to_string(dir.join("worker.py")).unwrap(),
            "print('hi')"
        );

        let verification = store.verify_component(&component).unwrap();
        assert!(verification.verified);
        assert!(verification.issues.is_empty());
        assert!(verification.extra_files.is_empty());

        let status = store.status_all(&RuntimeManifest {
            schema_version: 1,
            generated_at: None,
            components: vec![component.clone()],
        });
        assert_eq!(status[0].state, ComponentState::Installed);
        assert!(status[0].satisfied());
        assert!(status[0].detail.contains("2 个文件"));
    }

    #[test]
    fn install_refuses_sources_that_do_not_match_the_manifest() {
        let fixture = Fixture::new("mismatch");
        let root = fixture.path("runtime");
        let component = make_component(&fixture, "m", "1.0", &[("a.bin", b"original")]);
        let store = open_store(&root);

        // 篡改源文件内容 → 摘要不符，拒绝安装，磁盘状态不变。
        fixture.write("src/m/a.bin", b"tampered");
        let error = store
            .install_local(&component, &fixture.path("src/m"))
            .unwrap_err();
        assert!(matches!(error, StoreError::FileDigestMismatch { .. }));
        assert!(store.current_version("m").unwrap().is_none());
        assert!(!store.version_dir("m", "1.0").exists());

        // 大小先于摘要被检查：声明大小不符也拒绝。
        let mut sized = component.clone();
        sized.files[0].size = Some(9999);
        fixture.write("src/m/a.bin", b"original");
        assert!(matches!(
            store
                .install_local(&sized, &fixture.path("src/m"))
                .unwrap_err(),
            StoreError::FileSizeMismatch { .. }
        ));

        // 源目录里缺文件。
        let mut missing = component.clone();
        missing.files.push(ComponentFile {
            path: "extra.bin".into(),
            sha256: sha256_bytes(b"x"),
            size: Some(1),
            executable: false,
        });
        assert!(matches!(
            store
                .install_local(&missing, &fixture.path("src/m"))
                .unwrap_err(),
            StoreError::SourceFileMissing { .. }
        ));

        // 源目录本身不存在。
        assert!(matches!(
            store
                .install_local(&component, &fixture.path("nope"))
                .unwrap_err(),
            StoreError::SourceMissing(_)
        ));
    }

    #[test]
    fn verification_detects_tampering_and_reports_extra_files() {
        let fixture = Fixture::new("verify");
        let root = fixture.path("runtime");
        let component = make_component(&fixture, "v", "1.0", &[("a.bin", b"original")]);
        let store = open_store(&root);
        store
            .install_local(&component, &fixture.path("src/v"))
            .unwrap();

        // 安装后篡改已落盘的文件。
        let installed = root.join("v/versions/1.0/a.bin");
        fs::write(&installed, b"tampered!").unwrap();
        let verification = store.verify_component(&component).unwrap();
        assert!(!verification.verified);
        assert_eq!(
            verification.files[0].detail.as_deref(),
            Some("内容摘要不符")
        );
        assert!(verification.issues[0].contains("内容摘要不符"));

        // 删除文件 → 缺失。
        fs::remove_file(&installed).unwrap();
        let verification = store.verify_component(&component).unwrap();
        assert!(!verification.verified);
        assert_eq!(verification.files[0].detail.as_deref(), Some("文件缺失"));

        // 额外文件只算提示，不算失败。
        fs::write(&installed, b"original").unwrap();
        fs::write(root.join("v/versions/1.0/README.md"), b"docs").unwrap();
        let verification = store.verify_component(&component).unwrap();
        assert!(verification.verified);
        assert_eq!(verification.extra_files, vec!["README.md".to_string()]);

        // 目录整体缺失。
        fs::remove_dir_all(root.join("v")).unwrap();
        let verification = store.verify_component(&component).unwrap();
        assert!(!verification.verified);
        assert!(verification.issues[0].contains("目录不存在"));
    }

    #[test]
    fn version_mismatch_is_reported_but_installation_stays_valid() {
        let fixture = Fixture::new("versionmismatch");
        let root = fixture.path("runtime");
        let component = make_component(&fixture, "vm", "2.0", &[("a.bin", b"v2")]);
        let store = open_store(&root);
        store
            .install_local(&component, &fixture.path("src/vm"))
            .unwrap();

        let mut older = component.clone();
        older.version = "1.0".into();
        let status = store.status_all(&RuntimeManifest {
            schema_version: 1,
            generated_at: None,
            components: vec![older],
        });
        assert_eq!(status[0].state, ComponentState::VersionMismatch);
        assert_eq!(status[0].installed_version.as_deref(), Some("2.0"));
        assert!(!status[0].satisfied());
        assert!(status[0].detail.contains("清单要求 1.0"));
    }

    #[test]
    fn install_then_rollback_switches_pointer_without_redownload() {
        let fixture = Fixture::new("rollback");
        let root = fixture.path("runtime");
        let store = open_store(&root);
        // 两次安装共用同一个源目录：先按 1.0 的内容安装，再造 2.0 的源与清单。
        let v1 = make_component(&fixture, "rb", "1.0", &[("a.bin", b"one")]);
        store.install_local(&v1, &fixture.path("src/rb")).unwrap();
        let v2 = make_component(&fixture, "rb", "2.0", &[("a.bin", b"two")]);
        let report = store.install_local(&v2, &fixture.path("src/rb")).unwrap();
        assert_eq!(report.replaced_version.as_deref(), Some("1.0"));

        assert_eq!(
            store.installed_versions("rb").unwrap(),
            vec!["1.0".to_string(), "2.0".to_string()]
        );
        assert_eq!(store.current_version("rb").unwrap().as_deref(), Some("2.0"));
        assert_eq!(
            fs::read_to_string(root.join("rb/versions/2.0/a.bin")).unwrap(),
            "two"
        );

        let rollback = store.rollback(&v2).unwrap();
        assert_eq!(rollback.from, "2.0");
        assert_eq!(rollback.to, "1.0");
        assert!(rollback.files_present);
        assert_eq!(store.current_version("rb").unwrap().as_deref(), Some("1.0"));

        // 只有一个版本时不能回滚。
        let fixture2 = Fixture::new("rollback-single");
        let single_root = fixture2.path("runtime");
        let only = make_component(&fixture2, "solo", "1.0", &[("a.bin", b"x")]);
        let store2 = open_store(&single_root);
        store2
            .install_local(&only, &fixture2.path("src/solo"))
            .unwrap();
        assert!(matches!(
            store2.rollback(&only).unwrap_err(),
            StoreError::NoPreviousVersion { .. }
        ));

        // 完全没装过。
        let fixture3 = Fixture::new("rollback-none");
        let store3 = open_store(&fixture3.path("runtime"));
        assert!(matches!(
            store3.rollback(&v1).unwrap_err(),
            StoreError::NotInstalled { .. }
        ));
    }

    #[test]
    fn reinstall_same_version_is_idempotent() {
        let fixture = Fixture::new("reinstall");
        let root = fixture.path("runtime");
        let component = make_component(&fixture, "re", "1.0", &[("a.bin", b"x")]);
        let store = open_store(&root);
        let first = store
            .install_local(&component, &fixture.path("src/re"))
            .unwrap();
        let second = store
            .install_local(&component, &fixture.path("src/re"))
            .unwrap();
        assert!(first.replaced_version.is_none());
        // 同一版本重装不算「替换了别的版本」。
        assert!(second.replaced_version.is_none());
        assert_eq!(store.installed_versions("re").unwrap().len(), 1);
        assert!(second.verified);
    }

    #[test]
    fn installing_new_version_removes_stale_staging() {
        let fixture = Fixture::new("staging");
        let root = fixture.path("runtime");
        let component = make_component(&fixture, "st", "1.0", &[("a.bin", b"x")]);
        let store = open_store(&root);
        // 预置一个会被清掉的残留暂存目录。
        fs::create_dir_all(root.join("st/staging/leftover")).unwrap();
        fs::write(root.join("st/staging/leftover/junk"), b"junk").unwrap();
        store
            .install_local(&component, &fixture.path("src/st"))
            .unwrap();
        assert!(!root.join("st/staging").exists());
    }

    #[test]
    fn online_install_verifies_artifact_before_extracting() {
        let fixture = Fixture::new("online");
        let root = fixture.path("runtime");
        let mut component = make_component(&fixture, "net", "1.0", &[("a.bin", b"payload")]);
        // 造一个「归档产物」放进暂存区（真实环境由 curl 写出）。
        let artifact = b"fake-archive-bytes";
        component.source = ComponentSource {
            url: Some("https://example.invalid/models/net-1.0.zip".into()),
            sha256: Some(sha256_bytes(artifact)),
            archive: Some("zip".into()),
            layout: Some("net-1.0".into()),
            license: Some("MIT".into()),
        };
        let store = open_store(&root);
        // 下载工具写出归档、解压工具写出文件树——都由「会在命中时落盘」的执行器模拟。
        let runner = ProducingRunner::new(FakeRunner::new().on("tar", CommandOutput::ok("")))
            .produce(
                "curl",
                root.join("net/downloads"),
                vec![("net-1.0.zip", artifact)],
            )
            .produce(
                "tar",
                root.join("net/staging/extract"),
                vec![("net-1.0/a.bin", b"payload")],
            );
        let report = store.install_from_url(&component, &runner).unwrap();
        assert!(report.verified);
        assert_eq!(report.version, "1.0");
        // 下载与解压都必须被真的调用过，且调用顺序是先下载后解压。
        assert_eq!(runner.programs(), vec!["curl", "tar"]);
        assert!(runner.calls()[0].display().contains("net-1.0.zip"));
        assert!(runner.calls()[1].display().contains("-C"));
        // 归档留在缓存里，方便重装时免下载。
        assert!(root.join("net/downloads/net-1.0.zip").is_file());
    }

    #[test]
    fn online_install_rejects_bad_artifact_digest() {
        let fixture = Fixture::new("online-bad");
        let root = fixture.path("runtime");
        let mut component = make_component(&fixture, "netbad", "1.0", &[("a.bin", b"payload")]);
        component.source = ComponentSource {
            url: Some("https://example.invalid/x.zip".into()),
            sha256: Some(sha256_bytes(b"expected-bytes")),
            archive: Some("zip".into()),
            layout: None,
            license: None,
        };
        let store = open_store(&root);
        let cache_dir = root.join("netbad/downloads");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(cache_dir.join("x.zip"), b"different-bytes").unwrap();
        let error = store
            .install_from_url(&component, &FakeRunner::new())
            .unwrap_err();
        assert!(matches!(error, StoreError::ArtifactDigestMismatch { .. }));
        // 校验失败不能留下任何已安装版本。
        assert!(store.current_version("netbad").unwrap().is_none());
    }

    #[test]
    fn freshly_downloaded_bytes_are_verified_too() {
        let fixture = Fixture::new("online-fresh-bad");
        let root = fixture.path("runtime");
        let mut component = make_component(&fixture, "fresh", "1.0", &[("a.bin", b"payload")]);
        component.source = ComponentSource {
            url: Some("https://example.invalid/a.zip".into()),
            sha256: Some(sha256_bytes(b"what-we-expect")),
            archive: Some("zip".into()),
            layout: None,
            license: None,
        };
        let store = open_store(&root);
        let runner = ProducingRunner::new(FakeRunner::new().on("curl", CommandOutput::ok("")))
            .produce(
                "curl",
                root.join("fresh/downloads"),
                vec![("a.zip", b"what-we-actually-got")],
            );
        let error = store.install_from_url(&component, &runner).unwrap_err();
        assert!(matches!(error, StoreError::ArtifactDigestMismatch { .. }));
        // 校验失败必须清掉坏产物，且不能留下任何安装痕迹。
        assert!(!root.join("fresh/downloads/a.zip").exists());
        assert!(store.current_version("fresh").unwrap().is_none());
        assert!(!store.staging_dir("fresh").exists());
    }

    #[test]
    fn online_install_requires_url_and_digest() {
        let fixture = Fixture::new("online-req");
        let store = open_store(&fixture.path("runtime"));
        let mut component = make_component(&fixture, "need", "1.0", &[("a.bin", b"x")]);
        assert!(matches!(
            store
                .install_from_url(&component, &FakeRunner::new())
                .unwrap_err(),
            StoreError::NoDownloadUrl { .. }
        ));
        component.source.url = Some("https://example.invalid/a.zip".into());
        assert!(matches!(
            store
                .install_from_url(&component, &FakeRunner::new())
                .unwrap_err(),
            StoreError::MissingArtifactDigest { .. }
        ));
        component.source.sha256 = Some("not-a-digest".into());
        assert!(matches!(
            store
                .install_from_url(&component, &FakeRunner::new())
                .unwrap_err(),
            StoreError::MissingArtifactDigest { .. }
        ));
    }

    #[test]
    fn online_install_reports_download_failure() {
        let fixture = Fixture::new("online-fail");
        let root = fixture.path("runtime");
        let mut component = make_component(&fixture, "fail", "1.0", &[("a.bin", b"x")]);
        component.source = ComponentSource {
            url: Some("https://example.invalid/a.zip".into()),
            sha256: Some(sha256_bytes(b"x")),
            archive: Some("zip".into()),
            layout: None,
            license: None,
        };
        let store = open_store(&root);
        let runner = FakeRunner::new().on("curl", CommandOutput::new(22, "", "404 Not Found"));
        let error = store.install_from_url(&component, &runner).unwrap_err();
        match error {
            StoreError::Download { message, .. } => assert!(message.contains("404")),
            other => panic!("期望 Download 错误，得到 {other:?}"),
        }
        assert_eq!(runner.calls()[0].program, "curl");

        // 解压失败也要报出工具名与 stderr 首行。
        let fixture2 = Fixture::new("online-extract-fail");
        let root2 = fixture2.path("runtime");
        let mut component2 = make_component(&fixture2, "exfail", "1.0", &[("a.bin", b"payload")]);
        let artifact = b"archive";
        component2.source = ComponentSource {
            url: Some("https://example.invalid/a.7z".into()),
            sha256: Some(sha256_bytes(artifact)),
            archive: Some("7z".into()),
            layout: None,
            license: None,
        };
        let config = StoreConfig {
            root: root2.clone(),
            downloader: Downloader::default(),
            archiver: Archiver::SevenZip("7zz".into()),
        };
        let store2 = RuntimeStore::new(config);
        let cache_dir = root2.join("exfail/downloads");
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(cache_dir.join("a.7z"), artifact).unwrap();
        let runner = FakeRunner::new().on("7zz", CommandOutput::new(2, "", "corrupted archive"));
        let error = store2.install_from_url(&component2, &runner).unwrap_err();
        match error {
            StoreError::Extract { tool, message } => {
                assert_eq!(tool, "7zz");
                assert!(message.contains("corrupted archive"));
            }
            other => panic!("期望 Extract 错误，得到 {other:?}"),
        }
    }

    #[test]
    fn single_file_download_is_placed_at_declared_path() {
        let fixture = Fixture::new("single");
        let root = fixture.path("runtime");
        let payload = b"single-file-payload";
        let mut component = make_component(&fixture, "one", "1.0", &[("model.onnx", payload)]);
        component.source = ComponentSource {
            url: Some("https://example.invalid/model.onnx?token=abc".into()),
            sha256: Some(sha256_bytes(payload)),
            archive: None,
            layout: None,
            license: None,
        };
        let store = open_store(&root);
        let runner = ProducingRunner::new(FakeRunner::new()).produce(
            "curl",
            root.join("one/downloads"),
            vec![("model.onnx", payload)],
        );
        let report = store.install_from_url(&component, &runner).unwrap();
        assert!(report.verified);
        assert_eq!(
            fs::read_to_string(root.join("one/versions/1.0/model.onnx")).unwrap(),
            "single-file-payload"
        );
        // URL 里的 query 不影响取到的文件名。
        assert_eq!(url_file_name("https://a/b/c.zip?x=1#frag"), "c.zip");
        assert_eq!(url_file_name("https://a/b/"), "b");
    }

    #[test]
    fn generates_a_manifest_entry_from_a_directory() {
        let fixture = Fixture::new("generate");
        fixture.write("model/weights.bin", b"w");
        fixture.write("model/sub/net.py", b"print(1)");
        fixture.write("model/__pycache__/junk.pyc", b"junk");
        fixture.write("model/.DS_Store", b"junk");
        fixture.write("model/notes.tmp", b"junk");

        let options = GenerateOptions {
            id: "column-model".into(),
            role: RuntimeRoles::INPAINT_MODEL.into(),
            version: "2024.1".into(),
            title: Some("Column 基线模型".into()),
            optional: false,
            requires: vec![],
            url: Some("https://example.invalid/m.zip".into()),
            artifact_sha256: Some(
                "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into(),
            ),
            archive: Some("zip".into()),
            layout: Some("model".into()),
            license: Some("MIT".into()),
            backend: Some("column".into()),
            framework: Some("pytorch".into()),
            entrypoint: Some("sub/net.py".into()),
        };
        let component = generate_component(&fixture.path("model"), &options).unwrap();
        let paths: Vec<&str> = component
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        // 忽略项被排除，其余按路径排序。
        assert_eq!(paths, vec!["sub/net.py", "weights.bin"]);
        // 每个文件都记录了真实大小：`weights.bin` 1 字节 + `sub/net.py` 8 字节。
        let sizes: Vec<Option<u64>> = component.files.iter().map(|file| file.size).collect();
        assert_eq!(sizes, vec![Some(8), Some(1)]);
        assert_eq!(component.display_name(), "Column 基线模型");
        assert_eq!(component.declared_size(), 9);
        assert!(component.model.is_some());

        // 生成的清单项必须能通过清单校验。
        let manifest = RuntimeManifest {
            schema_version: 1,
            generated_at: None,
            components: vec![component.clone()],
        };
        manifest.validate().unwrap();

        // 目录指纹对同样内容稳定，对内容变化敏感。
        let before = directory_digest(&fixture.path("model")).unwrap();
        assert_eq!(before, directory_digest(&fixture.path("model")).unwrap());
        fixture.write("model/weights.bin", b"changed");
        assert_ne!(before, directory_digest(&fixture.path("model")).unwrap());

        assert!(matches!(
            generate_component(&fixture.path("missing"), &options).unwrap_err(),
            StoreError::SourceNotDirectory(_)
        ));
    }

    #[test]
    fn generated_entry_without_model_fields_has_no_model_block() {
        let fixture = Fixture::new("generate-noModel");
        fixture.write("tool/ffmpeg", b"binary");
        let options = GenerateOptions {
            id: "dossier-ffmpeg".into(),
            role: RuntimeRoles::FFMPEG.into(),
            version: "6.1".into(),
            title: None,
            optional: true,
            requires: vec![],
            url: None,
            artifact_sha256: None,
            archive: None,
            layout: None,
            license: None,
            backend: None,
            framework: None,
            entrypoint: None,
        };
        let component = generate_component(&fixture.path("tool"), &options).unwrap();
        assert!(component.model.is_none());
        assert_eq!(component.files[0].path, "ffmpeg");
        assert!(component.optional);
    }

    #[test]
    fn downloader_and_archiver_build_expected_commands() {
        let curl = Downloader::Curl("curl".into());
        let args = curl.args("https://x/y.zip", Path::new("/tmp/y.zip"));
        assert_eq!(args[0], "-L");
        assert!(args.contains(&"--fail".to_string()));
        assert!(args.contains(&"/tmp/y.zip".to_string()));
        assert_eq!(curl.program(), "curl");

        let wget = Downloader::Wget("wget".into());
        assert_eq!(wget.args("https://x/y", Path::new("/tmp/y"))[0], "-q");

        let seven = Archiver::SevenZip("7zz".into());
        let spec = seven.build(Path::new("/tmp/a.7z"), Path::new("/tmp/out"));
        assert!(spec.display().contains("x -y -o/tmp/out /tmp/a.7z"));

        let tar = Archiver::Tar("tar".into());
        let spec = tar.build(Path::new("/tmp/a.tar.gz"), Path::new("/tmp/out"));
        assert_eq!(spec.args[0], "-xf");
        assert!(spec.display().contains("-C /tmp/out"));
        // tar 从 -f 的文件名推断格式，不需要额外界别参数。
        assert!(!spec.display().contains("-t"));
    }

    #[test]
    fn state_file_corruption_is_reported() {
        let fixture = Fixture::new("state");
        let root = fixture.path("runtime");
        let component = make_component(&fixture, "corrupt", "1.0", &[("a.bin", b"x")]);
        let store = open_store(&root);
        fs::create_dir_all(root.join("corrupt")).unwrap();
        fs::write(root.join("corrupt/state.json"), b"{ not json").unwrap();
        let error = store.current_version("corrupt").unwrap_err();
        assert!(matches!(error, StoreError::InvalidState { .. }));
        assert!(matches!(
            store.rollback(&component).unwrap_err(),
            StoreError::InvalidState { .. }
        ));
    }
}
