"""人工复核队列（确认流分支B）：服务层流程 + API 端点与权限。"""

from __future__ import annotations

import pytest
from fastapi.testclient import TestClient

from drama_cloud.api import create_app
from drama_cloud.errors import ErrorClass, WorkError
from drama_cloud.tasks import review as review_service
from drama_cloud.tasks import service

from .helpers import make_task


@pytest.fixture()
def client(settings, conn, fake_commerce):
    return TestClient(create_app(settings))


def tokens(conn, fake_commerce, project=None) -> dict:
    """三种角色的令牌（身份来自假 commerce，角色由本地白名单决定）。

    传 `project` 时 operator 就是**项目属主**：隔离生效后，
    「读自己的复核记录」必须用属主的身份。
    """
    cache_key = f"_review_tokens_{'project' if project else 'plain'}"
    cached = getattr(fake_commerce, cache_key, None)
    if cached is not None:
        return cached
    admin = fake_commerce.user(conn, "admin@example.com", "admin")
    viewer = fake_commerce.user(conn, "viewer@example.com", "viewer")
    if project is not None:
        operator = fake_commerce.token("tester@example.com")
    else:
        operator = fake_commerce.user(conn, "op@example.com", "operator")
    result = {"admin": admin, "operator": operator, "viewer": viewer}
    setattr(fake_commerce, cache_key, result)
    return result


def auth(token: str) -> dict:
    return {"Authorization": f"Bearer {token}"}


def failed_task(conn, settings, storage, project_id) -> str:
    """造一个「自动校验失败」的任务：没有产物、只有一份待复核记录。"""
    task = make_task(conn, settings, storage, project_id, recipe_id="drama.logo.erase")
    conn.execute(
        "UPDATE tasks SET state = 'FAILED', error_class = %s, error_message = %s, "
        "attempts = 3, finished_at = '2026-01-01T00:00:00+00:00' WHERE id = %s",
        (str(ErrorClass.QUALITY_RETRYABLE), "擦除确认流未通过", task["id"]),
    )
    return task["id"]


def test_review_lifecycle_reject_then_rerun(conn, settings, storage, project):
    project_id, _, _ = project
    task_id = failed_task(conn, settings, storage, project_id)
    item = review_service.create_review(
        conn, task_id, review_service.REASON_AUTO_FAILED, severity="high",
        detail={"inpaint": {"summary": {"verdict": "fail"}}, "suggestions": ["加大 dilate"]},
        actor="worker", 
    )
    assert item["status"] == "pending"
    assert review_service.review_summary(conn, task_id)["pending"] == 1

    claimed = review_service.claim(conn, item["id"], actor="reviewer")
    assert claimed["claimed_by"] == "reviewer"

    decided = review_service.decide(conn, item["id"], "reject", note="残留明显", actor="reviewer")
    assert decided["status"] == "rejected"

    result = review_service.rerun(
        conn, settings, item["id"],
        user_params={"erase": {"dilate": 6, "regions": [{"x": 10, "y": 20, "width": 30, "height": 40}]}},
        note="修正 mask 后重跑", actor="reviewer",
    )
    assert result["review"]["status"] == "superseded"
    new_task = service.get_task(conn, result["task"]["id"], include_plan=True)
    assert new_task["id"] != task_id
    assert new_task["state"] == "QUEUED"

    steps = {step["id"]: step for step in new_task["plan"]["steps"]}
    assert steps["erase"]["params"]["dilate"] == 6
    assert steps["erase"]["params"]["regions"][0]["x"] == 10
    assert new_task["recipe_id"] == "drama.logo.erase"

    # 原任务与原始参数不被改写：重跑是「新的一次」，不是「覆盖这一次」。
    original = service.get_task(conn, task_id)
    assert original["state"] == "FAILED"
    assert original["review"]["status"] == "superseded"

    events = [row["action"] for row in conn.execute(
        "SELECT action FROM audit_events WHERE subject = %s ORDER BY id", (item["id"],)
    ).fetchall()]
    assert events == ["review.enqueue", "review.claim", "review.reject", "review.rerun"]


def test_approve_needs_an_artifact_and_cannot_repeat(conn, settings, storage, project):
    project_id, _, _ = project
    task_id = failed_task(conn, settings, storage, project_id)
    item = review_service.create_review(conn, task_id, review_service.REASON_AUTO_FAILED)

    with pytest.raises(WorkError) as error:
        review_service.decide(conn, item["id"], "approve")
    assert error.value.error_class is ErrorClass.PARAM_OUT_OF_RANGE

    review_service.decide(conn, item["id"], "reject", actor="reviewer")
    with pytest.raises(WorkError):
        review_service.decide(conn, item["id"], "reject", actor="reviewer")


def test_pending_review_is_deduplicated_per_task_and_reason(conn, settings, storage, project):
    project_id, _, _ = project
    task_id = failed_task(conn, settings, storage, project_id)
    first = review_service.create_review(conn, task_id, review_service.REASON_AUTO_FAILED)
    second = review_service.create_review(conn, task_id, review_service.REASON_AUTO_FAILED)
    assert first["id"] == second["id"]
    assert len(review_service.list_reviews(conn, status="pending", task_id=task_id)) == 1


def test_worker_reports_are_visible_on_the_task(conn, settings, storage, project):
    project_id, _, _ = project
    task_id = failed_task(conn, settings, storage, project_id)
    review_service.create_review(conn, task_id, review_service.REASON_AUTO_FAILED, severity="high")
    detail = service.get_task(conn, task_id)
    assert detail["review"]["pending"] == 1
    assert detail["review"]["items"][0]["severity"] == "high"


# ---------- API ----------


def test_review_endpoints_and_roles(client, conn, settings, storage, project, fake_commerce):
    """复核队列的读、裁决、重跑，以及角色边界。

    M5b 起按用户隔离：这里用项目属主（operator）来读自己的复核记录；
    viewer 是**另一个用户**，它看不到这条记录（404），而不是 403——
    按「不存在」处理才不泄露别人的记录是否存在。
    """
    project_id, _, _ = project
    token = tokens(conn, fake_commerce, project)
    task_id = failed_task(conn, settings, storage, project_id)
    item = review_service.create_review(
        conn, task_id, review_service.REASON_AUTO_FAILED, severity="high", actor="worker"
    )

    listing = client.get("/api/v1/reviews", headers=auth(token["operator"]),
                         params={"status": "pending"})
    assert listing.status_code == 200
    assert [row["id"] for row in listing.json()["reviews"]] == [item["id"]]

    assert client.get(f"/api/v1/reviews/{item['id']}",
                      headers=auth(token["operator"])).status_code == 200

    # 别的用户看不到这条记录（按不存在处理）。
    assert client.get(f"/api/v1/reviews/{item['id']}",
                      headers=auth(token["viewer"])).status_code == 404

    # 裁决是运维动作，只读角色不能做。
    assert client.post(f"/api/v1/reviews/{item['id']}/decision", headers=auth(token["viewer"]),
                       json={"decision": "reject"}).status_code in (403, 404)

    rerun = client.post(f"/api/v1/reviews/{item['id']}/rerun", headers=auth(token["operator"]),
                        json={"user_params": {"probe": {}}, "note": "再跑一次"})
    assert rerun.status_code == 201
    body = rerun.json()
    assert body["review"]["status"] == "superseded"
    assert body["task"]["state"] == "QUEUED"

    status = client.get("/api/v1/system/status", headers=auth(token["viewer"])).json()
    assert status["reviews_pending"] == 0


def test_task_endpoint_exposes_review_projection(client, conn, settings, storage, project, fake_commerce):
    project_id, _, _ = project
    token = tokens(conn, fake_commerce, project)
    task_id = failed_task(conn, settings, storage, project_id)
    review_service.create_review(conn, task_id, review_service.REASON_AUTO_FAILED)

    body = client.get(f"/api/v1/tasks/{task_id}", headers=auth(token["operator"])).json()
    assert body["review"]["pending"] == 1
    assert body["review"]["pending_ids"]
