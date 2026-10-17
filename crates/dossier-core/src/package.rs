//! 报审包：服务端生成文档，桌面端组装（总方案 3.3 节、M0 调研报告第 4 节）。
//!
//! - [`render_documents`]：**纯函数**，不碰磁盘、不读视频。输入是剧目信息和桌面端算好的分集哈希，
//!   输出是要写进包里的文本文件、需要盖章的两份文档的排版模型、本地文件的放置位置。
//!   服务端经 `dossier-artifacts render-package` 调用它，再渲染 PDF、盖章、给清单签名。
//!   文字内容只在这里写一份：服务端只负责把排版模型画成 PDF，不自己拼文字。
//! - [`assemble_package`]：桌面端把服务端发回的文件和本地视频组装成报审包。先验清单签名，
//!   再逐个文件重新算哈希，全部对上才落盘；先写临时目录，成功后改名，失败不留半个包。
//!
//! 合规设计不变：只做「真实文件的整理与格式化」，不生成任何伪造签名 / 印章 / 时间戳；
//! 签章位上放的是一眼能认出的占位图形（服务端 `MockSigner`），或者用户上传的盖章扫描件。

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use dossier_license::manifest::{self, FileSource, Manifest, ManifestError};
use dossier_license::TrustedKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::types::EpisodeSpec;

// ---------- 生成文档（服务端调用） ----------

/// 一部剧最多这么多集：再多就不像短剧了，也防止请求体被撑大。
pub const MAX_EPISODES: usize = 500;
/// 文字字段的长度上限（字符数）。
const MAX_TEXT_CHARS: usize = 200;

/// 成本配置表单 —— 只承载使用者提供的真实数据。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostSheetForm {
    pub company_name: String,
    /// 投资总额（万元）。
    pub total_investment_wan: Option<f64>,
    /// 演员成本占比（%），0-100。
    pub actor_ratio_percent: Option<f64>,
    pub contact: String,
}

/// 桌面端在本机算好的文件信息（视频不上传，只传哈希）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LocalAsset {
    pub file_name: String,
    pub size_bytes: u64,
    pub sha256: String,
}

/// 一集：文件信息 + 探测规格。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EpisodeEntry {
    pub file_name: String,
    pub episode_number: Option<u32>,
    pub size_bytes: u64,
    pub sha256: String,
    pub spec: EpisodeSpec,
}

/// 生成文档的输入。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenderRequest {
    pub drama_name: String,
    pub ai_generated: bool,
    pub copy_episodes: bool,
    #[serde(default)]
    pub cost_sheet: Option<CostSheetForm>,
    #[serde(default)]
    pub synopsis: Option<LocalAsset>,
    #[serde(default)]
    pub cover: Option<LocalAsset>,
    pub episodes: Vec<EpisodeEntry>,
}
/// 一个写进包里的文本文件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextFile {
    pub path: String,
    pub content: String,
}

/// 排版模型里的一块内容。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum Block {
    /// 普通段落。
    Paragraph { text: String },
    /// 灰色小字说明。
    Note { text: String },
    /// 两列表格：项目 | 内容。
    Table { rows: Vec<(String, String)> },
}

/// 签章框的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SealKind {
    CompanySeal,
    Signature,
}

/// 签章位置：A4 页面上从左上角量起，单位毫米。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SealSlot {
    pub page: u32,
    pub x_mm: f64,
    pub y_mm: f64,
    pub width_mm: f64,
    pub height_mm: f64,
    pub kind: SealKind,
    /// 框上方的说明文字，例如「盖章处」。
    pub label: String,
}

/// 需要盖章的文档：服务端照这个画 PDF，再在签章位上盖章。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SealedDocument {
    /// 包里的路径（不含签章状态后缀，服务端按签章方式决定最终文件名）。
    pub path: String,
    pub title: String,
    pub blocks: Vec<Block>,
    /// 正文不能写到这条线以下（毫米），下面留给签章位。
    pub content_bottom_mm: f64,
    pub slots: Vec<SealSlot>,
}

/// 由桌面端从本机放进包里的文件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalPlacement {
    pub path: String,
    pub size_bytes: u64,
    pub sha256: String,
}

/// 生成文档的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderedDocuments {
    pub text_files: Vec<TextFile>,
    pub sealed: Vec<SealedDocument>,
    /// 包里要有、由桌面端放入的文件（封面、简介、复制进包的分集）。
    pub local_files: Vec<LocalPlacement>,
    /// 不复制进包、只登记哈希的分集（`path` 是剧目目录里的文件名）。
    pub references: Vec<LocalPlacement>,
}

/// 请求不合法。消息是中文，服务端原样转给用户。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct RenderError(pub String);

/// 生成报审包的文档。同样的输入，输出逐字节相同。
pub fn render_documents(request: &RenderRequest) -> Result<RenderedDocuments, RenderError> {
    validate_request(request)?;
    let drama_name = request.drama_name.trim();
    let mut text_files = vec![TextFile {
        path: "01_剧集信息/剧名.txt".into(),
        content: drama_name.to_owned(),
    }];
    text_files.push(TextFile {
        path: "03_分集清单/分集清单.csv".into(),
        content: episode_csv(&request.episodes),
    });
    let form = request.cost_sheet.clone().unwrap_or_default();
    text_files.push(TextFile {
        path: "04_成本配置表/成本配置表.md".into(),
        content: render_cost_sheet_md(&form),
    });
    text_files.push(TextFile {
        path: "05_权属声明/内容权属声明.txt".into(),
        content: ownership_declaration(drama_name, request.ai_generated),
    });

    let mut local_files = Vec::new();
    if let Some(synopsis) = &request.synopsis {
        local_files.push(place("01_剧集信息", synopsis));
    }
    if let Some(cover) = &request.cover {
        local_files.push(place("02_封面海报", cover));
    }
    let mut references = Vec::new();
    for episode in &request.episodes {
        let asset = LocalAsset {
            file_name: episode.file_name.clone(),
            size_bytes: episode.size_bytes,
            sha256: episode.sha256.clone(),
        };
        if request.copy_episodes {
            local_files.push(place("06_分集视频", &asset));
        } else {
            references.push(LocalPlacement {
                path: asset.file_name,
                size_bytes: asset.size_bytes,
                sha256: asset.sha256,
            });
        }
    }

    // 本地文件和服务端文件不能撞名（例如简介文件恰好叫「剧名.txt」）。
    let mut seen = HashSet::new();
    let all_paths = text_files
        .iter()
        .map(|file| file.path.as_str())
        .chain(local_files.iter().map(|file| file.path.as_str()));
    for path in all_paths {
        if !seen.insert(path.to_lowercase()) {
            return Err(RenderError(format!(
                "包里有两个文件同名：{path}，请改一下文件名"
            )));
        }
    }

    Ok(RenderedDocuments {
        text_files,
        sealed: vec![
            cost_sheet_document(&form),
            ownership_document(drama_name, request.ai_generated),
        ],
        local_files,
        references,
    })
}

fn place(dir: &str, asset: &LocalAsset) -> LocalPlacement {
    LocalPlacement {
        path: format!("{dir}/{}", asset.file_name),
        size_bytes: asset.size_bytes,
        sha256: asset.sha256.clone(),
    }
}
fn validate_request(request: &RenderRequest) -> Result<(), RenderError> {
    let fail = |message: String| Err(RenderError(message));
    let name = request.drama_name.trim();
    if name.is_empty() {
        return fail("剧名不能为空".into());
    }
    if name.chars().count() > MAX_TEXT_CHARS {
        return fail(format!("剧名最长 {MAX_TEXT_CHARS} 个字"));
    }
    if !manifest::safe_relative_path(name) || name.contains('/') {
        // safe_relative_path 还限制了 512 字节（约 170 个汉字），所以这里也可能是「太长」。
        return fail("剧名里有不能用作文件名的字符，或者太长".into());
    }
    if request.episodes.len() > MAX_EPISODES {
        return fail(format!("一部剧最多 {MAX_EPISODES} 集"));
    }
    let assets = request
        .synopsis
        .iter()
        .chain(request.cover.iter())
        .map(|asset| (&asset.file_name, &asset.sha256))
        .chain(
            request
                .episodes
                .iter()
                .map(|ep| (&ep.file_name, &ep.sha256)),
        );
    for (file_name, sha256) in assets {
        if !manifest::safe_relative_path(file_name) || file_name.contains('/') {
            return fail(format!("文件名不合法：{file_name}"));
        }
        if !manifest::is_sha256(sha256) {
            return fail(format!("{file_name} 的校验和格式不对"));
        }
    }
    let mut names = HashSet::new();
    for episode in &request.episodes {
        if !names.insert(episode.file_name.to_lowercase()) {
            return fail(format!("分集文件名重复：{}", episode.file_name));
        }
        let spec = &episode.spec;
        let finite = spec.duration_sec.is_finite() && spec.duration_sec >= 0.0;
        if !finite || spec.fps.is_some_and(|fps| !fps.is_finite() || fps < 0.0) {
            return fail(format!("{} 的探测结果不合法", episode.file_name));
        }
        let codec_ok = |codec: &Option<String>| {
            codec.as_deref().is_none_or(|text| {
                text.chars().count() <= 32 && !text.chars().any(char::is_control)
            })
        };
        if !codec_ok(&spec.video_codec) || !codec_ok(&spec.audio_codec) {
            return fail(format!("{} 的编码信息不合法", episode.file_name));
        }
    }
    if let Some(form) = &request.cost_sheet {
        for (label, text) in [
            ("制作机构", &form.company_name),
            ("联系方式", &form.contact),
        ] {
            if text.chars().count() > MAX_TEXT_CHARS || text.chars().any(char::is_control) {
                return fail(format!("{label}最长 {MAX_TEXT_CHARS} 个字，不能换行"));
            }
        }
        let number_ok = |value: Option<f64>, max: f64| {
            value.is_none_or(|v| v.is_finite() && (0.0..=max).contains(&v))
        };
        if !number_ok(form.total_investment_wan, 1e9) {
            return fail("投资总额（万元）必须在 0 到 1,000,000,000 之间".into());
        }
        if !number_ok(form.actor_ratio_percent, 100.0) {
            return fail("演员成本占比必须在 0 到 100 之间".into());
        }
    }
    Ok(())
}

fn episode_csv(episodes: &[EpisodeEntry]) -> String {
    let mut csv = String::from("集号,文件名,时长(秒),宽,高,视频编码,音频编码,码率(Mbps),SHA256\n");
    for ep in episodes {
        let spec = &ep.spec;
        csv.push_str(&format!(
            "{},{},{:.1},{},{},{},{},{:.2},{}\n",
            ep.episode_number.map(|n| n.to_string()).unwrap_or_default(),
            csv_escape(&ep.file_name),
            spec.duration_sec,
            spec.width,
            spec.height,
            csv_escape(spec.video_codec.as_deref().unwrap_or("")),
            csv_escape(spec.audio_codec.as_deref().unwrap_or("")),
            spec.bitrate_bps.unwrap_or(0) as f64 / 1e6,
            ep.sha256,
        ));
    }
    csv
}

fn csv_escape(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

const PENDING: &str = "（待填写）";

fn or_pending(text: &str) -> String {
    if text.trim().is_empty() {
        PENDING.to_owned()
    } else {
        text.trim().to_owned()
    }
}

fn number_or_pending(value: Option<f64>, unit: &str) -> String {
    value.map_or_else(|| PENDING.to_owned(), |v| format!("{v} {unit}"))
}

fn cost_rows(form: &CostSheetForm) -> Vec<(String, String)> {
    vec![
        ("制作机构".into(), or_pending(&form.company_name)),
        (
            "投资总额".into(),
            number_or_pending(form.total_investment_wan, "万元"),
        ),
        (
            "演员成本占比".into(),
            number_or_pending(form.actor_ratio_percent, "%"),
        ),
        ("联系方式".into(), or_pending(&form.contact)),
    ]
}

const COST_NOTE: &str =
    "本表由 drama-dossier（剧备）根据使用者填写的真实数据生成，仅作格式化排版，不构成任何背书。";
const COST_DUTY: &str = "使用者须对以上数据的真实性负责。";

fn render_cost_sheet_md(form: &CostSheetForm) -> String {
    let mut text = format!("# 成本配置情况说明\n\n> {COST_NOTE}\n\n| 项目 | 内容 |\n|---|---|\n");
    for (label, value) in cost_rows(form) {
        text.push_str(&format!("| {label} | {} |\n", value.replace('|', "\\|")));
    }
    text.push_str(&format!("\n{COST_DUTY}\n"));
    text
}

fn ai_statement(ai_generated: bool) -> &'static str {
    if ai_generated {
        "本剧部分材料（剧名/简介/封面）由 AI 辅助生成，已按平台要求进行标识。"
    } else {
        "本剧材料未使用 AI 生成。"
    }
}

fn declaration_items(ai_generated: bool) -> [String; 4] {
    [
        "1. 本剧全部素材（含分集视频、封面、简介）的著作权或合法授权归提交方所有；".into(),
        "2. 提交的材料信息真实、准确、完整；".into(),
        format!("3. {}", ai_statement(ai_generated)),
        "4. 因材料不实引起的一切责任由提交方承担。".into(),
    ]
}

fn ownership_declaration(drama_name: &str, ai_generated: bool) -> String {
    let items = declaration_items(ai_generated).join("\n");
    format!(
        "内容权属声明\n\n剧目名称：{drama_name}\n\n本人/本单位声明：\n{items}\n\n\
         声明人（签字/盖章）：________________\n日期：________________\n"
    )
}

// 签章位：A4（210 × 297 毫米），都放在第 1 页下方；正文写到 content_bottom_mm 为止。
const CONTENT_BOTTOM_MM: f64 = 200.0;

fn cost_sheet_document(form: &CostSheetForm) -> SealedDocument {
    SealedDocument {
        path: "04_成本配置表/成本配置表.pdf".into(),
        title: "成本配置情况说明".into(),
        blocks: vec![
            Block::Note {
                text: COST_NOTE.into(),
            },
            Block::Table {
                rows: cost_rows(form),
            },
            Block::Paragraph {
                text: COST_DUTY.into(),
            },
        ],
        content_bottom_mm: CONTENT_BOTTOM_MM,
        slots: vec![SealSlot {
            page: 1,
            x_mm: 130.0,
            y_mm: 215.0,
            width_mm: 50.0,
            height_mm: 50.0,
            kind: SealKind::CompanySeal,
            label: "制作机构盖章处".into(),
        }],
    }
}

fn ownership_document(drama_name: &str, ai_generated: bool) -> SealedDocument {
    let mut blocks = vec![
        Block::Paragraph {
            text: format!("剧目名称：{drama_name}"),
        },
        Block::Paragraph {
            text: "本人/本单位声明：".into(),
        },
    ];
    blocks.extend(
        declaration_items(ai_generated)
            .into_iter()
            .map(|text| Block::Paragraph { text }),
    );
    SealedDocument {
        path: "05_权属声明/内容权属声明.pdf".into(),
        title: "内容权属声明".into(),
        blocks,
        content_bottom_mm: CONTENT_BOTTOM_MM,
        slots: vec![
            SealSlot {
                page: 1,
                x_mm: 30.0,
                y_mm: 225.0,
                width_mm: 60.0,
                height_mm: 25.0,
                kind: SealKind::Signature,
                label: "声明人签字处".into(),
            },
            SealSlot {
                page: 1,
                x_mm: 130.0,
                y_mm: 215.0,
                width_mm: 50.0,
                height_mm: 50.0,
                kind: SealKind::CompanySeal,
                label: "盖章处".into(),
            },
        ],
    }
}
// ---------- 组装（桌面端调用） ----------

/// 组装报审包的输入。
pub struct AssembleRequest<'a> {
    /// 服务端发回的 `manifest.json` 原始字节和 `manifest.sig`。
    pub manifest: &'a [u8],
    pub signature: &'a [u8],
    /// 服务端发回的文件：包内路径 → 内容。
    pub server_files: &'a HashMap<String, Vec<u8>>,
    /// 本机文件：剧目目录里的文件名 → 实际路径（封面、简介、各集）。
    pub local_sources: &'a HashMap<String, PathBuf>,
    /// 最终的报审包目录（必须还不存在）。
    pub output_dir: &'a Path,
    pub trusted: &'a [TrustedKey],
    pub device_id: &'a str,
}

/// 组装结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageResult {
    pub package_dir: PathBuf,
    /// 包里的文件数（不含 `manifest.json` / `manifest.sig`）。
    pub file_count: u32,
    pub total_bytes: u64,
    pub package_id: String,
    /// 用的是占位章：不具法律效力。
    pub placeholder: bool,
    /// 用了占位章、带 `_未签署` 后缀的文档。
    pub unsigned_documents: Vec<String>,
    /// 每份要盖章的文档实际用的签章方式（`mock` / `scan`）。
    ///
    /// 混合签章时整包不能只写一个结论：使用者要能逐份看清哪份是扫描件、哪份仍是占位章。
    #[serde(default)]
    pub document_signers: Vec<DocumentSigner>,
}

/// 一份盖章文档的实际签章方式。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentSigner {
    /// 包里的相对路径（占位章的那份带 `_未签署` 后缀）。
    pub document: String,
    /// `mock` 或 `scan`。
    pub signer: String,
}

/// 组装失败。`code()` 给前端分支用。
#[derive(Debug, thiserror::Error)]
pub enum AssembleError {
    #[error("{0}")]
    Manifest(#[from] ManifestError),
    #[error("服务端发回的文件和清单对不上：{0}")]
    ServerFile(String),
    #[error("找不到本机文件 {0}：它在提交后被移走或改名了，请重新生成")]
    LocalMissing(String),
    #[error("{0} 在提交后被改动或调换过（内容和提交时不一致），请重新生成")]
    LocalChanged(String),
    #[error("报审包目录已存在：{0}。请选择新的输出目录或先移除旧目录。")]
    AlreadyExists(String),
    #[error("写报审包失败：{0}")]
    Io(#[from] std::io::Error),
}

impl AssembleError {
    pub fn code(&self) -> &'static str {
        match self {
            AssembleError::Manifest(_) => "manifest_invalid",
            AssembleError::ServerFile(_) => "server_file_mismatch",
            AssembleError::LocalMissing(_) => "local_missing",
            AssembleError::LocalChanged(_) => "local_changed",
            AssembleError::AlreadyExists(_) => "already_exists",
            AssembleError::Io(_) => "io",
        }
    }
}

/// 组装报审包。`progress(已完成, 总数, 说明)` 每处理完一个本机文件调用一次。
pub fn assemble_package(
    request: &AssembleRequest,
    mut progress: impl FnMut(usize, usize, &str),
) -> Result<PackageResult, AssembleError> {
    let manifest = manifest::verify(
        request.manifest,
        request.signature,
        request.trusted,
        request.device_id,
    )?;
    check_server_files(&manifest, request.server_files)?;
    if request.output_dir.exists() {
        return Err(AssembleError::AlreadyExists(
            request.output_dir.display().to_string(),
        ));
    }

    let staging = staging_dir(request.output_dir)?;
    let written = write_package(&manifest, request, &staging, &mut progress);
    let result = written.and_then(|()| {
        // 改名前再看一次：组装期间别人可能在同一个位置建了目录。
        if request.output_dir.exists() {
            return Err(AssembleError::AlreadyExists(
                request.output_dir.display().to_string(),
            ));
        }
        std::fs::rename(&staging, request.output_dir).map_err(AssembleError::from)
    });
    if let Err(error) = result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    Ok(PackageResult {
        package_dir: request.output_dir.to_path_buf(),
        file_count: manifest.files.len() as u32,
        total_bytes: manifest.files.iter().map(|file| file.size_bytes).sum(),
        package_id: manifest.package_id.clone(),
        placeholder: manifest.signature.placeholder,
        unsigned_documents: {
            // 优先用服务端给的「逐份签章方式」：混合签章时只有**部分**文档是占位章，
            // 把 documents 整个当成未签署的会把扫描件也列进去（那是不实陈述）。
            if manifest.signature.per_document.is_empty() {
                // 旧版服务端没有这个字段：退回「整包占位」的旧语义。
                if manifest.signature.placeholder {
                    manifest.signature.documents.clone()
                } else {
                    Vec::new()
                }
            } else {
                manifest
                    .signature
                    .per_document
                    .iter()
                    .filter(|item| item.signer == "mock")
                    .map(|item| item.document.clone())
                    .collect()
            }
        },
        document_signers: manifest
            .signature
            .per_document
            .iter()
            .map(|item| DocumentSigner {
                document: item.document.clone(),
                signer: item.signer.clone(),
            })
            .collect(),
    })
}

/// 服务端文件必须和清单里 `source: server` 的条目一一对应，内容的哈希和大小都一致。
fn check_server_files(
    manifest: &Manifest,
    server_files: &HashMap<String, Vec<u8>>,
) -> Result<(), AssembleError> {
    let mut expected = HashSet::new();
    for file in manifest
        .files
        .iter()
        .filter(|f| f.source == FileSource::Server)
    {
        expected.insert(file.path.as_str());
        let bytes = server_files
            .get(&file.path)
            .ok_or_else(|| AssembleError::ServerFile(format!("缺少 {}", file.path)))?;
        if bytes.len() as u64 != file.size_bytes || sha256_bytes(bytes) != file.sha256 {
            return Err(AssembleError::ServerFile(format!(
                "{} 内容不一致",
                file.path
            )));
        }
    }
    if let Some(extra) = server_files
        .keys()
        .find(|path| !expected.contains(path.as_str()))
    {
        return Err(AssembleError::ServerFile(format!("清单里没有 {extra}")));
    }
    Ok(())
}

/// 和正式目录同级的临时目录：改名是同一个文件系统里的原子操作。
fn staging_dir(output_dir: &Path) -> Result<PathBuf, AssembleError> {
    let parent = output_dir
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let name = output_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "报审包".into());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let staging = parent.join(format!(".{name}.partial-{}-{nanos}", std::process::id()));
    std::fs::create_dir(&staging)?;
    Ok(staging)
}
fn write_package(
    manifest: &Manifest,
    request: &AssembleRequest,
    staging: &Path,
    progress: &mut impl FnMut(usize, usize, &str),
) -> Result<(), AssembleError> {
    // 路径都已在验签时检查过（只许安全的相对路径），这里可以直接拼。
    for file in manifest
        .files
        .iter()
        .filter(|f| f.source == FileSource::Server)
    {
        let target = staging.join(&file.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, &request.server_files[&file.path])?;
    }

    let local: Vec<_> = manifest
        .files
        .iter()
        .filter(|f| f.source == FileSource::Local)
        .collect();
    let total = local.len() + manifest.references.len();
    let mut done = 0;
    for file in local {
        let name = file.path.rsplit('/').next().unwrap_or(&file.path);
        let source = local_source(request, name)?;
        let target = staging.join(&file.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // 边复制边算：复制进包的分集不用多读一遍。
        let (size, sha256) = copy_hashing(source, &target)?;
        if size != file.size_bytes || sha256 != file.sha256 {
            return Err(AssembleError::LocalChanged(name.to_owned()));
        }
        done += 1;
        progress(done, total, &format!("正在放入 {name}"));
    }
    for reference in &manifest.references {
        let source = local_source(request, &reference.path)?;
        let (size, sha256) = hash_file(source)?;
        if size != reference.size_bytes || sha256 != reference.sha256 {
            return Err(AssembleError::LocalChanged(reference.path.clone()));
        }
        done += 1;
        progress(done, total, &format!("正在核对 {}", reference.path));
    }

    // 清单原样写回：签名针对的是这些字节，重新序列化就验不过了。
    std::fs::write(staging.join("manifest.json"), request.manifest)?;
    std::fs::write(staging.join("manifest.sig"), request.signature)?;
    Ok(())
}

fn local_source<'r>(request: &'r AssembleRequest, name: &str) -> Result<&'r Path, AssembleError> {
    request
        .local_sources
        .get(name)
        .map(PathBuf::as_path)
        .filter(|path| path.is_file())
        .ok_or_else(|| AssembleError::LocalMissing(name.to_owned()))
}

/// 读一个文件，返回大小和 SHA-256（小写十六进制）。
pub fn hash_file(path: &Path) -> std::io::Result<(u64, String)> {
    stream_hashing(path, None)
}

fn copy_hashing(source: &Path, target: &Path) -> std::io::Result<(u64, String)> {
    let mut output = File::create(target)?;
    let result = stream_hashing(source, Some(&mut output))?;
    output.sync_all()?;
    Ok(result)
}

fn stream_hashing(path: &Path, mut sink: Option<&mut File>) -> std::io::Result<(u64, String)> {
    let mut hasher = Sha256::new();
    let mut file = File::open(path)?;
    let mut buffer = vec![0_u8; 1 << 20];
    let mut size = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        if let Some(sink) = sink.as_deref_mut() {
            sink.write_all(&buffer[..read])?;
        }
        size += read as u64;
    }
    Ok((size, format!("{:x}", hasher.finalize())))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
