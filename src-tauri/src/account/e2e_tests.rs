//! 端到端：桌面端的会话走真实 HTTP，连本机真实的 commerce 服务（PostgreSQL + Mailpit）。
//!
//! 默认忽略。跑法见 `docs/DEVELOPMENT.md`「桌面端账号与授权」第 4 节：
//! 需要 `DOSSIER_E2E_BASE`（服务端地址）、`DOSSIER_E2E_MAILPIT`（Mailpit 网页地址）、
//! `DOSSIER_E2E_PUBKEY`（服务端 keygen 打印的 public_key）、`DOSSIER_E2E_CARD`（一张没用过的月卡）。
//!
//! 只在 Linux 上跑：存储用的是真实的文件存储（`FileStore`），放在临时目录里；
//! Windows / macOS 上 `PlatformStore` 是系统钥匙串，测试不该往里写。

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use dossier_core::{
    assemble_package, diff_packages, hash_file, AssembleError, AssembleRequest, PackageResult,
};
use dossier_license::{
    derive_device_id, unix_now, LockReason, MachineIdentity, Platform, TrustedKey,
};
use serde_json::{json, Value};

use super::api::{HttpTransport, PackageResponse};
use super::session::{Session, Setup};
use super::store::{self, CredentialStore, PlatformStore};

const PASSWORD: &str = "correct-horse-1";
/// 没人监听的本机端口：模拟「服务端停了」。
const DEAD_BASE: &str = "http://127.0.0.1:9";

type Live = Session<HttpTransport, PlatformStore>;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("端到端测试需要环境变量 {name}"))
}

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

/// 一台「电脑」：固定的机器码 + 自己的临时存储目录。
struct Machine {
    setup: Setup,
    dir: PathBuf,
}

impl Machine {
    fn new(seed: u128) -> Self {
        let raw = format!("{:032x}", seed | 1);
        let key = TrustedKey::from_b64url(&env("DOSSIER_E2E_PUBKEY")).expect("服务端公钥格式正确");
        let dir = std::env::temp_dir().join(format!("dossier-e2e-{seed:x}"));
        Self {
            setup: Setup {
                identity: MachineIdentity {
                    platform: Platform::Linux,
                    device_id: derive_device_id(Platform::Linux, &raw),
                },
                keys: vec![key],
                app_version: "0.2.1".into(),
            },
            dir,
        }
    }

    fn session(&self, base: &str, now: fn() -> u64) -> Live {
        Session::new(
            HttpTransport::with_base(base),
            PlatformStore::open(self.dir.clone()),
            Ok(self.setup.clone()),
            now,
        )
    }

    fn record(&self, name: &str) -> Option<String> {
        PlatformStore::open(self.dir.clone()).load(name).unwrap()
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn two_hours_ago() -> u64 {
    unix_now() - 2 * 3600
}

/// 从 Mailpit 取最近一封发给 `to` 的验证码。
async fn mailed_code(to: &str) -> String {
    let mailpit = env("DOSSIER_E2E_MAILPIT");
    let client = reqwest::Client::new();
    let query = format!("to%3A{}", to.replace('@', "%40"));
    for _ in 0..50 {
        let found: Value = client
            .get(format!("{mailpit}/api/v1/search?query={query}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if let Some(id) = found["messages"][0]["ID"].as_str() {
            let message: Value = client
                .get(format!("{mailpit}/api/v1/message/{id}"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let text = message["Text"].as_str().unwrap_or_default();
            let (_, rest) = text.split_once("验证码是：").expect("邮件正文里有验证码");
            return rest.chars().take(6).collect();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("10 秒内 Mailpit 没有收到发给 {to} 的邮件");
}

#[tokio::test]
#[ignore = "需要本机真实的 commerce 服务和 Mailpit，见 docs/DEVELOPMENT.md"]
async fn register_redeem_kick_export_delete() {
    let base = env("DOSSIER_E2E_BASE");
    let email = format!("e2e-{:x}@example.test", unique() & 0xffff_ffff);
    let first = Machine::new(unique());
    let second = Machine::new(unique().rotate_left(17));
    assert_ne!(
        first.setup.identity.device_id,
        second.setup.identity.device_id
    );

    // 1. 第一台电脑：注册（验证码从 Mailpit 取）→ 自动绑定，没有使用权。
    let s1 = first.session(&base, unix_now);
    s1.restore().await;
    s1.send_code(&email, "register").await.expect("发验证码");
    let code = mailed_code(&email).await;
    s1.register(&email, &code, PASSWORD, true)
        .await
        .expect("注册成功");
    assert_eq!(s1.status().lock_reason, Some("no_entitlement"));
    assert!(first.record(store::SESSION).is_some());

    // 2. 兑换月卡：立即刷新授权，联网档，本地凭证里也是月卡。
    s1.redeem(&env("DOSSIER_E2E_CARD")).await.expect("兑换成功");
    let status = s1.status();
    assert_eq!(status.tier, "online", "{status:?}");
    assert_eq!(status.entitlement_kind, Some("timed"));
    assert!(s1.check_paid().await.is_ok());

    // 3. 第二台电脑登录同一个账号：名额满了，等用户先解绑一台。
    let s2 = second.session(&base, unix_now);
    s2.restore().await;
    s2.login(&email, PASSWORD).await.expect("登录成功");
    let status = s2.status();
    assert_eq!(status.lock_reason, Some("needs_activation"));
    let pending = status.pending_activation.expect("带上了设备列表");
    assert_eq!(pending["devices"].as_array().unwrap().len(), 1);

    // 4. 在第二台上踢掉第一台：第二台自动绑定上，进入联网档。
    let devices = s2.list_devices().await.expect("设备列表");
    let other = devices
        .devices
        .iter()
        .find(|device| !device.current)
        .expect("第一台电脑");
    s2.unbind(&other.id).await.expect("解绑成功");
    let status = s2.status();
    assert_eq!(status.tier, "online", "{status:?}");
    assert_eq!(status.entitlement_kind, Some("timed"));

    // 5. 第一台刷新失败：会话已被吊销，退出登录并清掉本机记录。
    s1.sync().await;
    let status = s1.status();
    assert!(!status.logged_in);
    assert_eq!(
        status.signed_out_reason.as_deref(),
        Some(LockReason::DeviceUnbound.message())
    );
    assert_eq!(first.record(store::SESSION), None);
    assert_eq!(first.record(store::LICENSE), None);

    // 6. 断网（服务端停了）：第二台重启后用本地凭证进入离线档；把时间往回调 2 小时则判时钟倒退。
    let offline = second.session(DEAD_BASE, unix_now);
    offline.restore().await;
    assert_eq!(offline.status().tier, "offline");
    assert!(offline.check_local().is_ok());
    assert!(offline.check_paid().await.is_err());
    let rolled_back = second.session(DEAD_BASE, two_hours_ago);
    rolled_back.restore().await;
    assert_eq!(rolled_back.status().lock_reason, Some("clock_rollback"));

    // 7. 导出：两台电脑的绑定记录（含被踢掉的那台）、一条兑换记录。
    let data = s2.export().await.expect("导出成功");
    assert_eq!(data["account"]["email"], email);
    assert_eq!(data["devices"].as_array().unwrap().len(), 2);
    assert!(data["devices"]
        .as_array()
        .unwrap()
        .iter()
        .any(|device| device["unbind_reason"] == "user"));
    assert_eq!(data["redemptions"].as_array().unwrap().len(), 1);
    let text = data.to_string();
    assert!(
        !text.contains(&first.setup.identity.device_id),
        "导出不含机器码摘要"
    );

    // 8. 注销：本机两条记录清空；之后用同一个邮箱和密码登录失败。
    s2.delete_account(PASSWORD).await.expect("注销成功");
    assert!(!s2.status().logged_in);
    assert_eq!(second.record(store::SESSION), None);
    assert_eq!(second.record(store::LICENSE), None);
    let third = Machine::new(unique().rotate_left(41));
    let s3 = third.session(&base, unix_now);
    s3.restore().await;
    let error = s3.login(&email, PASSWORD).await.unwrap_err();
    assert_eq!(error.code(), "invalid_credentials");
}

/// 一部临时样例剧：两集（内容随便写，服务端只认校验值），用完删掉。
struct SampleDrama {
    dir: PathBuf,
    episodes: Vec<(String, PathBuf)>,
}

impl SampleDrama {
    fn new(seed: u128) -> Self {
        let dir = std::env::temp_dir().join(format!("dossier-e2e-drama-{seed:x}"));
        std::fs::create_dir_all(&dir).unwrap();
        let episodes = ["第01集.mp4", "第02集.mp4"]
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let path = dir.join(name);
                std::fs::write(&path, format!("episode-{index}-{seed:x}").repeat(1000)).unwrap();
                (name.to_string(), path)
            })
            .collect();
        Self { dir, episodes }
    }

    fn local_sources(&self) -> HashMap<String, PathBuf> {
        self.episodes.iter().cloned().collect()
    }

    /// `POST /packages` 的请求体：和 `package::prepare` 拼的一样，校验值现算。
    fn body(&self, device_id: &str) -> Value {
        let episodes: Vec<Value> = self
            .episodes
            .iter()
            .enumerate()
            .map(|(index, (name, path))| {
                let (size, sha256) = hash_file(path).unwrap();
                json!({
                    "file_name": name,
                    "episode_number": index + 1,
                    "size_bytes": size,
                    "sha256": sha256,
                    "spec": {
                        "duration_sec": 60.0, "width": 1080, "height": 1920,
                        "video_codec": "h264", "audio_codec": "aac", "fps": 30.0,
                        "bitrate_bps": 4_000_000,
                    },
                })
            })
            .collect();
        json!({
            "device_id": device_id,
            "drama_name": "端到端样例剧",
            "ai_generated": false,
            "copy_episodes": true,
            "cost_sheet": null,
            "synopsis": null,
            "cover": null,
            "episodes": episodes,
            "signer": "mock",
        })
    }
}

impl Drop for SampleDrama {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn decode_server_files(response: &PackageResponse) -> HashMap<String, Vec<u8>> {
    response
        .files
        .iter()
        .map(|file| {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(file.content.as_bytes())
                .expect("服务端发回的是合法 base64");
            (file.path.clone(), bytes)
        })
        .collect()
}

/// 用服务端的回复在 `output` 组装一次报审包。
fn assemble(
    response: &PackageResponse,
    manifest: &[u8],
    sources: &HashMap<String, PathBuf>,
    output: &Path,
    keys: &[TrustedKey],
    device_id: &str,
) -> Result<PackageResult, AssembleError> {
    let server_files = decode_server_files(response);
    assemble_package(
        &AssembleRequest {
            manifest,
            signature: response.signature.as_bytes(),
            server_files: &server_files,
            local_sources: sources,
            output_dir: output,
            trusted: keys,
            device_id,
        },
        |_, _, _| {},
    )
}

/// 要第二张没用过的月卡 `DOSSIER_E2E_PACKAGE_CARD`（第一张在上面的测试里兑换掉了）。
#[tokio::test]
#[ignore = "需要本机真实的 commerce 服务、Mailpit 和 target/debug/dossier-artifacts，见 docs/DEVELOPMENT.md"]
async fn package_and_recipe_against_real_server() {
    let base = env("DOSSIER_E2E_BASE");
    let email = format!("e2e-pkg-{:x}@example.test", unique() & 0xffff_ffff);
    let machine = Machine::new(unique().rotate_left(7));
    let drama = SampleDrama::new(unique());

    // 1. 注册 → 兑换：这台电脑绑定、有使用权。
    let session = machine.session(&base, unix_now);
    session.restore().await;
    session
        .send_code(&email, "register")
        .await
        .expect("发验证码");
    let code = mailed_code(&email).await;
    session
        .register(&email, &code, PASSWORD, true)
        .await
        .expect("注册成功");
    session
        .redeem(&env("DOSSIER_E2E_PACKAGE_CARD"))
        .await
        .expect("兑换成功");
    let (device_id, keys) = session.signing_context().expect("有机器码和公钥");

    // 2. 服务端生成文档：只发校验值，不发视频。
    let body = drama.body(&device_id);
    assert!(
        !body.to_string().contains("episode-0-"),
        "请求里没有视频内容"
    );
    let response = session.create_package(body).await.expect("服务端生成成功");
    let manifest = response.manifest.as_bytes().to_vec();
    let sources = drama.local_sources();

    // 3. 正常组装：清单原样写回，两集都复制进包，差异对比能读。
    let good = drama.dir.join("报审包");
    let result =
        assemble(&response, &manifest, &sources, &good, &keys, &device_id).expect("组装成功");
    assert!(result.file_count >= 2, "{result:?}");
    assert_eq!(std::fs::read(good.join("manifest.json")).unwrap(), manifest);
    for (name, _) in &drama.episodes {
        assert!(good.join("06_分集视频").join(name).is_file(), "{name}");
    }
    assert!(!diff_packages(&good, &good).unwrap().has_changes());

    // 4. 清单被改一个字节：验签失败，不留目录。
    let mut tampered = manifest.clone();
    let at = tampered
        .iter()
        .position(|&b| b == b'1')
        .expect("清单里有数字");
    tampered[at] = b'2';
    let out = drama.dir.join("报审包-篡改清单");
    let error = assemble(&response, &tampered, &sources, &out, &keys, &device_id).unwrap_err();
    assert_eq!(error.code(), "manifest_invalid", "{error}");
    assert!(!out.exists());

    // 5. 清单是别的电脑的：拒绝。
    let other = Machine::new(unique().rotate_left(29));
    let out = drama.dir.join("报审包-别的电脑");
    let error = assemble(
        &response,
        &manifest,
        &sources,
        &out,
        &keys,
        &other.setup.identity.device_id,
    )
    .unwrap_err();
    assert_eq!(error.code(), "manifest_invalid", "{error}");
    assert!(!out.exists());

    // 6. 两集调换（文件名对调）：内容和提交时的校验值对不上，拒绝，不留目录。
    let mut swapped = sources.clone();
    let first = swapped["第01集.mp4"].clone();
    let second = swapped["第02集.mp4"].clone();
    swapped.insert("第01集.mp4".into(), second);
    swapped.insert("第02集.mp4".into(), first);
    let out = drama.dir.join("报审包-调换");
    let error = assemble(&response, &manifest, &swapped, &out, &keys, &device_id).unwrap_err();
    assert_eq!(error.code(), "local_changed", "{error}");
    assert!(!out.exists());

    // 7. 提交后改了一集：拒绝。
    std::fs::write(&drama.episodes[1].1, b"changed after submit").unwrap();
    let out = drama.dir.join("报审包-改了一集");
    let error = assemble(&response, &manifest, &sources, &out, &keys, &device_id).unwrap_err();
    assert_eq!(error.code(), "local_changed", "{error}");
    assert!(!out.exists());

    // 8. 配方：签发后本机验签通过；消费一次成功，第二次被拒。
    let task = "ab".repeat(32);
    let recipe = session
        .issue_recipe(&task, "douyin", "medium")
        .await
        .expect("签发配方");
    assert_eq!(recipe.device_id, device_id);
    assert_eq!(recipe.task, task);
    session.consume_recipe(&recipe).await.expect("第一次消费");
    let error = session.consume_recipe(&recipe).await.unwrap_err();
    assert_eq!(error.code(), "recipe_consumed", "{error:?}");

    // 9. 清理：注销账号（服务端连同报审包、配方记录一起删）。
    session.delete_account(PASSWORD).await.expect("注销成功");
}
