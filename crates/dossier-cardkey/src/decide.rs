//! 使用权判定：把「凭证 + 本机机器码 + 当前时间 + 见过的最晚时间」压成一个结论。

use crate::clock::{check_clock, ClockVerdict};
use crate::keys::{KeyError, TrustedKey};
use crate::token::{verify_token, Plan, TokenError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockReason {
    /// 没有授权文件
    NotActivated,
    /// 公钥未配置 / 配置错误（构建问题，绝不放行）
    NotConfigured,
    /// 机器码读不到
    MachineUnavailable,
    Malformed,
    BadSignature,
    BadPayload,
    WrongMachine,
    ClockRollback,
    Expired,
    /// 金丝雀自检失败：验签逻辑被篡改
    Tampered,
    /// 服务器明确拒绝（作废 / 到期 / 凭证无效）
    Revoked,
    /// 授权状态长时间写不进磁盘（文件被设为只读等）
    Unpersisted,
    /// 授权文件暂时读不到（被占用、权限问题）；下次检查会自动重试
    Unreadable,
}

impl LockReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::NotActivated => "not_activated",
            Self::NotConfigured => "not_configured",
            Self::MachineUnavailable => "machine_unavailable",
            Self::Malformed => "malformed",
            Self::BadSignature => "bad_signature",
            Self::BadPayload => "bad_payload",
            Self::WrongMachine => "wrong_machine",
            Self::ClockRollback => "clock_rollback",
            Self::Expired => "expired",
            Self::Tampered => "tampered",
            Self::Revoked => "revoked",
            Self::Unpersisted => "unpersisted",
            Self::Unreadable => "unreadable",
        }
    }

    /// 给用户看的一句话。刻意不暴露内部细节。
    pub fn message(self) -> &'static str {
        match self {
            Self::NotActivated => "软件尚未激活，请输入卡密完成激活。",
            Self::NotConfigured => "授权组件未正确配置，请联系客服获取正式版本。",
            Self::MachineUnavailable => "无法读取本机标识，请联系客服。",
            Self::Malformed | Self::BadSignature | Self::BadPayload | Self::Tampered => {
                "授权信息无效，请重新激活。"
            }
            Self::WrongMachine => "授权与本机不符，请在本机重新激活或联系客服迁移。",
            Self::ClockRollback => "检测到系统时间异常，请校正系统时间后联网重试。",
            Self::Expired => "授权已过期或长时间未联网续期，请联网后重试，或输入新的卡密。",
            Self::Revoked => "授权已失效，请输入新的卡密或联系客服。",
            Self::Unpersisted => "无法保存授权状态，请检查授权文件所在目录是否可写。",
            Self::Unreadable => {
                "暂时无法读取授权文件，请稍后重试；若持续出现请检查杀毒软件或文件权限。"
            }
        }
    }
}

impl From<TokenError> for LockReason {
    fn from(e: TokenError) -> Self {
        match e {
            TokenError::Malformed => Self::Malformed,
            TokenError::BadSignature => Self::BadSignature,
            TokenError::BadPayload(_) => Self::BadPayload,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Active {
    pub plan: Plan,
    pub until: Option<u64>,
    pub exp: u64,
    pub iat: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Active(Active),
    Locked(LockReason),
}

impl Verdict {
    pub fn is_active(&self) -> bool {
        matches!(self, Verdict::Active(_))
    }
}

/// 判定顺序：格式 → 签名 → 载荷 → 机器码 → 时钟 → 有效期。任一步失败即锁定。
pub fn decide(
    token: &str,
    machine_id: &str,
    now: u64,
    last_seen: u64,
    keys: Result<&[TrustedKey], KeyError>,
) -> Verdict {
    let keys = match keys {
        Ok(k) if !k.is_empty() => k,
        _ => return Verdict::Locked(LockReason::NotConfigured),
    };
    let claims = match verify_token(token, keys) {
        Ok(c) => c,
        Err(e) => return Verdict::Locked(e.into()),
    };
    if claims.mid != machine_id {
        return Verdict::Locked(LockReason::WrongMachine);
    }
    if check_clock(now, last_seen, claims.iat) == ClockVerdict::Rollback {
        return Verdict::Locked(LockReason::ClockRollback);
    }
    if now >= claims.exp || claims.until.is_some_and(|u| now >= u) {
        return Verdict::Locked(LockReason::Expired);
    }
    Verdict::Active(Active {
        plan: claims.plan,
        until: claims.until,
        exp: claims.exp,
        iat: claims.iat,
    })
}
