"""处理配方的安全测试：只能消费一次、过期拒绝、别人的配方和别的电脑拒绝；
参数都在该档上下限内，同一种子算出同样的参数；签名用途是 recipe，授权凭证的验签器不认它。"""

from __future__ import annotations

import json

import pytest

from drama_cloud.commerce import license_token as lt
from drama_cloud.commerce.platform_specs import PLATFORM_SPECS
from drama_cloud.commerce.recipes import QUALITY_FLOORS, RECIPE_TTL_SECONDS, STRENGTHS, derive_params

from .conftest import activate, auth, device_id, new_cards

EMAIL = "recipe@example.com"
OTHER_EMAIL = "other@example.com"
PASSWORD = "correct-horse-1"
PC = device_id("pc-1")
PC2 = device_id("pc-2")
TASK = "c" * 64


def _entitled(client, register, pg, email: str, devices=(PC,)) -> dict:
    body = register(email, PASSWORD)
    token = body["tokens"]["access_token"]
    for device in devices:
        assert activate(client, token, device).status_code == 200
    [code] = new_cards(pg, days=30)
    assert client.post("/api/v1/cards/redeem", json={"code": code}, headers=auth(token)).status_code == 200
    return {"id": body["user"]["id"], "token": token}


@pytest.fixture()
def account(client, register, pg):
    # 默认每个账号只能绑一台电脑（device_limit = 1），这里只绑 PC。
    return _entitled(client, register, pg, EMAIL)


def issue(client, token: str, device: str = PC, task: str = TASK, platform: str = "universal",
          strength: str = "medium"):
    return client.post("/api/v1/recipes", headers=auth(token),
                       json={"device_id": device, "task": task, "platform": platform, "strength": strength})


def consume(client, token: str, recipe_id: str, device: str = PC):
    return client.post(f"/api/v1/recipes/{recipe_id}/consume", headers=auth(token), json={"device_id": device})


def _payload(token: str) -> dict:
    return json.loads(lt.b64url_decode(token.split(".")[0]))


def test_issue_signs_bound_recipe(client, pg, signer, account):
    response = issue(client, account["token"])
    assert response.status_code == 200, response.text
    body = response.json()
    payload = _payload(body["token"])
    assert payload["v"] == 1 and payload["kid"] == signer.kid
    assert payload["rid"] == body["recipe_id"]
    assert payload["sub"] == account["id"] and payload["dev"] == PC and payload["task"] == TASK
    assert payload["platform"] == "universal" and payload["strength"] == "medium"
    assert payload["exp"] - payload["iat"] == RECIPE_TTL_SECONDS
    assert payload["params"] == body["params"] == derive_params("universal", "medium", payload["seed"])
    # 只能用 recipe 前缀验过：授权凭证的验签器（同样是 载荷.签名 写法）不认它。
    with pytest.raises(lt.LicenseError) as caught:
        lt.verify(body["token"], [signer.public_key], PC, payload["iat"])
    assert caught.value.code == "bad_signature"
    # 库里记着种子，处理结果可以复现；没有剧目内容可记。
    row = pg.execute("SELECT * FROM recipes WHERE id = %s::uuid", (body["recipe_id"],)).fetchone()
    assert row["seed"] == payload["seed"] and row["consumed_at"] is None and row["task"] == TASK


def test_consume_only_once(client, pg, account):
    recipe_id = issue(client, account["token"]).json()["recipe_id"]
    first = consume(client, account["token"], recipe_id)
    assert first.status_code == 200, first.text
    assert first.json() == {"recipe_id": recipe_id, "consumed": True}
    second = consume(client, account["token"], recipe_id)
    assert second.status_code == 409
    assert second.json()["error"] == "recipe_consumed"
    consumed = pg.execute("SELECT consumed_at FROM recipes WHERE id = %s::uuid", (recipe_id,)).fetchone()
    assert consumed["consumed_at"] is not None


def test_consume_expired_is_410(client, pg, account):
    recipe_id = issue(client, account["token"]).json()["recipe_id"]
    pg.execute("UPDATE recipes SET issued_at = now() - interval '2 days', expires_at = now() - interval '1 second'"
               " WHERE id = %s::uuid", (recipe_id,))
    response = consume(client, account["token"], recipe_id)
    assert response.status_code == 410
    assert response.json()["error"] == "recipe_expired"
    assert pg.execute("SELECT consumed_at FROM recipes WHERE id = %s::uuid",
                      (recipe_id,)).fetchone()["consumed_at"] is None


def test_consume_on_another_computer_is_rejected(client, pg, account):
    # 配方是发给 PC 的；拿着配方编号从 PC2 来消费，先按机器码拒绝（在查设备绑定之前）。
    recipe_id = issue(client, account["token"], device=PC).json()["recipe_id"]
    response = consume(client, account["token"], recipe_id, device=PC2)
    assert response.status_code == 403
    assert response.json()["error"] == "wrong_device"
    assert consume(client, account["token"], recipe_id, device=PC).status_code == 200


def test_someone_elses_recipe_looks_like_it_does_not_exist(client, register, pg, account):
    recipe_id = issue(client, account["token"]).json()["recipe_id"]
    other = _entitled(client, register, pg, OTHER_EMAIL, devices=(device_id("pc-other"),))
    stolen = consume(client, other["token"], recipe_id, device=PC)
    missing = consume(client, other["token"], "00000000-0000-4000-8000-000000000000", device=PC)
    assert stolen.status_code == missing.status_code == 404
    assert stolen.json() == missing.json()
    # 主人还能正常用：别人的尝试没有把它用掉。
    assert consume(client, account["token"], recipe_id).status_code == 200


def test_consume_rechecks_entitlement(client, pg, account):
    recipe_id = issue(client, account["token"]).json()["recipe_id"]
    pg.execute("UPDATE entitlements SET expires_at = now() - interval '1 second' WHERE user_id = %s::uuid",
               (account["id"],))
    response = consume(client, account["token"], recipe_id)
    assert response.status_code == 403
    assert response.json()["error"] == "entitlement_required"


def test_issue_requires_bound_device_and_entitlement(client, register, pg):
    body = register(EMAIL, PASSWORD)
    token = body["tokens"]["access_token"]
    assert activate(client, token, PC).status_code == 200
    no_entitlement = issue(client, token)
    assert no_entitlement.status_code == 403 and no_entitlement.json()["error"] == "entitlement_required"
    unbound = issue(client, token, device=device_id("never-bound"))
    assert unbound.status_code == 403 and unbound.json()["error"] == "device_not_bound"
    assert pg.execute("SELECT count(*) AS n FROM recipes").fetchone()["n"] == 0


# ---------- 参数 ----------

def test_old_platform_douyin_is_rejected(client, pg, account):
    """平台只有 universal 一个取值：旧取值按参数不合法拒掉，而且不往库里写。"""
    response = issue(client, account["token"], platform="douyin")
    assert response.status_code == 422, response.text
    assert response.json()["error"] == "invalid_request"
    assert pg.execute("SELECT count(*) AS n FROM recipes").fetchone()["n"] == 0


@pytest.mark.parametrize("strength", sorted(STRENGTHS))
@pytest.mark.parametrize("platform", sorted(PLATFORM_SPECS))
def test_params_stay_within_bounds(platform, strength):
    for seed in range(300):
        params = derive_params(platform, strength, seed)
        assert params["platform"] == PLATFORM_SPECS[platform]
        min_ssim, min_psnr = QUALITY_FLOORS[strength]
        assert params["quality"] == {"min_ssim": min_ssim, "min_psnr": min_psnr}
        assert set(params["subtle"]) == set(STRENGTHS[strength])
        for name, (low, high) in STRENGTHS[strength].items():
            value = params["subtle"][name]
            magnitude = abs(value) if name in ("brightness", "hue_degrees", "pitch_semitones") else value
            assert low <= magnitude <= high, f"{platform}/{strength} seed={seed} {name}={value}"


def test_same_seed_same_params_and_seeds_differ():
    assert derive_params("universal", "heavy", 42) == derive_params("universal", "heavy", 42)
    assert derive_params("universal", "heavy", 42) != derive_params("universal", "heavy", 43)


def test_issued_seeds_are_random(client, account):
    seeds = {_payload(issue(client, account["token"]).json()["token"])["seed"] for _ in range(5)}
    assert len(seeds) == 5
