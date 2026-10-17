"""报审包计费（M5a）：先预扣、生成成功才结算、失败退还。

与 `test_packages.py` 的分工：那边的用例只关心包本身，默认给足积分；
这里专门盯钱，重点是「渲染失败也要把钱退回来」。
"""

from __future__ import annotations

import json

import pytest

from drama_cloud.commerce import credits

from .conftest import activate, auth, device_id, new_cards
from .test_packages import PASSWORD, PC, package_body

EMAIL = "pkg-billing@example.com"


def _account(client, register, pg, seed: int = 0) -> dict:
    body = register(EMAIL, PASSWORD)
    token = body["tokens"]["access_token"]
    assert activate(client, token, PC).status_code == 200
    [code] = new_cards(pg, days=30)
    assert client.post("/api/v1/cards/redeem", json={"code": code}, headers=auth(token)).status_code == 200
    if seed:
        credits.grant(pg, body["user"]["id"], seed, idempotency_key=f"seed:{EMAIL}")
    return {"id": body["user"]["id"], "token": token}


def _create(client, token: str, body: dict):
    return client.post("/api/v1/packages", headers=auth(token), json=body)


def test_package_charges_per_episode(client, pg, register):
    account = _account(client, register, pg, seed=100)
    body = package_body()
    episodes = len(body["episodes"])
    response = _create(client, account["token"], body)
    assert response.status_code == 200, response.text
    expected = episodes * 5
    assert response.json()["credits"] == expected
    assert credits.balance(pg, account["id"]) == 100 - expected


def test_insufficient_credits_blocks_generation_and_takes_nothing(client, pg, register):
    account = _account(client, register, pg, seed=3)  # 不够一份包
    response = _create(client, account["token"], package_body())
    assert response.status_code == 402, response.text
    assert response.json()["error"] == "insufficient_credits"
    assert credits.balance(pg, account["id"]) == 3
    # 没有留下预扣。
    holds = pg.execute(
        "SELECT count(*) AS n FROM credit_ledger WHERE user_id = %s::uuid AND kind = 'hold'",
        (account["id"],),
    ).fetchone()["n"]
    assert holds == 0
    # 也没有生成任何包记录。
    packages = pg.execute(
        "SELECT count(*) AS n FROM packages WHERE user_id = %s::uuid", (account["id"],)
    ).fetchone()["n"]
    assert packages == 0


def test_render_failure_refunds_the_hold(client, pg, register, monkeypatch, signer):
    """渲染阶段出错：预扣必须退回，账本里应当有 hold 与 release 两条，且余额复原。

    这是「三段式」里最容易漏的一条：如果只在成功后结算、失败时什么都不做，
    钱就永久留在「预扣中」，2 小时后才会被回收——而用户当时就该看到钱回来了。
    """
    from drama_cloud.commerce import package_service as ps

    account = _account(client, register, pg, seed=100)
    before = credits.balance(pg, account["id"])

    # 让渲染抛错（模拟 dossier-artifacts 找不到 / 超时 / 输出不是 JSON）。
    def boom(self, payload):
        raise ps.CommerceError(503, "service_unavailable", "服务端缺少报审包生成工具，请联系管理员")

    monkeypatch.setattr(ps.PackageService, "_render", boom)

    response = _create(client, account["token"], package_body())
    assert response.status_code == 503, response.text
    # 钱回来了。
    assert credits.balance(pg, account["id"]) == before, "渲染失败必须退还"
    kinds = [
        row["kind"]
        for row in pg.execute(
            "SELECT kind FROM credit_ledger WHERE user_id = %s::uuid ORDER BY id", (account["id"],)
        ).fetchall()
    ]
    assert "hold" in kinds and "release" in kinds, kinds
    # 预扣被标为已退还，不是「预扣中」躺在那里。
    state = pg.execute(
        "SELECT settled_state FROM credit_ledger"
        " WHERE user_id = %s::uuid AND kind = 'hold' ORDER BY id DESC LIMIT 1",
        (account["id"],),
    ).fetchone()["settled_state"]
    assert state == "released"


def test_rejected_scan_also_refunds(client, pg, register):
    """参数层面的拒绝（扫描件不合法）发生在渲染之前，同样不能留钱。"""
    account = _account(client, register, pg, seed=100)
    before = credits.balance(pg, account["id"])
    body = package_body(signer="scan", scans={"04_成本配置表/成本配置表.pdf": "not base64!!"})
    response = _create(client, account["token"], body)
    assert response.status_code == 422, response.text
    assert credits.balance(pg, account["id"]) == before


def test_audit_log_does_not_leak_the_drama_name_through_credits(client, pg, register):
    """计费相关的审计与流水里不能出现剧名（审计日志一直只存剧名哈希）。"""
    account = _account(client, register, pg, seed=100)
    body = package_body()
    drama = body["drama_name"]
    assert _create(client, account["token"], body).status_code == 200

    # 扫全部审计 detail 与流水的 ref/memo，确认没有剧名明文。
    audit_text = json.dumps(
        [dict(row) for row in pg.execute("SELECT detail FROM audit_log").fetchall()],
        ensure_ascii=False,
        default=str,
    )
    assert drama not in audit_text, "审计日志里出现了剧名明文"
    ledger_text = json.dumps(
        [dict(row) for row in pg.execute("SELECT ref, memo FROM credit_ledger").fetchall()],
        ensure_ascii=False,
        default=str,
    )
    assert drama not in ledger_text, "积分流水里出现了剧名明文"
