//! 剧集库扫描：约定「素材根目录下，一部剧一个子目录」。

use std::path::Path;

use crate::types::{Drama, EpisodeFile};

const VIDEO_EXTS: &[&str] = &["mp4", "mov", "mkv", "flv", "ts", "m4v", "webm"];
const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp"];
const TEXT_EXTS: &[&str] = &["txt", "md"];

/// 应用自己会在剧目录里生成的产物子目录，扫描分集时必须跳过：
/// 否则转码输出（同名文件）会被当成新一集，转码一次集数就翻倍，
/// 下次转码还会把旧产物当素材重新处理。
/// 「报审包」对应 `package.rs`，「微调输出 / 重组输出」对应桌面端转码落点。
const PRODUCT_SUBDIRS: &[&str] = &["报审包", "微调输出", "重组输出"];

/// 扫描素材根目录，返回按名称排序的剧目列表。
pub fn scan_library(root: &Path) -> std::io::Result<Vec<Drama>> {
    validate_library_root(root)?;
    let mut dramas = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        dramas.push(scan_drama_dir(&name, &path));
    }
    dramas.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(dramas)
}

/// 扫描单个剧目录（递归一层子目录）。
pub fn scan_drama_dir(name: &str, dir: &Path) -> Drama {
    let mut episodes: Vec<EpisodeFile> = Vec::new();
    let mut cover = None;
    let mut synopsis = None;

    fn visit(
        p: &std::path::Path,
        episodes: &mut Vec<EpisodeFile>,
        cover: &mut Option<std::path::PathBuf>,
        synopsis: &mut Option<std::path::PathBuf>,
    ) {
        let file_name = p
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if file_name.starts_with('.') {
            return;
        }
        let lower = file_name.to_lowercase();
        let ext = p
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if VIDEO_EXTS.contains(&ext.as_str()) {
            let size = p.metadata().map(|m| m.len()).unwrap_or(0);
            episodes.push(EpisodeFile {
                path: p.to_path_buf(),
                episode_number: parse_episode_number(&file_name),
                file_name,
                size_bytes: size,
            });
        } else if IMAGE_EXTS.contains(&ext.as_str())
            && cover.is_none()
            && ["封面", "cover", "poster"]
                .iter()
                .any(|k| lower.contains(k))
        {
            *cover = Some(p.to_path_buf());
        } else if TEXT_EXTS.contains(&ext.as_str())
            && synopsis.is_none()
            && ["简介", "intro", "synopsis"]
                .iter()
                .any(|k| lower.contains(k))
        {
            *synopsis = Some(p.to_path_buf());
        }
    }

    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() {
                visit(&p, &mut episodes, &mut cover, &mut synopsis);
            } else if p.is_dir()
                && !e.file_name().to_string_lossy().starts_with('.')
                && !PRODUCT_SUBDIRS.contains(&e.file_name().to_string_lossy().as_ref())
            {
                if let Ok(sub) = std::fs::read_dir(&p) {
                    for se in sub.flatten() {
                        if se.path().is_file() {
                            visit(&se.path(), &mut episodes, &mut cover, &mut synopsis);
                        }
                    }
                }
            }
        }
    }

    episodes.sort_by(|a, b| match (a.episode_number, b.episode_number) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.file_name.cmp(&b.file_name),
    });

    Drama {
        name: name.to_string(),
        dir: dir.to_path_buf(),
        episodes,
        cover,
        synopsis,
    }
}

/// 校验素材根目录是否可用，错误信息可直接展示给用户。
pub fn validate_library_root(root: &Path) -> std::io::Result<()> {
    let meta = std::fs::metadata(root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "素材根目录不存在或无法访问：{}。请重新选择包含剧目子目录的文件夹。",
                root.display()
            ),
        )
    })?;
    if !meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!(
                "素材根目录不是文件夹：{}。请选择包含剧目子目录的文件夹。",
                root.display()
            ),
        ));
    }
    Ok(())
}

/// 校验剧目名称是单一目录名：拒绝路径分隔符、绝对路径与 `..`。
pub fn validate_drama_name(name: &str) -> std::io::Result<()> {
    let mut components = Path::new(name).components();
    let is_single = matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    );
    if !is_single {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("剧目名称不合法：{name}。只能使用单个子目录名，不能包含路径分隔符。"),
        ));
    }
    Ok(())
}

/// 校验单个剧目录是否可用，错误信息可直接展示给用户。
pub fn validate_drama_dir(dir: &Path) -> std::io::Result<()> {
    let meta = std::fs::metadata(dir).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "剧目目录不存在或无法访问：{}。请重新扫描素材根目录。",
                dir.display()
            ),
        )
    })?;
    if !meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!(
                "剧目路径不是文件夹：{}。请重新扫描素材根目录。",
                dir.display()
            ),
        ));
    }
    Ok(())
}

/// 从文件名解析集号。支持 `第12集/话/回`、`E12`、独立数字段（`xxx_12`）。
pub fn parse_episode_number(file_name: &str) -> Option<u32> {
    let s = file_name;
    if let Some(di) = s.find('第') {
        let rest = &s[di + '第'.len_utf8()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            let after = &rest[digits.len()..];
            if after.starts_with('集') || after.starts_with('话') || after.starts_with('回') {
                if let Ok(n) = digits.parse::<u32>() {
                    return Some(n);
                }
            }
        }
    }
    let stem = std::path::Path::new(s)
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| s.to_string());
    let lower = stem.to_lowercase();
    if let Some(epos) = lower.find('e') {
        let rest = &lower[epos + 1..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if !digits.is_empty() {
            let prev_ok = epos == 0
                || !lower[..epos]
                    .chars()
                    .last()
                    .map(|c| c.is_ascii_alphanumeric())
                    .unwrap_or(false);
            if prev_ok {
                if let Ok(n) = digits.parse::<u32>() {
                    return Some(n);
                }
            }
        }
    }
    let mut best: Option<u32> = None;
    let mut tok = String::new();
    for c in lower.chars().chain(['-']) {
        if c.is_ascii_digit() {
            tok.push(c);
        } else {
            if !tok.is_empty() {
                if let Ok(n) = tok.parse::<u32>() {
                    if (1..10_000).contains(&n) {
                        best = Some(n);
                    }
                }
                tok.clear();
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn episode_number_parsing() {
        assert_eq!(parse_episode_number("老汉续命记-第1集.mp4"), Some(1));
        assert_eq!(parse_episode_number("剧名_第12集.mov"), Some(12));
        assert_eq!(parse_episode_number("drama-e12.mp4"), Some(12));
        assert_eq!(parse_episode_number("show E03.mp4"), Some(3));
        assert_eq!(parse_episode_number("xxx_12.mp4"), Some(12));
        assert_eq!(parse_episode_number("12.mp4"), Some(12));
    }

    #[test]
    fn scan_drama_layout() {
        let base = std::env::temp_dir();
        let p = base.join(format!("dossier-test-{}", std::process::id()));
        std::fs::create_dir_all(p.join("测试剧").join("视频")).unwrap();
        std::fs::write(p.join("测试剧").join("简介.txt"), "内容").unwrap();
        std::fs::write(p.join("测试剧").join("封面.jpg"), b"x").unwrap();
        std::fs::write(p.join("测试剧").join("视频").join("第1集.mp4"), b"x").unwrap();
        std::fs::write(p.join("测试剧").join("视频").join("第2集.mp4"), b"x").unwrap();
        std::fs::write(p.join("测试剧").join("海报备份.png"), b"x").unwrap();

        let drama = scan_drama_dir("测试剧", &p.join("测试剧"));
        assert_eq!(drama.episodes.len(), 2);
        assert_eq!(drama.episodes[0].episode_number, Some(1));
        assert!(drama.cover.is_some());
        assert!(drama.synopsis.is_some());
        std::fs::remove_dir_all(&p).ok();
    }

    #[test]
    fn scan_skips_product_subdirs() {
        // 转码输出 / 报审包里的视频不能算新一集，否则集数翻倍、产物被当素材。
        let base = std::env::temp_dir();
        let p = base.join(format!("dossier-product-{}", std::process::id()));
        std::fs::create_dir_all(p.join("剧A").join("微调输出")).unwrap();
        std::fs::create_dir_all(p.join("剧A").join("报审包").join("06_分集视频")).unwrap();
        std::fs::write(p.join("剧A").join("第1集.mp4"), b"x").unwrap();
        std::fs::write(p.join("剧A").join("微调输出").join("第1集.mp4"), b"x").unwrap();
        std::fs::write(
            p.join("剧A").join("报审包").join("06_分集视频").join("第1集.mp4"),
            b"x",
        )
        .unwrap();

        let drama = scan_drama_dir("剧A", &p.join("剧A"));
        assert_eq!(drama.episodes.len(), 1);
        std::fs::remove_dir_all(&p).ok();
    }

    #[test]
    fn validates_paths_with_actionable_errors() {
        let base = std::env::temp_dir().join(format!("dossier-validate-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(base.join("剧A")).unwrap();
        let file = base.join("素材.txt");
        std::fs::write(&file, b"x").unwrap();

        assert!(validate_library_root(&base).is_ok());
        assert!(validate_library_root(&file).is_err());
        assert!(validate_library_root(&base.join("缺失")).is_err());

        assert!(validate_drama_dir(&base.join("剧A")).is_ok());
        assert!(validate_drama_dir(&base.join("缺失")).is_err());
        assert!(validate_drama_dir(&file).is_err());

        assert!(validate_drama_name("剧A").is_ok());
        assert!(validate_drama_name("剧 A").is_ok());
        assert!(validate_drama_name("").is_err());
        assert!(validate_drama_name("..").is_err());
        assert!(validate_drama_name("a/b").is_err());
        assert!(validate_drama_name("./剧A").is_err());

        std::fs::remove_dir_all(&base).ok();
    }
}
