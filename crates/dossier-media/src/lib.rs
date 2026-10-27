//! dossier-media — 后期制作适配层（技术学习用途）
//!
//! 与 `dossier-inpaint`（擦除）互补：这里放**剪辑/后期**这一侧的能力，
//! 每个模块都遵守同一条项目铁律——
//!
//! 1. **不捆绑外部工具**：只调用用户环境里已有的 `ffmpeg` / `rubberband` / `demucs` /
//!    `whisper` / `tesseract`，缺了就把「缺什么、装什么」说清楚；
//! 2. **业务规则在库里，命令层极薄**：`plan_*` 负责把参数变成一条**可审阅、可复现**的
//!    命令（[`dossier_runtime::CommandSpec`]），`main.rs` 只做参数解析与打印；
//! 3. **可测性靠注入**：所有外部调用都走 [`dossier_runtime::CommandRunner`]，
//!    单测用 `FakeRunner` 回放，端到端用真进程；
//! 4. **不静默降级**：能力缺失或参数做不到（例如没有 rubberband 却要求「变速不变调」）
//!    一律显式报错，不用「听起来差不多」的实现糊过去。
//!
//! 模块：
//! - [`tools`]：外部工具盘点 + 能力标签（给云端模块注册表用）；
//! - [`render`]：合成渲染（文字/图片叠加、字幕烧录）；
//! - [`shot`]：镜头切点检测；
//! - [`timestretch`]：变速变调（Rubber Band 优先，`atempo` 降级要显式声明）；
//! - [`separate`]：音源分离（Demucs）；
//! - [`asr`]：语音识别转字幕（Whisper）；
//! - [`ocr`]：OCR 文本识别（Tesseract），补上「只有检测没有识别」的缺口；
//! - [`srt`]：字幕文件读写（ASR/OCR 的公共出口）。

pub mod asr;
pub mod ocr;
pub mod ocr_backend;
pub mod render;
pub mod separate;
pub mod shot;
pub mod srt;
pub mod timestretch;
pub mod tools;

pub use dossier_runtime::{CommandError, CommandOutput, CommandRunner, CommandSpec, SystemRunner};
pub use srt::{parse_srt, parse_timestamp, to_srt, SubtitleCue, SubtitleError};

/// 把一次失败的外部执行压成一行文本，用于放进错误信息。
///
/// 只取 stderr 的尾部：工具（ffmpeg/demucs/whisper）失败时真正的原因在最后几行，
/// 前面的滚动日志只会把错误信息淹掉。
pub fn failure_message(output: &CommandOutput) -> String {
    let text = if output.stderr.trim().is_empty() {
        output.stdout.as_str()
    } else {
        output.stderr.as_str()
    };
    let tail: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let start = tail.len().saturating_sub(3);
    let merged = tail[start..].join(" / ");
    if merged.is_empty() {
        format!("退出码 {}", output.code_label())
    } else {
        format!("退出码 {}：{merged}", output.code_label())
    }
}
