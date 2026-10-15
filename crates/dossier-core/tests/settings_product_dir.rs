//! `dossier_core::resolve_product_dir` 的跨 crate 契约。
//!
//! 这里不重复 `settings.rs` 里的单测，而是**换一个 crate 视角编译**：桌面端
//! （`src-tauri`）走的是 `dossier_core::resolve_product_dir` 这条公开路径，
//! 少一个 `pub use` 只有外部 crate 才看得见——本机编不了 `src-tauri`，
//! 所以由这个测试来兜住这条契约。
//!
//! 写成集成测试而不是 doctest 是有原因的：`tauri-build` 在 MSVC 下会往自己的
//! `OUT_DIR` 写一个空壳 `msvcrt.lib` 并把它加进原生库搜索路径，而配套的
//! `/NODEFAULTLIB:msvcrt.lib` 只作用于 `src-tauri` 自己的产物；doctest 的链接会先
//! 搜到这个空壳，于是报 `LNK4003` 加一串 `memcpy` 未解析（只影响 Windows CI）。
//! 细节见 `docs/DEVELOPMENT.md`「质量门槛」。

use std::path::{Path, PathBuf};

use dossier_core::resolve_product_dir;

/// 三层落点各走一遍：本次覆盖 → 默认输出目录 → 剧目目录内。
#[test]
fn public_path_follows_the_three_tiers() {
    // 1. 生成页/工具箱填的目录就是产物目录本身，不再拼接。
    assert_eq!(
        resolve_product_dir(
            Some(Path::new("/tmp/本次")),
            Some(Path::new("/tmp/产物根")),
            Path::new("/素材/剧A"),
            "剧A",
            "报审包",
        ),
        PathBuf::from("/tmp/本次"),
        "本次覆盖应当直接胜出"
    );

    // 2. 设置里的默认输出目录是「产物根目录」，按「根目录 / 剧名 / 产物名」落盘。
    assert_eq!(
        resolve_product_dir(
            None,
            Some(Path::new("/tmp/产物根")),
            Path::new("/素材/剧A"),
            "剧A",
            "报审包",
        ),
        PathBuf::from("/tmp/产物根/剧A/报审包"),
        "默认输出目录应当按剧名分目录"
    );

    // 3. 都没填：留在剧目目录内，与加这个设置之前的行为一致。
    assert_eq!(
        resolve_product_dir(None, None, Path::new("/素材/剧A"), "剧A", "报审包"),
        PathBuf::from("/素材/剧A/报审包"),
        "没填时应当退回剧目目录内"
    );
}
