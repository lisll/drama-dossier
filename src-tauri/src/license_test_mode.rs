//! TEST-MODE：离线测试版授权（一张通用测试卡密，不联网）。
//!
//! 只在 Cargo 开关 `license-test-mode` 打开时编译（当前默认打开）。测试通过后按
//! docs/LICENSING.md「删除测试版」整体删掉：本文件、`license-test-mode` 开关，
//! 以及所有标了 `// TEST-MODE` 的接入点。
//!
//! 激活流程：输入 [`TEST_CARD`] → 用内置测试私钥给本机机器码签一份试用凭证（30 天）
//! → 交给与正式版完全相同的 `State::accept` 验签、绑定机器、写入授权文件。
//! 所以签名、机器绑定、到期、防改校验都走正式代码，测试到的就是上线后真正运行的那部分。
//!
//! 注意：测试私钥就在本文件里，任何拿到源码的人都能签发测试凭证。
//! 测试版只能发给内部测试人员，不能当正式版发布。

use dossier_cardkey::{issue_token, normalize_card, public_key_b64, signing_key, Plan};

/// 通用测试卡密（展示形式）。
pub const TEST_CARD: &str = "DDK-TEST-CARD-2345-6789";
/// 测试凭证有效期：30 天。到期后再输一次测试卡密即可续用。
pub const TEST_DAYS: u64 = 30;
/// 测试版专用签名种子（与测试向量里公开的两把不同）。
const TEST_SEED: [u8; 32] = [
    0x18, 0xd9, 0xa7, 0x1d, 0xd8, 0x4a, 0x7d, 0x1a, 0xc8, 0xa9, 0x1c, 0x91, 0x38, 0x1f, 0xa6, 0xa7,
    0x1b, 0x4a, 0x46, 0xa1, 0x39, 0x40, 0x5a, 0x20, 0x62, 0x37, 0x4e, 0xb3, 0x02, 0x21, 0xd4, 0xe6,
];
/// 测试凭证里的授权编号；正式服务器从 1 开始递增，这里用一个不会撞上的大数便于辨认
const TEST_LID: u64 = 9_000_000_000_000;

/// 测试版信任的公钥（base64url）。
pub fn public_key() -> String {
    public_key_b64(&signing_key(TEST_SEED))
}

/// 用户输入是不是测试卡密（与正式卡密同样的规范化规则：忽略大小写、空格、短横线）。
pub fn is_test_card(input: &str) -> bool {
    normalize_card(input).as_deref() == Some(TEST_CARD)
}

/// 为本机签发一份测试凭证。
pub fn issue(machine_id: &str, now: u64) -> String {
    let until = now + TEST_DAYS * 86_400;
    issue_token(
        &signing_key(TEST_SEED),
        TEST_LID,
        machine_id,
        Plan::Trial,
        Some(until),
        now,
        until,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::license_state::State;
    use dossier_cardkey::{parse_key_list, LockReason, Verdict, TEST_VECTOR_KEYS};

    const MID: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const OTHER: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    const T0: u64 = 1_800_000_000;

    fn dir(name: &str) -> std::path::PathBuf {
        let p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../target/license-test-mode-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn card_matching_is_exact_but_forgiving_on_format() {
        assert_eq!(normalize_card(TEST_CARD).as_deref(), Some(TEST_CARD));
        for ok in [
            TEST_CARD,
            "ddk-test-card-2345-6789",
            "TEST CARD 2345 6789",
            " TESTCARD23456789 ",
        ] {
            assert!(is_test_card(ok), "{ok}");
        }
        for bad in [
            "DDK-TEST-CARD-2345-6788",
            "DDK-AAAA-BBBB-CCCC-DDDD",
            "",
            "TEST",
        ] {
            assert!(!is_test_card(bad), "{bad}");
        }
    }

    #[test]
    fn test_key_is_not_a_public_vector_key() {
        assert!(!TEST_VECTOR_KEYS.contains(&public_key().as_str()));
    }

    #[test]
    fn issued_token_activates_through_real_state_machine() {
        let d = dir("activate");
        let keys = parse_key_list(&[public_key().as_str()]).unwrap();
        let s = State::new(d.join("license.json"), Ok(MID.into()));
        let v = s.accept(issue(MID, T0), T0, Ok(&keys), true).unwrap();
        match &v {
            Verdict::Active(a) => {
                assert_eq!(a.plan, Plan::Trial);
                assert_eq!(a.until, Some(T0 + TEST_DAYS * 86_400));
            }
            other => panic!("{other:?}"),
        }
        // 30 天内有效，到期即锁
        assert!(s.evaluate(T0 + 29 * 86_400, Ok(&keys), true).0.is_active());
        assert_eq!(
            s.evaluate(T0 + 30 * 86_400, Ok(&keys), true).0,
            Verdict::Locked(LockReason::Expired)
        );
        // 授权文件拷到另一台电脑：校验码绑定机器码，无效
        let other = State::new(d.join("license.json"), Ok(OTHER.into()));
        assert_eq!(
            other.evaluate(T0 + 10, Ok(&keys), true).0,
            Verdict::Locked(LockReason::Malformed)
        );
        // 给别的机器签的测试凭证，本机不接受
        assert_eq!(
            s.accept(issue(OTHER, T0), T0, Ok(&keys), true),
            Err(LockReason::WrongMachine)
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn production_keys_reject_test_tokens() {
        // 正式版只信任 PRODUCTION_KEYS：测试凭证在正式版里验不过
        let stranger = parse_key_list(&["A6EHv_POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbg"]).unwrap();
        assert_eq!(
            dossier_cardkey::decide(&issue(MID, T0), MID, T0 + 1, 0, Ok(&stranger)),
            Verdict::Locked(LockReason::BadSignature)
        );
    }
}
