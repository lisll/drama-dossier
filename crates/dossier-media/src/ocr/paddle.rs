//! PaddleOCR 后端：调用用户自己装好的 PaddleOCR（3.x 命令行）。
//!
//! 这一级**不引入任何依赖**，只是把图片交给子进程，再读它写出的 JSON。放在降级链
//! 第一级，是因为 PP-OCR 的中文画面文字效果最好；代价是用户得自己装 Python 与
//! paddlepaddle，所以它不能是唯一方案。
//!
//! 以下约定都来自本机实测（paddleocr 3.7.0 + paddlepaddle 3.3.1，CPU），不是凭文档猜：
//! - 命令：`paddleocr ocr -i <图片或目录> ... --save_path <目录>`，布尔参数写 `False`；
//! - **必须 `--enable_mkldnn False`**：开着 oneDNN 时 CPU 推理直接崩溃
//!   （`ConvertPirAttribute2RuntimeAttribute not support`）；
//! - 结果读 `--save_path` 下的 `<原图名>_res.json`，字段 `rec_texts` / `rec_scores`
//!   （0~1）/ `rec_boxes`（`[x1, y1, x2, y2]`），没有文字时三个数组都为空；
//! - **不要解析终端输出**：终端打印的是 Python 字典，数组会被省略成 `...`，数据不完整；
//! - 模型用 `PP-OCRv5_mobile`：默认的 PP-OCRv6_medium 在 CPU 上 36 帧要 185 秒，
//!   mobile 只要 46 秒，干净烧字幕的识别结果一致；
//! - 每次启动都要加载模型（约 10 秒），所以一集的帧要**整批**交给一次调用。

use std::path::{Path, PathBuf};
use std::time::Duration;

use dossier_runtime::{CommandRunner, CommandSpec};
use serde::Deserialize;

use super::{OcrError, OcrLine};

/// 启动 + 加载模型的固定开销上限（首次运行还要下载约 20 MB 模型）。
const BASE_TIMEOUT: Duration = Duration::from_secs(300);
/// 每张图额外给的时间。实测 mobile 模型约 1 秒/张，留足慢机器的余量。
const PER_IMAGE_TIMEOUT: Duration = Duration::from_secs(15);

/// 检测与识别模型。理由见模块文档。
const DETECTION_MODEL: &str = "PP-OCRv5_mobile_det";
const RECOGNITION_MODEL: &str = "PP-OCRv5_mobile_rec";

/// 识别一张图（整批接口的单张特例）。
pub fn recognize(
    image: &Path,
    program: &Path,
    save_dir: &Path,
    runner: &dyn CommandRunner,
) -> Result<Vec<OcrLine>, OcrError> {
    recognize_batch(&[image.to_path_buf()], program, save_dir, runner)?
        .into_iter()
        .next()
        .unwrap_or_else(|| Err(OcrError::OutputMissing(save_dir.to_path_buf())))
}

/// 整批识别：一次启动 PaddleOCR 处理所有图片，模型只加载一次。
///
/// - 外层 `Err`：整批失败（程序不存在、进程退出码非 0、图片不在同一目录）——这是
///   「这一级跑不起来」，调用方据此降级；
/// - 内层每张一个 `Result`：某张图缺结果文件或格式不认识，只算这一张失败。
///
/// 返回顺序与 `images` 一致。
pub fn recognize_batch(
    images: &[PathBuf],
    program: &Path,
    save_dir: &Path,
    runner: &dyn CommandRunner,
) -> Result<Vec<Result<Vec<OcrLine>, OcrError>>, OcrError> {
    if !program.is_file() {
        return Err(OcrError::ToolMissing(format!(
            "PaddleOCR 入口不存在：{}",
            program.display()
        )));
    }
    let present: Vec<&PathBuf> = images.iter().filter(|image| image.is_file()).collect();
    if present.is_empty() {
        return Ok(images
            .iter()
            .map(|image| Err(OcrError::InputMissing(image.clone())))
            .collect());
    }
    let input = batch_input(&present)?;

    std::fs::create_dir_all(save_dir).map_err(|error| OcrError::RunFailed {
        command: "prepare".to_string(),
        message: format!(
            "创建 PaddleOCR 输出目录 {} 失败：{error}",
            save_dir.display()
        ),
    })?;
    // 清掉这批图片上一次留下的结果，避免把旧结果当成这次的。只删本批对应的文件。
    for image in &present {
        let _ = std::fs::remove_file(result_path(save_dir, image));
    }

    let spec = build_command(program, &input, save_dir, present.len());
    let output = runner.run(&spec).map_err(|error| OcrError::RunFailed {
        command: spec.display(),
        message: error.to_string(),
    })?;
    if !output.success() {
        return Err(OcrError::RunFailed {
            command: spec.display(),
            message: crate::failure_message(&output),
        });
    }

    Ok(images
        .iter()
        .map(|image| {
            if !image.is_file() {
                return Err(OcrError::InputMissing(image.clone()));
            }
            load_result(&result_path(save_dir, image))
        })
        .collect())
}

/// 这批图片交给 `-i` 的输入：单张就传文件本身（不会顺带处理同目录的其他图），
/// 多张必须在同一目录，传目录。
fn batch_input(images: &[&PathBuf]) -> Result<PathBuf, OcrError> {
    if let [only] = images {
        return Ok((*only).clone());
    }
    let parent = images[0]
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    if images
        .iter()
        .any(|image| image.parent() != Some(parent.as_path()))
    {
        return Err(OcrError::RunFailed {
            command: "paddleocr".to_string(),
            message: "整批识别要求图片在同一目录".to_string(),
        });
    }
    Ok(parent)
}

/// 组装命令行（参数来源见模块文档）。
fn build_command(program: &Path, input: &Path, save_dir: &Path, count: usize) -> CommandSpec {
    let timeout = BASE_TIMEOUT + PER_IMAGE_TIMEOUT * u32::try_from(count).unwrap_or(u32::MAX);
    CommandSpec::new(program.display().to_string())
        .args(["ocr", "-i"])
        .arg(input.display().to_string())
        .args(["--text_detection_model_name", DETECTION_MODEL])
        .args(["--text_recognition_model_name", RECOGNITION_MODEL])
        .args(["--use_doc_orientation_classify", "False"])
        .args(["--use_doc_unwarping", "False"])
        .args(["--use_textline_orientation", "False"])
        .args(["--enable_mkldnn", "False"])
        .arg("--save_path")
        .arg(save_dir.display().to_string())
        // 模型从百度 BOS 下载：国内可直连，实测连通性检查最终选的也是它。
        .env("PADDLE_PDX_MODEL_SOURCE", "BOS")
        .timeout(timeout)
}

/// `<save_dir>/<原图名>_res.json`
fn result_path(save_dir: &Path, image: &Path) -> PathBuf {
    let stem = image
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_default();
    save_dir.join(format!("{stem}_res.json"))
}

/// PaddleOCR 结果文件里我们用到的字段。
#[derive(Debug, Deserialize)]
struct PaddleResult {
    rec_texts: Vec<String>,
    #[serde(default)]
    rec_scores: Vec<f64>,
    #[serde(default)]
    rec_boxes: Vec<Vec<f64>>,
}

/// 读一张图的结果文件。
fn load_result(path: &Path) -> Result<Vec<OcrLine>, OcrError> {
    if !path.is_file() {
        return Err(OcrError::OutputMissing(path.to_path_buf()));
    }
    let raw = std::fs::read_to_string(path).map_err(|error| OcrError::RunFailed {
        command: "paddleocr".to_string(),
        message: format!("读取结果 {} 失败：{error}", path.display()),
    })?;
    parse_result(&raw).map_err(|message| OcrError::RunFailed {
        command: "paddleocr".to_string(),
        message: format!("结果 {} 格式不认识：{message}", path.display()),
    })
}

/// 把结果 JSON 转成文本行。置信度换算到 0~100；缺分数记为未知（-1）。
pub fn parse_result(raw: &str) -> Result<Vec<OcrLine>, String> {
    let result: PaddleResult = serde_json::from_str(raw).map_err(|error| error.to_string())?;
    Ok(result
        .rec_texts
        .into_iter()
        .enumerate()
        .filter(|(_, text)| !text.trim().is_empty())
        .map(|(index, text)| {
            let confidence = result
                .rec_scores
                .get(index)
                .map_or(-1.0, |score| (score * 100.0).clamp(0.0, 100.0));
            let (left, top, width, height) = match result.rec_boxes.get(index).map(Vec::as_slice) {
                Some([x1, y1, x2, y2, ..]) => (
                    x1.round() as i64,
                    y1.round() as i64,
                    (x2 - x1).round() as i64,
                    (y2 - y1).round() as i64,
                ),
                _ => (0, 0, 0, 0),
            };
            OcrLine {
                text,
                confidence,
                left,
                top,
                width,
                height,
            }
        })
        .collect())
}

/// 找 PaddleOCR 入口：先找 PATH，再找 pip / Homebrew 的常见安装目录。
///
/// 后者是给 macOS 准备的：从访达启动的程序继承不到终端里配置的 PATH，
/// `pip install --user` 装到 `~/.local/bin` 的命令就找不到了。
pub fn find_program() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let path = std::env::var_os("PATH");
    let dirs = candidate_dirs(path.as_deref(), home.as_deref());
    find_in(&dirs)
}

fn candidate_dirs(path: Option<&std::ffi::OsStr>, home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = path
        .map(|p| std::env::split_paths(p).collect())
        .unwrap_or_default();
    for extra in ["/opt/homebrew/bin", "/usr/local/bin"] {
        dirs.push(PathBuf::from(extra));
    }
    if let Some(home) = home {
        dirs.push(home.join(".local/bin"));
    }
    dirs
}

fn find_in(dirs: &[PathBuf]) -> Option<PathBuf> {
    for dir in dirs {
        for name in ["paddleocr", "paddleocr.exe"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::test_support::{unique_dir, write_png};
    use dossier_runtime::{CommandError, CommandOutput};
    use std::sync::Mutex;

    /// 本机实测 paddleocr 3.7.0 写出的 caption_0001_res.json（删掉了用不到的字段）。
    const REAL_RESULT: &str = r#"{
        "input_path": "frames/caption_0001.png",
        "page_index": null,
        "rec_texts": ["你竟然还敢回来"],
        "rec_scores": [0.9999276399612427],
        "rec_polys": [[[403, 85], [1036, 85], [1036, 194], [403, 194]]],
        "rec_boxes": [[403, 85, 1036, 194]]
    }"#;
    /// 本机实测：没有文字的帧，三个数组都为空。
    const REAL_EMPTY: &str = r#"{"input_path": "frames/caption_0003.png", "rec_texts": [], "rec_scores": [], "rec_boxes": []}"#;

    /// 模拟 PaddleOCR：运行时把预设结果写进 `--save_path`，并记下命令。
    struct FakePaddle {
        results: Vec<(String, String)>,
        exit_code: i32,
        commands: Mutex<Vec<CommandSpec>>,
    }

    impl FakePaddle {
        fn writes(results: &[(&str, &str)]) -> Self {
            Self {
                results: results
                    .iter()
                    .map(|(name, body)| ((*name).to_string(), (*body).to_string()))
                    .collect(),
                exit_code: 0,
                commands: Mutex::new(Vec::new()),
            }
        }

        fn fails() -> Self {
            Self {
                exit_code: 1,
                ..Self::writes(&[])
            }
        }

        fn last(&self) -> CommandSpec {
            self.commands
                .lock()
                .unwrap()
                .last()
                .cloned()
                .expect("应当调用过")
        }
    }

    impl CommandRunner for FakePaddle {
        fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, CommandError> {
            self.commands.lock().unwrap().push(spec.clone());
            if self.exit_code != 0 {
                return Ok(CommandOutput::new(self.exit_code, "", "ValueError: boom"));
            }
            let save = spec.args.iter().position(|a| a == "--save_path").unwrap();
            let save_dir = PathBuf::from(&spec.args[save + 1]);
            for (name, body) in &self.results {
                std::fs::write(save_dir.join(name), body).unwrap();
            }
            Ok(CommandOutput::new(0, "", ""))
        }
    }

    fn value_after<'a>(spec: &'a CommandSpec, flag: &str) -> Option<&'a str> {
        let index = spec.args.iter().position(|a| a == flag)?;
        spec.args.get(index + 1).map(String::as_str)
    }

    #[test]
    fn parses_the_real_result_file() {
        let lines = parse_result(REAL_RESULT).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "你竟然还敢回来");
        assert!((lines[0].confidence - 99.992_763_996_124_27).abs() < 1e-6);
        assert_eq!((lines[0].left, lines[0].top), (403, 85));
        assert_eq!((lines[0].width, lines[0].height), (633, 109));
    }

    #[test]
    fn empty_result_means_no_text_not_an_error() {
        assert!(parse_result(REAL_EMPTY).unwrap().is_empty());
    }

    #[test]
    fn missing_scores_or_boxes_degrade_gracefully() {
        let lines = parse_result(r#"{"rec_texts": ["甲", "  ", "乙"]}"#).unwrap();
        assert_eq!(lines.len(), 2, "空白文本要跳过");
        assert!(
            lines[0].confidence_unknown(),
            "缺分数记为未知，交给 passes 放行"
        );
        assert_eq!(lines[1].width, 0);
    }

    #[test]
    fn unrecognized_json_is_an_error_not_an_empty_result() {
        assert!(
            parse_result(r#"{"res": {}}"#).is_err(),
            "缺 rec_texts 必须报错"
        );
        assert!(
            parse_result("{'res': ...}").is_err(),
            "终端打印的字典格式不是 JSON"
        );
    }

    #[test]
    fn command_matches_the_tested_cli() {
        let dir = unique_dir("paddle-cmd");
        let program = write_png(&dir, "paddleocr");
        let frames = dir.join("frames");
        std::fs::create_dir_all(&frames).unwrap();
        let images = vec![
            write_png(&frames, "caption_0001.png"),
            write_png(&frames, "caption_0002.png"),
        ];
        let runner = FakePaddle::writes(&[]);
        recognize_batch(&images, &program, &dir.join("out"), &runner).unwrap();

        let spec = runner.last();
        assert_eq!(spec.args[0], "ocr");
        assert_eq!(
            value_after(&spec, "-i"),
            Some(frames.display().to_string().as_str()),
            "多张传目录"
        );
        assert_eq!(
            value_after(&spec, "--enable_mkldnn"),
            Some("False"),
            "关 oneDNN，否则 CPU 推理崩溃"
        );
        assert_eq!(
            value_after(&spec, "--use_textline_orientation"),
            Some("False")
        );
        assert_eq!(
            value_after(&spec, "--text_detection_model_name"),
            Some(DETECTION_MODEL)
        );
        assert!(
            !spec.args.iter().any(|a| a == "--image_dir" || a == "false"),
            "不能再用 2.x 参数或小写布尔值"
        );
        assert!(spec
            .env
            .iter()
            .any(|(k, v)| k == "PADDLE_PDX_MODEL_SOURCE" && v == "BOS"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn batch_maps_each_result_file_back_to_its_frame() {
        let dir = unique_dir("paddle-map");
        let program = write_png(&dir, "paddleocr");
        let images = vec![
            write_png(&dir, "caption_0001.png"),
            write_png(&dir, "caption_0002.png"),
            write_png(&dir, "caption_0003.png"),
        ];
        // 第 2 帧故意不写结果，模拟单张缺结果文件。
        let runner = FakePaddle::writes(&[
            ("caption_0001_res.json", REAL_RESULT),
            ("caption_0003_res.json", REAL_EMPTY),
        ]);
        let results = recognize_batch(&images, &program, &dir.join("out"), &runner).unwrap();

        assert_eq!(results.len(), 3, "结果与输入一一对应");
        assert_eq!(results[0].as_ref().unwrap()[0].text, "你竟然还敢回来");
        assert!(
            matches!(results[1], Err(OcrError::OutputMissing(_))),
            "缺结果只算这一张失败"
        );
        assert!(results[2].as_ref().unwrap().is_empty());
        assert_eq!(runner.commands.lock().unwrap().len(), 1, "整批只启动一次");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_results_from_a_previous_run_are_not_reused() {
        let dir = unique_dir("paddle-stale");
        let program = write_png(&dir, "paddleocr");
        let image = write_png(&dir, "caption_0001.png");
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("caption_0001_res.json"), REAL_RESULT).unwrap();

        // 这次 PaddleOCR 什么都没写：不能把上次的结果当成这次的。
        let error = recognize(&image, &program, &out, &FakePaddle::writes(&[])).unwrap_err();
        assert!(matches!(error, OcrError::OutputMissing(_)), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn single_image_passes_the_file_not_its_directory() {
        let dir = unique_dir("paddle-single");
        let program = write_png(&dir, "paddleocr");
        let image = write_png(&dir, "caption_0001.png");
        write_png(&dir, "caption_0002.png");
        let runner = FakePaddle::writes(&[("caption_0001_res.json", REAL_RESULT)]);
        let lines = recognize(&image, &program, &dir.join("out"), &runner).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(
            value_after(&runner.last(), "-i"),
            Some(image.display().to_string().as_str()),
            "单张不能传目录，否则会顺带识别同目录的其他图"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nonzero_exit_fails_the_whole_batch_with_stderr() {
        let dir = unique_dir("paddle-exit");
        let program = write_png(&dir, "paddleocr");
        let images = vec![write_png(&dir, "a.png"), write_png(&dir, "b.png")];
        let error =
            recognize_batch(&images, &program, &dir.join("out"), &FakePaddle::fails()).unwrap_err();
        assert!(matches!(error, OcrError::RunFailed { .. }));
        assert!(error.to_string().contains("ValueError"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn images_from_different_directories_are_rejected() {
        let dir = unique_dir("paddle-dirs");
        let program = write_png(&dir, "paddleocr");
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let images = vec![write_png(&dir, "a.png"), write_png(&other, "b.png")];
        let runner = FakePaddle::writes(&[]);
        assert!(recognize_batch(&images, &program, &dir.join("out"), &runner).is_err());
        assert!(runner.commands.lock().unwrap().is_empty(), "不该启动进程");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_program_is_reported_before_running() {
        let dir = unique_dir("paddle-noprog");
        let image = write_png(&dir, "a.png");
        let error = recognize(
            &image,
            Path::new("/definitely/not/paddleocr"),
            &dir,
            &FakePaddle::writes(&[]),
        )
        .unwrap_err();
        assert!(matches!(error, OcrError::ToolMissing(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真实 PaddleOCR 端到端（默认跳过，加载模型要十几秒）。手动运行：
    /// `DOSSIER_PADDLEOCR=<paddleocr 路径> DOSSIER_PADDLEOCR_FRAMES=<帧目录> \
    ///  cargo test -p dossier-media real_paddleocr -- --ignored`
    /// 帧目录里的 `caption_0001.png` 应当画着「你竟然还敢回来」。
    #[test]
    #[ignore = "需要本机装好的 PaddleOCR"]
    fn real_paddleocr_recognizes_burned_in_captions() {
        let (Some(program), Some(frames)) = (
            std::env::var_os("DOSSIER_PADDLEOCR"),
            std::env::var_os("DOSSIER_PADDLEOCR_FRAMES"),
        ) else {
            eprintln!("跳过：未设置 DOSSIER_PADDLEOCR / DOSSIER_PADDLEOCR_FRAMES");
            return;
        };
        let frames = PathBuf::from(frames);
        let mut images: Vec<PathBuf> = std::fs::read_dir(&frames)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "png"))
            .collect();
        images.sort();
        let out = unique_dir("paddle-real");
        let results = recognize_batch(
            &images,
            Path::new(&program),
            &out,
            &dossier_runtime::SystemRunner::new(),
        )
        .expect("整批识别应成功");
        assert_eq!(results.len(), images.len());
        let first = results[0].as_ref().expect("第 1 帧应有结果");
        assert!(
            first
                .iter()
                .any(|line| line.text.contains("你竟然还敢回来") && line.passes(60.0)),
            "{first:?}"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn program_is_found_in_user_install_dirs_outside_path() {
        let dir = unique_dir("paddle-find");
        let home = dir.join("home");
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_png(&bin, "paddleocr");
        // PATH 里只有一个空目录：模拟从访达启动、继承不到终端 PATH。
        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let path = std::env::join_paths([&empty]).unwrap();
        let dirs = candidate_dirs(Some(&path), Some(&home));
        assert_eq!(find_in(&dirs), Some(bin.join("paddleocr")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
