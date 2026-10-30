//! 剧集库扫描：约定「素材根目录下，一部剧一个子目录」。

use std::path::Path;

use crate::types::{Drama, EpisodeFile};

const VIDEO_EXTS: &[&str] = &["mp4", "mov", "mkv", "flv", "ts", "m4v", "webm"];
const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp"];
const TEXT_EXTS: &[&str] = &["txt", "md"];

/// 报审包的默认落点子目录（`package.rs`）。
pub const PACKAGE_OUTPUT_DIR: &str = "报审包";
/// 桌面端「智能微调」的默认落点子目录。
pub const NUDGE_OUTPUT_DIR: &str = "微调输出";
/// 桌面端「分集重组」的默认落点子目录。
pub const RECUT_OUTPUT_DIR: &str = "重组输出";
/// 桌面端「深度处理」的默认落点子目录（成片在其下的「视频成片/」，材料在目录本层）。
pub const DEEP_OUTPUT_DIR: &str = "深度处理";

/// 应用自己会在剧目录里生成的产物子目录，扫描分集时必须跳过：
/// 否则转码输出（同名文件）会被当成新一集，转码一次集数就翻倍，
/// 下次转码还会把旧产物当素材重新处理。深度处理目录本层还有 AI 生成的 `简介.txt`，
/// 不跳过就会被当成「素材自带的简介」，下一轮把上一轮 AI 的输出当原始输入。
/// 各模式的落点名都从上面的常量来，桌面端拼路径时用同一组常量，两边不会对不上。
const PRODUCT_SUBDIRS: &[&str] = &[
    PACKAGE_OUTPUT_DIR,
    NUDGE_OUTPUT_DIR,
    RECUT_OUTPUT_DIR,
    DEEP_OUTPUT_DIR,
];

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

    // 集号相同（重复集号）时按文件名、再按完整路径排：顺序不随目录读取顺序变化。
    episodes.sort_by(|a, b| {
        let by_number = match (a.episode_number, b.episode_number) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        };
        by_number
            .then_with(|| a.file_name.cmp(&b.file_name))
            .then_with(|| a.path.cmp(&b.path))
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

/// 从文件名解析集号，优先级从高到低：
/// 1. `第12集` / `第12话` / `第12回`（文件名里任意位置，取第一个成立的：`第2季第3集` → 3）；
/// 2. 词边界的 `E12` / `EP12`（前一个字符不是字母数字），或季集写法 `S01E05`；
/// 3. 主干里的独立数字，取最后一个 1–9999 的数字段；先排除分辨率与编码标记
///    （`1080p`、`1080i`、`4k`、`60fps`、`x264`/`h265`、`1920x1080` 两侧），全被排除时退回取最后一个。
pub fn parse_episode_number(file_name: &str) -> Option<u32> {
    parse_chinese_marker(file_name).or_else(|| {
        let stem = std::path::Path::new(file_name)
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_lowercase())
            .unwrap_or_else(|| file_name.to_lowercase());
        let chars: Vec<char> = stem.chars().collect();
        parse_e_marker(&chars).or_else(|| parse_standalone_number(&chars))
    })
}

fn episode_value(digits: &str) -> Option<u32> {
    digits
        .parse::<u32>()
        .ok()
        .filter(|n| (1..10_000).contains(n))
}

fn parse_chinese_marker(s: &str) -> Option<u32> {
    for (at, _) in s.match_indices('第') {
        let rest = &s[at + '第'.len_utf8()..];
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            continue;
        }
        let after = &rest[digits.len()..];
        if after.starts_with(['集', '话', '回']) {
            if let Some(n) = episode_value(&digits) {
                return Some(n);
            }
        }
    }
    None
}

fn digits_at(chars: &[char], start: usize) -> String {
    chars[start.min(chars.len())..]
        .iter()
        .take_while(|c| c.is_ascii_digit())
        .collect()
}

/// `e` 前面是不是「s + 数字」（`s01e05`），且 `s` 本身在词边界上。
fn preceded_by_season(chars: &[char], e_at: usize) -> bool {
    let mut i = e_at;
    while i > 0 && chars[i - 1].is_ascii_digit() {
        i -= 1;
    }
    i < e_at && i > 0 && chars[i - 1] == 's' && (i == 1 || !chars[i - 2].is_ascii_alphanumeric())
}

fn parse_e_marker(chars: &[char]) -> Option<u32> {
    for (at, &c) in chars.iter().enumerate() {
        if c != 'e' {
            continue;
        }
        let boundary = at == 0 || !chars[at - 1].is_ascii_alphanumeric();
        if !boundary && !preceded_by_season(chars, at) {
            continue;
        }
        let start = if chars.get(at + 1) == Some(&'p') {
            at + 2
        } else {
            at + 1
        };
        let digits = digits_at(chars, start);
        if let Some(n) = episode_value(&digits) {
            return Some(n);
        }
    }
    None
}

/// 从 `at` 起是否紧跟某个后缀，且后缀之后不是字母（`1080p_` 算，`05part` 不算）。
fn followed_by_word(chars: &[char], at: usize, word: &str) -> bool {
    let word: Vec<char> = word.chars().collect();
    chars.len() >= at + word.len()
        && chars[at..at + word.len()] == word[..]
        && !chars
            .get(at + word.len())
            .is_some_and(|c| c.is_ascii_alphabetic())
}

/// 这一段数字是不是分辨率 / 帧率 / 编码标记，而不是集号。
fn is_media_token(chars: &[char], start: usize, end: usize, value: u32) -> bool {
    if ["p", "i", "k", "fps"]
        .iter()
        .any(|word| followed_by_word(chars, end, word))
    {
        return true;
    }
    // x264 / h265 / h.264 等编码标记。
    if matches!(value, 264 | 265) && start > 0 {
        let before = chars[start - 1];
        if matches!(before, 'x' | 'h') || (before == '.' && start >= 2 && chars[start - 2] == 'h') {
            return true;
        }
    }
    // 1080p60：紧跟在「数字 + p」后面的帧率。
    if start >= 2 && chars[start - 1] == 'p' && chars[start - 2].is_ascii_digit() {
        return true;
    }
    // 1920x1080：x 两侧都是数字时，两段都是分辨率。
    let x_after =
        chars.get(end) == Some(&'x') && chars.get(end + 1).is_some_and(char::is_ascii_digit);
    let x_before = start >= 2 && chars[start - 1] == 'x' && chars[start - 2].is_ascii_digit();
    x_after || x_before
}

fn parse_standalone_number(chars: &[char]) -> Option<u32> {
    let mut last_any = None;
    let mut last_clean = None;
    let mut at = 0;
    while at < chars.len() {
        if !chars[at].is_ascii_digit() {
            at += 1;
            continue;
        }
        let start = at;
        while at < chars.len() && chars[at].is_ascii_digit() {
            at += 1;
        }
        let digits: String = chars[start..at].iter().collect();
        if let Some(n) = episode_value(&digits) {
            last_any = Some(n);
            // `剧名_05_v2`：词首 v 后的数字是版本号，但只有前面已经有一个干净的集号时才这么认，
            // 否则 `v05_1080p` 这种唯一的集号会被排除掉。
            let version = last_clean.is_some() && is_version_suffix(chars, start);
            if !version && !is_media_token(chars, start, at, n) {
                last_clean = Some(n);
            }
        }
    }
    last_clean.or(last_any)
}

/// 数字前是词首的 `v`（`_v2`、`-v3`；`dev05` 不算）。
fn is_version_suffix(chars: &[char], start: usize) -> bool {
    start > 0 && chars[start - 1] == 'v' && (start < 2 || !chars[start - 2].is_ascii_alphabetic())
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
    fn episode_number_ignores_resolution_and_codec_tags() {
        let cases = [
            ("S01E05_1080p.mp4", Some(5)),
            ("the_e03_720p.mp4", Some(3)),
            ("EP01.mp4", Some(1)),
            ("Ep.05.mp4", Some(5)),
            ("S1E2.mkv", Some(2)),
            ("episode10.mp4", Some(10)),
            ("ep01v2.mp4", Some(1)),
            ("第2季第3集.mp4", Some(3)),
            ("剧名-第1集-1080p.mp4", Some(1)),
            ("剧名_12_1080p.mp4", Some(12)),
            ("剧名_05_1920x1080.mp4", Some(5)),
            ("drama_x264_07.mp4", Some(7)),
            ("drama_07_h265.mp4", Some(7)),
            ("4K_08.mp4", Some(8)),
            ("剧名_09_60fps.mp4", Some(9)),
            ("2024短剧_05.mp4", Some(5)),
            ("v2_03.mp4", Some(3)),
            ("第01集(1).mp4", Some(1)),
            ("ch05.mp4", Some(5)),
            ("ep05part.mp4", Some(5)),
            ("show_05part2.mp4", Some(2)),
            ("1-2.mp4", Some(2)),
            ("剧名_05_v2.mp4", Some(5)),
            ("v05_1080p.mp4", Some(5)),
            ("v2_03.mp4", Some(3)),
            ("第5集v2.mp4", Some(5)),
            ("p05.mp4", Some(5)),
            // 第 标记同样只认 1–9999。
            ("第0集.mp4", None),
            ("第4294967295集.mp4", None),
            ("剧名_05_1080p60.mp4", Some(5)),
            ("剧名.H.264_05.mp4", Some(5)),
            ("剧名_05.h.265.mp4", Some(5)),
            ("dev05.mp4", Some(5)),
            // 超出 1–9999 的数字不当集号（E 标记与独立数字都一样），避免异常文件名解析出极大集号。
            ("E4294967295.mp4", None),
            ("life.mp4", None),
            // 全是分辨率标记时退回取最后一个数字，不比旧规则差。
            ("1080p.mp4", Some(1080)),
        ];
        for (name, expected) in cases {
            assert_eq!(parse_episode_number(name), expected, "{name}");
        }
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
    fn duplicate_episode_numbers_sort_by_name_then_path() {
        let p = std::env::temp_dir().join(format!("dossier-dup-order-{}", std::process::id()));
        let d = p.join("剧B");
        std::fs::create_dir_all(d.join("b")).unwrap();
        std::fs::create_dir_all(d.join("a")).unwrap();
        std::fs::write(d.join("b").join("第3集.mp4"), b"x").unwrap();
        std::fs::write(d.join("a").join("第3集.mp4"), b"x").unwrap();
        std::fs::write(d.join("z_03.mp4"), b"x").unwrap();
        std::fs::write(d.join("第2集.mp4"), b"x").unwrap();
        let drama = scan_drama_dir("剧B", &d);
        let order: Vec<String> = drama
            .episodes
            .iter()
            .map(|e| {
                e.path
                    .strip_prefix(&d)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        // 文件名按字节序：ASCII 的 `z_03` 排在 `第3集` 前；同名的两个按路径 a/ 在 b/ 前。
        assert_eq!(
            order,
            ["第2集.mp4", "z_03.mp4", "a/第3集.mp4", "b/第3集.mp4"]
        );
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
            p.join("剧A")
                .join("报审包")
                .join("06_分集视频")
                .join("第1集.mp4"),
            b"x",
        )
        .unwrap();

        let drama = scan_drama_dir("剧A", &p.join("剧A"));
        assert_eq!(drama.episodes.len(), 1);
        std::fs::remove_dir_all(&p).ok();
    }

    #[test]
    fn scan_ignores_generated_synopsis_in_deep_output() {
        // 深度处理在 <剧目录>/深度处理/ 写 AI 生成的 简介.txt：它不能被当成素材自带的简介。
        let p = std::env::temp_dir().join(format!("dossier-deep-synopsis-{}", std::process::id()));
        std::fs::remove_dir_all(&p).ok();
        let drama_dir = p.join("剧A");
        std::fs::create_dir_all(drama_dir.join(DEEP_OUTPUT_DIR).join("视频成片")).unwrap();
        std::fs::write(drama_dir.join("第1集.mp4"), b"x").unwrap();
        std::fs::write(drama_dir.join(DEEP_OUTPUT_DIR).join("简介.txt"), "AI 生成").unwrap();
        std::fs::write(
            drama_dir
                .join(DEEP_OUTPUT_DIR)
                .join("视频成片")
                .join("第01集.mp4"),
            b"x",
        )
        .unwrap();

        // 剧目录没有自己的简介：不能退而读到生成的那份
        let drama = scan_drama_dir("剧A", &drama_dir);
        assert_eq!(drama.synopsis, None, "生成的简介被当成了素材简介");
        assert_eq!(drama.episodes.len(), 1);

        // 剧目录有自己的简介：必须用它
        std::fs::write(drama_dir.join("简介.txt"), "原始").unwrap();
        let drama = scan_drama_dir("剧A", &drama_dir);
        assert_eq!(drama.synopsis, Some(drama_dir.join("简介.txt")));
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
