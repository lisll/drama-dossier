//! `assemble_package` 的安全测试：清单被改、签名换人、不是本机、服务端文件被改、
//! 分集被调换或修改、目录已存在、路径想写到包外面——每种都拒绝，而且不留半个目录。
//!
//! 清单在这里用测试私钥签（和服务端 `package_service.encode_manifest / sign_manifest` 同样的格式），
//! 本机文件放在临时目录里。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use dossier_core::{assemble_package, diff_packages, AssembleError, AssembleRequest};
use dossier_license::envelope::Purpose;
use dossier_license::TrustedKey;
use ed25519_dalek::{Signer as _, SigningKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const DEVICE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER_DEVICE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const COST_PDF: &str = "04_成本配置表/成本配置表_未签署.pdf";

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn trusted(signing: &SigningKey) -> TrustedKey {
    TrustedKey::from_bytes(signing.verifying_key().as_bytes()).unwrap()
}

/// 和服务端 `sign_manifest` 一样：`{"kid", "sig"}`，签名用途是报审包清单。
fn sign(signing: &SigningKey, manifest: &[u8]) -> Vec<u8> {
    let mut message = Purpose::PackageManifest.context().to_vec();
    message.extend_from_slice(manifest);
    let sig = URL_SAFE_NO_PAD.encode(signing.sign(&message).to_bytes());
    let kid = trusted(signing).kid().to_owned();
    serde_json::to_vec(&json!({ "kid": kid, "sig": sig })).unwrap()
}

/// 一部剧：两集、一张封面，服务端发回剧名和一份 PDF。
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    local: HashMap<String, PathBuf>,
    server: HashMap<String, Vec<u8>>,
    signing: SigningKey,
    copy_episodes: bool,
}

impl Fixture {
    fn new(copy_episodes: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let drama = root.join("测试剧");
        std::fs::create_dir_all(&drama).unwrap();
        let mut local = HashMap::new();
        for (name, content) in [
            ("第01集.mp4", b"episode one".as_slice()),
            ("第02集.mp4", b"episode two, longer".as_slice()),
            ("封面.jpg", b"cover".as_slice()),
        ] {
            let path = drama.join(name);
            std::fs::write(&path, content).unwrap();
            local.insert(name.to_owned(), path);
        }
        let server = HashMap::from([
            (
                "01_剧集信息/剧名.txt".to_owned(),
                "测试剧".as_bytes().to_vec(),
            ),
            (COST_PDF.to_owned(), b"%PDF-1.7 fake".to_vec()),
        ]);
        Self {
            _dir: dir,
            root,
            local,
            server,
            signing: key(7),
            copy_episodes,
        }
    }

    fn local_entry(&self, name: &str) -> (u64, String) {
        let bytes = std::fs::read(&self.local[name]).unwrap();
        (bytes.len() as u64, sha256(&bytes))
    }

    /// 按「提交那一刻」的本机文件拼清单。
    fn manifest(&self) -> Value {
        let mut files: Vec<Value> = self
            .server
            .iter()
            .map(|(path, bytes)| {
                json!({"path": path, "sizeBytes": bytes.len(), "sha256": sha256(bytes), "source": "server"})
            })
            .collect();
        files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
        let (size, hash) = self.local_entry("封面.jpg");
        files.push(json!({"path": "02_封面海报/封面.jpg", "sizeBytes": size, "sha256": hash, "source": "local"}));
        let mut references = Vec::new();
        for name in ["第01集.mp4", "第02集.mp4"] {
            let (size, hash) = self.local_entry(name);
            if self.copy_episodes {
                files.push(
                    json!({"path": format!("06_分集视频/{name}"), "sizeBytes": size,
                                  "sha256": hash, "source": "local"}),
                );
            } else {
                references.push(json!({"path": name, "sizeBytes": size, "sha256": hash}));
            }
        }
        json!({
            "schema": "drama-dossier/package@2",
            "packageId": "00000000-0000-4000-8000-0000000000bb",
            "generatedAt": "2026-01-01T00:00:00Z",
            "userId": "00000000-0000-4000-8000-000000000001",
            "deviceId": DEVICE,
            "dramaName": "测试剧",
            "aiGenerated": false,
            "copyEpisodes": self.copy_episodes,
            "signature": {"signer": "mock", "placeholder": true, "documents": [COST_PDF]},
            "files": files,
            "references": references,
        })
    }

    fn output(&self, name: &str) -> PathBuf {
        self.root.join("产物").join(name)
    }

    fn assemble(
        &self,
        manifest: &[u8],
        signature: &[u8],
        output: &Path,
        trusted_keys: &[TrustedKey],
        device: &str,
    ) -> Result<dossier_core::PackageResult, AssembleError> {
        let request = AssembleRequest {
            manifest,
            signature,
            server_files: &self.server,
            local_sources: &self.local,
            output_dir: output,
            trusted: trusted_keys,
            device_id: device,
        };
        assemble_package(&request, |_, _, _| {})
    }

    /// 用当前状态签一份清单，再组装。
    fn signed_assemble(&self, output: &Path) -> Result<dossier_core::PackageResult, AssembleError> {
        let manifest = serde_json::to_vec_pretty(&self.manifest()).unwrap();
        let signature = sign(&self.signing, &manifest);
        self.assemble(
            &manifest,
            &signature,
            output,
            &[trusted(&self.signing)],
            DEVICE,
        )
    }
}

/// 失败之后：正式目录不存在，同级也没有留下 `.xxx.partial-…` 临时目录。
fn assert_nothing_left(output: &Path) {
    assert!(!output.exists(), "失败后不应留下 {}", output.display());
    if let Some(parent) = output.parent().filter(|parent| parent.exists()) {
        let leftovers: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".partial-"))
            .collect();
        assert!(leftovers.is_empty(), "留下了临时目录：{leftovers:?}");
    }
}
// ---------- 正常情况 ----------

#[test]
fn assembles_and_existing_diff_can_read_it() {
    let fixture = Fixture::new(false);
    let first = fixture.output("报审包");
    let result = fixture.signed_assemble(&first).expect("正常组装");
    assert_eq!(result.package_dir, first);
    assert!(result.placeholder);
    assert_eq!(result.unsigned_documents, [COST_PDF]);
    assert_eq!(result.file_count, 3);
    assert_eq!(
        std::fs::read(first.join("02_封面海报/封面.jpg")).unwrap(),
        b"cover"
    );
    assert_eq!(
        std::fs::read(first.join(COST_PDF)).unwrap(),
        b"%PDF-1.7 fake"
    );
    // 不复制分集时，包里没有视频。
    assert!(!first.join("06_分集视频").exists());
    // manifest.json 是原样字节：签名针对的就是它。
    let manifest = std::fs::read(first.join("manifest.json")).unwrap();
    assert_eq!(
        manifest,
        serde_json::to_vec_pretty(&fixture.manifest()).unwrap()
    );
    assert!(first.join("manifest.sig").is_file());
    assert_nothing_left(&fixture.output("不存在"));

    // 现有的「差异对比」能读 package@2 的包。
    let second = fixture.output("报审包-第二次");
    fixture.signed_assemble(&second).expect("再组装一次");
    let diff = diff_packages(&first, &second).expect("差异对比能读新清单");
    assert!(!diff.has_changes());
    assert_eq!(diff.unchanged, 3);
}

#[test]
fn copies_episodes_when_asked() {
    let fixture = Fixture::new(true);
    let output = fixture.output("报审包");
    let mut seen = Vec::new();
    let manifest = serde_json::to_vec_pretty(&fixture.manifest()).unwrap();
    let signature = sign(&fixture.signing, &manifest);
    let request = AssembleRequest {
        manifest: &manifest,
        signature: &signature,
        server_files: &fixture.server,
        local_sources: &fixture.local,
        output_dir: &output,
        trusted: &[trusted(&fixture.signing)],
        device_id: DEVICE,
    };
    let result = assemble_package(&request, |done, total, _| seen.push((done, total))).unwrap();
    assert_eq!(result.file_count, 5);
    assert_eq!(
        std::fs::read(output.join("06_分集视频/第02集.mp4")).unwrap(),
        b"episode two, longer"
    );
    assert_eq!(seen, [(1, 3), (2, 3), (3, 3)], "每放一个本机文件报一次进度");
}

// ---------- 清单和签名 ----------

fn expect_manifest_error(result: Result<dossier_core::PackageResult, AssembleError>, code: &str) {
    match result {
        Err(AssembleError::Manifest(error)) => assert_eq!(error.code(), code),
        other => panic!("期望清单错误 {code}，实际 {other:?}"),
    }
}

#[test]
fn rejects_manifest_changed_by_one_byte() {
    let fixture = Fixture::new(false);
    let output = fixture.output("报审包");
    let manifest = serde_json::to_vec_pretty(&fixture.manifest()).unwrap();
    let signature = sign(&fixture.signing, &manifest);
    let mut tampered = manifest.clone();
    // 「看起来正常」的篡改：键按字母序排，第一个 false 是 aiGenerated；改成 true 后仍是合法 JSON，长度不变。
    let at = String::from_utf8_lossy(&tampered).find("false").unwrap();
    tampered.splice(at..at + 5, b"true ".iter().copied());
    let result = fixture.assemble(
        &tampered,
        &signature,
        &output,
        &[trusted(&fixture.signing)],
        DEVICE,
    );
    expect_manifest_error(result, "bad_signature");
    assert_nothing_left(&output);
}

#[test]
fn rejects_signature_from_someone_else() {
    let fixture = Fixture::new(false);
    let output = fixture.output("报审包");
    let manifest = serde_json::to_vec_pretty(&fixture.manifest()).unwrap();
    // 别人的私钥签的：内容完全正常，但不是受信的服务端。
    let signature = sign(&key(9), &manifest);
    let result = fixture.assemble(
        &manifest,
        &signature,
        &output,
        &[trusted(&fixture.signing)],
        DEVICE,
    );
    expect_manifest_error(result, "bad_signature");
    assert_nothing_left(&output);
}

#[test]
fn rejects_manifest_for_another_computer() {
    let fixture = Fixture::new(false);
    let output = fixture.output("报审包");
    let manifest = serde_json::to_vec_pretty(&fixture.manifest()).unwrap();
    let signature = sign(&fixture.signing, &manifest);
    let result = fixture.assemble(
        &manifest,
        &signature,
        &output,
        &[trusted(&fixture.signing)],
        OTHER_DEVICE,
    );
    expect_manifest_error(result, "wrong_device");
    assert_nothing_left(&output);
}

#[test]
fn rejects_path_escaping_the_package_even_if_signed() {
    // 私钥出事时的最后一道防线：签名对，但路径想写到包外面。
    let mut fixture = Fixture::new(false);
    fixture.server.insert("../逃出去.txt".into(), b"x".to_vec());
    let output = fixture.output("报审包");
    let result = fixture.signed_assemble(&output);
    expect_manifest_error(result, "bad_payload");
    assert_nothing_left(&output);
    assert!(!fixture.root.join("产物/逃出去.txt").exists());
}
// ---------- 服务端文件 ----------

/// 先按原内容签清单，再改服务端文件：模拟传输途中被改。
fn assemble_after(
    fixture: &Fixture,
    change: impl FnOnce(&mut HashMap<String, Vec<u8>>),
) -> (PathBuf, AssembleError) {
    let manifest = serde_json::to_vec_pretty(&fixture.manifest()).unwrap();
    let signature = sign(&fixture.signing, &manifest);
    let mut server = fixture.server.clone();
    change(&mut server);
    let output = fixture.output("报审包");
    let request = AssembleRequest {
        manifest: &manifest,
        signature: &signature,
        server_files: &server,
        local_sources: &fixture.local,
        output_dir: &output,
        trusted: &[trusted(&fixture.signing)],
        device_id: DEVICE,
    };
    let error = assemble_package(&request, |_, _, _| {}).expect_err("应当拒绝");
    (output, error)
}

#[test]
fn rejects_server_file_changed_in_transit() {
    let fixture = Fixture::new(false);
    let (output, error) = assemble_after(&fixture, |server| {
        server.insert(COST_PDF.into(), b"%PDF-1.7 evil".to_vec());
    });
    assert_eq!(error.code(), "server_file_mismatch", "{error}");
    assert_nothing_left(&output);
}

#[test]
fn rejects_missing_or_extra_server_file() {
    let fixture = Fixture::new(false);
    let (output, error) = assemble_after(&fixture, |server| {
        server.remove(COST_PDF);
    });
    assert_eq!(error.code(), "server_file_mismatch", "{error}");
    assert_nothing_left(&output);

    let (output, error) = assemble_after(&fixture, |server| {
        server.insert("07_多出来的/x.txt".into(), b"x".to_vec());
    });
    assert_eq!(error.code(), "server_file_mismatch", "{error}");
    assert_nothing_left(&output);
}

// ---------- 本机文件：提交之后被改动、调换、移走 ----------

/// 先按原内容签清单（提交那一刻），再动本机文件，然后组装。
fn assemble_after_local_change(
    copy_episodes: bool,
    change: impl FnOnce(&Fixture),
) -> (PathBuf, AssembleError) {
    let fixture = Fixture::new(copy_episodes);
    let manifest = serde_json::to_vec_pretty(&fixture.manifest()).unwrap();
    let signature = sign(&fixture.signing, &manifest);
    change(&fixture);
    let output = fixture.output("报审包");
    let error = fixture
        .assemble(
            &manifest,
            &signature,
            &output,
            &[trusted(&fixture.signing)],
            DEVICE,
        )
        .expect_err("应当拒绝");
    assert_nothing_left(&output);
    (output, error)
}

#[test]
fn rejects_episode_modified_after_submit() {
    for copy in [false, true] {
        let (_, error) = assemble_after_local_change(copy, |fixture| {
            std::fs::write(&fixture.local["第02集.mp4"], b"episode two, edited!").unwrap();
        });
        assert_eq!(error.code(), "local_changed", "copy={copy}: {error}");
        assert!(error.to_string().contains("第02集.mp4"), "{error}");
    }
}

#[test]
fn rejects_two_episodes_swapped() {
    for copy in [false, true] {
        let (_, error) = assemble_after_local_change(copy, |fixture| {
            let one = fixture.local["第01集.mp4"].clone();
            let two = fixture.local["第02集.mp4"].clone();
            let parked = one.with_extension("tmp");
            std::fs::rename(&one, &parked).unwrap();
            std::fs::rename(&two, &one).unwrap();
            std::fs::rename(&parked, &two).unwrap();
        });
        assert_eq!(error.code(), "local_changed", "copy={copy}: {error}");
    }
}

#[test]
fn rejects_episode_removed_after_submit() {
    let (_, error) = assemble_after_local_change(false, |fixture| {
        std::fs::remove_file(&fixture.local["第01集.mp4"]).unwrap();
    });
    assert_eq!(error.code(), "local_missing", "{error}");
}

#[test]
fn rejects_cover_modified_after_submit() {
    let (_, error) = assemble_after_local_change(false, |fixture| {
        std::fs::write(&fixture.local["封面.jpg"], b"COVER").unwrap();
    });
    assert_eq!(error.code(), "local_changed", "{error}");
}

// ---------- 目标目录 ----------

#[test]
fn rejects_existing_output_dir_without_touching_it() {
    let fixture = Fixture::new(false);
    let output = fixture.output("报审包");
    std::fs::create_dir_all(&output).unwrap();
    std::fs::write(output.join("旧文件.txt"), b"old").unwrap();
    let error = fixture.signed_assemble(&output).expect_err("应当拒绝");
    assert_eq!(error.code(), "already_exists", "{error}");
    let entries: Vec<_> = std::fs::read_dir(&output).unwrap().collect();
    assert_eq!(entries.len(), 1, "旧目录不应被改动");
    assert_eq!(std::fs::read(output.join("旧文件.txt")).unwrap(), b"old");
}

// ---------- 混合签章（M4b-2） ----------

/// 混合签章：清单里逐份写明签章方式时，`unsigned_documents` 只列占位章那几份。
///
/// 这一条守的是「不实陈述」：只上传了一份扫描件时，若把 `documents` 整个当成未签署
/// 的列表，界面会把已经盖了真章的文档也标成「未签署」；反过来若只写一个整包结论，
/// 使用者又会以为整包都盖好了。
#[test]
fn mixed_signing_lists_only_the_placeholder_documents() {
    let fixture = Fixture::new(false);
    let scanned = "04_成本配置表/成本配置表.pdf";
    // 两份文档的内容一致（都是同一段假 PDF），但签章方式不同。
    let content = b"%PDF-1.7 fake".to_vec();
    let size = content.len();
    let hash = sha256(&content);
    // 清单里两份文档：一份扫描件（无后缀）、一份占位章（有后缀）。
    // 基于 Fixture 的清单改：它已经带齐剧名、封面、分集等全部服务端文件，
    // 只把签章相关信息换掉，避免手写清单漏文件（那会撞上「清单里没有 xxx」的校验）。
    let mut manifest = fixture.manifest();
    manifest["signature"] = json!({
        "signer": "scan",
        "placeholder": true,
        "documents": [scanned, COST_PDF],
        "perDocument": [
            {"document": scanned, "signer": "scan"},
            {"document": COST_PDF, "signer": "mock"},
        ],
    });
    // 再加一份扫描件签章后的文档（无 `_未签署` 后缀）。
    if let Some(files) = manifest["files"].as_array_mut() {
        files.push(json!({
            "path": scanned, "sizeBytes": size, "sha256": hash, "source": "server"
        }));
    }
    let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
    let signature = sign(&fixture.signing, &bytes);
    let mut server = fixture.server.clone();
    server.insert(scanned.to_string(), content);
    let output = fixture.output("报审包-混合");
    let request = AssembleRequest {
        manifest: &bytes,
        signature: &signature,
        server_files: &server,
        local_sources: &fixture.local,
        output_dir: &output,
        trusted: &[trusted(&fixture.signing)],
        device_id: DEVICE,
    };
    let result = assemble_package(&request, |_, _, _| {}).expect("混合签章应当能组装");

    // 整包仍有未签署的文档。
    assert!(result.placeholder);
    // 只有占位章那份被列进未签署列表——扫描件那份不该被标成未签署。
    assert_eq!(result.unsigned_documents, [COST_PDF]);
    // 逐份签章方式如实带出来，界面据此逐条标色。
    assert_eq!(result.document_signers.len(), 2);
    let mut by_signer: Vec<(String, String)> = result
        .document_signers
        .iter()
        .map(|item| (item.document.clone(), item.signer.clone()))
        .collect();
    by_signer.sort();
    assert_eq!(
        by_signer,
        vec![
            (scanned.to_string(), "scan".to_string()),
            (COST_PDF.to_string(), "mock".to_string()),
        ]
    );
}

/// 旧版服务端不发 `perDocument`：退回「整包占位」的旧语义，不能让旧包验不过。
#[test]
fn manifests_without_per_document_still_assemble() {
    let fixture = Fixture::new(false);
    let result = fixture
        .signed_assemble(&fixture.output("报审包-旧格式"))
        .expect("旧格式清单应当能组装");
    assert!(result.placeholder);
    assert_eq!(result.unsigned_documents, [COST_PDF]);
    assert!(
        result.document_signers.is_empty(),
        "旧格式没有逐份信息，不该凭空造出来"
    );
}
