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
    if pkg_dir.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "报审包目录已存在：{}。请选择新的输出目录或先移除旧目录。",
                pkg_dir.display()
            ),
        ));
    }

    let parent = pkg_dir.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let staging = parent.join(format!(
        ".{}.part-{}-{}",
        pkg_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "报审包".into()),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default()
    ));
    let written = write_package_contents(&staging, drama_name, cover, synopsis, episodes, opts)
        .and_then(|summary| {
            // 写完才改名：最终目录要么不存在，要么是完整的包。
            std::fs::rename(&staging, &pkg_dir)?;
            Ok(summary)
        });
    match written {
        Ok((file_count, total_bytes)) => Ok(PackageResult {
            package_dir: pkg_dir,
            file_count,
            total_bytes,
        }),
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            Err(error)
        }
    }
}

/// 把报审包的全部内容写进 `pkg_dir`（临时目录），返回 `(文件数, 总字节数)`。
fn write_package_contents(
    pkg_dir: &Path,
    drama_name: &str,
    cover: Option<&Path>,
    synopsis: Option<&Path>,
    episodes: &[ProbedEpisode],
    opts: &PackageOptions,
) -> std::io::Result<(u32, u64)> {
    std::fs::create_dir_all(pkg_dir)?;

    let mut manifest_files: Vec<ManifestFile> = Vec::new();

    // 01 剧集信息
    let info_dir = pkg_dir.join("01_剧集信息");
    std::fs::create_dir_all(&info_dir)?;
    std::fs::write(info_dir.join("剧名.txt"), drama_name)?;
    // 包内统一命名（MATERIAL_SPEC「v1 报审包结构」）：简介.<原扩展名>、封面.<原扩展名>、第NN集.<原扩展名>。
    if let Some(s) = synopsis {
        std::fs::copy(s, info_dir.join(fixed_name("简介", s)))?;
    }
    if let Some(c) = cover {
        let poster_dir = pkg_dir.join("02_封面海报");
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
    let list_dir = pkg_dir.join("03_分集清单");
    std::fs::create_dir_all(&list_dir)?;
    // 带 BOM：Windows 上双击用 Excel 打开时中文不乱码。
    std::fs::write(list_dir.join("分集清单.csv"), csv_text_with_bom(&csv))?;

    // 04 成本配置表（真实数据模板）
    let cost_dir = pkg_dir.join("04_成本配置表");
    std::fs::create_dir_all(&cost_dir)?;
    if let Some(form) = &opts.cost_sheet {
        std::fs::write(cost_dir.join("成本配置表.md"), render_cost_sheet_md(form))?;
    } else {
        std::fs::write(
            cost_dir.join("成本配置表.md"),
            render_cost_sheet_md(&CostSheetForm::default()),
        )?;
    }

    // 05 权属声明（固定模板，使用者如实填写）
    let decl_dir = pkg_dir.join("05_权属声明");
    std::fs::create_dir_all(&decl_dir)?;
    std::fs::write(
        decl_dir.join("内容权属声明.txt"),
        ownership_declaration(drama_name, opts),
    )?;

    // 06 分集视频（可选复制）
    if opts.copy_episodes {
        let ep_dir = pkg_dir.join("06_分集视频");
        std::fs::create_dir_all(&ep_dir)?;
        for (ep, name) in episodes.iter().zip(&names) {
            std::fs::copy(&ep.file.path, ep_dir.join(name))?;
        }
    }

    // manifest
    collect_files(pkg_dir, &mut manifest_files)?;
    let manifest = Manifest {
        schema: "drama-dossier/package@1".into(),
        drama_name: drama_name.into(),
        generated_at: chrono_now(),
        ai_generated: opts.ai_generated,
        copy_episodes: opts.copy_episodes,
        files: manifest_files,
    };
    std::fs::write(
        pkg_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;

    let file_count = manifest.files.len() as u32;
    let total_bytes = manifest.files.iter().map(|f| f.size_bytes).sum();
    Ok((file_count, total_bytes))
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
}
