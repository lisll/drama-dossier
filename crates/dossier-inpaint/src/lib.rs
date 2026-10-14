//! dossier-inpaint — 硬字幕擦除管线（技术学习用途）
//!
//! 链路：`probe` → ROI/mask → `frames`（ffmpeg rawvideo）→ 文本检测 →
//! 时序修复（E2FGVI worker 或本地 mock）→ 合成回写 → 重新封装音轨。
//!
//! 合规边界：只用于处理使用者自有的素材（含自有素材的角标/台标清理），
//! 不提供 DRM 绕过能力，也不处理他人作品的版权标识。

pub mod detect;
pub mod erase;
pub mod frames;
pub mod inpaint;
pub mod mask;
pub mod patch;
pub mod pipeline;
pub mod probe;
pub mod verify;
pub mod worker;
