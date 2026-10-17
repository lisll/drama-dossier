"""POST /packages 的安全测试：只有登录、绑定、有使用权的本机能生成；PDF 盖的是占位章；
清单签名能验过；服务端只记元数据，审计日志里没有剧名、公司名、联系方式原文；扫描件替换对应页面。

需要 `dossier-artifacts`：`cargo build -p dossier-core --bin dossier-artifacts`
（查找顺序见 drama_cloud/toolchain.py）。找不到时这些测试会失败而不是跳过。
"""

from __future__ import annotations

import base64
import hashlib
import io
import json

import pytest
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
from PIL import Image
from pypdf import PdfReader

from drama_cloud.commerce import credits, entitlements, license_token as lt, package_service

from .conftest import activate, auth, device_id, new_cards

EMAIL = "pkg@example.com"
OTHER_EMAIL = "other@example.com"
PASSWORD = "correct-horse-1"
PC = device_id("pc-1")
DRAMA = "雪夜归人"
COMPANY = "星河影业有限公司"
CONTACT = "张三 13800000000"
COST_PDF = "04_成本配置表/成本配置表_未签署.pdf"
OWNERSHIP_PDF = "05_权属声明/内容权属声明_未签署.pdf"
SEALED = ("04_成本配置表/成本配置表.pdf", "05_权属声明/内容权属声明.pdf")


def _episode(number: int) -> dict:
    return {
        "file_name": f"第{number:02d}集.mp4",
        "episode_number": number,
        "size_bytes": 1000 + number,
        "sha256": hashlib.sha256(f"episode-{number}".encode()).hexdigest(),
        "spec": {"duration_sec": 90.0, "width": 1080, "height": 1920, "video_codec": "h264",
                 "audio_codec": "aac", "fps": 30.0, "bitrate_bps": 4_000_000},
    }


def package_body(device: str = PC, **overrides) -> dict:
    body = {
        "device_id": device,
        "drama_name": DRAMA,
        "ai_generated": False,
        "copy_episodes": False,
        "cost_sheet": {"company_name": COMPANY, "total_investment_wan": 80.0, "actor_ratio_percent": 35.0,
                       "contact": CONTACT},
        "cover": {"file_name": "封面.jpg", "size_bytes": 2048, "sha256": "e" * 64},
        "episodes": [_episode(1), _episode(2)],
        "signer": "mock",
    }
    body.update(overrides)
    return body


def create(client, token: str, body: dict):
    return client.post("/api/v1/packages", json=body, headers=auth(token))


@pytest.fixture()
def account(client, register, pg):
    """注册、绑定 PC、兑换一张月卡，并给够生成报审包的积分。"""
    body = register(EMAIL, PASSWORD)
    token = body["tokens"]["access_token"]
    assert activate(client, token, PC).status_code == 200
    [code] = new_cards(pg, days=30)
    assert client.post("/api/v1/cards/redeem", json={"code": code}, headers=auth(token)).status_code == 200
    # M5a 起生成报审包要扣积分。这些用例只关心包本身，
    # 余额不足的场景在 test_package_billing.py 里专门测。
    credits.grant(pg, body["user"]["id"], 1000, idempotency_key=f"seed:{EMAIL}")
    return {"id": body["user"]["id"], "token": token}


def _files(response) -> dict:
    return {item["path"]: base64.b64decode(item["content"]) for item in response["files"]}


def _pdf_text(data: bytes) -> str:
    return "\n".join(page.extract_text() or "" for page in PdfReader(io.BytesIO(data)).pages)


def _package_rows(pg, user_id: str) -> list:
    return pg.execute("SELECT * FROM packages WHERE user_id = %s::uuid", (user_id,)).fetchall()


# ---------- 正常生成 ----------

def test_creates_signed_package_with_placeholder_seal(client, pg, signer, account):
    response = create(client, account["token"], package_body())
    assert response.status_code == 200, response.text
    body = response.json()

    # 清单签名：用途前缀是 package-manifest，用服务端公钥验得过；换成授权凭证的前缀就验不过。
    manifest_bytes = body["manifest"].encode("utf-8")
    sig_file = json.loads(body["signature"])
    assert sig_file["kid"] == signer.kid
    public = Ed25519PublicKey.from_public_bytes(signer.public_key)
    raw_sig = lt.b64url_decode(sig_file["sig"])
    public.verify(raw_sig, lt.PURPOSE_CONTEXTS["package-manifest"] + manifest_bytes)
    with pytest.raises(Exception):
        public.verify(raw_sig, lt.PURPOSE_CONTEXTS["license"] + manifest_bytes)

    manifest = json.loads(manifest_bytes)
    assert manifest["schema"] == "drama-dossier/package@2"
    assert manifest["deviceId"] == PC
    assert manifest["userId"] == account["id"]
    assert manifest["packageId"] == body["package_id"]
    assert manifest["signature"] == {"signer": "mock", "placeholder": True,
                                     "documents": [COST_PDF, OWNERSHIP_PDF],
                                     "perDocument": [
                                         {"document": COST_PDF, "signer": "mock"},
                                         {"document": OWNERSHIP_PDF, "signer": "mock"},
                                     ]}

    # 服务端文件和清单逐个对得上；本机文件（封面）只登记，不在 files 里返回内容。
    files = _files(body)
    server_entries = {entry["path"]: entry for entry in manifest["files"] if entry["source"] == "server"}
    assert set(files) == set(server_entries)
    for path, content in files.items():
        assert server_entries[path]["sizeBytes"] == len(content)
        assert server_entries[path]["sha256"] == hashlib.sha256(content).hexdigest()
    local = [entry for entry in manifest["files"] if entry["source"] == "local"]
    assert local == [{"path": "02_封面海报/封面.jpg", "sizeBytes": 2048, "sha256": "e" * 64, "source": "local"}]
    # 不复制分集：分集只在 references 里登记桌面端给的哈希。
    assert [ref["sha256"] for ref in manifest["references"]] == [_episode(1)["sha256"], _episode(2)["sha256"]]
    csv = files["03_分集清单/分集清单.csv"].decode("utf-8")
    assert _episode(1)["sha256"] in csv and _episode(2)["sha256"] in csv

    # 两份 PDF：能打开，带占位章和水印的文字，文件名带 _未签署。
    for path in (COST_PDF, OWNERSHIP_PDF):
        text = _pdf_text(files[path])
        assert "样章" in text and "未签署" in text, path
        assert "草稿" in text, path
    assert COMPANY in _pdf_text(files[COST_PDF])

    # 只记元数据：一行 packages，清单哈希对得上。
    [row] = _package_rows(pg, account["id"])
    assert str(row["id"]) == body["package_id"]
    assert row["device_id"] == PC and row["signer"] == "mock" and row["episode_count"] == 2
    assert bytes(row["manifest_sha256"]) == hashlib.sha256(manifest_bytes).digest()


def test_audit_log_has_no_drama_company_or_contact(client, pg, account):
    assert create(client, account["token"], package_body()).status_code == 200
    rows = pg.execute(
        "SELECT action, detail::text AS detail FROM audit_log WHERE action = 'package.create'"
    ).fetchall()
    assert len(rows) == 1
    everything = json.dumps([dict(row) for row in pg.execute("SELECT * FROM audit_log").fetchall()],
                            default=str, ensure_ascii=False)
    for secret in (DRAMA, COMPANY, CONTACT, "13800000000"):
        assert secret not in everything, f"审计日志里出现了原文：{secret}"
    detail = json.loads(rows[0]["detail"])
    # 只记剧名哈希的前 16 位，用来排查「同一部剧生成了几次」，反推不出剧名。
    assert detail == {"drama": hashlib.sha256(DRAMA.encode("utf-8")).hexdigest()[:16], "episodes": 2,
                      "signer": "mock"}


def test_export_lists_package_metadata_only(client, account):
    assert create(client, account["token"], package_body()).status_code == 200
    exported = client.get("/api/v1/me/export", headers=auth(account["token"])).json()
    [item] = exported["packages"]
    assert set(item) == {"id", "created_at", "signer", "episode_count"}
    assert DRAMA not in json.dumps(exported, ensure_ascii=False)


# ---------- 谁能生成 ----------

def test_rejects_unbound_device(client, pg, account):
    response = create(client, account["token"], package_body(device=device_id("pc-never-bound")))
    assert response.status_code == 403
    assert response.json()["error"] == "device_not_bound"
    assert _package_rows(pg, account["id"]) == []


def test_rejects_device_bound_to_another_account(client, pg, register, account):
    """别的账号绑定的电脑：机器码合法、也确实绑定过，但不是绑在当前账号上。"""
    other = register(OTHER_EMAIL, PASSWORD)
    other_pc = device_id("pc-other")
    assert activate(client, other["tokens"]["access_token"], other_pc).status_code == 200
    response = create(client, account["token"], package_body(device=other_pc))
    assert response.status_code == 403
    assert response.json()["error"] == "device_not_bound"
    assert pg.execute("SELECT count(*) AS n FROM packages").fetchone()["n"] == 0


def test_unbinding_this_device_revokes_the_session(client, pg, account):
    [binding] = client.get("/api/v1/devices", headers=auth(account["token"])).json()["devices"]
    assert client.delete(f"/api/v1/devices/{binding['id']}", headers=auth(account["token"])).status_code == 204
    response = create(client, account["token"], package_body())
    assert response.status_code == 401, response.text
    assert _package_rows(pg, account["id"]) == []


def test_rejects_device_unbound_while_session_alive(client, pg, account):
    """会话没被吊销（比如解绑走的是别的路径），只要设备不再算绑定，照样拒绝。"""
    pg.execute("UPDATE devices SET unbound_at = now(), unbind_reason = 'user' WHERE device_id = %s", (PC,))
    response = create(client, account["token"], package_body())
    assert response.status_code == 403, response.text
    assert response.json()["error"] == "device_not_bound"
    assert _package_rows(pg, account["id"]) == []


def test_rejects_without_entitlement(client, pg, register):
    body = register(EMAIL, PASSWORD)
    token = body["tokens"]["access_token"]
    assert activate(client, token, PC).status_code == 200
    response = create(client, token, package_body())
    assert response.status_code == 403
    assert response.json()["error"] == "entitlement_required"
    assert _package_rows(pg, body["user"]["id"]) == []


def test_rejects_expired_entitlement(client, pg, account):
    pg.execute("UPDATE entitlements SET expires_at = now() - interval '1 second' WHERE user_id = %s::uuid",
               (account["id"],))
    assert not entitlements.current(pg, account["id"]).active
    response = create(client, account["token"], package_body())
    assert response.status_code == 403
    assert response.json()["error"] == "entitlement_required"


# ---------- 扫描件签章 ----------

def _png() -> str:
    buffer = io.BytesIO()
    Image.new("RGB", (210, 297), "white").save(buffer, format="PNG")
    return base64.b64encode(buffer.getvalue()).decode("ascii")


def test_scan_replaces_pages_and_is_not_placeholder(client, pg, account):
    body = package_body(signer="scan", scans={path: _png() for path in SEALED})
    response = create(client, account["token"], body)
    assert response.status_code == 200, response.text
    manifest = json.loads(response.json()["manifest"])
    assert manifest["signature"] == {
        "signer": "scan",
        "placeholder": False,
        "documents": list(SEALED),
        "perDocument": [{"document": path, "signer": "scan"} for path in SEALED],
    }
    files = _files(response.json())
    for path in SEALED:
        assert path in files, "扫描件签章不加 _未签署 后缀"
        pdf = PdfReader(io.BytesIO(files[path]))
        assert len(pdf.pages) == 1
        # 整页换成了扫描图：原来的文字、占位章、水印都不在了。
        assert "样章" not in (pdf.pages[0].extract_text() or "")
    assert not any("_未签署" in path for path in files)
    [row] = _package_rows(pg, account["id"])
    assert row["signer"] == "scan"


@pytest.mark.parametrize("scans, fragment", [
    ({SEALED[0]: base64.b64encode(b"GIF89a....").decode()}, "PNG、JPEG"),
    ({SEALED[0]: "not base64!!"}, "base64"),
    ({"01_剧集信息/剧名.txt": "AAAA"}, "不需要盖章"),
])
def test_rejects_bad_scans(client, pg, account, scans, fragment):
    response = create(client, account["token"], package_body(signer="scan", scans=scans))
    assert response.status_code == 422, response.text
    assert response.json()["error"] == "scan_invalid"
    assert fragment in response.json()["message"]
    assert _package_rows(pg, account["id"]) == []


def test_mixed_signing_keeps_the_unsigned_document_placeholder(client, pg, account):
    """只上传一份扫描件时：给的那份用扫描件，没给的那份仍是占位章。

    用户的决定是「支持混合」。这条用例把三件事钉住：
    1. 不给全也不报错（否则只准备了一份扫描件的人就交付不了）；
    2. 没给的那份仍然带 `_未签署` 后缀、仍然是占位章；
    3. 清单里逐份写明签章方式，界面才能逐份标红。
    """
    scanned = SEALED[0]
    response = create(client, account["token"],
                      package_body(signer="scan", scans={scanned: _png()}))
    assert response.status_code == 200, response.text
    manifest = json.loads(response.json()["manifest"])
    signature = manifest["signature"]
    # 整包仍有未签署的文档：`placeholder` 必须为 True，不能因为「有扫描件」就报成已签。
    assert signature["placeholder"] is True
    assert signature["signer"] == "scan"
    unsigned = [path for path in signature["documents"] if "_未签署" in path]
    assert len(unsigned) == 1, signature["documents"]
    assert unsigned[0].startswith(SEALED[1].removesuffix(".pdf")), signature["documents"]

    per_document = {item["document"]: item["signer"] for item in signature["perDocument"]}
    assert len(per_document) == len(SEALED)
    # 给的那份是扫描件（文件名没有后缀），没给的那份是占位章（带后缀）。
    assert per_document[scanned] == "scan"
    assert per_document[unsigned[0]] == "mock"

    files = _files(response.json())
    # 扫描件那份：整页换掉了。
    pdf = PdfReader(io.BytesIO(files[scanned]))
    assert "样章" not in (pdf.pages[0].extract_text() or "")
    # 占位章那份：仍然带水印与「未签署」字样。
    placeholder = PdfReader(io.BytesIO(files[unsigned[0]]))
    assert "样章" in (placeholder.pages[0].extract_text() or "")
    [row] = _package_rows(pg, account["id"])
    assert row["signer"] == "scan"


def test_rejects_scans_with_mock_signer(client, account):
    response = create(client, account["token"], package_body(scans={SEALED[0]: _png()}))
    assert response.status_code == 422
    assert response.json()["error"] == "scan_invalid"


# ---------- 缺工具时明确报错，不生成残缺的包 ----------

def test_missing_artifacts_tool_is_503_and_writes_nothing(client, pg, account, monkeypatch):
    monkeypatch.setattr(package_service.toolchain, "resolve_binary", lambda *args, **kwargs: None)
    response = create(client, account["token"], package_body())
    assert response.status_code == 503
    assert response.json()["error"] == "service_unavailable"
    assert _package_rows(pg, account["id"]) == []
    assert pg.execute("SELECT count(*) AS n FROM audit_log WHERE action = 'package.create'").fetchone()["n"] == 0


def test_missing_or_broken_font_fails_at_startup(commerce_settings, pool, mailer, tmp_path):
    """字体在启动时加载：找不到或不是字体就起不来，并说清是哪个文件，不会等到生成时才出残缺的 PDF。"""
    from dataclasses import replace

    from drama_cloud.commerce.api import create_app
    from drama_cloud.commerce.config import ConfigError

    missing = tmp_path / "no-such-font.ttc"
    with pytest.raises(ConfigError, match="DRAMA_COMMERCE_PDF_FONT") as error:
        create_app(replace(commerce_settings, pdf_font=str(missing)), pool=pool, mailer=mailer)
    assert str(missing) in str(error.value)

    broken = tmp_path / "broken.ttf"
    broken.write_bytes(b"not a font")
    with pytest.raises(ConfigError, match="不是可用的字体文件"):
        create_app(replace(commerce_settings, pdf_font=str(broken)), pool=pool, mailer=mailer)


def test_rejected_by_renderer_is_422_with_chinese_reason(client, pg, account):
    # 两集同名：Pydantic 不管，Rust 的 render_documents 会拒绝（退出码 2），原因原样转给用户。
    body = package_body(episodes=[_episode(1), _episode(1)])
    response = create(client, account["token"], body)
    assert response.status_code == 422, response.text
    assert response.json()["error"] == "invalid_request"
    assert response.json()["message"]
    assert _package_rows(pg, account["id"]) == []
