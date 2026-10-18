"""服务端处理的扣费（M5b-4）。

钱的事每条路径都要钉住，这里的重点是**每条失败路径**：

* 余额不足 → 402，且**不建任务**（不能先跑后扣）；
* 问不到 commerce → 503，同样不建任务；
* 建任务本身失败 → 把预扣退回去（不能吞掉用户的钱）；
* 任务成功 → 结算；任务失败/取消 → 退还；
* 同一个 Idempotency-Key 重放 → 只扣一次。
"""

from __future__ import annotations

import pytest
from fastapi.testclient import TestClient

from drama_cloud import credit_client
from drama_cloud.api import create_app

from .conftest import _seed_recipes
from .helpers import make_asset


def auth(token: str) -> dict:
    return {"Authorization": f"Bearer {token}"}


class FakeCredits:
    """假的 commerce 计费客户端：记下所有动作，便于断言「扣了几次、退了几次」。"""

    def __init__(self):
        self.configured = True
        self.balance_value = 1000
        self.cost_per_unit = 5
        self.holds: list = []
        self.settles: list = []
        self.releases: list = []
        self.unavailable = False
        self.insufficient = False

    def cost(self, subject: str, units: int) -> int:
        return self.cost_per_unit * max(1, units)

    def hold(self, user_id: str, amount: int, ref: str, memo: str = "") -> str:
        if self.unavailable:
            raise credit_client.CreditsUnavailable("账号服务暂时不可用")
        if self.insufficient:
            raise credit_client.InsufficientCredits(amount, 0, f"积分不足：需要 {amount} 积分")
        self.holds.append({"user_id": user_id, "amount": amount, "ref": ref})
        self.balance_value -= amount
        return f"hold:{ref}"

    def settle(self, ref: str) -> credit_client.CreditOutcome:
        self.settles.append(ref)
        return credit_client.CreditOutcome(ok=True)

    def release(self, ref: str, memo: str = "") -> credit_client.CreditOutcome:
        self.releases.append(ref)
        return credit_client.CreditOutcome(ok=True)

    def balance(self, user_id: str) -> int:
        return self.balance_value


@pytest.fixture()
def credits():
    return FakeCredits()


@pytest.fixture()
def client(settings, conn, fake_commerce, credits, monkeypatch):
    """接了假 commerce 计费的 app。"""
    monkeypatch.setattr(credit_client, "CreditClient", lambda *a, **k: credits)
    import drama_cloud.api as api_module

    # api.py 里是延迟导入 `CreditClient`，所以打桩模块级名字就够。
    return TestClient(create_app(settings))


@pytest.fixture()
def project(client, conn, fake_commerce):
    from drama_cloud.auth import add_admin, principal_for_email
    from drama_cloud.tasks import service

    email = "payer@example.com"
    user_id, token = fake_commerce.register(email)
    principal_for_email(conn, user_id, email)
    add_admin(conn, email, actor="test")
    project_id = service.ensure_default_project(conn, user_id)
    _seed_recipes(conn)
    return project_id, user_id, token


def _asset(conn, storage, project_id):
    return make_asset(conn, storage, project_id, name="ep01.mp4")


def test_a_task_holds_credits_once(client, conn, storage, project, credits):
    project_id, user_id, token = project
    asset = _asset(conn, storage, project_id)

    response = client.post(
        "/api/v1/tasks",
        headers=auth(token),
        json={"asset_ids": [asset["id"]], "recipe_id": "drama.no_edit.generic"},
    )
    assert response.status_code == 201, response.text
    assert len(credits.holds) == 1
    assert credits.holds[0]["amount"] == 5  # 一集 5 积分
    assert credits.holds[0]["user_id"] == user_id
    assert credits.holds[0]["ref"].startswith("server_task:")


def test_insufficient_credits_refuses_and_creates_nothing(client, conn, storage, project, credits):
    project_id, user_id, token = project
    asset = _asset(conn, storage, project_id)
    credits.insufficient = True

    response = client.post(
        "/api/v1/tasks",
        headers=auth(token),
        json={"asset_ids": [asset["id"]], "recipe_id": "drama.no_edit.generic"},
    )
    assert response.status_code == 402, response.text
    body = response.json()["detail"]
    assert body["error"] == "insufficient_credits"
    assert body["required"] == 5
    assert body["balance"] == 0
    # 没有任务被建出来。
    assert conn.execute("SELECT count(*) AS n FROM tasks").fetchone()["n"] == 0


def test_unreachable_commerce_refuses_and_creates_nothing(client, conn, storage, project, credits):
    """问不到账号服务就 503：绝不「先跑着、回头再扣」。"""
    project_id, user_id, token = project
    asset = _asset(conn, storage, project_id)
    credits.unavailable = True

    response = client.post(
        "/api/v1/tasks",
        headers=auth(token),
        json={"asset_ids": [asset["id"]], "recipe_id": "drama.no_edit.generic"},
    )
    assert response.status_code == 503, response.text
    assert conn.execute("SELECT count(*) AS n FROM tasks").fetchone()["n"] == 0


def test_a_failed_task_creation_releases_the_hold(client, conn, storage, project, credits):
    """建任务失败要把钱退回去：不能因为参数错就扣用户的钱。"""
    project_id, user_id, token = project
    response = client.post(
        "/api/v1/tasks",
        headers=auth(token),
        json={"asset_ids": ["ast_missing"], "recipe_id": "drama.no_edit.generic"},
    )
    assert response.status_code == 400, response.text
    assert len(credits.holds) == 1
    assert len(credits.releases) == 1
    assert credits.releases[0] == credits.holds[0]["ref"]


def test_a_successful_task_settles(client, conn, storage, project, credits, settings):
    """任务跑成功后结算（不是退还）。"""
    from drama_cloud.tasks import service
    from drama_cloud.tasks.runner import Worker

    project_id, user_id, token = project
    asset = _asset(conn, storage, project_id)
    created = client.post(
        "/api/v1/tasks",
        headers=auth(token),
        json={"asset_ids": [asset["id"]], "recipe_id": "drama.no_edit.generic"},
    ).json()

    # 直接调结算入口，验证终态 → 结算的分支。
    service.settle_task_billing(conn, created["id"], True, actor="worker")
    assert credits.settles == [credits.holds[0]["ref"]]
    assert credits.releases == []


def test_a_failed_task_releases(client, conn, storage, project, credits):
    from drama_cloud.tasks import service

    project_id, user_id, token = project
    asset = _asset(conn, storage, project_id)
    created = client.post(
        "/api/v1/tasks",
        headers=auth(token),
        json={"asset_ids": [asset["id"]], "recipe_id": "drama.no_edit.generic"},
    ).json()

    service.settle_task_billing(conn, created["id"], False, actor="worker")
    assert credits.releases == [credits.holds[0]["ref"]]
    assert credits.settles == []


def test_replaying_the_same_idempotency_key_holds_only_once(client, conn, storage, project, credits):
    """同一个 Idempotency-Key 重放：只扣一次，且返回同一个任务。"""
    project_id, user_id, token = project
    asset = _asset(conn, storage, project_id)
    body = {"asset_ids": [asset["id"]], "recipe_id": "drama.no_edit.generic"}
    headers = {**auth(token), "Idempotency-Key": "same-key"}

    first = client.post("/api/v1/tasks", headers=headers, json=body)
    second = client.post("/api/v1/tasks", headers=headers, json=body)
    assert first.status_code == 201 and second.status_code == 201
    assert first.json()["id"] == second.json()["id"]
    assert len(credits.holds) == 1, "重放不该再扣一次"


def test_more_episodes_cost_more(client, conn, storage, project, credits):
    project_id, user_id, token = project
    assets = [_asset(conn, storage, project_id) for _ in range(3)]
    client.post(
        "/api/v1/tasks",
        headers=auth(token),
        json={"asset_ids": [a["id"] for a in assets], "recipe_id": "drama.no_edit.generic"},
    )
    assert credits.holds[0]["amount"] == 15  # 3 集 × 5


def test_configured_requires_both_url_and_secret():
    """没配齐就当作「没接计费」：既不扣费，也不会拿空密钥去调内部接口。

    测的是模块级 `is_configured`（纯函数），不受 client 夹具打桩的影响。
    """
    assert credit_client.is_configured(None, None) is False
    assert credit_client.is_configured("", "secret") is False
    assert credit_client.is_configured("http://commerce", "") is False
    assert credit_client.is_configured("http://commerce", "   ") is False
    assert credit_client.is_configured("http://commerce", "secret") is True
