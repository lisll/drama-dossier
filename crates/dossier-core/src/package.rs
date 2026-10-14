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

/// 生成报审包。
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
    if pkg_dir.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "报审包目录已存在：{}。请选择新的输出目录或先移除旧目录。",
                pkg_dir.display()
            ),
        ));
    }

    std::fs::create_dir_all(&pkg_dir)?;

    let mut manifest_files: Vec<ManifestFile> = Vec::new();

    // 01 剧集信息
    let info_dir = pkg_dir.join("01_剧集信息");
    std::fs::create_dir_all(&info_dir)?;
    std::fs::write(info_dir.join("剧名.txt"), drama_name)?;
    if let Some(s) = synopsis {
        if let Some(name) = s.file_name() {
            std::fs::copy(s, info_dir.join(name))?;
        }
    }
    if let Some(c) = cover {
        let poster_dir = pkg_dir.join("02_封面海报");
        std::fs::create_dir_all(&poster_dir)?;
        if let Some(name) = c.file_name() {
            std::fs::copy(c, poster_dir.join(name))?;
        }
    }

    // 03 分集清单
    let mut csv = String::from("集号,文件名,时长(秒),宽,高,视频编码,音频编码,码率(Mbps),SHA256\n");
    for ep in episodes {
        let no = ep
            .file
            .episode_number
            .map(|n| n.to_string())
            .unwrap_or_default();
        csv.push_str(&format!(
            "{},{},{:.1},{},{},{},{},{:.2},{}\n",
            no,
            csv_escape(&ep.file.file_name),
            ep.spec.duration_sec,
            ep.spec.width,
            ep.spec.height,
            ep.spec.video_codec.as_deref().unwrap_or(""),
            ep.spec.audio_codec.as_deref().unwrap_or(""),
            ep.spec.bitrate_bps.unwrap_or(0) as f64 / 1e6,
            sha256_file(&ep.file.path)?,
        ));
    }
    let list_dir = pkg_dir.join("03_分集清单");
    std::fs::create_dir_all(&list_dir)?;
    std::fs::write(list_dir.join("分集清单.csv"), csv)?;

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
        for ep in episodes {
            if let Some(name) = Path::new(&ep.file.file_name).file_name() {
                std::fs::copy(&ep.file.path, ep_dir.join(name))?;
            }
        }
    }

    // manifest
    collect_files(&pkg_dir, &mut manifest_files)?;
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
    Ok(PackageResult {
        package_dir: pkg_dir,
        file_count,
        total_bytes,
    })
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

fn sha256_file(p: &Path) -> std::io::Result<String> {
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

fn csv_escape(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
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
        f.company_name,
        f.total_investment_wan
            .map(|v| v.to_string())
            .unwrap_or_else(|| "（待填写）".into()),
        f.actor_ratio_percent
            .map(|v| v.to_string())
            .unwrap_or_else(|| "（待填写）".into()),
        f.contact,
    )
}

fn ownership_declaration(drama_name: &str, opts: &PackageOptions) -> String {
    let ai = if opts.ai_generated {
        "本剧部分材料（剧名/简介/封面）由 AI 辅助生成，已按平台要求进行标识。"
    } else {
        "本剧材料未使用 AI 生成。"
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
}
