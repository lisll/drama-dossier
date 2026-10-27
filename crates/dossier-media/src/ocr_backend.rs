//! OCR 后端的按序选择与降级。
//!
//! 顺序固定为：PaddleOCR → RapidOCR → 系统自带 OCR → 手动安装的 tesseract。
//! 每一级都先做"可用性自检"，再决定是否采用；某一级不可用就自动落到下一级，
//! 并把原因记下来（日志与界面用），而不是直接报"OCR 失败"。
//!
//! 设计原则：
//! - **顺序是一处常量**：实测发现某级更强时只需调顺序，不动架构；
//! - **自检不装东西**：只探测"本机现在有没有"，安装动作仍由「一键安装内容模型」负责；
//! - **永远有兜底**：四级依次失败时，返回 `None`，调用方退化为"仅对白证据"，
//!   任务照样完成——OCR 是增强项，不是功能前提。

use std::path::{Path, PathBuf};

/// OCR 后端。顺序即优先级，见 [`ORDER`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Backend {
    /// ① PaddleOCR（中文画面文字效果最好；需要 Python + paddlepaddle）
    PaddleOcr,
    /// ② RapidOCR（ONNX，模型约 21 MB，无需 paddlepaddle 框架）
    RapidOcr,
    /// ③ 系统自带：Windows.Media.Ocr / macOS Vision
    System,
    /// ④ 手动安装的 tesseract（现状，作最后兜底）
    Tesseract,
}

impl Backend {
    /// 界面上显示的短名。
    pub fn label(self) -> &'static str {
        match self {
            Backend::PaddleOcr => "PaddleOCR",
            Backend::RapidOcr => "RapidOCR",
            Backend::System => "系统 OCR",
            Backend::Tesseract => "tesseract",
        }
    }
}

/// 降级链的顺序。**只改这里**就能调整优先级。
pub const ORDER: [Backend; 4] = [
    Backend::PaddleOcr,
    Backend::RapidOcr,
    Backend::System,
    Backend::Tesseract,
];

/// 一次自检看到的"本机现状"。
#[derive(Debug, Clone, Default)]
pub struct Environment {
    /// 找到的 PaddleOCR 入口（Python 模块或可执行文件）
    pub paddle_program: Option<PathBuf>,
    /// 找到的 RapidOCR 入口
    pub rapid_program: Option<PathBuf>,
    /// 系统 OCR 是否可用（Windows.Media.Ocr / macOS Vision 都是系统组件，
    /// 但 Windows 还额外需要中文 OCR 语言包，所以由调用方给出结论）
    pub system_available: bool,
    /// 找到的 tesseract 路径，以及它是否带 chi_sim
    pub tesseract: Option<(PathBuf, bool)>,
}

/// 一次选择的结果。
#[derive(Debug, Clone)]
pub struct Selection {
    /// 最终选用的后端
    pub backend: Backend,
    /// 对应的可执行文件（系统 OCR 为 `None`）
    pub program: Option<PathBuf>,
    /// 跳过的级别及原因，按顺序记录，供日志与界面使用
    pub skipped: Vec<(Backend, String)>,
}

/// 按 [`ORDER`] 逐个判断，返回第一个可用的后端。
///
/// 全部不可用时返回 `None`，并把每一级的跳过原因带出来。
pub fn select(env: &Environment) -> (Option<Selection>, Vec<(Backend, String)>) {
    let mut skipped = Vec::new();
    for backend in ORDER {
        match backend {
            Backend::PaddleOcr => match &env.paddle_program {
                Some(program) => {
                    return (
                        Some(Selection {
                            backend,
                            program: Some(program.clone()),
                            skipped,
                        }),
                        Vec::new(),
                    )
                }
                None => skipped.push((
                    backend,
                    "未安装 PaddleOCR（需要 Python 与 paddlepaddle）".to_string(),
                )),
            },
            Backend::RapidOcr => match &env.rapid_program {
                Some(program) => {
                    return (
                        Some(Selection {
                            backend,
                            program: Some(program.clone()),
                            skipped,
                        }),
                        Vec::new(),
                    )
                }
                None => skipped.push((backend, "未安装 RapidOCR（模型约 21 MB）".to_string())),
            },
            Backend::System => {
                if env.system_available {
                    return (
                        Some(Selection {
                            backend,
                            program: None,
                            skipped,
                        }),
                        Vec::new(),
                    );
                }
                // Windows 上没装中文 OCR 语言包也会走到这里，原因由调用方给出
                skipped.push((backend, "系统 OCR 不可用或缺少中文语言包".to_string()));
            }
            Backend::Tesseract => match &env.tesseract {
                Some((program, true)) => {
                    return (
                        Some(Selection {
                            backend,
                            program: Some(program.clone()),
                            skipped,
                        }),
                        Vec::new(),
                    )
                }
                Some((_, false)) => {
                    skipped.push((backend, "tesseract 缺少 chi_sim 中文语言包".to_string()))
                }
                None => skipped.push((backend, "未找到 tesseract".to_string())),
            },
        }
    }
    (None, skipped)
}

/// 把"没有任何后端可用"的原因拼成一句可展示的话（用于 `guidance`）。
pub fn unavailable_hint(skipped: &[(Backend, String)]) -> String {
    if skipped.is_empty() {
        return "未找到可用的 OCR 后端".to_string();
    }
    let detail = skipped
        .iter()
        .map(|(backend, reason)| format!("{}：{reason}", backend.label()))
        .collect::<Vec<_>>()
        .join("；");
    format!("OCR 未启用（材料将只依据对白）。{detail}")
}

/// 检查一个可能的 OCR 可执行文件是否真的能跑（`--version`/`--help` 秒级返回）。
pub fn program_works(program: &Path) -> bool {
    use dossier_runtime::CommandRunner;
    if !program.is_file() {
        return false;
    }
    let spec = dossier_runtime::CommandSpec::new(program.display().to_string())
        .arg("--version")
        .timeout(std::time::Duration::from_secs(10));
    dossier_runtime::SystemRunner::new()
        .run(&spec)
        .map(|output| output.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(name: &str) -> PathBuf {
        PathBuf::from(format!("/fake/{name}"))
    }

    #[test]
    fn prefers_paddle_then_rapid_then_system_then_tesseract() {
        // 四者都在：选 PaddleOCR
        let env = Environment {
            paddle_program: Some(path("paddle")),
            rapid_program: Some(path("rapid")),
            system_available: true,
            tesseract: Some((path("tesseract"), true)),
        };
        let (sel, _) = select(&env);
        assert_eq!(sel.unwrap().backend, Backend::PaddleOcr);

        // 没有 Paddle：落到 RapidOCR
        let env = Environment {
            paddle_program: None,
            ..env
        };
        let (sel, _) = select(&env);
        assert_eq!(sel.unwrap().backend, Backend::RapidOcr);

        // 前两者都没有：落到系统 OCR
        let env = Environment {
            paddle_program: None,
            rapid_program: None,
            system_available: true,
            tesseract: Some((path("tesseract"), true)),
        };
        let (sel, _) = select(&env);
        assert_eq!(sel.unwrap().backend, Backend::System);

        // 只剩 tesseract：用它
        let env = Environment {
            system_available: false,
            tesseract: Some((path("tesseract"), true)),
            ..Default::default()
        };
        let (sel, _) = select(&env);
        assert_eq!(sel.unwrap().backend, Backend::Tesseract);
    }

    #[test]
    fn skipped_reasons_are_kept_in_order() {
        let env = Environment {
            tesseract: Some((path("tesseract"), true)),
            ..Default::default()
        };
        let (sel, _) = select(&env);
        let sel = sel.unwrap();
        assert_eq!(sel.backend, Backend::Tesseract);
        // 前面三级都应留下原因
        let reasons: Vec<Backend> = sel.skipped.iter().map(|(b, _)| *b).collect();
        assert_eq!(
            reasons,
            vec![Backend::PaddleOcr, Backend::RapidOcr, Backend::System]
        );
    }

    #[test]
    fn tesseract_without_chinese_is_skipped() {
        let env = Environment {
            tesseract: Some((path("tesseract"), false)),
            ..Default::default()
        };
        let (sel, skipped) = select(&env);
        assert!(sel.is_none(), "缺 chi_sim 时不能选 tesseract");
        assert_eq!(skipped.len(), 4);
        assert!(skipped
            .iter()
            .any(|(b, r)| *b == Backend::Tesseract && r.contains("chi_sim")));
    }

    #[test]
    fn nothing_available_returns_none_with_hint() {
        let env = Environment::default();
        let (sel, skipped) = select(&env);
        assert!(sel.is_none());
        let hint = unavailable_hint(&skipped);
        assert!(hint.contains("只依据对白"), "提示要说清后果：{hint}");
        assert!(hint.contains("PaddleOCR") && hint.contains("tesseract"));
    }

    #[test]
    fn missing_file_never_counts_as_available() {
        assert!(!program_works(Path::new("/definitely/not/here")));
    }
}
