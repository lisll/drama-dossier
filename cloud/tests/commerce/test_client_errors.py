"""客户端错误上报（M7-6）。

总方案的要求很具体：「只报错误码、版本和系统信息，不带邮箱、文件路径、
视频内容」。所以这份测试的重点是**不该进来的东西进不来**：

* 多传的字段（邮箱、路径、令牌）一律被丢弃；
* 带斜杠的 `location` 被清空（万一客户端把路径塞进来了）；
* 同一个错误只累加计数，不逐条存（否则表会被噪音撑爆）；
* 后台查看要管理员令牌。
"""

from __future__ import annotations

import pytest
from fastapi.testclient import TestClient

from drama_cloud.commerce.admins import AdminService
from drama_cloud.commerce.api import create_app

from .conftest import FAST_HASHER

ADMIN_PASSWORD = "admin-password-1234"


@pytest.fixture()
def client(commerce_settings, pool, pg, mailer):
    app = create_app(commerce_settings, pool=pool, mailer=mailer, hasher=FAST_HASHER)
    with TestClient(app) as test_client:
        yield test_client


@pytest.fixture()
def admin_token(client, pg):
    AdminService(hasher=FAST_HASHER).create(pg, "boss@example.com", ADMIN_PASSWORD)
    response = client.post(
        "/api/v1/admin/login",
        json={"email": "boss@example.com", "password": ADMIN_PASSWORD},
    )
    return response.json()["token"]


def admin_auth(token: str) -> dict:
    return {"Authorization": f"Bearer {token}"}


def _report(client, **overrides):
    body = {"code": "insufficient_credits", "app_version": "0.2.1", "platform": "linux"}
    body.update(overrides)
    return client.post("/api/v1/client/errors", json=body)


def test_a_report_is_accepted_without_login(client):
    """出错时用户可能还没登录——「启动就崩」恰恰最需要上报。"""
    response = _report(client)
    assert response.status_code == 202, response.text
    assert response.json()["accepted"] is True


def test_extra_fields_are_dropped(client, pg):
    """多传的字段一律丢弃：白名单之外的绝不入库。"""
    response = client.post(
        "/api/v1/client/errors",
        json={
            "code": "io",
            "app_version": "0.2.1",
            "platform": "linux",
            # 下面这些都是**不该**被接受的。
            "email": "someone@example.com",
            "path": "/home/user/剧名/第01集.mp4",
            "token": "at_secret",
            "message": "读取 /data/剧名 失败",
        },
    )
    # strict 模型会直接拒绝多余字段（422），这比「悄悄丢掉」更明确。
    assert response.status_code == 422, response.text
    # 无论哪种处理方式，库里都不该有这些东西。
    row = pg.execute("SELECT * FROM client_errors").fetchone()
    assert row is None


def test_the_database_has_no_column_for_sensitive_data(pg):
    """结构性保证：表里根本没有能放路径或邮箱的列。"""
    columns = {
        row["column_name"]
        for row in pg.execute(
            "SELECT column_name FROM information_schema.columns"
            " WHERE table_name = 'client_errors'"
        ).fetchall()
    }
    assert columns == {
        "id", "code", "app_version", "platform", "location",
        "occurrences", "first_seen", "last_seen",
    }


def test_a_location_with_a_slash_is_cleared(client, pg):
    """`location` 是模块名（如 upload），不是路径。带斜杠的一律按空处理。"""
    _report(client, location="upload")
    _report(client, code="io", location="/home/user/剧名")
    rows = pg.execute("SELECT code, location FROM client_errors").fetchall()
    by_code = {row["code"]: row["location"] for row in rows}
    assert by_code["insufficient_credits"] == "upload"
    assert by_code["io"] == "", "带斜杠的位置必须被清空"


def test_repeats_only_increment_the_counter(client, pg):
    """同一个错误上报多次只累加计数，不逐条存。"""
    for _ in range(5):
        assert _report(client).status_code == 202
    rows = pg.execute("SELECT occurrences FROM client_errors").fetchall()
    assert len(rows) == 1, "同一个错误应当只有一行"
    assert rows[0]["occurrences"] == 5


def test_different_versions_are_counted_separately(client, pg):
    _report(client, app_version="0.2.1")
    _report(client, app_version="0.2.2")
    assert pg.execute("SELECT count(*) AS n FROM client_errors").fetchone()["n"] == 2


def test_an_unknown_platform_falls_back_to_unknown(client, pg):
    """平台值不认识就记 unknown，不拒绝整条上报——上报本身不该因为这点信息丢了。"""
    assert _report(client, platform="plan9").status_code == 202
    row = pg.execute("SELECT platform FROM client_errors").fetchone()
    assert row["platform"] == "unknown"


def test_an_empty_code_is_refused(client):
    assert _report(client, code="").status_code == 422


def test_reports_are_rate_limited(client):
    codes = [_report(client).status_code for _ in range(320)]
    assert 429 in codes, "超过每小时上限应当被限流"


# ---------- 后台查看 ----------

def test_listing_errors_needs_an_admin(client):
    assert client.get("/api/v1/admin/client-errors").status_code == 401


def test_a_user_token_cannot_read_the_error_list(client, register):
    body = register("plain@example.com", "correct-horse-1")
    token = body["tokens"]["access_token"]
    assert client.get(
        "/api/v1/admin/client-errors", headers=admin_auth(token)
    ).status_code == 401


def test_the_admin_sees_the_errors_with_counts(client, admin_token):
    _report(client)
    _report(client)
    _report(client, code="network")
    listing = client.get(
        "/api/v1/admin/client-errors", headers=admin_auth(admin_token)
    ).json()
    by_code = {row["code"]: row for row in listing["items"]}
    assert by_code["insufficient_credits"]["occurrences"] == 2
    assert by_code["network"]["occurrences"] == 1
