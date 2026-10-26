//! 剧备授权凭证：离线验签、机器码、时钟防回拨与使用权判定。
//!
//! 协议与服务端 `license-server/src/dd_license/token.py` 一致，两端由
//! `tests/vectors_v1.json` 逐条对拍。本 crate 是纯逻辑：不依赖 Tauri、不发网络请求，
//! 机器码读取之外不碰文件系统。

pub mod card;
pub mod clock;
pub mod decide;
pub mod keys;
pub mod machine;
pub mod token;

pub use card::{normalize_card, CARD_ALPHABET};
pub use clock::{check_clock, next_last_seen, ClockVerdict, ROLLBACK_TOLERANCE_SECS};
pub use decide::{decide, Active, LockReason, Verdict};
pub use keys::{parse_key_list, production_keys, KeyError, TrustedKey};
pub use machine::{derive_machine_id, display_code, read_machine_id, MachineError, Platform};
pub use token::{verify_token, Claims, Plan, TokenError, SIGNING_CONTEXT};
