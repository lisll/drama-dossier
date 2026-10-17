//! 续跑校验 `recipe::verify_consumed` 的安全边界（M4b-1，计划第 5 节「配方」）。
//!
//! 续跑是唯一不联网就能开工的路径，所以这里逐项钉住：
//! - 别的任务的配方验不过（任务哈希绑定）；
//! - 换电脑的配方验不过（机器码绑定）；
//! - 签名、用途前缀、载荷任何一项不对都拒绝；
//! - 过期的配方**可以**走续跑（有效期只防囤配方，配方已在服务端消费掉了）；
//!   而首次开工的 `verify` 对同一个配方必须拒绝。
//!
//! 这里自己签配方：种子是写死的测试值，和共享测试向量的两个公开种子都不同。

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use dossier_license::keys::key_id;
use dossier_license::recipe::{self, RecipeError};
use dossier_license::version_task::verify_consumed;
use dossier_license::TrustedKey;
use ed25519_dalek::{Signer, SigningKey};

/// 配方的签名前缀（和授权凭证、报审包清单各不相同）。
const RECIPE_CONTEXT: &[u8] = b"drama-dossier/recipe/v1\n";
const LICENSE_CONTEXT: &[u8] = b"drama-dossier/license/v1\n";

const DEVICE: &str = "1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";
const OTHER_DEVICE: &str = "fedcba0987654321fedcba0987654321fedcba0987654321fedcba0987654321";
const TASK: &str = "aaaa567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";
const OTHER_TASK: &str = "bbbb567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";

const ISSUED_AT: u64 = 1_767_225_600;
/// 服务端的配方有效期是 24 小时。
const EXPIRES_AT: u64 = ISSUED_AT + 24 * 3600;

fn signing_key(fill: u8) -> SigningKey {
    SigningKey::from_bytes(&[fill; 32])
}

fn trusted(key: &SigningKey) -> Vec<TrustedKey> {
    vec![TrustedKey::from_bytes(&key.verifying_key().to_bytes()).unwrap()]
}

/// 用指定前缀和私钥把一段载荷签成 `载荷.签名`。
fn sign_with(context: &[u8], key: &SigningKey, payload: &str) -> String {
    let mut message = context.to_vec();
    message.extend_from_slice(payload.as_bytes());
    format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(payload.as_bytes()),
        URL_SAFE_NO_PAD.encode(key.sign(&message).to_bytes())
    )
}

/// 一份合法的配方载荷；`overrides` 里的键会替换掉同名字段（值是 JSON 片段原文）。
fn payload(key: &SigningKey, overrides: &[(&str, &str)]) -> String {
    let kid = key_id(&key.verifying_key().to_bytes());
    let mut fields: Vec<(&str, String)> = vec![
        ("v", "1".to_string()),
        ("kid", format!("\"{kid}\"")),
        ("rid", "\"r-1\"".to_string()),
        ("sub", "\"u-1\"".to_string()),
        ("dev", format!("\"{DEVICE}\"")),
        ("task", format!("\"{TASK}\"")),
        ("platform", "\"universal\"".to_string()),
        ("strength", "\"medium\"".to_string()),
        ("seed", "42".to_string()),
        (
            "params",
            r#"{"platform":{"width":1080,"height":1920,"fps":30,"video_bitrate_kbps":5000,"audio_bitrate_kbps":128,"max_duration_sec":900,"container":"mp4","video_codec":"h264","audio_codec":"aac"},"subtle":{"grain":4.0,"zoom_percent":2.0,"brightness":0.01,"saturation":1.0,"hue_degrees":1.0,"tempo":1.0,"pitch_semitones":0.1,"trim_head_frames":2,"trim_tail_frames":2},"quality":{"min_ssim":0.92,"min_psnr":35.0}}"#
                .to_string(),
        ),
        ("iat", ISSUED_AT.to_string()),
        ("exp", EXPIRES_AT.to_string()),
    ];
    for (name, value) in overrides {
        match fields.iter_mut().find(|(key, _)| key == name) {
            Some(slot) => slot.1 = (*value).to_string(),
            None => fields.push((name, (*value).to_string())),
        }
    }
    let body: Vec<String> = fields
        .iter()
        .map(|(name, value)| format!("\"{name}\":{value}"))
        .collect();
    format!("{{{}}}", body.join(","))
}

fn token(key: &SigningKey, overrides: &[(&str, &str)]) -> String {
    sign_with(RECIPE_CONTEXT, key, &payload(key, overrides))
}

#[test]
fn consumed_recipe_passes_for_its_own_device_and_task() {
    let key = signing_key(7);
    let recipe = verify_consumed(&token(&key, &[]), &trusted(&key), DEVICE, TASK)
        .expect("自己的机器、自己的任务，续跑要能过");
    assert_eq!(recipe.device_id, DEVICE);
    assert_eq!(recipe.task, TASK);
    assert_eq!(recipe.platform, "universal");
    assert_eq!(recipe.strength, "medium");
    assert_eq!(recipe.seed, 42);
    // 参数原样带回来：续跑必须用同一套参数，才能产出同一份结果。
    assert_eq!(
        recipe.params["quality"]["min_ssim"].as_f64(),
        Some(0.92),
        "画质下限要原样来自配方"
    );
}

#[test]
fn another_tasks_recipe_is_refused() {
    let key = signing_key(7);
    let token = token(&key, &[]);
    // 同一台电脑、同一个账号，但任务哈希不是这一个：换一集、改模板、换档位都会走到这里。
    assert_eq!(
        verify_consumed(&token, &trusted(&key), DEVICE, OTHER_TASK).err(),
        Some(RecipeError::WrongTask),
        "别的任务的配方不能用来续跑这个任务"
    );
}

#[test]
fn another_devices_recipe_is_refused() {
    let key = signing_key(7);
    // 配方签给另一台电脑：把任务目录整个拷到新机器上也开不了工。
    let token = token(&key, &[("dev", &format!("\"{OTHER_DEVICE}\""))]);
    assert_eq!(
        verify_consumed(&token, &trusted(&key), DEVICE, TASK).err(),
        Some(RecipeError::WrongDevice),
        "别的电脑的配方不能在这台电脑上续跑"
    );
}

#[test]
fn device_check_comes_before_task_check() {
    let key = signing_key(7);
    // 机器码和任务同时不对时，先报机器码——和模块注释里写的检查顺序一致。
    let token = token(&key, &[("dev", &format!("\"{OTHER_DEVICE}\""))]);
    assert_eq!(
        verify_consumed(&token, &trusted(&key), DEVICE, OTHER_TASK).err(),
        Some(RecipeError::WrongDevice)
    );
}

#[test]
fn expired_recipe_can_be_resumed_but_not_started() {
    let key = signing_key(7);
    let token = token(&key, &[]);
    let keys = trusted(&key);
    // 有效期之后：续跑放行（配方已经在服务端消费掉了，再限时间没有意义）。
    verify_consumed(&token, &keys, DEVICE, TASK).expect("过期的配方仍可续跑");
    // 但首次开工的 verify 必须拒绝，否则「囤配方」就成立了。
    assert_eq!(
        recipe::verify(&token, &keys, DEVICE, TASK, EXPIRES_AT).err(),
        Some(RecipeError::Expired),
        "首次开工不能用过期配方"
    );
    // 「当前时间等于 exp」就算过期（COMMERCE.md 3.6 写明的边界）。
    recipe::verify(&token, &keys, DEVICE, TASK, EXPIRES_AT - 1).expect("到期前一秒还能用");
}

#[test]
fn tampered_signature_and_payload_are_refused() {
    let key = signing_key(7);
    let keys = trusted(&key);

    // 改签名的最后一个字符。
    let mut token = token(&key, &[]);
    let last = token.pop().expect("签名非空");
    token.push(if last == 'A' { 'B' } else { 'A' });
    assert_eq!(
        verify_consumed(&token, &keys, DEVICE, TASK).err(),
        Some(RecipeError::BadSignature)
    );

    // 改载荷但留着原签名：把 medium 换成 heavy（想白拿更宽的画质下限）。
    let original = payload(&key, &[]);
    let forged = original.replace("\"medium\"", "\"heavy\"");
    assert_ne!(forged, original, "替换要真的生效");
    let signature = sign_with(RECIPE_CONTEXT, &key, &original);
    let signature = signature.split('.').nth(1).expect("取签名段").to_string();
    let token = format!("{}.{signature}", URL_SAFE_NO_PAD.encode(forged.as_bytes()));
    assert_eq!(
        verify_consumed(&token, &keys, DEVICE, TASK).err(),
        Some(RecipeError::BadSignature),
        "改了载荷就对不上原签名"
    );

    // 换一把私钥签（自己造的「服务端」）。
    let attacker = signing_key(9);
    let token = sign_with(RECIPE_CONTEXT, &attacker, &payload(&attacker, &[]));
    assert_eq!(
        verify_consumed(&token, &keys, DEVICE, TASK).err(),
        Some(RecipeError::BadSignature),
        "不是内置公钥签的配方一律不认"
    );
}

#[test]
fn license_context_signature_is_not_a_recipe() {
    let key = signing_key(7);
    // 同一份载荷、同一把私钥，只把签名前缀换成授权凭证的：必须验不过。
    let body = payload(&key, &[]);
    let token = sign_with(LICENSE_CONTEXT, &key, &body);
    assert_eq!(
        verify_consumed(&token, &trusted(&key), DEVICE, TASK).err(),
        Some(RecipeError::BadSignature),
        "用途前缀不同的签名不能互相冒充"
    );
    // 换回配方前缀就能过，说明上面失败的原因只是前缀。
    verify_consumed(
        &sign_with(RECIPE_CONTEXT, &key, &body),
        &trusted(&key),
        DEVICE,
        TASK,
    )
    .expect("配方前缀签的能过");
}

#[test]
fn malformed_tokens_are_refused() {
    let key = signing_key(7);
    let keys = trusted(&key);
    for bad in [
        "",
        ".",
        "onlypayload",
        "a.b.c",
        "!!!.!!!",
        "aGVsbG8.",
        ".aGVsbG8",
    ] {
        assert_eq!(
            verify_consumed(bad, &keys, DEVICE, TASK).err(),
            Some(RecipeError::Malformed),
            "格式不对的配方：{bad}"
        );
    }
}

#[test]
fn bad_payload_fields_are_refused() {
    let key = signing_key(7);
    let keys = trusted(&key);
    let kid = key_id(&key.verifying_key().to_bytes());
    let cases: Vec<(&str, Vec<(&str, String)>)> = vec![
        ("版本号不认识", vec![("v", "2".into())]),
        ("缺 kid", vec![("kid", "null".into())]),
        (
            "kid 不是签名那把钥匙",
            vec![("kid", "\"deadbeefdeadbeef\"".into())],
        ),
        // 平台只允许 universal：旧的按平台签发的配方（douyin 等）一律不认。
        ("旧平台取值 douyin", vec![("platform", "\"douyin\"".into())]),
        ("平台空串", vec![("platform", "\"\"".into())]),
        ("强度档不认识", vec![("strength", "\"extreme\"".into())]),
        ("缺 params", vec![("params", "null".into())]),
        ("params 不是对象", vec![("params", "[]".into())]),
        ("任务为空串", vec![("task", "\"\"".into())]),
        ("机器码为空串", vec![("dev", "\"\"".into())]),
        ("配方编号为空串", vec![("rid", "\"\"".into())]),
        ("账号为空串", vec![("sub", "\"\"".into())]),
        ("缺种子", vec![("seed", "null".into())]),
        ("种子是小数", vec![("seed", "4.2".into())]),
        (
            "有效期不晚于签发时间",
            vec![
                ("exp", ISSUED_AT.to_string()),
                ("iat", ISSUED_AT.to_string()),
            ],
        ),
        ("签发时间是负数", vec![("iat", "-1".into())]),
    ];
    for (name, overrides) in cases {
        let borrowed: Vec<(&str, &str)> = overrides
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect();
        let token = token(&key, &borrowed);
        assert_eq!(
            verify_consumed(&token, &keys, DEVICE, TASK).err(),
            Some(RecipeError::BadPayload),
            "{name} 应该被判为载荷无效"
        );
    }
    // 对照：kid 对得上、其余字段合法时能过，说明上面失败不是别的原因。
    let token = token(&key, &[("kid", &format!("\"{kid}\""))]);
    verify_consumed(&token, &keys, DEVICE, TASK).expect("对照用例要能过");
}

#[test]
fn empty_trusted_key_list_refuses_everything() {
    let key = signing_key(7);
    // 编译时没注入公钥（公钥列表为空）时，任何配方都验不过——不能退化成「不校验」。
    assert_eq!(
        verify_consumed(&token(&key, &[]), &[], DEVICE, TASK).err(),
        Some(RecipeError::BadSignature)
    );
}

#[test]
fn second_trusted_key_is_accepted_for_rotation() {
    let old = signing_key(7);
    let new = signing_key(8);
    let keys = vec![
        TrustedKey::from_bytes(&old.verifying_key().to_bytes()).unwrap(),
        TrustedKey::from_bytes(&new.verifying_key().to_bytes()).unwrap(),
    ];
    // 密钥轮换过渡期（M7）：列表里任意一把签的配方都能续跑。
    verify_consumed(&token(&old, &[]), &keys, DEVICE, TASK).expect("旧钥匙签的能过");
    verify_consumed(&token(&new, &[]), &keys, DEVICE, TASK).expect("新钥匙签的能过");
}
