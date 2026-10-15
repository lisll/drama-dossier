//! dossier-license — 桌面端的离线授权逻辑（纯逻辑，不联网）。
//!
//! - [`token`]：授权凭证验签。规则以服务端参考实现 `license_token.verify` 和
//!   测试向量 `cloud/tests/commerce/license_vectors_v1.json` 为准；
//! - [`keys`]：内置的受信公钥列表（为密钥轮换留位置）；
//! - [`clock`]：防时钟倒退（记住见过的最晚时间）；
//! - [`access`]：把凭证、时钟、联网结果合成「联网 / 离线 / 锁定」三档；
//! - [`machine`]：机器码（系统编号加盐做 SHA-256，原始编号不出本机）。
//!
//! 边界：这里的检查只能抬高门槛。改过的客户端可以删掉检查后自己编译，
//! 真正的保护在服务端（见总方案 3.3 节）。

pub mod access;
pub mod clock;
pub mod keys;
pub mod machine;
pub mod token;

pub use access::{decide, Access, Connectivity, LockReason, ServerEntitlement, Tier};
pub use clock::{ClockVerdict, ROLLBACK_TOLERANCE_SECS};
pub use keys::{trusted_keys, KeyError, TrustedKey};
pub use machine::{
    default_device_name, derive_device_id, display_code, read_device_id, MachineError,
    MachineIdentity, Platform,
};
pub use token::{verify, EntitlementKind, License, TokenError};

/// 当前 Unix 秒数。系统时间早于 1970 年时按 0 处理（只会让凭证显得更早到期，不会放宽检查）。
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}
