"""报审包与处理配方接口的「样板」测试：接口的共同前置条件（登录、限流、请求体上限）和参数校验
各走一遍，确认状态码、`error` 码稳定，而且被拒绝的请求不会往库里写东西。

这里不生成真的报审包（正常生成、签章、扫描件替换页面在 test_packages.py 里），
限流也不真发 30 次请求：按 ratelimit.py 的算法直接把该账号在当窗的计数写到上限，再打一次看 429。

需要 PostgreSQL 和 `dossier-artifacts`：`cargo build -p dossier-core --bin dossier-artifacts`
（查找顺序见 drama_cloud/toolchain.py）。
"""

from __future__ import annotations

import base64
import hashlib
import io
import json
import uuid
from typing import Optional

import pytest
from PIL import Image
from pypdf import PdfWriter

from drama_cloud.commerce.body_limit import DEFAULT_LIMIT, PACKAGE_LIMIT
from drama_cloud.commerce.documents.signer import MAX_SCAN_BYTES
from drama_cloud.commerce.package_routes import PACKAGE_PER_USER, RECIPE_PER_USER
from drama_cloud.commerce.schemas import SCAN_B64_MAX

from .conftest import activate, auth, device_id, new_cards

EMAIL = "boilerplate@example.com"
PASSWORD = "correct-horse-1"
PC = device_id("pc-1")
DRAMA = "雪夜归人"
COMPANY = "星河影业有限公司"
CONTACT = "张三 13800000000"
TASK = "c" * 64
#: 报审包里要盖章的两份文档；扫描件签章必须两份都给（见 package_service 的 sealed 列表）。
SEALED = ("04_成本配置表/成本配置表.pdf", "05_权属声明/内容权属声明.pdf")
_PNG_MAGIC = b"\x89PNG\r\n\x1a\n"


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


def issue(client, token: str, device: str = PC, task: str = TASK, platform: str = "universal",
          strength: str = "medium"):
    return client.post("/api/v1/recipes", headers=auth(token),
                       json={"device_id": device, "task": task, "platform": platform, "strength": strength})


def consume(client, token: str, recipe_id: str, device: str = PC):
    return client.post(f"/api/v1/recipes/{recipe_id}/consume", headers=auth(token), json={"device_id": device})


@pytest.fixture()
def account(client, register, pg):
    """注册、绑定 PC、兑换一张月卡。"""
    body = register(EMAIL, PASSWORD)
    token = body["tokens"]["access_token"]
    assert activate(client, token, PC).status_code == 200
    [code] = new_cards(pg, days=30)
    assert client.post("/api/v1/cards/redeem", json={"code": code}, headers=auth(token)).status_code == 200
    return {"id": body["user"]["id"], "token": token}


def _package_rows(pg, user_id: str) -> list:
    return pg.execute("SELECT * FROM packages WHERE user_id = %s::uuid", (user_id,)).fetchall()


def _recipe_count(pg) -> int:
    return pg.execute("SELECT count(*) AS n FROM recipes").fetchone()["n"]


def _json_headers(token: str) -> dict:
    return {**auth(token), "content-type": "application/json"}


def _chunks(data: bytes, size: int = 1 << 16):
    """按块吐出请求体：httpx 会改用 chunked 编码，不发 Content-Length。"""
    for start in range(0, len(data), size):
        yield data[start:start + size]


def _b64(data: bytes) -> str:
    return base64.b64encode(data).decode("ascii")


def _png() -> str:
    buffer = io.BytesIO()
    Image.new("RGB", (210, 297), "white").save(buffer, format="PNG")
    return _b64(buffer.getvalue())


def _pdf(pages: int, encrypt: bool = False) -> str:
    writer = PdfWriter()
    for _ in range(pages):
        writer.add_blank_page(width=595, height=842)
    if encrypt:
        writer.encrypt("secret")
    out = io.BytesIO()
    writer.write(out)
    return _b64(out.getvalue())


def _scans(first: str, second: Optional[str] = None) -> dict:
    """两份要盖章的文档都给扫描件；第二份不给就配一张正常的 PNG。"""
    return {SEALED[0]: first, SEALED[1]: _png() if second is None else second}


#: 5 MB + 1 字节，开头是真的 PNG 文件头：base64 之后仍在字段上限内，所以拦它的是「不能超过 5 MB」。
OVERSIZED_SCAN = _b64(_PNG_MAGIC + bytes(MAX_SCAN_BYTES + 1 - len(_PNG_MAGIC)))


# ---------- 未登录 ----------

def test_requires_login(client, pg):
    """三个接口都不接受匿名请求，也不会留下任何记录。"""
    responses = [
        client.post("/api/v1/packages", json=package_body()),
        client.post("/api/v1/recipes", json={"device_id": PC, "task": TASK, "platform": "universal",
                                             "strength": "medium"}),
        client.post(f"/api/v1/recipes/{uuid.uuid4()}/consume", json={"device_id": PC}),
    ]
    for response in responses:
        assert response.status_code == 401, response.text
        assert response.json()["error"] == "unauthorized"
    assert pg.execute("SELECT count(*) AS n FROM packages").fetchone()["n"] == 0
    assert _recipe_count(pg) == 0


# ---------- 限流 ----------

def _fill_rate_limit(pg, rule, user_id: str) -> None:
    """把某个桶在当前时间窗里的计数直接写到上限。

    桶名和窗口起点的算法要和 ratelimit.py 的 _HIT 一致：`<rule.name>:<user_id>`，
    窗口起点是 `floor(epoch / 窗口秒数) * 窗口秒数`。这样下一次请求进来就是超限的那一次。
    """
    pg.execute(
        "INSERT INTO rate_limits (bucket, window_start, hits)"
        " VALUES (%s, to_timestamp((floor(extract(epoch FROM now()) / %s::int) * %s::int)::float8), %s)"
        " ON CONFLICT (bucket, window_start) DO UPDATE SET hits = %s",
        (f"{rule.name}:{user_id}", rule.window_seconds, rule.window_seconds, rule.limit, rule.limit),
    )


def test_packages_rate_limited(client, pg, account, fresh_window):
    fresh_window(PACKAGE_PER_USER.window_seconds, needed_seconds=30)
    _fill_rate_limit(pg, PACKAGE_PER_USER, account["id"])
    response = create(client, account["token"], package_body())
    assert response.status_code == 429, response.text
    assert response.json()["error"] == "rate_limited"
    # 429 要告诉客户端还有多少秒可以重试。
    assert 1 <= int(response.headers["Retry-After"]) <= PACKAGE_PER_USER.window_seconds
    assert _package_rows(pg, account["id"]) == []


def test_recipes_rate_limited(client, pg, account, fresh_window):
    fresh_window(RECIPE_PER_USER.window_seconds, needed_seconds=30)
    _fill_rate_limit(pg, RECIPE_PER_USER, account["id"])
    response = issue(client, account["token"])
    assert response.status_code == 429, response.text
    assert response.json()["error"] == "rate_limited"
    assert 1 <= int(response.headers["Retry-After"]) <= RECIPE_PER_USER.window_seconds
    assert _recipe_count(pg) == 0


# ---------- 请求体上限 ----------

def test_body_over_default_limit_is_413(client, pg, account):
    """普通接口（配方）超过 1 MiB 在读请求体时就挡掉，不等到解析 JSON。"""
    body = json.dumps({"pad": "x" * DEFAULT_LIMIT}).encode("utf-8")
    assert len(body) > DEFAULT_LIMIT
    response = client.post("/api/v1/recipes", content=body, headers=_json_headers(account["token"]))
    assert response.status_code == 413, response.text
    assert response.json()["error"] == "payload_too_large"
    assert _recipe_count(pg) == 0


def test_packages_limit_is_looser_than_other_routes(client, pg, account):
    """/api/v1/packages 的上限要给 4 份扫描件留位置，所以比普通接口宽。"""
    over = json.dumps({"pad": "x" * PACKAGE_LIMIT}).encode("utf-8")
    assert len(over) > PACKAGE_LIMIT
    rejected = client.post("/api/v1/packages", content=over, headers=_json_headers(account["token"]))
    assert rejected.status_code == 413, rejected.text
    assert rejected.json()["error"] == "payload_too_large"

    # 2 MiB：超过普通接口的 1 MiB，但在报审包的上限内——不该是 413，字段不认识才是它被拒的原因。
    within = json.dumps({"pad": "x" * (2 << 20)}).encode("utf-8")
    assert DEFAULT_LIMIT < len(within) < PACKAGE_LIMIT
    accepted = client.post("/api/v1/packages", content=within, headers=_json_headers(account["token"]))
    assert accepted.status_code == 422, accepted.text
    assert accepted.json()["error"] == "invalid_request"
    assert _package_rows(pg, account["id"]) == []


def test_chunked_body_over_limit_is_413(client, pg, account):
    """不带 Content-Length、分块发送：中间件边收边数，超了照样 413。

    用默认的 client（服务端异常会直接抛出来）：中间件在进应用之前就拒掉，应用侧不该有任何异常。
    """
    body = b'{"pad":"' + b"x" * DEFAULT_LIMIT + b'"}'
    assert len(body) > DEFAULT_LIMIT
    response = client.post("/api/v1/recipes", content=_chunks(body),
                           headers=_json_headers(account["token"]))
    assert response.status_code == 413, response.text[:200]
    assert response.json()["error"] == "payload_too_large"
    assert _recipe_count(pg) == 0


def test_chunked_body_within_limit_reaches_the_app(client, pg, account):
    """分块发送、但没超限：中间件收齐后原样交给应用，应用照常解析（这里是参数不合法的 422）。"""
    body = json.dumps({"platform": "bilibili"}).encode("utf-8")
    response = client.post("/api/v1/recipes", content=_chunks(body, size=4),
                           headers=_json_headers(account["token"]))
    assert response.status_code == 422, response.text
    assert response.json()["error"] == "invalid_request"
    assert _recipe_count(pg) == 0


# ---------- 参数校验：报审包 ----------

def _many_episodes(count: int) -> list:
    return [_episode(number) for number in range(1, count + 1)]


@pytest.mark.parametrize("overrides", [
    pytest.param({"episodes": _many_episodes(501)}, id="episodes-501"),
    pytest.param({"drama_name": ""}, id="drama-name-empty"),
    pytest.param({"drama_name": "剧" * 201}, id="drama-name-201"),
    pytest.param({"cover": {"file_name": "封面.jpg", "size_bytes": 2048, "sha256": "E" * 64}},
                 id="sha256-uppercase"),
    pytest.param({"extra_field": "x"}, id="unknown-field"),
    pytest.param({"ai_generated": "true"}, id="ai-generated-string"),
    pytest.param({"signer": "provider"}, id="signer-provider"),
    pytest.param({"scans": {f"04_成本配置表/文档{index}.pdf": "AAAA" for index in range(5)}}, id="scans-5"),
])
def test_rejects_bad_package_request(client, pg, account, overrides):
    response = create(client, account["token"], package_body(**overrides))
    assert response.status_code == 422, response.text
    assert response.json()["error"] == "invalid_request"
    assert _package_rows(pg, account["id"]) == []


# ---------- 参数校验：处理配方 ----------

@pytest.mark.parametrize("overrides", [
    pytest.param({"platform": "bilibili"}, id="platform-bilibili"),
    pytest.param({"strength": "extreme"}, id="strength-extreme"),
    pytest.param({"task": "C" * 64}, id="task-uppercase"),
    pytest.param({"task": "c" * 63}, id="task-too-short"),
])
def test_rejects_bad_recipe_request(client, pg, account, overrides):
    response = issue(client, account["token"], **overrides)
    assert response.status_code == 422, response.text
    assert response.json()["error"] == "invalid_request"
    assert _recipe_count(pg) == 0


# ---------- 路径参数 ----------

def test_consume_rejects_non_uuid_recipe_id(client, account):
    response = consume(client, account["token"], "not-a-uuid")
    assert response.status_code == 422, response.text
    assert response.json()["error"] == "invalid_request"
    assert response.json()["fields"][0]["field"] == "path.recipe_id"


# ---------- 扫描件签章 ----------

def test_oversized_scan_still_fits_the_schema_limit():
    """超大扫描件 base64 之后仍在字段上限内：上面那个 422 是「超过 5 MB」拦的，不是字段长度拦的。"""
    assert len(OVERSIZED_SCAN) <= SCAN_B64_MAX
    assert len(base64.b64decode(OVERSIZED_SCAN)) == MAX_SCAN_BYTES + 1


@pytest.mark.parametrize("scans, fragment", [
    pytest.param(_scans(OVERSIZED_SCAN), "5 MB", id="too-large"),
    pytest.param(_scans(_b64(_PNG_MAGIC + "这不是图片".encode("utf-8"))), "打不开", id="png-garbage"),
    pytest.param(_scans(_pdf(1, encrypt=True)), "加密", id="encrypted-pdf"),
    pytest.param(_scans(_pdf(2)), "页数", id="page-count"),
])
def test_rejects_bad_scans(client, pg, account, scans, fragment):
    """签名方式选 scan 时，两份文档的扫描件都要给：图片坏、PDF 加密、页数不一致都按 scan_invalid 拒掉。"""
    response = create(client, account["token"], package_body(signer="scan", scans=scans))
    assert response.status_code == 422, response.text
    assert response.json()["error"] == "scan_invalid"
    assert fragment in response.json()["message"]
    assert _package_rows(pg, account["id"]) == []
