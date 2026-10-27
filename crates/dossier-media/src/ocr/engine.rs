//! 各 OCR 后端的实际调用与运行期降级。
//!
//! [`crate::ocr_backend`] 回答「本机应该用哪一级」，本模块回答「这一级具体怎么跑，
//! 跑不通怎么办」。两者分开是因为它们的失败模式不同：
//!
//! - 自检失败（没装、没语言包）在开始前就能判定，走 `ocr_backend` 的跳过逻辑；
//! - **运行期失败**（自检通过但真的跑不起来：依赖坏了、语言包被卸载、模型损坏）
//!   只有跑一次才知道，需要在这里降级。
//!
//! 降级规则（每一帧都按这三条走）：
//! 1. **未确认**的级别失败 → 降到下一级，并用下一级重试**同一帧**；
//! 2. 某一级成功过一次就算**已确认**。已确认的级别之后再失败，只算这一帧失败，
//!    不再降级——能成功过，说明后端本身是好的，问题出在这一帧；
//! 3. 当前级别最多停在链上最后一级，**永不越界**。所以每一帧至少会被最后一级尝试一次，
//!    一帧失败不会连累后面的帧。

use std::path::{Path, PathBuf};

use dossier_runtime::CommandRunner;

use super::{OcrError, OcrLine, RecognizePlan};
use crate::ocr_backend::{Backend, Environment, Selection};

/// 一次识别请求：一张图 + 中间产物路径前缀 + 语言。
#[derive(Debug, Clone)]
pub struct RecognizeRequest {
    /// 待识别的图片（抽帧产物）。
    pub image: PathBuf,
    /// 识别中间产物的路径前缀（tesseract 会写成 `<output_base>.tsv`）。
    pub output_base: PathBuf,
    /// 语言标记。tesseract 用 `chi_sim+eng` 这种写法，其他后端按需解释。
    pub language: String,
}

impl RecognizeRequest {
    pub fn new(image: impl Into<PathBuf>, output_base: impl Into<PathBuf>) -> Self {
        Self {
            image: image.into(),
            output_base: output_base.into(),
            language: "chi_sim+eng".to_string(),
        }
    }
}

/// 一条降级记录：某一级失败了，于是换到下一级。
#[derive(Debug, Clone, PartialEq)]
pub struct Downgrade {
    /// 失败的那一级
    pub from: Backend,
    /// 换到的下一级
    pub to: Backend,
    /// 失败原因
    pub reason: String,
}

/// 一个已就绪的识别管线。
///
/// 由 [`OcrPipeline::new`] 用自检结果建立，之后每帧调用 [`OcrPipeline::recognize`]。
#[derive(Debug, Clone)]
pub struct OcrPipeline {
    /// 自检给出的候选链，从高到低
    chain: Vec<Candidate>,
    /// 当前实际在用的下标（指向 `chain`，恒小于 `chain.len()`）
    current: usize,
    /// 当前级别是否成功过（成功过就不再因单帧失败降级）
    confirmed: bool,
    /// 已经发生的降级，供日志与界面展示
    downgrades: Vec<Downgrade>,
}

/// 候选链上的一级：后端 + 它的入口（系统 OCR 为空路径）。
#[derive(Debug, Clone)]
struct Candidate {
    backend: Backend,
    program: PathBuf,
}

impl OcrPipeline {
    /// 按自检结果建立管线。
    ///
    /// `selection` 是 [`crate::ocr_backend::select`] 选中的第一级，`env` 用来补齐
    /// 它之后的备选（自检里已经知道哪些还可用）。返回 `None` 表示一级都没有。
    pub fn new(selection: Option<Selection>, env: &Environment) -> Option<Self> {
        let selection = selection?;
        let program = selection.program.clone().unwrap_or_default();
        let mut chain = vec![Candidate {
            backend: selection.backend,
            program,
        }];
        // 把 selection 之后仍然可用的级别按 ORDER 补进来，供运行期降级。
        // 排在 selection 之前的级别在自检时就不可用，这里也补不进来。
        for backend in crate::ocr_backend::ORDER {
            if chain.iter().any(|c| c.backend == backend) {
                continue;
            }
            if let Some(candidate) = candidate_for(backend, env) {
                chain.push(candidate);
            }
        }
        Some(Self {
            chain,
            current: 0,
            confirmed: false,
            downgrades: Vec::new(),
        })
    }

    /// 当前实际在用的后端。
    pub fn backend(&self) -> Backend {
        self.chain[self.current].backend
    }

    /// 已经发生的降级记录。
    pub fn downgrades(&self) -> &[Downgrade] {
        &self.downgrades
    }

    /// 识别一张图，规则见模块文档。
    ///
    /// 失败时错误信息列出这一帧试过的每一级及原因，不会出现空的详情。
    pub fn recognize(
        &mut self,
        request: &RecognizeRequest,
        runner: &dyn CommandRunner,
    ) -> Result<Vec<OcrLine>, OcrError> {
        let mut failures: Vec<(Backend, String)> = Vec::new();
        loop {
            let candidate = &self.chain[self.current];
            let backend = candidate.backend;
            match run_backend(candidate, request, runner) {
                Ok(lines) => {
                    self.confirmed = true;
                    return Ok(lines);
                }
                Err(error) => {
                    let message = error.to_string();
                    failures.push((backend, message.clone()));
                    let next = self.current + 1;
                    // 已确认的级别，或已经是最后一级：只算这一帧失败。
                    if self.confirmed || next >= self.chain.len() {
                        break;
                    }
                    self.downgrades.push(Downgrade {
                        from: backend,
                        to: self.chain[next].backend,
                        reason: message,
                    });
                    self.current = next;
                }
            }
        }

        Err(OcrError::RunFailed {
            command: "ocr".to_string(),
            message: format!("这一帧 OCR 失败（{}）", describe(&failures)),
        })
    }

    /// 识别一集的全部帧，返回与 `frames` 一一对应的结果。
    ///
    /// PaddleOCR 每次启动都要加载模型（实测约 10 秒），所以当前级别是它时整批交给
    /// 一次调用；整批失败按 [`OcrPipeline::recognize`] 同样的规则处理：未确认就降级
    /// 并让下一级逐帧重试，已确认或已是最后一级就整批记为失败。其他后端逐帧调用。
    ///
    /// `output_dir` 下放各后端的中间产物（`<帧名>.tsv`、`paddle/<帧名>_res.json`）。
    pub fn recognize_frames(
        &mut self,
        frames: &[PathBuf],
        output_dir: &Path,
        runner: &dyn CommandRunner,
    ) -> Vec<Result<Vec<OcrLine>, OcrError>> {
        if frames.is_empty() {
            return Vec::new();
        }
        let mut failures: Vec<(Backend, String)> = Vec::new();
        while self.chain[self.current].backend == Backend::PaddleOcr {
            let program = self.chain[self.current].program.clone();
            match super::paddle::recognize_batch(
                frames,
                &program,
                &output_dir.join("paddle"),
                runner,
            ) {
                Ok(results) => {
                    self.confirmed = true;
                    return results;
                }
                Err(error) => {
                    let message = error.to_string();
                    failures.push((Backend::PaddleOcr, message.clone()));
                    let next = self.current + 1;
                    if self.confirmed || next >= self.chain.len() {
                        let detail = describe(&failures);
                        return frames
                            .iter()
                            .map(|_| {
                                Err(OcrError::RunFailed {
                                    command: "ocr".to_string(),
                                    message: format!("整批 OCR 失败（{detail}）"),
                                })
                            })
                            .collect();
                    }
                    self.downgrades.push(Downgrade {
                        from: Backend::PaddleOcr,
                        to: self.chain[next].backend,
                        reason: message,
                    });
                    self.current = next;
                }
            }
        }
        frames
            .iter()
            .map(|frame| {
                let stem = frame.file_stem().unwrap_or_default();
                self.recognize(&RecognizeRequest::new(frame, output_dir.join(stem)), runner)
            })
            .collect()
    }
}

/// 「PaddleOCR：原因；tesseract：原因」——错误里列出试过的每一级。
fn describe(failures: &[(Backend, String)]) -> String {
    failures
        .iter()
        .map(|(backend, message)| format!("{}：{message}", backend.label()))
        .collect::<Vec<_>>()
        .join("；")
}

/// 某一级在自检环境里是否仍可用，可用则给出它的入口（系统 OCR 没有可执行文件）。
fn candidate_for(backend: Backend, env: &Environment) -> Option<Candidate> {
    let program = match backend {
        Backend::PaddleOcr => env.paddle_program.clone()?,
        Backend::RapidOcr => env.rapid_program.clone()?,
        // 系统 OCR 不需要可执行文件，入口留空。
        Backend::System => {
            if !env.system_available {
                return None;
            }
            PathBuf::new()
        }
        Backend::Tesseract => {
            let (path, has_chinese) = env.tesseract.clone()?;
            if !has_chinese {
                return None;
            }
            path
        }
    };
    Some(Candidate { backend, program })
}

/// 真正调用某一级。
fn run_backend(
    candidate: &Candidate,
    request: &RecognizeRequest,
    runner: &dyn CommandRunner,
) -> Result<Vec<OcrLine>, OcrError> {
    match candidate.backend {
        Backend::Tesseract => {
            if candidate.program.as_os_str().is_empty() {
                return Err(OcrError::ToolMissing("tesseract 路径缺失".to_string()));
            }
            let mut plan = RecognizePlan::new(&request.image, &request.output_base);
            plan.language = request.language.clone();
            plan.execute(runner, &candidate.program.display().to_string())
        }
        Backend::PaddleOcr => {
            if candidate.program.as_os_str().is_empty() {
                return Err(OcrError::ToolMissing("PaddleOCR 入口缺失".to_string()));
            }
            // 结果文件放在中间产物前缀的同级 paddle/ 目录，与整批识别一致。
            let save_dir = request
                .output_base
                .parent()
                .map_or_else(|| PathBuf::from("paddle"), |parent| parent.join("paddle"));
            super::paddle::recognize(&request.image, &candidate.program, &save_dir, runner)
        }
        Backend::System => super::system::recognize(&request.image, runner),
        Backend::RapidOcr => Err(OcrError::ToolMissing(
            "RapidOCR 后端尚未实现（本轮不做）".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr_backend::Environment;
    use dossier_runtime::{CommandError, CommandOutput, CommandSpec};
    use std::collections::{HashMap, VecDeque};
    use std::path::Path;
    use std::sync::Mutex;

    /// 按程序名分别排队返回结果的假执行器。
    ///
    /// `--version` 探测一律成功（tesseract 每次识别前都会探测一次），其余调用按
    /// 该程序的队列依次出结果；队列空了就返回失败，避免测试意外「成功」。
    #[derive(Default)]
    struct ScriptedRunner {
        queues: Mutex<HashMap<String, VecDeque<bool>>>,
        calls: Mutex<Vec<String>>,
    }

    impl ScriptedRunner {
        fn script(self, program: &str, results: &[bool]) -> Self {
            self.queues
                .lock()
                .unwrap()
                .insert(program.to_string(), results.iter().copied().collect());
            self
        }

        /// 某个程序被真正调用（不含 `--version`）的次数。
        fn calls_to(&self, program: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|p| p.as_str() == program)
                .count()
        }
    }

    impl CommandRunner for ScriptedRunner {
        fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, CommandError> {
            let name = Path::new(&spec.program)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if spec.args.iter().any(|a| a == "--version") {
                return Ok(CommandOutput::new(0, "", ""));
            }
            self.calls.lock().unwrap().push(name.clone());
            let ok = self
                .queues
                .lock()
                .unwrap()
                .get_mut(&name)
                .and_then(|queue| queue.pop_front())
                .unwrap_or(false);
            Ok(if ok {
                CommandOutput::new(0, "", "")
            } else {
                CommandOutput::new(1, "", "boom")
            })
        }
    }

    /// 每个测试一个独立目录，避免并行运行时互相覆盖。
    struct Workspace {
        dir: PathBuf,
    }

    impl Workspace {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("dossier-ocr-engine-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        /// 一个存在的文件，充当图片或可执行入口（前置检查只看文件是否存在）。
        fn file(&self, name: &str) -> PathBuf {
            let path = self.dir.join(name);
            std::fs::write(&path, crate::ocr::test_support::TINY_PNG).unwrap();
            path
        }

        /// 第 `index` 帧的请求；`with_tsv` 为真时预先放好 tesseract 的产物，
        /// 让 tesseract 这一帧「识别成功」。
        fn request(&self, index: usize, with_tsv: bool) -> RecognizeRequest {
            let image = self.file(&format!("frame_{index}.png"));
            let base = self.dir.join(format!("ocr_{index}"));
            if with_tsv {
                std::fs::write(
                    base.with_extension("tsv"),
                    "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
                     5\t1\t1\t1\t1\t1\t0\t0\t10\t10\t90\t字幕\n",
                )
                .unwrap();
            }
            RecognizeRequest::new(image, base)
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// PaddleOCR + tesseract 两级的候选链；两级都只通过假执行器控制，
    /// 不触碰系统 OCR，所以三个平台上的结果一致。
    fn paddle_then_tesseract(ws: &Workspace) -> OcrPipeline {
        let env = Environment {
            paddle_program: Some(ws.file("paddleocr")),
            rapid_program: None,
            system_available: false,
            tesseract: Some((ws.dir.join("tesseract"), true)),
        };
        let (selection, _) = crate::ocr_backend::select(&env);
        OcrPipeline::new(selection, &env).expect("应当建出管线")
    }

    #[test]
    fn pipeline_reports_the_selected_backend() {
        let env = Environment {
            tesseract: Some((PathBuf::from("/fake/tesseract"), true)),
            ..Default::default()
        };
        let (selection, _) = crate::ocr_backend::select(&env);
        let pipeline = OcrPipeline::new(selection, &env).expect("应当建出管线");
        assert_eq!(pipeline.backend(), Backend::Tesseract);
        assert!(pipeline.downgrades().is_empty());
    }

    #[test]
    fn no_selection_means_no_pipeline() {
        let env = Environment::default();
        let (selection, _) = crate::ocr_backend::select(&env);
        assert!(selection.is_none());
        assert!(OcrPipeline::new(selection, &env).is_none());
    }

    #[test]
    fn chain_contains_lower_backends_for_downgrade() {
        let ws = Workspace::new("chain");
        let pipeline = paddle_then_tesseract(&ws);
        assert_eq!(pipeline.backend(), Backend::PaddleOcr);
        let backends: Vec<_> = pipeline.chain.iter().map(|c| c.backend).collect();
        assert_eq!(backends, vec![Backend::PaddleOcr, Backend::Tesseract]);
    }

    /// 回归：第 1 帧所有级别都失败后，曾经越界导致后面每一帧都直接失败。
    #[test]
    fn a_frame_failing_everywhere_does_not_doom_later_frames() {
        let ws = Workspace::new("doom");
        let mut pipeline = paddle_then_tesseract(&ws);
        let runner = ScriptedRunner::default()
            .script("paddleocr", &[false])
            .script("tesseract", &[false, true]);

        // 第 1 帧：PaddleOCR 失败 → 降到 tesseract，tesseract 也失败。
        let error = pipeline
            .recognize(&ws.request(1, false), &runner)
            .unwrap_err()
            .to_string();
        assert!(error.contains("PaddleOCR"), "错误要列出每一级：{error}");
        assert!(error.contains("tesseract"), "错误要列出每一级：{error}");
        assert_eq!(pipeline.backend(), Backend::Tesseract);

        // 第 2 帧：仍然会被最后一级（tesseract）尝试，并且成功。
        let lines = pipeline.recognize(&ws.request(2, true), &runner).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(runner.calls_to("tesseract"), 2, "两帧都应调用 tesseract");
        assert_eq!(pipeline.downgrades().len(), 1, "降级只发生一次");
    }

    #[test]
    fn downgrade_happens_once_and_later_frames_stay_on_the_working_level() {
        let ws = Workspace::new("once");
        let mut pipeline = paddle_then_tesseract(&ws);
        let runner = ScriptedRunner::default()
            .script("paddleocr", &[false])
            .script("tesseract", &[true, true, true]);

        for index in 1..=3 {
            pipeline
                .recognize(&ws.request(index, true), &runner)
                .unwrap();
        }
        assert_eq!(
            pipeline.downgrades(),
            &[Downgrade {
                from: Backend::PaddleOcr,
                to: Backend::Tesseract,
                reason: pipeline.downgrades()[0].reason.clone(),
            }]
        );
        assert_eq!(
            runner.calls_to("paddleocr"),
            1,
            "降级后不再回头试 PaddleOCR"
        );
        assert_eq!(runner.calls_to("tesseract"), 3);
    }

    #[test]
    fn batch_failure_downgrades_once_and_the_next_level_runs_per_frame() {
        let ws = Workspace::new("batch-down");
        let mut pipeline = paddle_then_tesseract(&ws);
        // 整批 PaddleOCR 失败一次；tesseract 逐帧：第 1 帧成功、第 2 帧失败。
        let runner = ScriptedRunner::default()
            .script("paddleocr", &[false])
            .script("tesseract", &[true, false]);
        let frames = vec![ws.file("caption_0001.png"), ws.file("caption_0002.png")];
        let out = ws.dir.join("ocr");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(
            out.join("caption_0001.tsv"),
            "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
             5\t1\t1\t1\t1\t1\t0\t0\t10\t10\t90\t字幕\n",
        )
        .unwrap();

        let results = pipeline.recognize_frames(&frames, &out, &runner);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].as_ref().unwrap()[0].text, "字幕");
        assert!(results[1].is_err(), "已确认的 tesseract 单帧失败只算这一帧");
        assert_eq!(runner.calls_to("paddleocr"), 1, "整批只试一次");
        assert_eq!(pipeline.downgrades().len(), 1);
        assert_eq!(pipeline.backend(), Backend::Tesseract);
    }

    #[test]
    fn batch_failure_on_the_last_level_fails_every_frame_with_a_reason() {
        let ws = Workspace::new("batch-last");
        let env = Environment {
            paddle_program: Some(ws.file("paddleocr")),
            ..Default::default()
        };
        let (selection, _) = crate::ocr_backend::select(&env);
        let mut pipeline = OcrPipeline::new(selection, &env).unwrap();
        let runner = ScriptedRunner::default().script("paddleocr", &[false]);
        let frames = vec![ws.file("a.png"), ws.file("b.png")];
        let results = pipeline.recognize_frames(&frames, &ws.dir.join("ocr"), &runner);
        assert_eq!(results.len(), 2);
        for result in results {
            let message = result.unwrap_err().to_string();
            assert!(message.contains("PaddleOCR"), "{message}");
        }
        assert!(pipeline.downgrades().is_empty(), "只有一级时没有可降的");
    }

    #[test]
    fn no_frames_means_no_calls() {
        let ws = Workspace::new("batch-empty");
        let mut pipeline = paddle_then_tesseract(&ws);
        let runner = ScriptedRunner::default();
        assert!(pipeline.recognize_frames(&[], &ws.dir, &runner).is_empty());
        assert_eq!(runner.calls_to("paddleocr"), 0);
    }

    #[test]
    fn a_confirmed_level_failing_on_one_frame_is_not_a_downgrade() {
        let ws = Workspace::new("confirmed");
        // 只有 tesseract 一级时，它就是最后一级；先成功一次使其「已确认」。
        let env = Environment {
            tesseract: Some((ws.dir.join("tesseract"), true)),
            ..Default::default()
        };
        let (selection, _) = crate::ocr_backend::select(&env);
        let mut pipeline = OcrPipeline::new(selection, &env).unwrap();
        let runner = ScriptedRunner::default().script("tesseract", &[true, false, true]);

        assert!(pipeline.recognize(&ws.request(1, true), &runner).is_ok());
        assert!(pipeline.recognize(&ws.request(2, false), &runner).is_err());
        assert!(pipeline.recognize(&ws.request(3, true), &runner).is_ok());
        assert!(pipeline.downgrades().is_empty());
        assert!(pipeline.confirmed);
    }
}
