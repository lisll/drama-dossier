//! 运行时组件清单（`runtime-manifest.json`）。
//!
//! 解决的问题：擦除/分离/识别这些能力依赖一堆**外部**东西（ffmpeg、python、torch、
//! 模型权重、字体、工具），而现在只有一句 `fetch_models.sh` 的打印指引——装了没有、
//! 装的是哪个版本、文件有没有被截断，全靠人肉记忆。
//!
//! 这里把「一个组件应该长什么样」写成可校验的数据：每个组件声明角色、版本、依赖、
//! 文件清单（逐个 SHA-256 + 大小），模型类组件额外声明后端与框架。[`validate`] 负责
//! 把不合法的清单挡在安装之前，[`RuntimeManifest::installation_order`] 负责给出
//! 依赖优先的安装顺序。
//!
//! [`validate`]: RuntimeManifest::validate

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component as PathComponent, Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::digest::is_sha256_hex;

/// 当前清单格式版本。加字段用 `#[serde(default)]`，改语义才升版本。
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// 组件角色。角色决定「这个组件是干什么的」，也是能力协商（[`crate::caps`]）的
/// 索引键。参照同类产品的 22 类角色设计，按本项目实际用到的能力裁剪。
pub struct RuntimeRoles;

impl RuntimeRoles {
    pub const FFMPEG: &'static str = "ffmpeg";
    pub const FFPROBE: &'static str = "ffprobe";
    pub const PYTHON_RUNTIME: &'static str = "python_runtime";
    pub const TORCH_RUNTIME: &'static str = "torch_runtime";
    pub const ONNX_RUNTIME: &'static str = "onnx_runtime";
    pub const MODEL_WORKER: &'static str = "model_worker";
    pub const TEXT_DETECTOR_MODEL: &'static str = "text_detector_model";
    pub const INPAINT_MODEL: &'static str = "inpaint_model";
    pub const SOURCE_SEPARATION_MODEL: &'static str = "source_separation_model";
    pub const TIME_STRETCH_TOOL: &'static str = "time_stretch_tool";
    pub const SPEECH_RECOGNIZER_MODEL: &'static str = "speech_recognizer_model";
    pub const TEXT_RECOGNIZER_MODEL: &'static str = "text_recognizer_model";
    pub const SHOT_DETECTOR_MODEL: &'static str = "shot_detector_model";
    pub const FONT_PACK: &'static str = "font_pack";
    pub const ARCHIVE_TOOL: &'static str = "archive_tool";
    pub const DOWNLOAD_TOOL: &'static str = "download_tool";
    pub const SIGNATURE_TOOL: &'static str = "signature_tool";
    pub const GPU_TOOLKIT: &'static str = "gpu_toolkit";

    /// 全部已知角色（清单校验用；写错角色名直接报错，而不是静静忽略）。
    pub const ALL: &'static [&'static str] = &[
        Self::FFMPEG,
        Self::FFPROBE,
        Self::PYTHON_RUNTIME,
        Self::TORCH_RUNTIME,
        Self::ONNX_RUNTIME,
        Self::MODEL_WORKER,
        Self::TEXT_DETECTOR_MODEL,
        Self::INPAINT_MODEL,
        Self::SOURCE_SEPARATION_MODEL,
        Self::TIME_STRETCH_TOOL,
        Self::SPEECH_RECOGNIZER_MODEL,
        Self::TEXT_RECOGNIZER_MODEL,
        Self::SHOT_DETECTOR_MODEL,
        Self::FONT_PACK,
        Self::ARCHIVE_TOOL,
        Self::DOWNLOAD_TOOL,
        Self::SIGNATURE_TOOL,
        Self::GPU_TOOLKIT,
    ];

    /// 必须带 `model` 段落（后端 / 框架）的角色。
    pub const MODEL_ROLES: &'static [&'static str] = &[
        Self::TEXT_DETECTOR_MODEL,
        Self::INPAINT_MODEL,
        Self::SOURCE_SEPARATION_MODEL,
        Self::SPEECH_RECOGNIZER_MODEL,
        Self::TEXT_RECOGNIZER_MODEL,
        Self::SHOT_DETECTOR_MODEL,
    ];

    pub fn is_known(role: &str) -> bool {
        Self::ALL.contains(&role)
    }

    pub fn is_model(role: &str) -> bool {
        Self::MODEL_ROLES.contains(&role)
    }
}

/// 整份运行时清单。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeManifest {
    pub schema_version: u32,
    /// 生成时间（RFC 3339）；手写清单时可为空。
    #[serde(default)]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub components: Vec<Component>,
}

/// 单个可交付组件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Component {
    /// 稳定标识，同时用作磁盘目录名（`[A-Za-z0-9._-]`）。
    pub id: String,
    pub role: String,
    #[serde(default)]
    pub title: Option<String>,
    pub version: String,
    /// 可选组件缺失时不算失败，只是在能力协商里降级。
    #[serde(default)]
    pub optional: bool,
    /// 依赖的其它组件 id（安装顺序、可用性判定都看它）。
    #[serde(default)]
    pub requires: Vec<String>,
    pub source: ComponentSource,
    pub files: Vec<ComponentFile>,
    #[serde(default)]
    pub model: Option<ModelInfo>,
    #[serde(default)]
    pub notes: Option<String>,
}

impl Component {
    /// 清单里所有文件的大小合计；未声明大小时忽略该文件。
    pub fn declared_size(&self) -> u64 {
        self.files.iter().filter_map(|file| file.size).sum()
    }

    pub fn display_name(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.id)
    }
}

/// 组件的获取方式。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ComponentSource {
    /// 下载地址（可选：也可以完全离线，用 `--from <本地目录>` 安装）。
    #[serde(default)]
    pub url: Option<String>,
    /// 下载产物的 SHA-256（归档或单文件）。有 `url` 时几乎总该有它。
    #[serde(default)]
    pub sha256: Option<String>,
    /// 归档格式：`zip` / `tar.gz` / `7z`；`None` 表示 url 直接是组件文件本身。
    #[serde(default)]
    pub archive: Option<String>,
    /// 归档内需要上提到组件根目录的子目录。
    #[serde(default)]
    pub layout: Option<String>,
    /// 许可证标识（权重分发尤其需要写清楚）。
    #[serde(default)]
    pub license: Option<String>,
}

/// 组件内的一个文件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentFile {
    /// 相对组件根目录的路径。
    pub path: String,
    pub sha256: String,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub executable: bool,
}

/// 模型类组件的后端信息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    /// 后端标识，例如 `e2fgvi` / `lama` / `propainter` / `dbnet` / `demucs`。
    pub backend: String,
    /// 运行框架：`pytorch` / `onnx` / `native`。
    pub framework: String,
    /// worker 侧入口脚本（相对组件根目录）。
    #[serde(default)]
    pub entrypoint: Option<String>,
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("清单文件读取失败（{path}）：{source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("清单解析失败：{0}")]
    Parse(String),
    #[error("清单校验未通过：\n- {}", .issues.join("\n- "))]
    Invalid { issues: Vec<String> },
    #[error("清单里没有组件 `{0}`")]
    UnknownComponent(String),
}

/// 校验时接受的最大问题条数，避免一份坏清单一口气刷屏。
const MAX_REPORTED_ISSUES: usize = 40;

impl RuntimeManifest {
    pub fn from_json(raw: &[u8]) -> Result<Self, ManifestError> {
        serde_json::from_slice(raw).map_err(|error| ManifestError::Parse(error.to_string()))
    }

    pub fn load(path: &Path) -> Result<Self, ManifestError> {
        let raw = std::fs::read(path).map_err(|source| ManifestError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_json(&raw)
    }

    pub fn to_json(&self) -> Result<Vec<u8>, ManifestError> {
        let mut raw = serde_json::to_vec_pretty(self)
            .map_err(|error| ManifestError::Parse(error.to_string()))?;
        raw.push(b'\n');
        Ok(raw)
    }

    pub fn save(&self, path: &Path) -> Result<(), ManifestError> {
        let raw = self.to_json()?;
        std::fs::write(path, raw).map_err(|source| ManifestError::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn find(&self, id: &str) -> Option<&Component> {
        self.components.iter().find(|component| component.id == id)
    }

    /// 按角色筛选组件。
    pub fn by_role(&self, role: &str) -> Vec<&Component> {
        self.components
            .iter()
            .filter(|component| component.role == role)
            .collect()
    }

    /// 收集所有问题（不提前返回），便于一次性把清单里的错误都告诉作者。
    pub fn collect_issues(&self) -> Vec<String> {
        let mut issues = Vec::new();
        if self.schema_version != MANIFEST_SCHEMA_VERSION {
            issues.push(format!(
                "清单版本 {} 不受支持（当前支持 {}）",
                self.schema_version, MANIFEST_SCHEMA_VERSION
            ));
        }
        if self.components.is_empty() {
            issues.push("清单里没有任何组件".to_string());
        }

        let mut seen_ids: BTreeSet<&str> = BTreeSet::new();
        for component in &self.components {
            if component.id.trim().is_empty() {
                issues.push("存在 id 为空的组件".to_string());
                continue;
            }
            if !seen_ids.insert(component.id.as_str()) {
                issues.push(format!("组件 id 重复：{}", component.id));
            }
            issues.extend(component_issues(component));
        }

        // 依赖必须指向存在的组件，且不能成环。
        let ids: BTreeSet<&str> = self.components.iter().map(|c| c.id.as_str()).collect();
        for component in &self.components {
            for dependency in &component.requires {
                if !ids.contains(dependency.as_str()) {
                    issues.push(format!(
                        "组件 {} 依赖了不存在的组件 {}",
                        component.id, dependency
                    ));
                } else if dependency == &component.id {
                    issues.push(format!("组件 {} 依赖了自己", component.id));
                }
            }
        }
        for cycle in dependency_cycles(self) {
            issues.push(format!("组件依赖成环：{}", cycle.join(" → ")));
        }
        issues.truncate(MAX_REPORTED_ISSUES);
        issues
    }

    pub fn validate(&self) -> Result<(), ManifestError> {
        let issues = self.collect_issues();
        if issues.is_empty() {
            Ok(())
        } else {
            Err(ManifestError::Invalid { issues })
        }
    }

    /// 依赖优先的安装顺序。仅在 [`validate`](Self::validate) 通过后才有意义。
    pub fn installation_order(&self) -> Vec<&Component> {
        let index: BTreeMap<&str, &Component> = self
            .components
            .iter()
            .map(|component| (component.id.as_str(), component))
            .collect();
        let mut emitted: Vec<&str> = Vec::new();
        let mut done: BTreeSet<&str> = BTreeSet::new();
        // 组件数量是几十级，反复扫描足够快，也不需要额外递归状态机。
        while emitted.len() < self.components.len() {
            let mut progressed = false;
            for component in &self.components {
                if done.contains(component.id.as_str()) {
                    continue;
                }
                // 依赖缺失不算「未就绪」——那属于清单校验该报的错，不是排序的死锁。
                let ready = component
                    .requires
                    .iter()
                    .all(|dep| !index.contains_key(dep.as_str()) || done.contains(dep.as_str()));
                if ready {
                    done.insert(component.id.as_str());
                    emitted.push(component.id.as_str());
                    progressed = true;
                }
            }
            if !progressed {
                // 有环或依赖缺失：把剩下的原序追加，保证「返回全部组件」这一约定。
                for component in &self.components {
                    if !done.contains(component.id.as_str()) {
                        done.insert(component.id.as_str());
                        emitted.push(component.id.as_str());
                    }
                }
            }
        }
        emitted
            .iter()
            .filter_map(|id| index.get(id).copied())
            .collect()
    }
}

/// `id` 与 `version` 会拼成安装目录 `<root>/<id>/versions/<version>`，安装时还会先删除该目录，
/// 所以两者都必须是单段安全名。安装入口（`store` 的 `install_local` / `install_from_url`）
/// 也会单独调用它：调用方可能没先 `validate` 整份清单。
pub(crate) fn install_path_issues(component: &Component) -> Vec<String> {
    let mut issues = Vec::new();
    let id = &component.id;
    if !is_safe_component_id(id) {
        issues.push(format!(
            "组件 id `{id}` 含非法字符；只允许字母、数字、点、下划线和短横线"
        ));
    }
    if component.version.trim().is_empty() {
        issues.push(format!("组件 {id} 没有声明版本"));
    } else if !is_safe_component_id(&component.version) {
        issues.push(format!(
            "组件 {id} 的版本 `{}` 含非法字符；只允许字母、数字、点、下划线和短横线，且不能以点开头",
            component.version
        ));
    }
    // 文件路径与 layout 同样会拼进安装目录 / 源目录，安装入口也要挡住
    for file in &component.files {
        if !is_safe_relative_path(&file.path) {
            issues.push(format!(
                "组件 {id} 的文件路径 `{}` 不是安全的相对路径",
                file.path
            ));
        }
    }
    if let Some(layout) = &component.source.layout {
        if !is_safe_relative_path(layout) {
            issues.push(format!("组件 {id} 的 layout `{layout}` 不是安全的相对路径"));
        }
    }
    issues
}

fn component_issues(component: &Component) -> Vec<String> {
    let mut issues = install_path_issues(component);
    let id = &component.id;
    if component.role.trim().is_empty() {
        issues.push(format!("组件 {id} 没有声明角色（role）"));
    } else if !RuntimeRoles::is_known(&component.role) {
        issues.push(format!(
            "组件 {id} 的角色 `{}` 未知；可用角色见 RuntimeRoles::ALL",
            component.role
        ));
    }
    if component.files.is_empty() {
        issues.push(format!("组件 {id} 的文件清单为空"));
    }
    if let Some(url) = &component.source.url {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            issues.push(format!("组件 {id} 的下载地址不是 http(s)：{url}"));
        }
        match &component.source.sha256 {
            Some(digest) if is_sha256_hex(digest) => {}
            Some(digest) => issues.push(format!(
                "组件 {id} 的下载产物摘要不是合法 SHA-256：{digest}"
            )),
            None => issues.push(format!("组件 {id} 声明了下载地址却没有给出产物 SHA-256")),
        }
    }
    if let Some(archive) = &component.source.archive {
        if !matches!(archive.as_str(), "zip" | "tar.gz" | "tar" | "7z") {
            issues.push(format!("组件 {id} 的归档格式 `{archive}` 不受支持"));
        }
    }
    if RuntimeRoles::is_model(&component.role) && component.model.is_none() {
        issues.push(format!("模型组件 {id} 缺少 model 段（backend/framework）"));
    }
    if let Some(model) = &component.model {
        if model.backend.trim().is_empty() || model.framework.trim().is_empty() {
            issues.push(format!(
                "组件 {id} 的 model 段必须同时给出 backend 与 framework"
            ));
        }
    }

    let mut seen_paths: BTreeSet<&str> = BTreeSet::new();
    for file in &component.files {
        if !seen_paths.insert(file.path.as_str()) {
            issues.push(format!("组件 {id} 的文件路径重复：{}", file.path));
        }
        if !is_sha256_hex(&file.sha256) {
            issues.push(format!(
                "组件 {id} 的文件 {} 摘要不是合法 SHA-256",
                file.path
            ));
        }
    }
    issues
}

/// 组件版本号是否可以安全地作为目录名（与组件 id 同一套规则）。
/// `store` 读取本地 state.json 里的版本号时也用它：那份文件可能被改过。
pub(crate) fn is_safe_version(version: &str) -> bool {
    is_safe_component_id(version)
}

fn is_safe_component_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && !id.starts_with('.')
}

/// 相对路径安全校验：拒绝绝对路径、盘符、`..` 与空段。
pub fn is_safe_relative_path(path: &str) -> bool {
    if path.trim().is_empty() {
        return false;
    }
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        return false;
    }
    for segment in candidate.components() {
        match segment {
            PathComponent::Normal(_) => {}
            _ => return false,
        }
    }
    true
}

/// 找出所有依赖环，返回可读的环路径。
fn dependency_cycles(manifest: &RuntimeManifest) -> Vec<Vec<String>> {
    let graph: BTreeMap<&str, Vec<&str>> = manifest
        .components
        .iter()
        .map(|component| {
            (
                component.id.as_str(),
                component
                    .requires
                    .iter()
                    .map(String::as_str)
                    .filter(|dep| manifest.find(dep).is_some())
                    .collect(),
            )
        })
        .collect();

    let mut cycles = Vec::new();
    let mut visited: BTreeSet<&str> = BTreeSet::new();
    let mut stack: Vec<&str> = Vec::new();
    let mut on_stack: BTreeSet<&str> = BTreeSet::new();
    let mut reported: BTreeSet<String> = BTreeSet::new();

    fn walk<'a>(
        node: &'a str,
        graph: &BTreeMap<&'a str, Vec<&'a str>>,
        visited: &mut BTreeSet<&'a str>,
        stack: &mut Vec<&'a str>,
        on_stack: &mut BTreeSet<&'a str>,
        cycles: &mut Vec<Vec<String>>,
        reported: &mut BTreeSet<String>,
    ) {
        visited.insert(node);
        stack.push(node);
        on_stack.insert(node);
        for next in graph.get(node).into_iter().flatten() {
            if on_stack.contains(next) {
                let start = stack.iter().position(|item| item == next).unwrap_or(0);
                let mut cycle: Vec<String> = stack[start..]
                    .iter()
                    .map(|item| (*item).to_string())
                    .collect();
                cycle.push((*next).to_string());
                let key = cycle.join("→");
                if reported.insert(key) {
                    cycles.push(cycle);
                }
            } else if !visited.contains(next) {
                walk(next, graph, visited, stack, on_stack, cycles, reported);
            }
        }
        stack.pop();
        on_stack.remove(node);
    }

    for component in &manifest.components {
        if !visited.contains(component.id.as_str()) {
            walk(
                component.id.as_str(),
                &graph,
                &mut visited,
                &mut stack,
                &mut on_stack,
                &mut cycles,
                &mut reported,
            );
        }
    }
    cycles
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, digest: &str) -> ComponentFile {
        ComponentFile {
            path: path.to_string(),
            sha256: digest.to_string(),
            size: Some(1024),
            executable: false,
        }
    }

    const DIGEST_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn component(id: &str, role: &str, version: &str) -> Component {
        Component {
            id: id.to_string(),
            role: role.to_string(),
            title: None,
            version: version.to_string(),
            optional: false,
            requires: Vec::new(),
            source: ComponentSource::default(),
            files: vec![file("bin/tool", DIGEST_A)],
            model: None,
            notes: None,
        }
    }

    fn manifest(components: Vec<Component>) -> RuntimeManifest {
        RuntimeManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            generated_at: None,
            components,
        }
    }

    #[test]
    fn accepts_a_minimal_valid_manifest() {
        let manifest = manifest(vec![component(
            "dossier-ffmpeg",
            RuntimeRoles::FFMPEG,
            "6.1",
        )]);
        manifest.validate().unwrap();
        let raw = manifest.to_json().unwrap();
        let parsed = RuntimeManifest::from_json(&raw).unwrap();
        assert_eq!(parsed, manifest);
        assert_eq!(parsed.find("dossier-ffmpeg").unwrap().declared_size(), 1024);
        assert_eq!(parsed.by_role(RuntimeRoles::FFMPEG).len(), 1);
    }

    #[test]
    fn rejects_unknown_role_and_bad_digest() {
        let mut bad = component("x", "not_a_role", "1.0");
        bad.files = vec![file("bin/tool", "not-a-digest")];
        let issues = manifest(vec![bad]).collect_issues();
        assert!(issues.iter().any(|issue| issue.contains("角色")));
        assert!(issues.iter().any(|issue| issue.contains("SHA-256")));
    }

    #[test]
    fn rejects_unsafe_paths_and_ids() {
        let mut bad = component("ok", RuntimeRoles::FFMPEG, "1");
        bad.files = vec![file("../escape.sh", DIGEST_A)];
        let issues = manifest(vec![bad]).collect_issues();
        assert!(issues.iter().any(|issue| issue.contains("安全的相对路径")));

        let mut bad_id = component("bad/id", RuntimeRoles::FFMPEG, "1");
        bad_id.id = "bad/id".into();
        assert!(manifest(vec![bad_id])
            .collect_issues()
            .iter()
            .any(|issue| issue.contains("非法字符")));

        for version in [
            "../../x",
            "/etc",
            "a/b",
            "..",
            ".hidden",
            "a\\b",
            "1.0.0+build",
        ] {
            let bad_version = component("ok", RuntimeRoles::FFMPEG, version);
            assert!(
                manifest(vec![bad_version])
                    .collect_issues()
                    .iter()
                    .any(|issue| issue.contains("的版本")),
                "版本 {version} 应被拒绝"
            );
        }
        for version in ["1.2.3", "2024-01", "v2_rc1", "6.1"] {
            assert!(
                component_issues(&component("ok", RuntimeRoles::FFMPEG, version)).is_empty(),
                "版本 {version} 应通过"
            );
        }

        assert!(is_safe_relative_path("models/e2fgvi.pth"));
        assert!(!is_safe_relative_path("/etc/passwd"));
        assert!(!is_safe_relative_path("a/../../b"));
        assert!(!is_safe_relative_path(""));
    }

    #[test]
    fn requires_model_block_for_model_roles() {
        let mut model = component("e2fgvi", RuntimeRoles::INPAINT_MODEL, "1.0");
        assert!(manifest(vec![model.clone()])
            .collect_issues()
            .iter()
            .any(|issue| issue.contains("缺少 model 段")));
        model.model = Some(ModelInfo {
            backend: "e2fgvi".into(),
            framework: "pytorch".into(),
            entrypoint: Some("worker.py".into()),
        });
        manifest(vec![model]).validate().unwrap();
    }

    #[test]
    fn download_source_needs_https_and_digest() {
        let mut component = component("w", RuntimeRoles::INPAINT_MODEL, "1");
        component.model = Some(ModelInfo {
            backend: "lama".into(),
            framework: "pytorch".into(),
            entrypoint: None,
        });
        component.source = ComponentSource {
            url: Some("file:///tmp/x.zip".into()),
            sha256: None,
            archive: Some("rar".into()),
            layout: None,
            license: None,
        };
        let issues = manifest(vec![component]).collect_issues();
        assert!(issues.iter().any(|issue| issue.contains("不是 http(s)")));
        assert!(issues
            .iter()
            .any(|issue| issue.contains("没有给出产物 SHA-256")));
        assert!(issues.iter().any(|issue| issue.contains("归档格式")));
    }

    #[test]
    fn detects_missing_dependency_and_cycles() {
        let mut a = component("a", RuntimeRoles::FFMPEG, "1");
        a.requires = vec!["missing".into()];
        let issues = manifest(vec![a]).collect_issues();
        assert!(issues
            .iter()
            .any(|issue| issue.contains("依赖了不存在的组件")));

        let mut left = component("left", RuntimeRoles::FFMPEG, "1");
        left.requires = vec!["right".into()];
        let mut right = component("right", RuntimeRoles::FFPROBE, "1");
        right.requires = vec!["left".into()];
        let issues = manifest(vec![left, right]).collect_issues();
        assert!(issues.iter().any(|issue| issue.contains("依赖成环")));
    }

    #[test]
    fn rejects_duplicate_ids_and_wrong_schema_version() {
        let dup = manifest(vec![
            component("same", RuntimeRoles::FFMPEG, "1"),
            component("same", RuntimeRoles::FFPROBE, "1"),
        ]);
        assert!(dup
            .collect_issues()
            .iter()
            .any(|issue| issue.contains("id 重复")));

        let mut wrong = manifest(vec![component("x", RuntimeRoles::FFMPEG, "1")]);
        wrong.schema_version = 99;
        assert!(wrong
            .collect_issues()
            .iter()
            .any(|issue| issue.contains("不受支持")));

        let empty = manifest(Vec::new());
        assert!(empty
            .collect_issues()
            .iter()
            .any(|issue| issue.contains("没有任何组件")));
    }

    #[test]
    fn installation_order_puts_dependencies_first() {
        let mut ffmpeg = component("dossier-ffmpeg", RuntimeRoles::FFMPEG, "6.1");
        ffmpeg.requires = vec!["dossier-archive".into()];
        let archive = component("dossier-archive", RuntimeRoles::ARCHIVE_TOOL, "24.0");
        let manifest = manifest(vec![ffmpeg, archive]);
        let order: Vec<&str> = manifest
            .installation_order()
            .iter()
            .map(|component| component.id.as_str())
            .collect();
        assert_eq!(order, vec!["dossier-archive", "dossier-ffmpeg"]);
    }

    #[test]
    fn parses_camel_case_json_and_reports_missing_component() {
        let raw = br#"{
          "schemaVersion": 1,
          "components": [
            {
              "id": "owner-fonts",
              "role": "font_pack",
              "version": "2024.1",
              "source": {"license": "OFL-1.1"},
              "files": [{"path": "NotoSans.ttf", "sha256": "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc", "size": 123}],
              "optional": true
            }
          ]
        }"#;
        let manifest = RuntimeManifest::from_json(raw).unwrap();
        manifest.validate().unwrap();
        assert!(manifest.find("owner-fonts").unwrap().optional);
        assert!(manifest.find("nope").is_none());
        assert!(RuntimeManifest::from_json(b"{").is_err());
    }
}
