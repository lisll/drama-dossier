//! 报审包生成：把一部剧的材料整理成结构化目录 + 清单。
//!
//! 合规设计：
//! - 只做「真实文件的整理与格式化」，不生成任何伪造签名/印章/时间戳；
//! - 包内固定包含《内容权属声明》模板，由使用者如实填写并负责。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;

use crate::publish::{csv_escape, csv_text_with_bom};
use crate::types::ProbedEpisode;

/// 报审包生成选项。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageOptions {
    /// 输出根目录（默认在剧目录内创建 `报审包/`）。
    pub output_dir: Option<PathBuf>,
    /// 是否把分集视频复制进包（false 时清单仅记录原路径与校验和）。
    pub copy_episodes: bool,
    /// 成本配置表表单（真实数据，由使用者填写）。
    pub cost_sheet: Option<CostSheetForm>,
    /// AI 生成内容声明（若剧名/简介/封面由 AI 辅助生成，应如实勾选）。
    pub ai_generated: bool,
    /// 使用者确认成片画面已按平台要求标注 AI 生成声明（例如深度处理烧录了 AI 合规角标）。
    /// 只有确认了，权属声明里才写「已按平台要求进行 AI 生成标识」——不替使用者声称没核实过的事。
    #[serde(default)]
    pub ai_label_confirmed: bool,
    /// 输出目录已存在时是否覆盖（默认不覆盖：先让使用者确认）。
    #[serde(default)]
    pub overwrite: bool,
    /// 覆盖方式（只在使用 `overwrite` 时有意义）。
    #[serde(default)]
    pub overwrite_mode: OverwriteMode,
    /// 是否把深度处理的 AI 制作证明与过程图收进包（`07_证明与过程图/`）。
    #[serde(default = "default_true")]
    pub with_proof: bool,
    /// 深度处理输出目录（读取证明与过程图的地方）；None / 空表示没有跑的记录。
    #[serde(default)]
    pub proof_source_dir: Option<PathBuf>,
}

fn default_true() -> bool {
    true
}

/// 覆盖旧包的方式。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OverwriteMode {
    /// 整体替换：旧包整个换掉，使用者放进旧包里的其他文件也会消失。
    #[default]
    ReplaceAll,
    /// 只替换标准文件：只覆盖本程序生成的那些路径，旧包里其他文件原地保留，
    /// 并按标准命名清理本次不再生成的旧文件。
    StandardOnly,
}

/// 报审包的固定目录名与包内文件名。`StandardOnly` 靠它区分「本程序生成的文件」与「使用者自己放的文件」。
pub const DIR_INFO: &str = "01_剧集信息";
pub const DIR_COVER: &str = "02_封面海报";
pub const DIR_LIST: &str = "03_分集清单";
pub const DIR_COST: &str = "04_成本配置表";
pub const DIR_DECL: &str = "05_权属声明";
pub const DIR_EPISODES: &str = "06_分集视频";
pub const DIR_PROOF: &str = "07_证明与过程图";
pub const FILE_TITLE: &str = "剧名.txt";
pub const FILE_LIST: &str = "分集清单.csv";
pub const FILE_COST: &str = "成本配置表.md";
pub const FILE_DECL: &str = "内容权属声明.txt";
pub const FILE_MANIFEST: &str = "manifest.json";
/// 包内保留的文件（不含目录）：固定名单里的几个，外加按命名规则生成的分集、简介、封面、证明与过程图。
fn is_standard_named_file(rel: &str) -> bool {
    let (dir, name) = match rel.split_once('/') {
        Some(parts) => parts,
        // 包根：只有 manifest.json 是本程序生成的。
        None => return rel == FILE_MANIFEST,
    };
    match dir {
        DIR_INFO => name == FILE_TITLE || stem_is(name, "简介"),
        DIR_COVER => stem_is(name, "封面"),
        DIR_LIST => name == FILE_LIST,
        DIR_COST => name == FILE_COST,
        DIR_DECL => name == FILE_DECL,
        DIR_EPISODES => is_episode_name(name),
        DIR_PROOF => is_proof_name(name),
        _ => false,
    }
}

/// 文件名主干是否为 `stem`（扩展名任取）：`简介.txt`、`封面.PNG` 都算。
fn stem_is(name: &str, stem: &str) -> bool {
    match name.split_once('.') {
        Some((base, ext)) => base == stem && !ext.is_empty(),
        None => name == stem,
    }
}

/// `第NN集.<ext>`（数字至少两位，与生成规则一致），允许重复集号时的 `-2` 后缀。
fn is_episode_name(name: &str) -> bool {
    let Some((base, ext)) = name.split_once('.') else {
        return false;
    };
    if ext.is_empty() {
        return false;
    }
    let Some(rest) = base.strip_prefix('第') else {
        return false;
    };
    let Some((digits, tail)) = rest.split_once('集') else {
        return false;
    };
    if digits.len() < 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    tail.is_empty()
        || tail
            .strip_prefix('-')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// `AI制作证明.png|txt`、`过程图_NN.png`，以及随包附上的 `剧目海报.<ext>`。
fn is_proof_name(name: &str) -> bool {
    matches!(name, "AI制作证明.png" | "AI制作证明.txt")
        || is_process_image(name)
        || stem_is(name, "剧目海报")
}

fn is_process_image(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("过程图_") else {
        return false;
    };
    let Some((digits, ext)) = rest.split_once('.') else {
        return false;
    };
    ext == "png" && digits.len() == 2 && digits.bytes().all(|b| b.is_ascii_digit())
}

/// 从深度处理输出目录收进包的证明材料。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProofMaterials {
    /// 包内相对路径（`07_证明与过程图/…`）与源文件，按固定顺序。
    pub files: Vec<(String, PathBuf)>,
    /// 没有收进来时的原因说明（给界面与清单看）。
    pub note: Option<String>,
}

/// 输出目录里的证明材料：AI 制作证明（png 优先，退回 txt）与原色过程图，外加一张剧目海报。
///
/// 只认本程序生成的文件名；目录里没有就如实说明原因，不阻断生成。
pub fn collect_proof_materials(proof_dir: Option<&Path>) -> ProofMaterials {
    let Some(dir) = proof_dir else {
        return ProofMaterials {
            files: Vec::new(),
            note: Some("未指定深度处理输出目录，包内不含证明与过程图".into()),
        };
    };
    if !dir.is_dir() {
        return ProofMaterials {
            files: Vec::new(),
            note: Some(format!(
                "没有找到深度处理输出目录（{}），包内不含证明与过程图；请先跑一次深度处理",
                dir.display()
            )),
        };
    }
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for name in ["AI制作证明.png", "AI制作证明.txt"] {
        let path = dir.join(name);
        if path.is_file() {
            files.push((format!("{DIR_PROOF}/{name}"), path));
            break;
        }
    }
    for index in 1..=4 {
        let name = format!("过程图_{index:02}.png");
        let path = dir.join(&name);
        if path.is_file() {
            files.push((format!("{DIR_PROOF}/{name}"), path));
        }
    }
    // 剧目海报（本程序按 `剧目海报.<ext>` 生成）也一并附上，方便与样包的材料构成一致。
    for ext in ["png", "jpg", "jpeg", "webp"] {
        let path = dir.join(format!("剧目海报.{ext}"));
        if path.is_file() {
            files.push((format!("{DIR_PROOF}/剧目海报.{ext}"), path));
            break;
        }
    }
    let note = files.is_empty().then(|| {
        format!(
            "深度处理输出目录（{}）里没有证明或过程图，包内不含这些材料",
            dir.display()
        )
    });
    ProofMaterials { files, note }
}

/// 把证明材料复制进 `07_证明与过程图/`；一个都没有时不建目录。
fn copy_proof_materials(
    pkg_dir: &Path,
    opts: &PackageOptions,
) -> std::io::Result<Option<ProofMaterials>> {
    if !opts.with_proof {
        return Ok(Some(ProofMaterials {
            files: Vec::new(),
            note: Some("本次未勾选「生成证明与过程图」".into()),
        }));
    }
    let materials = collect_proof_materials(opts.proof_source_dir.as_deref());
    if materials.files.is_empty() {
        return Ok(Some(materials));
    }
    let dir = pkg_dir.join(DIR_PROOF);
    std::fs::create_dir_all(&dir)?;
    for (rel, source) in &materials.files {
        let name = rel.strip_prefix(&format!("{DIR_PROOF}/")).unwrap_or(rel);
        std::fs::copy(source, dir.join(name))?;
    }
    Ok(Some(materials))
}

/// `StandardOnly` 覆盖的合并结果：替换了哪些、清理了哪些过期文件、保留了哪些使用者自放的文件。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeReport {
    /// 写进（或替换）的文件数。
    pub replaced: u32,
    /// `StandardOnly`：被清掉的过期标准文件。
    pub removed: Vec<String>,
    /// `StandardOnly`：保留的、使用者自己放的文件。
    pub kept: Vec<String>,
    /// `ReplaceAll`：旧包里被整体换掉的文件数（含使用者自放的文件）。
    pub dropped: u32,
}

/// 路径比较用的键：标准文件名只有程序生成的几种写法，按小写比较不会误判，
/// 也能认出不区分大小写的文件系统上「`.JPG` 与 `.jpg` 是同一个文件」。
fn path_key(rel: &str) -> String {
    rel.to_lowercase()
}

/// 旧包里本次不再生成的标准文件（按 [`path_key`] 比较；非标准文件不在其列）。
fn stale_entries(fresh: &[String], existing: &[String]) -> Vec<String> {
    let fresh_keys: std::collections::HashSet<String> =
        fresh.iter().map(|rel| path_key(rel)).collect();
    existing
        .iter()
        .filter(|rel| is_standard_named_file(rel) && !fresh_keys.contains(&path_key(rel)))
        .cloned()
        .collect()
}

/// 写入前的预检：`rel` 在包内的每一级前缀和叶子本身都不能是链接，
/// 叶子已存在时必须是普通文件——否则 rename 会写到包外、替换掉用户的链接，或因目标是目录而失败。
fn ensure_writable_target(pkg_dir: &Path, rel: &str) -> std::io::Result<()> {
    let mut cursor = pkg_dir.to_path_buf();
    let parts: Vec<&str> = rel.split('/').collect();
    for (index, part) in parts.iter().enumerate() {
        cursor.push(part);
        let Ok(meta) = std::fs::symlink_metadata(&cursor) else {
            return Ok(()); // 这一级不存在，后面的也不会存在
        };
        let is_leaf = index + 1 == parts.len();
        if meta.file_type().is_symlink() {
            return Err(std::io::Error::other(format!(
                "旧包里的 {} 是快捷方式/链接，为防写到包外已中止，旧包未改动",
                cursor.display()
            )));
        }
        if is_leaf && !meta.is_file() {
            return Err(std::io::Error::other(format!(
                "旧包里的 {} 不是普通文件，无法替换，旧包未改动",
                cursor.display()
            )));
        }
        if !is_leaf && !meta.is_dir() {
            return Err(std::io::Error::other(format!(
                "旧包里的 {} 不是文件夹，无法写入，旧包未改动",
                cursor.display()
            )));
        }
    }
    Ok(())
}

/// `create_dir_all` 的记账版：返回本次新建的各层（由外到内），失败回滚时逆序删除。
fn create_dirs_tracked(dir: &Path, created: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let mut missing = Vec::new();
    let mut cursor = Some(dir);
    while let Some(path) = cursor {
        if std::fs::symlink_metadata(path).is_ok() {
            break;
        }
        missing.push(path.to_path_buf());
        cursor = path.parent();
    }
    for path in missing.into_iter().rev() {
        std::fs::create_dir(&path)?;
        created.push(path);
    }
    Ok(())
}

/// 本次合并的临时文件名：带 nonce，只清理自己的，不会扫掉另一个实例正在用的。
fn merge_temp_path(target: &Path, nonce: &str) -> PathBuf {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    target.with_file_name(format!(".{name}.dossier-new-{nonce}"))
}

/// 删除本次的临时文件：只删普通文件，跳过目录与链接。
fn remove_temp_file(path: &Path) {
    if std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file()) {
        let _ = std::fs::remove_file(path);
    }
}

/// 把 staging 里的标准文件覆盖进 `pkg_dir`，清掉旧包里本次不再生成的标准文件，
/// 其余文件（使用者自己放进去的、链接）原地保留。分三个阶段：
/// 1. 预检 + 复制到带 nonce 的临时名：任何失败都回滚本次写入，旧包一个字节不动；
/// 2. 逐个改名到正式名，`manifest.json` 最后；
/// 3. 清理过期文件（只在新旧两种写法同时作为独立条目存在时才删大小写不同的旧写法）。
fn merge_standard_files_with_nonce(
    staging: &Path,
    pkg_dir: &Path,
    nonce: &str,
) -> std::io::Result<MergeReport> {
    let mut fresh: Vec<String> = Vec::new();
    collect_relative_files(staging, staging, &mut fresh, &mut Vec::new())?;
    fresh.sort();
    // manifest 最后改名：中途失败时旧 manifest 仍对应旧包。
    if let Some(index) = fresh.iter().position(|rel| rel == FILE_MANIFEST) {
        let manifest = fresh.remove(index);
        fresh.push(manifest);
    }
    let mut existing: Vec<String> = Vec::new();
    let mut links: Vec<String> = Vec::new();
    collect_relative_files(pkg_dir, pkg_dir, &mut existing, &mut links)?;

    // 阶段 1：预检全部目标，再复制到临时名。
    for rel in &fresh {
        ensure_writable_target(pkg_dir, rel)?;
    }
    let mut created_dirs: Vec<PathBuf> = Vec::new();
    let mut temps: Vec<(PathBuf, PathBuf)> = Vec::new();
    let copied: std::io::Result<()> = (|| {
        for rel in &fresh {
            let target = pkg_dir.join(rel);
            if let Some(parent) = target.parent() {
                create_dirs_tracked(parent, &mut created_dirs)?;
            }
            let temp = merge_temp_path(&target, nonce);
            temps.push((temp.clone(), target));
            std::fs::copy(staging.join(rel), &temp)?;
        }
        Ok(())
    })();
    if let Err(error) = copied {
        for (temp, _) in &temps {
            remove_temp_file(temp);
        }
        for dir in created_dirs.iter().rev() {
            let _ = std::fs::remove_dir(dir);
        }
        return Err(error);
    }

    // 阶段 2：改名到正式名。
    for (index, (temp, target)) in temps.iter().enumerate() {
        if let Err(error) = std::fs::rename(temp, target) {
            for (rest, _) in &temps[index..] {
                remove_temp_file(rest);
            }
            return Err(std::io::Error::new(
                error.kind(),
                format!(
                    "替换 {} 失败：{error}。包可能不完整，请改用「整体替换」重新生成；\
                     如在 Windows 上，请先关闭正在使用包内文件的程序",
                    target.display()
                ),
            ));
        }
    }

    // 阶段 3：清理过期标准文件。
    let mut report = MergeReport {
        replaced: fresh.len() as u32,
        ..MergeReport::default()
    };
    let fresh_exact: std::collections::HashSet<&str> = fresh.iter().map(String::as_str).collect();
    let mut failed: Vec<String> = Vec::new();
    let mut remove = |rel: &str, report: &mut MergeReport| {
        let path = pkg_dir.join(rel);
        match std::fs::remove_file(&path) {
            Ok(()) => {
                report.removed.push(rel.to_string());
                remove_empty_parents(&path, pkg_dir);
            }
            Err(error) => failed.push(format!("{rel}（{error}）")),
        }
    };
    for rel in stale_entries(&fresh, &existing) {
        remove(&rel, &mut report);
    }
    // 大小写不同的同名旧文件：只有它和新写法同时作为独立条目存在时才删（区分大小写的文件系统）。
    for rel in &existing {
        if fresh_exact.contains(rel.as_str()) || !is_standard_named_file(rel) {
            continue;
        }
        let Some(twin) = fresh.iter().find(|new| path_key(new) == path_key(rel)) else {
            continue;
        };
        if listed_exactly(pkg_dir, rel) && listed_exactly(pkg_dir, twin) {
            remove(rel, &mut report);
        }
    }
    report.kept = existing
        .iter()
        .filter(|rel| !is_standard_named_file(rel))
        .cloned()
        .chain(links)
        .collect();
    if !failed.is_empty() {
        return Err(std::io::Error::other(format!(
            "新文件已全部写入；以下过期文件没能删除：{}",
            failed.join("；")
        )));
    }
    Ok(report)
}

/// `rel` 的这个确切写法是否作为独立条目出现在父目录的列表里。
fn listed_exactly(pkg_dir: &Path, rel: &str) -> bool {
    let path = pkg_dir.join(rel);
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    std::fs::read_dir(parent)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .any(|entry| entry.file_name() == name)
        })
        .unwrap_or(false)
}

/// 删掉因清理过期文件而变空的上级目录（包根 `pkg_dir` 本身不删）。
fn remove_empty_parents(path: &Path, pkg_dir: &Path) {
    let mut cursor = path.parent();
    while let Some(dir) = cursor {
        if dir == pkg_dir || std::fs::remove_dir(dir).is_err() {
            break;
        }
        cursor = dir.parent();
    }
}

/// 目录下所有文件的相对路径（`/` 分隔），跳过以 `.` 开头的临时文件。
/// 不跟随链接：链接（含 Windows junction）记进 `links`，既不进入也不当文件处理。
fn collect_relative_files(
    root: &Path,
    current: &Path,
    out: &mut Vec<String>,
    links: &mut Vec<String>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            links.push(rel);
        } else if kind.is_dir() {
            collect_relative_files(root, &path, out, links)?;
        } else {
            out.push(rel);
        }
    }
    Ok(())
}

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

/// 生成结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageResult {
    pub package_dir: PathBuf,
    pub file_count: u32,
    pub total_bytes: u64,
    /// 本次使用的覆盖方式；没有覆盖旧包时为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overwrite: Option<MergeReport>,
    /// 证明与过程图的收集情况（含「为什么包里没有」的说明）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<ProofMaterials>,
}

/// 生成报审包：先写到同级的临时目录，`manifest.json` 写完后整体改名为最终目录。
///
/// 中途失败（磁盘满、某个文件读不到）时删除临时目录：不会留下半个包，
/// 也就不会出现「下一次因为目录已存在而拒绝生成、只能手动删」的情况。
pub fn generate_report_package(
    drama_name: &str,
    drama_dir: &Path,
    cover: Option<&Path>,
    synopsis: Option<&Path>,
    episodes: &[ProbedEpisode],
    opts: &PackageOptions,
) -> std::io::Result<PackageResult> {
    let pkg_dir = opts
        .output_dir
        .clone()
        .unwrap_or_else(|| drama_dir.join("报审包"));
    if let Some(form) = &opts.cost_sheet {
        form.validate()
            .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
    }
    // 入口判一次三态：只有「有内容」的目录才需要确认；要覆盖时先过安全闸并记下 `guarded`。
    // 落盘前 `commit_staging` 还会再判一次：生成要跑几分钟，目录可能在这期间被写进东西。
    let guarded = match target_state(&pkg_dir) {
        TargetState::Missing | TargetState::Empty => false,
        TargetState::Occupied if !opts.overwrite => return Err(package_exists_error(&pkg_dir)),
        TargetState::Occupied => {
            guard_overwrite_target(&pkg_dir, drama_dir)?;
            true
        }
    };

    let parent = pkg_dir.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let nonce = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default()
    );
    let leaf = pkg_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "报审包".into());
    let staging = parent.join(format!(".{leaf}.part-{nonce}"));
    let written = write_package_contents(&staging, drama_name, cover, synopsis, episodes, opts)
        .and_then(|summary| {
            let overwrite = commit_staging(
                &staging,
                &pkg_dir,
                guarded,
                opts.overwrite_mode,
                &nonce,
                summary.0,
            )?;
            Ok((summary.0, summary.1, overwrite, summary.2))
        });
    match written {
        Ok((file_count, total_bytes, overwrite, proof)) => Ok(PackageResult {
            package_dir: pkg_dir,
            file_count,
            total_bytes,
            overwrite,
            proof,
        }),
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            Err(error)
        }
    }
}

/// 报审包 manifest 的 schema 标识：覆盖前据此确认目标真的是剧备生成的包。
pub const PACKAGE_SCHEMA: &str = "drama-dossier/package@1";

/// 「报审包目录已存在」类错误的固定前缀：前端只认这个前缀给出覆盖按钮，
/// 不再匹配中文文本（中文 Windows 的系统错误 183 也含「已存在」字样）。
pub const PACKAGE_EXISTS_MARK: &str = "[PACKAGE_EXISTS] ";

/// manifest 读取上限：真包的 manifest 只有几十 KB，超过就不当报审包处理。
const MANIFEST_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// 视作「空」时可以忽略的系统杂项文件（按不区分大小写比较）。
const SYSTEM_JUNK: [&str; 3] = [".ds_store", "thumbs.db", "desktop.ini"];

fn package_exists_error(pkg_dir: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "{PACKAGE_EXISTS_MARK}报审包目录已存在：{}。可以选择覆盖旧包，或换一个输出目录。",
            pkg_dir.display()
        ),
    )
}

/// 目标位置的状态：不存在 / 空目录（只有系统杂项）/ 有内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetState {
    Missing,
    Empty,
    Occupied,
}

/// 判定目标位置状态；读不了目录（如没有权限）按「有内容」处理，绝不当成空目录去删。
fn target_state(dir: &Path) -> TargetState {
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return TargetState::Missing;
    };
    if !meta.is_dir() {
        return TargetState::Occupied;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return TargetState::Occupied;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return TargetState::Occupied;
        };
        let name = entry.file_name().to_string_lossy().to_lowercase();
        let is_file = entry
            .file_type()
            .map(|kind| kind.is_file())
            .unwrap_or(false);
        if !(is_file && SYSTEM_JUNK.contains(&name.as_str())) {
            return TargetState::Occupied;
        }
    }
    TargetState::Empty
}

/// 目录里是不是一个剧备生成的报审包：`manifest.json` 是普通文件、不太大、schema 对得上。
fn looks_like_package(dir: &Path) -> bool {
    let path = dir.join(FILE_MANIFEST);
    let Ok(meta) = std::fs::symlink_metadata(&path) else {
        return false;
    };
    if !meta.is_file() || meta.len() > MANIFEST_MAX_BYTES {
        return false;
    }
    std::fs::read(&path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<serde_json::Value>(&raw).ok())
        .and_then(|value| {
            value
                .get("schema")?
                .as_str()
                .map(|schema| schema == PACKAGE_SCHEMA)
        })
        .unwrap_or(false)
}

/// 把写好的 staging 落到 `pkg_dir`。落盘前重新判定目标状态：
/// - 不存在：直接改名；
/// - 空目录：清掉系统杂项、`remove_dir`（只能删空目录，本身就是保险）后改名；
/// - 有内容但入口没过安全闸（生成期间被写进了东西）：按「已存在」拒绝；
/// - 有内容且过了安全闸：再确认一次是报审包，然后按覆盖方式替换。
fn commit_staging(
    staging: &Path,
    pkg_dir: &Path,
    guarded: bool,
    mode: OverwriteMode,
    nonce: &str,
    file_count: u32,
) -> std::io::Result<Option<MergeReport>> {
    match target_state(pkg_dir) {
        TargetState::Missing => {
            std::fs::rename(staging, pkg_dir)?;
            Ok(None)
        }
        TargetState::Empty => {
            for name in std::fs::read_dir(pkg_dir)?.filter_map(Result::ok) {
                std::fs::remove_file(name.path())?;
            }
            if std::fs::remove_dir(pkg_dir).is_err() {
                // 删杂项和删目录之间又被写进了东西：当作冲突，用户的内容不动。
                return Err(package_exists_error(pkg_dir));
            }
            if let Err(error) = std::fs::rename(staging, pkg_dir) {
                // 改名失败：把用户原来的空文件夹补回来，别让它凭空消失。
                let _ = std::fs::create_dir(pkg_dir);
                return Err(error);
            }
            Ok(None)
        }
        TargetState::Occupied if !guarded => Err(package_exists_error(pkg_dir)),
        TargetState::Occupied => {
            if !looks_like_package(pkg_dir) {
                return Err(refuse_overwrite(pkg_dir, NOT_A_PACKAGE));
            }
            match mode {
                OverwriteMode::ReplaceAll => {
                    replace_all(staging, pkg_dir, nonce, file_count).map(Some)
                }
                OverwriteMode::StandardOnly => {
                    let report = merge_standard_files_with_nonce(staging, pkg_dir, nonce)?;
                    let _ = std::fs::remove_dir_all(staging);
                    Ok(Some(report))
                }
            }
        }
    }
}

/// 整体替换：旧包先挪成备份 → 新包就位 → 删备份。任何时刻正式目录都是完整的旧包或新包。
fn replace_all(
    staging: &Path,
    pkg_dir: &Path,
    nonce: &str,
    replaced: u32,
) -> std::io::Result<MergeReport> {
    let parent = pkg_dir.parent().unwrap_or(Path::new("."));
    let leaf = pkg_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "报审包".into());
    let backup = parent.join(format!(".{leaf}.old-{nonce}"));
    std::fs::rename(pkg_dir, &backup)?;
    if let Err(error) = std::fs::rename(staging, pkg_dir) {
        // 新包没能就位：把旧包改回原名，别让用户发现包不见了。
        let _ = std::fs::rename(&backup, pkg_dir);
        return Err(error);
    }
    // 新包已就位：备份里的文件数只用于报告，统计出错不能把成功报成失败。
    let dropped = count_files_best_effort(&backup);
    let _ = std::fs::remove_dir_all(&backup);
    Ok(MergeReport {
        replaced,
        removed: Vec::new(),
        kept: Vec::new(),
        dropped,
    })
}

/// 目录下的文件数；读不全时返回 0（只用于报告，不影响结果）。
fn count_files_best_effort(dir: &Path) -> u32 {
    let mut files: Vec<String> = Vec::new();
    collect_relative_files(dir, dir, &mut files, &mut Vec::new())
        .map(|()| files.len() as u32)
        .unwrap_or(0)
}

const NOT_A_PACKAGE: &str =
    "这不是剧备生成的报审包（缺少 manifest.json），为防误删不能覆盖。请换一个空文件夹或新文件夹";

fn refuse_overwrite(pkg_dir: &Path, reason: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("拒绝覆盖 {reason}：{}", pkg_dir.display()),
    )
}

/// 覆盖前的安全闸：只允许替换一个真正的报审包目录。
/// 拒绝：链接、无法确认位置、剧目目录本身或它的上级目录（含磁盘根）、不是剧备生成的包。
fn guard_overwrite_target(pkg_dir: &Path, drama_dir: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(pkg_dir)?;
    if meta.file_type().is_symlink() {
        return Err(refuse_overwrite(pkg_dir, "旧包位置是快捷方式/链接"));
    }
    if !meta.is_dir() {
        return Err(refuse_overwrite(pkg_dir, "不是目录的旧包位置"));
    }
    // 规范化失败就拒绝，不退回字面比较：Windows 上 `Path` 比较区分大小写，字面比较会漏判。
    let (Ok(pkg), Ok(drama)) = (
        std::fs::canonicalize(pkg_dir),
        std::fs::canonicalize(drama_dir),
    ) else {
        return Err(refuse_overwrite(
            pkg_dir,
            "无法确认旧包位置（请手动删除旧包或换一个目录）",
        ));
    };
    if pkg.parent().is_none() {
        return Err(refuse_overwrite(pkg_dir, "磁盘根目录"));
    }
    // `Path::starts_with` 按路径组件比较，`D:\剧集` 不会被当成 `D:\剧` 的下级。
    if drama.starts_with(&pkg) {
        return Err(refuse_overwrite(pkg_dir, "剧目目录本身或它的上级目录"));
    }
    if !looks_like_package(pkg_dir) {
        return Err(refuse_overwrite(pkg_dir, NOT_A_PACKAGE));
    }
    Ok(())
}

/// 把报审包的全部内容写进 `pkg_dir`（临时目录），返回 `(文件数, 总字节数, 证明材料)`。
fn write_package_contents(
    pkg_dir: &Path,
    drama_name: &str,
    cover: Option<&Path>,
    synopsis: Option<&Path>,
    episodes: &[ProbedEpisode],
    opts: &PackageOptions,
) -> std::io::Result<(u32, u64, Option<ProofMaterials>)> {
    std::fs::create_dir_all(pkg_dir)?;

    let mut manifest_files: Vec<ManifestFile> = Vec::new();

    // 01 剧集信息
    let info_dir = pkg_dir.join(DIR_INFO);
    std::fs::create_dir_all(&info_dir)?;
    std::fs::write(info_dir.join(FILE_TITLE), drama_name)?;
    // 包内统一命名（MATERIAL_SPEC「v1 报审包结构」）：简介.<原扩展名>、封面.<原扩展名>、第NN集.<原扩展名>。
    if let Some(s) = synopsis {
        std::fs::copy(s, info_dir.join(fixed_name("简介", s)))?;
    }
    if let Some(c) = cover {
        let poster_dir = pkg_dir.join(DIR_COVER);
        std::fs::create_dir_all(&poster_dir)?;
        std::fs::copy(c, poster_dir.join(fixed_name("封面", c)))?;
    }
    let names = package_episode_names(episodes);

    // 03 分集清单
    let mut csv =
        String::from("集号,文件名,时长(秒),宽,高,视频编码,音频编码,码率(Mbps),SHA256,包内文件名\n");
    for (ep, package_name) in episodes.iter().zip(&names) {
        let no = ep
            .file
            .episode_number
            .map(|n| n.to_string())
            .unwrap_or_default();
        csv.push_str(&format!(
            "{},{},{:.1},{},{},{},{},{:.2},{},{}\n",
            no,
            csv_escape(&ep.file.file_name),
            ep.spec.duration_sec,
            ep.spec.width,
            ep.spec.height,
            csv_escape(ep.spec.video_codec.as_deref().unwrap_or("")),
            csv_escape(ep.spec.audio_codec.as_deref().unwrap_or("")),
            ep.spec.bitrate_bps.unwrap_or(0) as f64 / 1e6,
            sha256_file(&ep.file.path)?,
            if opts.copy_episodes {
                csv_escape(package_name)
            } else {
                String::new()
            },
        ));
    }
    let list_dir = pkg_dir.join(DIR_LIST);
    std::fs::create_dir_all(&list_dir)?;
    // 带 BOM：Windows 上双击用 Excel 打开时中文不乱码。
    std::fs::write(list_dir.join(FILE_LIST), csv_text_with_bom(&csv))?;

    // 04 成本配置表（真实数据模板）
    let cost_dir = pkg_dir.join(DIR_COST);
    std::fs::create_dir_all(&cost_dir)?;
    if let Some(form) = &opts.cost_sheet {
        std::fs::write(cost_dir.join(FILE_COST), render_cost_sheet_md(form))?;
    } else {
        std::fs::write(
            cost_dir.join(FILE_COST),
            render_cost_sheet_md(&CostSheetForm::default()),
        )?;
    }

    // 05 权属声明（固定模板，使用者如实填写）
    let decl_dir = pkg_dir.join(DIR_DECL);
    std::fs::create_dir_all(&decl_dir)?;
    std::fs::write(
        decl_dir.join(FILE_DECL),
        ownership_declaration(drama_name, opts),
    )?;

    // 06 分集视频（可选复制）
    if opts.copy_episodes {
        let ep_dir = pkg_dir.join(DIR_EPISODES);
        std::fs::create_dir_all(&ep_dir)?;
        for (ep, name) in episodes.iter().zip(&names) {
            std::fs::copy(&ep.file.path, ep_dir.join(name))?;
        }
    }

    // 07 证明与过程图（来自深度处理输出目录；一个都没有时不建目录）
    let proof = copy_proof_materials(pkg_dir, opts)?;

    // manifest
    collect_files(pkg_dir, &mut manifest_files)?;
    let manifest = Manifest {
        schema: PACKAGE_SCHEMA.into(),
        drama_name: drama_name.into(),
        generated_at: chrono_now(),
        ai_generated: opts.ai_generated,
        copy_episodes: opts.copy_episodes,
        files: manifest_files,
        proof_files: proof
            .as_ref()
            .map(|proof| {
                proof
                    .files
                    .iter()
                    .map(|(rel, source)| ManifestFile {
                        path: rel.clone(),
                        size_bytes: std::fs::metadata(source).map(|m| m.len()).unwrap_or(0),
                        sha256: sha256_file(source).unwrap_or_default(),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        proof_source: proof.as_ref().and_then(|proof| {
            proof.note.as_ref().map(|note| ProofSourceRecord {
                dir: opts
                    .proof_source_dir
                    .as_ref()
                    .map(|dir| dir.display().to_string()),
                note: note.clone(),
            })
        }),
    };
    std::fs::write(
        pkg_dir.join(FILE_MANIFEST),
        serde_json::to_vec_pretty(&manifest)?,
    )?;

    let file_count = manifest.files.len() as u32;
    let total_bytes = manifest.files.iter().map(|f| f.size_bytes).sum();
    Ok((file_count, total_bytes, proof))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    schema: String,
    drama_name: String,
    generated_at: String,
    ai_generated: bool,
    copy_episodes: bool,
    files: Vec<ManifestFile>,
    /// 收进包的证明与过程图（这些路径同时也出现在 `files` 里，这里单列一份方便平台与核对工具直接取用）。
    proof_files: Vec<ManifestFile>,
    /// 证明材料的来源目录与「没有收进来」的原因；收齐时不写这一项。
    #[serde(skip_serializing_if = "Option::is_none")]
    proof_source: Option<ProofSourceRecord>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProofSourceRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    dir: Option<String>,
    note: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ManifestFile {
    path: String,
    size_bytes: u64,
    sha256: String,
}

fn collect_files(dir: &Path, out: &mut Vec<ManifestFile>) -> std::io::Result<()> {
    for entry in walkdir::WalkDir::new(dir).follow_links(false) {
        let e = entry.map_err(std::io::Error::other)?;
        if e.file_type().is_file() {
            let rel = e
                .path()
                .strip_prefix(dir)
                .unwrap_or(e.path())
                .to_string_lossy()
                .replace('\\', "/");
            out.push(ManifestFile {
                path: rel,
                size_bytes: e.metadata().map(|m| m.len()).unwrap_or(0),
                sha256: sha256_file(e.path())?,
            });
        }
    }
    Ok(())
}

/// 文件的 SHA-256（小写十六进制），64 KiB 分块流式读取，大视频文件也不会整份读进内存。
/// 报审包清单与 AI 制作证明的文件指纹共用这一个实现。
pub fn sha256_file(p: &Path) -> std::io::Result<String> {
    let mut h = Sha256::new();
    let mut file = File::open(p)?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        h.update(&buffer[..read]);
    }
    Ok(format!("{:x}", h.finalize()))
}

/// `<主名>.<原扩展名>`；原文件没有扩展名时只用主名。
fn fixed_name(stem: &str, original: &Path) -> String {
    match original.extension().and_then(|ext| ext.to_str()) {
        Some(ext) if !ext.is_empty() => format!("{stem}.{ext}"),
        _ => stem.to_string(),
    }
}

/// 分集在 `06_分集视频/` 里的文件名，与 `episodes` 一一对应。
///
/// - 有集号：`第NN集.<原扩展名>`，NN 位数取 max(2, 最大集号位数)；
/// - 没有集号：保留原文件名；
/// - 重名（重复集号、或原名恰好撞上）：按（有集号在前、集号、完整路径）的固定顺序，
///   后来者依次加 `-2`、`-3`……；比较不区分大小写（Windows / macOS 默认文件系统）。
pub(crate) fn package_episode_names(episodes: &[ProbedEpisode]) -> Vec<String> {
    let width = episodes
        .iter()
        .filter_map(|ep| ep.file.episode_number)
        .max()
        .map_or(2, |max| max.to_string().len().max(2));
    let wanted: Vec<(String, String)> = episodes
        .iter()
        .map(|ep| {
            let path = Path::new(&ep.file.file_name);
            let ext = path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| format!(".{ext}"))
                .unwrap_or_default();
            match ep.file.episode_number {
                Some(n) => (format!("第{n:0width$}集"), ext),
                None => (
                    path.file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    ext,
                ),
            }
        })
        .collect();
    let mut order: Vec<usize> = (0..episodes.len()).collect();
    order.sort_by(|&a, &b| {
        let (ea, eb) = (&episodes[a].file, &episodes[b].file);
        ea.episode_number
            .is_none()
            .cmp(&eb.episode_number.is_none())
            .then(ea.episode_number.cmp(&eb.episode_number))
            .then_with(|| ea.path.cmp(&eb.path))
    });
    let mut used = std::collections::HashSet::new();
    let mut names = vec![String::new(); episodes.len()];
    for index in order {
        let (stem, ext) = &wanted[index];
        let mut candidate = format!("{stem}{ext}");
        let mut n = 2;
        while !used.insert(candidate.to_lowercase()) {
            candidate = format!("{stem}-{n}{ext}");
            n += 1;
        }
        names[index] = candidate;
    }
    names
}

impl CostSheetForm {
    /// 后端再校验一次（前端已拦）：数值必须是有限数，投资额不小于 0，占比在 0–100 之间。
    pub fn validate(&self) -> Result<(), String> {
        if let Some(value) = self.total_investment_wan {
            if !value.is_finite() || value < 0.0 {
                return Err("投资总额必须是不小于 0 的数字。".into());
            }
        }
        if let Some(value) = self.actor_ratio_percent {
            if !value.is_finite() || !(0.0..=100.0).contains(&value) {
                return Err("演员成本占比必须在 0–100 之间。".into());
            }
        }
        Ok(())
    }
}

/// 成本表里的数值：最多两位小数、去掉末尾 0（`50`、`50.5`、`33.33`；`0.1+0.2` 显示 `0.3`）。
fn format_amount(value: f64) -> String {
    let text = format!("{value:.2}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text == "-0" {
        "0".into()
    } else {
        text.to_string()
    }
}

/// Markdown 表格单元格：`|` 会切断表格、换行会断行，分别转义 / 换成空格。
fn md_cell(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace(['\r', '\n'], " ")
}

fn render_cost_sheet_md(f: &CostSheetForm) -> String {
    format!(
        "# 成本配置情况说明\n\n\
         > 本表由 drama-dossier（剧备）根据使用者填写的真实数据生成，仅作格式化排版，不构成任何背书。\n\n\
         | 项目 | 内容 |\n|---|---|\n\
         | 制作机构 | {} |\n\
         | 投资总额 | {} 万元 |\n\
         | 演员成本占比 | {} % |\n\
         | 联系方式 | {} |\n\n\
         使用者须对以上数据的真实性负责。\n",
        md_cell(&f.company_name),
        f.total_investment_wan
            .map(format_amount)
            .unwrap_or_else(|| "（待填写）".into()),
        f.actor_ratio_percent
            .map(format_amount)
            .unwrap_or_else(|| "（待填写）".into()),
        md_cell(&f.contact),
    )
}

fn ownership_declaration(drama_name: &str, opts: &PackageOptions) -> String {
    // 只陈述使用者勾选确认过的事实：没确认成片已标注，就不写「已按平台要求进行标识」。
    let ai = match (opts.ai_generated, opts.ai_label_confirmed) {
        (true, true) => {
            "本剧部分材料（剧名/简介/封面）由 AI 辅助生成；成片已按平台要求进行 AI 生成标识。"
        }
        (true, false) => "本剧部分材料（剧名/简介/封面）由 AI 辅助生成。",
        (false, _) => "本剧材料未使用 AI 生成。",
    };
    format!(
        "内容权属声明\n\n\
         剧目名称：{drama_name}\n\n\
         本人/本单位声明：\n\
         1. 本剧全部素材（含分集视频、封面、简介）的著作权或合法授权归提交方所有；\n\
         2. 提交的材料信息真实、准确、完整；\n\
         3. {ai}\n\
         4. 因材料不实引起的一切责任由提交方承担。\n\n\
         声明人（签字/盖章）：________________\n\
         日期：________________\n"
    )
}

fn chrono_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{EpisodeFile, EpisodeSpec};

    #[test]
    fn generates_package_skeleton() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        let base = std::env::temp_dir().join(format!("dossier-pkg-{unique}"));
        let d = base.join("剧A");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("简介.txt"),
            "这是一部用于测试的短剧简介，超过三十个字以通过检查。",
        )
        .unwrap();
        std::fs::write(d.join("封面.jpg"), b"jpg").unwrap();
        let eps = vec![ProbedEpisode {
            file: EpisodeFile {
                path: d.join("第1集.mp4"),
                file_name: "第1集.mp4".into(),
                size_bytes: 3,
                episode_number: Some(1),
            },
            spec: EpisodeSpec {
                duration_sec: 61.0,
                width: 1080,
                height: 1920,
                bitrate_bps: Some(7_000_000),
                ..Default::default()
            },
        }];
        std::fs::write(d.join("第1集.mp4"), b"mp4").unwrap();

        let r = generate_report_package(
            "剧A",
            &d,
            Some(&d.join("封面.jpg")),
            Some(&d.join("简介.txt")),
            &eps,
            &PackageOptions {
                output_dir: Some(d.join("out")),
                copy_episodes: false,
                cost_sheet: Some(CostSheetForm {
                    company_name: "测试文化".into(),
                    total_investment_wan: Some(50.0),
                    actor_ratio_percent: Some(30.0),
                    contact: "-".into(),
                }),
                ai_generated: true,
                ai_label_confirmed: false,
                ..base_opts(d.join("out"))
            },
        )
        .unwrap();
        assert!(r.package_dir.join("manifest.json").is_file());
        assert!(r.package_dir.join("01_剧集信息").join("剧名.txt").is_file());
        assert!(r
            .package_dir
            .join("03_分集清单")
            .join("分集清单.csv")
            .is_file());
        assert!(r
            .package_dir
            .join("05_权属声明")
            .join("内容权属声明.txt")
            .is_file());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn manifest_uses_documented_camel_case_schema() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        let base = std::env::temp_dir().join(format!("dossier-manifest-{unique}"));
        let d = base.join("剧A");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("剧名.txt"), "剧A").unwrap();

        let r = generate_report_package(
            "剧A",
            &d,
            None,
            None,
            &[],
            &PackageOptions {
                output_dir: Some(d.join("out")),
                copy_episodes: false,
                cost_sheet: None,
                ai_generated: false,
                ai_label_confirmed: false,
                ..base_opts(d.join("out"))
            },
        )
        .unwrap();

        let raw = std::fs::read(r.package_dir.join("manifest.json")).unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        let obj = manifest.as_object().unwrap();
        for key in [
            "schema",
            "dramaName",
            "generatedAt",
            "aiGenerated",
            "copyEpisodes",
            "files",
        ] {
            assert!(obj.contains_key(key), "manifest 缺少字段 {key}");
        }
        for key in [
            "drama_name",
            "generated_at",
            "ai_generated",
            "copy_episodes",
        ] {
            assert!(
                !obj.contains_key(key),
                "manifest 残留 snake_case 字段 {key}"
            );
        }
        let first = manifest["files"].as_array().unwrap().first().unwrap();
        assert!(first.get("path").is_some());
        assert!(first.get("sizeBytes").is_some());
        assert!(first.get("sha256").is_some());
        std::fs::remove_dir_all(&base).ok();
    }

    fn base_opts(output: PathBuf) -> PackageOptions {
        PackageOptions {
            output_dir: Some(output),
            copy_episodes: false,
            cost_sheet: None,
            ai_generated: true,
            ai_label_confirmed: false,
            overwrite: false,
            overwrite_mode: OverwriteMode::ReplaceAll,
            with_proof: true,
            proof_source_dir: None,
        }
    }

    /// 中途失败（分集文件读不到，算 sha256 时出错）：最终目录不存在、临时目录也被清掉，
    /// 下一次可以直接重新生成，不会因为「目录已存在」被拒。
    #[test]
    fn failed_generation_leaves_no_partial_package() {
        let base = std::env::temp_dir().join(format!("dossier-pkg-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let d = base.join("剧A");
        std::fs::create_dir_all(&d).unwrap();
        let missing = vec![ProbedEpisode {
            file: EpisodeFile {
                path: d.join("不存在的第1集.mp4"),
                file_name: "不存在的第1集.mp4".into(),
                size_bytes: 0,
                episode_number: Some(1),
            },
            spec: EpisodeSpec::default(),
        }];
        let out = d.join("报审包");
        assert!(
            generate_report_package("剧A", &d, None, None, &missing, &base_opts(out.clone()))
                .is_err()
        );
        assert!(!out.exists(), "失败时不能留下半个包");
        let leftovers: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".part-"))
            .collect();
        assert!(leftovers.is_empty(), "临时目录必须清掉");
        // 修好之后可以直接重新生成
        assert!(
            generate_report_package("剧A", &d, None, None, &[], &base_opts(out.clone())).is_ok()
        );
        assert!(out.join("manifest.json").is_file());
        // 目录已存在时仍然拒绝覆盖
        assert!(generate_report_package("剧A", &d, None, None, &[], &base_opts(out)).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn ownership_declaration_only_claims_what_was_confirmed() {
        let mut opts = base_opts(PathBuf::from("/x"));
        let unconfirmed = ownership_declaration("剧A", &opts);
        assert!(unconfirmed.contains("由 AI 辅助生成"));
        assert!(
            !unconfirmed.contains("已按平台要求"),
            "没确认就不能声称已标识"
        );
        opts.ai_label_confirmed = true;
        assert!(ownership_declaration("剧A", &opts).contains("已按平台要求进行 AI 生成标识"));
        opts.ai_generated = false;
        assert!(ownership_declaration("剧A", &opts).contains("未使用 AI 生成"));
    }

    fn probed(dir: &Path, rel: &str, number: Option<u32>) -> ProbedEpisode {
        let path = dir.join(rel);
        ProbedEpisode {
            file: EpisodeFile {
                file_name: path.file_name().unwrap().to_string_lossy().into_owned(),
                path,
                size_bytes: 3,
                episode_number: number,
            },
            spec: EpisodeSpec {
                duration_sec: 61.0,
                width: 1080,
                height: 1920,
                bitrate_bps: Some(5_000_000),
                ..Default::default()
            },
        }
    }

    #[test]
    fn episode_names_follow_spec_and_resolve_collisions() {
        let d = Path::new("/素材/剧A");
        let eps = vec![
            probed(d, "b/第3集.mp4", Some(3)),
            probed(d, "a/ep03.MP4", Some(3)),
            probed(d, "第1集.mkv", Some(1)),
            probed(d, "花絮.mp4", None),
            // 无集号、原名恰好撞上 第01集（扩展名大小写不同也算同名）。
            probed(d, "x/第01集.MKV", None),
        ];
        // 同集号按完整路径排：a/ 在 b/ 前，a/ep03.MP4 拿基础名，b/ 的加 -2。
        assert_eq!(
            package_episode_names(&eps),
            [
                "第03集-2.mp4",
                "第03集.MP4",
                "第01集.mkv",
                "花絮.mp4",
                "第01集-2.MKV"
            ]
        );
        let big = vec![
            probed(d, "e100.mp4", Some(100)),
            probed(d, "e7.mp4", Some(7)),
        ];
        assert_eq!(package_episode_names(&big), ["第100集.mp4", "第007集.mp4"]);
    }

    #[test]
    fn package_copies_every_duplicate_and_lists_them_consistently() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        let base = std::env::temp_dir().join(format!("dossier-pkg-dup-{unique}"));
        let d = base.join("剧A");
        std::fs::create_dir_all(d.join("上")).unwrap();
        std::fs::create_dir_all(d.join("下")).unwrap();
        std::fs::write(d.join("上").join("=第1集.mp4"), b"one").unwrap();
        std::fs::write(d.join("下").join("=第1集.mp4"), b"two").unwrap();
        std::fs::write(d.join("海报cover.png"), b"png").unwrap();
        std::fs::write(d.join("intro.md"), "简介内容").unwrap();
        let eps = vec![
            probed(&d, "上/=第1集.mp4", Some(1)),
            probed(&d, "下/=第1集.mp4", Some(1)),
        ];
        let out = d.join("out");
        let mut opts = base_opts(out.clone());
        opts.copy_episodes = true;
        generate_report_package(
            "剧A",
            &d,
            Some(&d.join("海报cover.png")),
            Some(&d.join("intro.md")),
            &eps,
            &opts,
        )
        .unwrap();
        assert!(out.join("02_封面海报").join("封面.png").is_file());
        assert!(out.join("01_剧集信息").join("简介.md").is_file());
        assert!(out.join("01_剧集信息").join("剧名.txt").is_file());
        let videos = out.join("06_分集视频");
        assert_eq!(std::fs::read(videos.join("第01集.mp4")).unwrap(), b"one");
        assert_eq!(std::fs::read(videos.join("第01集-2.mp4")).unwrap(), b"two");

        let csv = std::fs::read_to_string(out.join("03_分集清单").join("分集清单.csv")).unwrap();
        assert!(csv.starts_with('\u{feff}'), "分集清单要带 BOM");
        let rows: Vec<&str> = csv.trim_start_matches('\u{feff}').lines().collect();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].ends_with(",包内文件名"));
        assert!(
            rows[1].starts_with("1,'=第1集.mp4,"),
            "公式注入防护：{}",
            rows[1]
        );
        assert!(rows[1].ends_with(",第01集.mp4"), "{}", rows[1]);
        assert!(rows[2].ends_with(",第01集-2.mp4"), "{}", rows[2]);

        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(out.join("manifest.json")).unwrap()).unwrap();
        let videos_in_manifest = manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["path"].as_str().unwrap().starts_with("06_分集视频/"))
            .count();
        assert_eq!(videos_in_manifest, 2, "清单与包内文件一一对应");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn cost_sheet_formats_numbers_and_escapes_cells() {
        let md = render_cost_sheet_md(&CostSheetForm {
            company_name: "A|B\n文化".into(),
            total_investment_wan: Some(0.1 + 0.2),
            actor_ratio_percent: Some(33.333_333),
            contact: "张三 | 138".into(),
        });
        assert!(md.contains("| 制作机构 | A\\|B 文化 |"), "{md}");
        assert!(md.contains("| 投资总额 | 0.3 万元 |"), "{md}");
        assert!(md.contains("| 演员成本占比 | 33.33 % |"), "{md}");
        assert!(md.contains("| 联系方式 | 张三 \\| 138 |"), "{md}");
        assert_eq!(format_amount(50.0), "50");
        assert_eq!(format_amount(50.5), "50.5");
        assert_eq!(format_amount(0.0), "0");
    }

    #[test]
    fn cost_sheet_rejects_invalid_numbers_before_writing() {
        let base = std::env::temp_dir().join(format!("dossier-pkg-cost-{}", std::process::id()));
        let out = base.join("out");
        for (investment, ratio) in [
            (Some(f64::NAN), None),
            (Some(f64::INFINITY), None),
            (Some(-1.0), None),
            (None, Some(101.0)),
            (None, Some(-0.5)),
        ] {
            let mut opts = base_opts(out.clone());
            opts.cost_sheet = Some(CostSheetForm {
                total_investment_wan: investment,
                actor_ratio_percent: ratio,
                ..Default::default()
            });
            let error = generate_report_package("剧A", &base, None, None, &[], &opts).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert!(!out.exists(), "校验失败时不能写出任何东西");
        }
        std::fs::remove_dir_all(&base).ok();
    }

    /// 独立临时目录（并行测试互不干扰）。
    fn scratch(tag: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        let base = std::env::temp_dir().join(format!("dossier-pkg-{tag}-{unique}"));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn standard_paths_are_recognized_precisely() {
        for rel in [
            "manifest.json",
            "01_剧集信息/剧名.txt",
            "01_剧集信息/简介.md",
            "02_封面海报/封面.PNG",
            "03_分集清单/分集清单.csv",
            "04_成本配置表/成本配置表.md",
            "05_权属声明/内容权属声明.txt",
            "06_分集视频/第09集.mp4",
            "06_分集视频/第09集-2.mkv",
            "07_证明与过程图/AI制作证明.png",
            "07_证明与过程图/过程图_02.png",
            "07_证明与过程图/剧目海报.webp",
        ] {
            assert!(is_standard_named_file(rel), "应算标准文件：{rel}");
        }
        for rel in [
            "06_分集视频/花絮.mp4",
            "06_分集视频/第9集.mp4",
            "06_分集视频/第09集-花絮.mp4",
            "07_证明与过程图/AI制作证明-旧.png",
            "07_证明与过程图/过程图_2.png",
            "01_剧集信息/剧名-备份.txt",
            "01_剧集信息/说明.txt",
            "说明.txt",
        ] {
            assert!(!is_standard_named_file(rel), "不该算标准文件：{rel}");
        }
    }

    /// 目录已存在时默认不覆盖，要求先确认。
    #[test]
    fn existing_directory_needs_confirmation() {
        let base = scratch("confirm");
        let out = base.join("报审包");
        generate_report_package("剧A", &base, None, None, &[], &base_opts(out.clone())).unwrap();
        let error = generate_report_package("剧A", &base, None, None, &[], &base_opts(out.clone()))
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(error.to_string().contains("可以选择覆盖旧包"));
        std::fs::remove_dir_all(&base).ok();
    }

    /// 整体替换：用户放进旧包的文件一并消失，不留备份残留。
    #[test]
    fn replace_all_swaps_the_whole_package() {
        let base = scratch("replace");
        let out = base.join("报审包");
        generate_report_package("剧A", &base, None, None, &[], &base_opts(out.clone())).unwrap();
        std::fs::write(out.join("我放的说明.txt"), "手写").unwrap();
        let mut opts = base_opts(out.clone());
        opts.overwrite = true;
        opts.overwrite_mode = OverwriteMode::ReplaceAll;
        let result = generate_report_package("剧A", &base, None, None, &[], &opts).unwrap();
        assert!(result.overwrite.is_some());
        assert!(
            !out.join("我放的说明.txt").exists(),
            "整体替换要连自放文件一起换掉"
        );
        assert!(out.join("manifest.json").is_file());
        let leftovers: Vec<String> = std::fs::read_dir(&base)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".old-") || name.contains(".part-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "不能留下备份或临时目录：{leftovers:?}"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// 只替换标准文件：用户自放的文件保留，本次不再生成的标准文件被清掉。
    #[test]
    fn standard_only_keeps_user_files_and_cleans_stale_ones() {
        let base = scratch("standard");
        let d = base.join("剧A");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("第1集.mp4"), b"one").unwrap();
        std::fs::write(d.join("第2集.mp4"), b"two").unwrap();
        let out = base.join("报审包");
        let eps = vec![
            probed(&d, "第1集.mp4", Some(1)),
            probed(&d, "第2集.mp4", Some(2)),
        ];
        let mut first = base_opts(out.clone());
        first.copy_episodes = true;
        generate_report_package("剧A", &d, None, None, &eps, &first).unwrap();
        assert!(out.join("06_分集视频").join("第02集.mp4").is_file());
        std::fs::write(out.join("我放的说明.txt"), "手写").unwrap();
        std::fs::write(out.join("06_分集视频").join("花絮.mp4"), "x").unwrap();

        // 第二次只给一集：第02集是过期标准文件，应被清掉；手写文件与花絮保留。
        let mut second = base_opts(out.clone());
        second.copy_episodes = true;
        second.overwrite = true;
        second.overwrite_mode = OverwriteMode::StandardOnly;
        let result = generate_report_package("剧A", &d, None, None, &eps[..1], &second).unwrap();
        let report = result.overwrite.as_ref().unwrap();
        assert_eq!(report.removed, vec!["06_分集视频/第02集.mp4".to_string()]);
        assert!(
            report.kept.contains(&"我放的说明.txt".to_string()),
            "{report:?}"
        );
        assert!(
            report.kept.contains(&"06_分集视频/花絮.mp4".to_string()),
            "{report:?}"
        );
        assert!(out.join("我放的说明.txt").is_file());
        assert!(out.join("06_分集视频").join("花絮.mp4").is_file());
        assert!(!out.join("06_分集视频").join("第02集.mp4").exists());
        assert!(out.join("06_分集视频").join("第01集.mp4").is_file());
        std::fs::remove_dir_all(&base).ok();
    }

    /// 安全闸：不允许把剧目目录本身当旧包换掉。
    #[test]
    fn overwrite_refuses_drama_directory_itself() {
        let base = scratch("guard");
        let d = base.join("剧A");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("第1集.mp4"), b"one").unwrap();
        let mut opts = base_opts(d.clone());
        opts.overwrite = true;
        let error = generate_report_package("剧A", &d, None, None, &[], &opts).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("剧目目录本身"));
        assert!(d.join("第1集.mp4").is_file(), "素材不能被删");
        std::fs::remove_dir_all(&base).ok();
    }

    /// 「选择输出目录」只能选已有文件夹：空文件夹（含系统杂项）不带 overwrite 也要能直接生成。
    #[test]
    fn generate_into_existing_empty_dir() {
        for junk in [None, Some(".DS_Store"), Some("Thumbs.db")] {
            let base = scratch("empty");
            let out = base.join("我建的空文件夹");
            std::fs::create_dir_all(&out).unwrap();
            if let Some(name) = junk {
                std::fs::write(out.join(name), b"x").unwrap();
            }
            let result =
                generate_report_package("剧A", &base, None, None, &[], &base_opts(out.clone()))
                    .unwrap_or_else(|error| panic!("空目录（{junk:?}）应能直接生成：{error}"));
            assert!(result.overwrite.is_none());
            assert!(out.join(FILE_MANIFEST).is_file());
            let leftovers: Vec<String> = std::fs::read_dir(&base)
                .unwrap()
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.contains(".part-"))
                .collect();
            assert!(leftovers.is_empty(), "{leftovers:?}");
            std::fs::remove_dir_all(&base).ok();
        }
    }

    /// 入口判定为空、写完 staging 后目录被写进了东西：不能在没过安全闸的情况下替换。
    #[test]
    fn occupied_after_check_is_not_replaced() {
        let base = scratch("toctou");
        let out = base.join("报审包");
        let staging = base.join(".报审包.part-test");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join(FILE_MANIFEST), b"{}").unwrap();
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("用户刚放的.mp4"), b"user").unwrap();
        let error = commit_staging(&staging, &out, false, OverwriteMode::ReplaceAll, "test", 1)
            .unwrap_err();
        assert!(
            error.to_string().starts_with(PACKAGE_EXISTS_MARK),
            "{error}"
        );
        assert_eq!(std::fs::read(out.join("用户刚放的.mp4")).unwrap(), b"user");
        std::fs::remove_dir_all(&base).ok();
    }

    /// 素材根目录（剧目目录的上级）不能当旧包整体换掉。
    #[test]
    fn overwrite_refuses_ancestor_of_drama_dir() {
        let base = scratch("ancestor");
        let d = base.join("剧A");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("第1集.mp4"), b"one").unwrap();
        // 伪造一个 manifest，确认「上级目录」这条规则本身就能拦下，不依赖 manifest 检查。
        std::fs::write(
            base.join(FILE_MANIFEST),
            format!(r#"{{"schema":"{PACKAGE_SCHEMA}"}}"#),
        )
        .unwrap();
        for mode in [OverwriteMode::ReplaceAll, OverwriteMode::StandardOnly] {
            let mut opts = base_opts(base.clone());
            opts.overwrite = true;
            opts.overwrite_mode = mode;
            let error = generate_report_package("剧A", &d, None, None, &[], &opts).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{error}");
            assert!(error.to_string().contains("上级目录"), "{error}");
            assert!(!error.to_string().contains(PACKAGE_EXISTS_MARK));
            assert_eq!(std::fs::read(d.join("第1集.mp4")).unwrap(), b"one");
        }
        std::fs::remove_dir_all(&base).ok();
    }

    /// 没有剧备 manifest 的目录：两种覆盖模式都拒绝，里面的文件一个不动。
    #[test]
    fn overwrite_refuses_dir_without_manifest() {
        for mode in [OverwriteMode::ReplaceAll, OverwriteMode::StandardOnly] {
            let base = scratch("nomanifest");
            let out = base.join("别的文件夹");
            std::fs::create_dir_all(&out).unwrap();
            std::fs::write(out.join("第01集.mp4"), b"user").unwrap();
            let drama = base.join("剧A");
            std::fs::create_dir_all(&drama).unwrap();
            let mut opts = base_opts(out.clone());
            opts.overwrite = true;
            opts.overwrite_mode = mode;
            let error = generate_report_package("剧A", &drama, None, None, &[], &opts).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{error}");
            assert!(error.to_string().contains("manifest.json"), "{error}");
            assert_eq!(std::fs::read(out.join("第01集.mp4")).unwrap(), b"user");
            std::fs::remove_dir_all(&base).ok();
        }
    }

    /// 旧包位置本身是符号链接：拒绝，链接指向的真包不动。
    #[cfg(unix)]
    #[test]
    fn overwrite_refuses_symlinked_package() {
        let base = scratch("symlink");
        let real = base.join("真包");
        generate_report_package("剧A", &base, None, None, &[], &base_opts(real.clone())).unwrap();
        std::fs::write(real.join("我放的.txt"), b"keep").unwrap();
        // 名字里不能含「链接」：否则错误信息里的路径本身就能让断言通过，测不出拒绝原因。
        let link = base.join("link-pkg");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut opts = base_opts(link.clone());
        opts.overwrite = true;
        let error = generate_report_package("剧A", &base, None, None, &[], &opts).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{error}");
        assert!(error.to_string().contains("快捷方式/链接"), "{error}");
        assert_eq!(std::fs::read(real.join("我放的.txt")).unwrap(), b"keep");
        assert!(
            std::fs::symlink_metadata(&link).is_ok_and(|meta| meta.file_type().is_symlink()),
            "链接本身也不能被挪走"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// 安全闸本身就要拒绝没有 manifest 的目录（落盘前的二次校验是第二道防线，不能替代这一道）。
    #[test]
    fn guard_alone_refuses_dir_without_manifest() {
        let base = scratch("guardonly");
        let drama = base.join("剧A");
        std::fs::create_dir_all(&drama).unwrap();
        let out = base.join("别的文件夹");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("第01集.mp4"), b"user").unwrap();
        let error = guard_overwrite_target(&out, &drama).unwrap_err();
        assert!(error.to_string().contains("manifest.json"), "{error}");
        std::fs::write(
            out.join(FILE_MANIFEST),
            format!(r#"{{"schema":"{PACKAGE_SCHEMA}"}}"#),
        )
        .unwrap();
        assert!(guard_overwrite_target(&out, &drama).is_ok());
        std::fs::remove_dir_all(&base).ok();
    }

    /// 默认产物根目录形态 `<根>/<剧名>/报审包`：带 manifest 的真包允许覆盖。
    #[test]
    fn overwrite_allows_package_under_output_root() {
        let base = scratch("root");
        let d = base.join("素材").join("剧A");
        std::fs::create_dir_all(&d).unwrap();
        let out = base.join("产物").join("剧A").join("报审包");
        generate_report_package("剧A", &d, None, None, &[], &base_opts(out.clone())).unwrap();
        let mut opts = base_opts(out.clone());
        opts.overwrite = true;
        assert!(generate_report_package("剧A", &d, None, None, &[], &opts).is_ok());
        std::fs::remove_dir_all(&base).ok();
    }

    /// 手工构造一个「旧包」：每个文件内容为 old，带合法 manifest。
    fn old_package(dir: &Path, files: &[&str]) {
        std::fs::create_dir_all(dir).unwrap();
        for rel in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"old").unwrap();
        }
        std::fs::write(
            dir.join(FILE_MANIFEST),
            format!(r#"{{"schema":"{PACKAGE_SCHEMA}","v":"old"}}"#),
        )
        .unwrap();
    }

    /// 手工构造 staging：每个文件内容为 new。
    fn new_staging(dir: &Path, files: &[&str]) {
        for rel in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"new").unwrap();
        }
    }

    /// 只差大小写的旧文件不算过期：修复前按原文比较会把 `.JPG` 判为过期（Linux 上也能先红）。
    #[test]
    fn stale_entries_ignores_case_only_difference() {
        let fresh = vec!["02_封面海报/封面.jpg".to_string()];
        let existing = vec![
            "02_封面海报/封面.JPG".to_string(),
            "06_分集视频/第09集.mp4".to_string(),
            "我的说明.txt".to_string(),
        ];
        assert_eq!(
            stale_entries(&fresh, &existing),
            vec!["06_分集视频/第09集.mp4"]
        );
    }

    /// 端到端回归：大小写变化后读到的是新内容；区分大小写的文件系统上旧写法被清掉。
    #[test]
    fn standard_only_case_change_end_to_end() {
        let base = scratch("case");
        let pkg = base.join("pkg");
        let staging = base.join("staging");
        old_package(&pkg, &["02_封面海报/封面.JPG"]);
        new_staging(&staging, &["02_封面海报/封面.jpg", FILE_MANIFEST]);
        merge_standard_files_with_nonce(&staging, &pkg, "t").unwrap();
        assert_eq!(
            std::fs::read(pkg.join("02_封面海报/封面.jpg")).unwrap(),
            b"new"
        );
        #[cfg(target_os = "linux")]
        assert!(!pkg.join("02_封面海报/封面.JPG").exists());
        std::fs::remove_dir_all(&base).ok();
    }

    /// 旧包的分集目录是指向包外的链接：中止，包外文件不动。
    #[cfg(unix)]
    #[test]
    fn standard_only_does_not_follow_links() {
        let base = scratch("link");
        let pkg = base.join("pkg");
        let outside = base.join("外部素材");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("第09集.mp4"), b"user").unwrap();
        old_package(&pkg, &[]);
        std::os::unix::fs::symlink(&outside, pkg.join(DIR_EPISODES)).unwrap();
        let staging = base.join("staging");
        new_staging(&staging, &["06_分集视频/第01集.mp4", FILE_MANIFEST]);
        let error = merge_standard_files_with_nonce(&staging, &pkg, "t").unwrap_err();
        assert!(error.to_string().contains("链接"), "{error}");
        assert_eq!(std::fs::read(outside.join("第09集.mp4")).unwrap(), b"user");
        assert!(!outside.join("第01集.mp4").exists(), "不能写到包外");
        std::fs::remove_dir_all(&base).ok();
    }

    /// 目标位置是目录：阶段一预检就中止，旧包内容全部还是 old。
    #[test]
    fn standard_only_refuses_directory_at_target() {
        let base = scratch("dirtarget");
        let pkg = base.join("pkg");
        old_package(&pkg, &["02_封面海报/封面.png"]);
        std::fs::create_dir_all(pkg.join("01_剧集信息/剧名.txt")).unwrap();
        let staging = base.join("staging");
        new_staging(
            &staging,
            &[
                "01_剧集信息/剧名.txt",
                "02_封面海报/封面.png",
                FILE_MANIFEST,
            ],
        );
        assert!(merge_standard_files_with_nonce(&staging, &pkg, "t").is_err());
        assert_eq!(
            std::fs::read(pkg.join("02_封面海报/封面.png")).unwrap(),
            b"old"
        );
        let manifest = std::fs::read_to_string(pkg.join(FILE_MANIFEST)).unwrap();
        assert!(manifest.contains(r#""v":"old""#));
        std::fs::remove_dir_all(&base).ok();
    }

    /// 复制阶段失败：旧包不变，陷阱目录不被误删，本次的临时文件不残留。
    #[test]
    fn standard_only_copy_failure_leaves_old_package() {
        let base = scratch("copyfail");
        let pkg = base.join("pkg");
        old_package(&pkg, &["01_剧集信息/剧名.txt", "02_封面海报/封面.png"]);
        let staging = base.join("staging");
        new_staging(
            &staging,
            &[
                "01_剧集信息/剧名.txt",
                "02_封面海报/封面.png",
                FILE_MANIFEST,
            ],
        );
        // 排序后 manifest 最后复制：在它的临时名位置放一个同名目录，制造 copy 失败。
        let trap = merge_temp_path(&pkg.join(FILE_MANIFEST), "t");
        std::fs::create_dir_all(&trap).unwrap();
        assert!(merge_standard_files_with_nonce(&staging, &pkg, "t").is_err());
        for rel in ["01_剧集信息/剧名.txt", "02_封面海报/封面.png"] {
            assert_eq!(std::fs::read(pkg.join(rel)).unwrap(), b"old", "{rel}");
        }
        assert!(std::fs::read_to_string(pkg.join(FILE_MANIFEST))
            .unwrap()
            .contains(r#""v":"old""#));
        assert!(trap.is_dir(), "清理不能删目录");
        for rel in ["01_剧集信息/剧名.txt", "02_封面海报/封面.png"] {
            assert!(
                !merge_temp_path(&pkg.join(rel), "t").exists(),
                "{rel} 的临时文件残留"
            );
        }
        std::fs::remove_dir_all(&base).ok();
    }

    /// 统计失败（目录不存在）返回 0，不报错。
    #[test]
    fn count_files_best_effort_tolerates_missing_dir() {
        assert_eq!(count_files_best_effort(Path::new("/不存在的/备份目录")), 0);
    }

    /// 「已存在」错误带固定前缀（前端据此给覆盖按钮）；安全闸拒绝不带。
    #[test]
    fn exists_error_carries_marker() {
        let base = scratch("marker");
        let out = base.join("报审包");
        generate_report_package("剧A", &base, None, None, &[], &base_opts(out.clone())).unwrap();
        let exists =
            generate_report_package("剧A", &base, None, None, &[], &base_opts(out.clone()))
                .unwrap_err();
        assert_eq!(exists.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(
            exists.to_string().starts_with(PACKAGE_EXISTS_MARK),
            "{exists}"
        );
        let plain = base.join("普通目录");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("x.txt"), b"x").unwrap();
        let mut opts = base_opts(plain);
        opts.overwrite = true;
        let refused = generate_report_package("剧A", &base, None, None, &[], &opts).unwrap_err();
        assert!(
            !refused.to_string().contains(PACKAGE_EXISTS_MARK),
            "{refused}"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// 证明材料：有就收进 07_，一个都没有时不建目录并写明原因。
    #[test]
    fn proof_materials_are_collected_or_explained() {
        let base = scratch("proof");
        let deep = base.join("深度处理");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("AI制作证明.png"), b"png").unwrap();
        for index in 1..=4 {
            std::fs::write(deep.join(format!("过程图_{index:02}.png")), b"png").unwrap();
        }
        std::fs::write(deep.join("剧目海报.png"), b"png").unwrap();
        let out = base.join("报审包");
        let mut opts = base_opts(out.clone());
        opts.proof_source_dir = Some(deep.clone());
        let result = generate_report_package("剧A", &base, None, None, &[], &opts).unwrap();
        let proof = result.proof.unwrap();
        assert_eq!(proof.files.len(), 6, "{proof:?}");
        assert!(out.join("07_证明与过程图").join("AI制作证明.png").is_file());
        assert_eq!(
            std::fs::read_dir(out.join("07_证明与过程图"))
                .unwrap()
                .count(),
            6
        );
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(out.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["proofFiles"].as_array().unwrap().len(), 6);
        assert!(manifest.get("proofSource").is_none(), "收齐时不写说明");

        // 换一部没有深度处理的剧：不建 07_，且写明原因。
        let empty = base.join("报审包2");
        let mut opts2 = base_opts(empty.clone());
        opts2.proof_source_dir = Some(base.join("不存在"));
        let result = generate_report_package("剧B", &base, None, None, &[], &opts2).unwrap();
        assert!(result.proof.unwrap().files.is_empty());
        assert!(!empty.join("07_证明与过程图").exists());
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(empty.join("manifest.json")).unwrap()).unwrap();
        assert!(manifest["proofSource"]["note"]
            .as_str()
            .unwrap()
            .contains("没有找到深度处理输出目录"));

        // 取消勾选：同样不建目录，说明里写成未勾选。
        let third = base.join("报审包3");
        let mut opts3 = base_opts(third.clone());
        opts3.with_proof = false;
        opts3.proof_source_dir = Some(deep);
        let result = generate_report_package("剧C", &base, None, None, &[], &opts3).unwrap();
        assert!(result.proof.unwrap().note.unwrap().contains("未勾选"));
        assert!(!third.join("07_证明与过程图").exists());
        std::fs::remove_dir_all(&base).ok();
    }
}
