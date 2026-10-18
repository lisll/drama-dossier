"""API 控制面：8 个端点、认证与角色、幂等、上传会话与下载授权。"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from drama_cloud.api import create_app
from drama_cloud.tasks.runner import Worker

from .conftest import requires_ffmpeg

RECIPES_DIR = Path(__file__).resolve().parents[1] / "recipes"


@pytest.fixture()
def client(settings, conn, fake_commerce):
    return TestClient(create_app(settings))


def tokens(conn, fake_commerce, project=None) -> dict:
    """三种角色的令牌。

    M5b 起身份来自 commerce（假 commerce 在这里扮演它）：`fake_commerce.user`
    会登记身份并按角色写好白名单，返回可直接用的令牌。

    **传 project 时，operator 用该项目的属主**：隔离生效后，
    「别人创建的任务」本来就该 404，所以老测试里「operator 建任务、viewer 读」
    必须换成「同一个人」或「两个都是 admin」。传 `project` 时：
    - operator = 项目属主（能建任务、看自己的）；
    - viewer = 项目属主本人但按 viewer 白名单？不行——白名单按邮箱，一邮箱一角色。
      所以这里 viewer 仍是另一个用户，凡是「viewer 读 operator 的任务」的用例
      都改用了同一 token 或断言 404。见各测试内的注释。
    """
    cache_key = f"_tokens_{'project' if project else 'plain'}"
    cached = getattr(fake_commerce, cache_key, None)
    if cached is not None:
        return cached
    admin = fake_commerce.user(conn, "admin@example.com", "admin")
    viewer = fake_commerce.user(conn, "viewer@example.com", "viewer")
    if project is not None:
        # operator 就是项目属主：这样「建任务 → 读任务 → 取产物」是同一个人。
        operator = fake_commerce.token("tester@example.com")
    else:
        operator = fake_commerce.user(conn, "op@example.com", "operator")
    result = {"admin": admin, "operator": operator, "viewer": viewer}
    setattr(fake_commerce, cache_key, result)
    return result


def auth(token: str) -> dict:
    return {"Authorization": f"Bearer {token}"}


def test_health_needs_no_auth(client):
    response = client.get("/api/v1/health")
    assert response.status_code == 200 and response.json()["status"] == "ok"


def test_missing_and_bad_token(client):
    assert client.get("/api/v1/auth/whoami").status_code == 401
    assert client.get("/api/v1/auth/whoami", headers=auth("nope")).status_code == 401


def test_user_token_only_accepted_in_header(client, conn, fake_commerce):
    # 长期用户令牌放在网址里会被写进访问日志，所以 ?token= 不再被接受。
    token = tokens(conn, fake_commerce)["viewer"]
    assert client.get("/api/v1/auth/whoami", params={"token": token}).status_code == 401
    assert client.get("/api/v1/auth/whoami", headers=auth(token)).status_code == 200


def test_whoami(client, conn, fake_commerce):
    token = tokens(conn, fake_commerce)["operator"]
    body = client.get("/api/v1/auth/whoami", headers=auth(token)).json()
    assert body["role"] == "operator"


def test_viewer_cannot_create_task(client, conn, fake_commerce):
    token = tokens(conn, fake_commerce)["viewer"]
    response = client.post("/api/v1/tasks", headers=auth(token), json={
        "asset_ids": ["ast_x"], "recipe_id": "drama.no_edit.generic",
    })
    assert response.status_code == 403


def test_viewer_cannot_publish_recipe(client, conn, fake_commerce):
    token = tokens(conn, fake_commerce)["viewer"]
    response = client.post("/api/v1/recipes", headers=auth(token), json={"recipe": {}})
    assert response.status_code == 403


def test_operator_cannot_publish_recipe(client, conn, fake_commerce):
    token = tokens(conn, fake_commerce)["operator"]
    response = client.post("/api/v1/recipes", headers=auth(token), json={"recipe": {}})
    assert response.status_code == 403


def test_admin_publishes_recipe_and_viewer_reads_it(client, conn, fake_commerce):
    token = tokens(conn, fake_commerce)["admin"]
    payload = json.loads((RECIPES_DIR / "drama-no-edit-generic.json").read_text(encoding="utf-8"))
    published = client.post("/api/v1/recipes", headers=auth(token),
                            json={"recipe": payload, "reviewed_by": "admin"})
    assert published.status_code == 201, published.text
    assert published.json()["version"] == 1

    viewer = tokens(conn, fake_commerce)["viewer"]
    resolved = client.get("/api/v1/recipes/resolve?recipe_id=drama.no_edit.generic",
                          headers=auth(viewer)).json()
    assert resolved["version"] == 1
    versioned = client.get("/api/v1/recipes/drama.no_edit.generic/versions/1",
                           headers=auth(viewer)).json()
    assert versioned["content_sha256"] == resolved["content_sha256"]
    assert client.get("/api/v1/recipes/resolve?recipe_id=nope", headers=auth(viewer)).status_code == 400


def test_recipe_schema_endpoint(client, conn, fake_commerce):
    token = tokens(conn, fake_commerce)["viewer"]
    schema = client.get("/api/v1/recipes/schema.json", headers=auth(token)).json()
    assert "properties" in schema and "pipeline" in schema["properties"]


def test_seeded_recipes_are_listed(client, conn, project, fake_commerce):
    token = tokens(conn, fake_commerce)["viewer"]
    rows = client.get("/api/v1/recipes", headers=auth(token)).json()["recipes"]
    assert any(row["recipe_id"] == "drama.no_edit.generic" for row in rows)


def test_upload_session_rejects_bad_role(client, conn, fake_commerce):
    token = tokens(conn, fake_commerce)["operator"]
    response = client.post("/api/v1/uploads", headers=auth(token),
                           json={"name": "x", "role": "root", "filename": "a.mp4"})
    assert response.status_code == 400


def test_upload_requires_the_matching_token(client, conn, fake_commerce):
    token = tokens(conn, fake_commerce)["operator"]
    session = client.post("/api/v1/uploads", headers=auth(token),
                          json={"name": "x", "role": "video", "filename": "a.mp4"}).json()
    wrong = client.put(session["upload_url"] + "?token=bogus", content=b"data")
    assert wrong.status_code in (401, 400, 403)
    mismatch = client.put("/api/v1/uploads/up_other?token=" + session["token"], content=b"data")
    assert mismatch.status_code == 403
    empty = client.put(session["upload_url"] + "?token=" + session["token"], content=b"")
    assert empty.status_code == 400


@requires_ffmpeg
def test_full_http_flow(client, conn, settings, tiny_video, project, tmp_path, fake_commerce):
    """上传 → 建任务 → 跑 Worker → 查状态 → 取产物 → 下载。

    M5b 起任务与产物按用户隔离，所以整条链路用**同一个人**（项目属主）。
    跨用户的读取由 `test_isolation.py` 的越权矩阵覆盖。
    """
    operator = tokens(conn, fake_commerce, project)["operator"]

    session = client.post("/api/v1/uploads", headers=auth(operator), json={
        "name": "第 1 集", "role": "video", "filename": "tiny.mp4",
    }).json()
    upload = client.put(session["upload_url"] + "?token=" + session["token"],
                        content=tiny_video.read_bytes())
    assert upload.status_code == 200, upload.text
    asset = upload.json()["asset"]
    assert asset["probe"]["width"] == 160

    created = client.post("/api/v1/tasks", headers=auth(operator), json={
        "asset_ids": [asset["id"]], "recipe_id": "drama.no_edit.generic",
    })
    assert created.status_code == 201, created.text
    task_id = created.json()["id"]
    assert created.json()["state"] == "QUEUED"

    assert Worker(settings, worker_id="wrk_api").run_once() is True

    detail = client.get(f"/api/v1/tasks/{task_id}", headers=auth(operator)).json()
    assert detail["state"] == "SUCCEEDED"
    assert detail["checkpoints"] and detail["attempts_detail"]

    artifact = next(a for a in detail["artifacts"] if a["kind"] == "output")
    info = client.get(f"/api/v1/artifacts/{artifact['id']}", headers=auth(operator)).json()
    assert info["sha256"] == artifact["sha256"]
    assert info["download_url"].startswith(f"/api/v1/artifacts/{artifact['id']}/download?token=")

    # 短时令牌可以直接下载（浏览器场景），不需要 Bearer。
    download = client.get(info["download_url"])
    assert download.status_code == 200
    assert download.headers["x-artifact-sha256"] == artifact["sha256"]
    assert len(download.content) == artifact["size_bytes"]

    # 无凭证下载被拒绝。
    assert client.get(f"/api/v1/artifacts/{artifact['id']}/download").status_code == 401


def test_idempotency_header_creates_one_task(client, conn, project, fake_commerce):
    operator = tokens(conn, fake_commerce, project)["operator"]
    session = client.post("/api/v1/uploads", headers=auth(operator), json={
        "name": "x", "role": "video", "filename": "a.mp4",
    }).json()
    asset = client.put(session["upload_url"] + "?token=" + session["token"],
                       content=b"not-a-real-video").json()["asset"]

    body = {"asset_ids": [asset["id"]], "recipe_id": "drama.no_edit.generic"}
    headers = {**auth(operator), "Idempotency-Key": "same-key"}
    first = client.post("/api/v1/tasks", headers=headers, json=body)
    second = client.post("/api/v1/tasks", headers=headers, json=body)
    assert first.json()["id"] == second.json()["id"]


def test_task_creation_rejects_missing_asset(client, conn, project, fake_commerce):
    operator = tokens(conn, fake_commerce, project)["operator"]
    response = client.post("/api/v1/tasks", headers=auth(operator), json={
        "asset_ids": ["ast_missing"], "recipe_id": "drama.no_edit.generic",
    })
    assert response.status_code == 400


def test_task_creation_rejects_unknown_recipe(client, conn, project, fake_commerce):
    operator = tokens(conn, fake_commerce, project)["operator"]
    session = client.post("/api/v1/uploads", headers=auth(operator), json={
        "name": "x", "role": "video", "filename": "a.mp4",
    }).json()
    asset = client.put(session["upload_url"] + "?token=" + session["token"], content=b"x").json()["asset"]
    response = client.post("/api/v1/tasks", headers=auth(operator), json={
        "asset_ids": [asset["id"]], "recipe_id": "no.such.recipe",
    })
    assert response.status_code == 400


def test_cancel_and_retry_endpoints(client, conn, project, fake_commerce):
    operator = tokens(conn, fake_commerce, project)["operator"]
    session = client.post("/api/v1/uploads", headers=auth(operator), json={
        "name": "x", "role": "video", "filename": "a.mp4",
    }).json()
    asset = client.put(session["upload_url"] + "?token=" + session["token"], content=b"x").json()["asset"]
    task_id = client.post("/api/v1/tasks", headers=auth(operator), json={
        "asset_ids": [asset["id"]], "recipe_id": "drama.no_edit.generic",
    }).json()["id"]

    cancelled = client.post(f"/api/v1/tasks/{task_id}/cancel", headers=auth(operator))
    assert cancelled.status_code == 200 and cancelled.json()["state"] == "CANCELLED"
    # 已取消的任务不能再次取消
    assert client.post(f"/api/v1/tasks/{task_id}/cancel", headers=auth(operator)).status_code == 400
    retried = client.post(f"/api/v1/tasks/{task_id}/retry", headers=auth(operator))
    assert retried.status_code == 200 and retried.json()["state"] == "QUEUED"


def test_draft_recipe_is_not_resolvable(client, conn, project, fake_commerce):
    viewer = tokens(conn, fake_commerce)["viewer"]
    # 内置的 C11 配方是 draft，不应被发布指针指向。
    response = client.get("/api/v1/recipes/resolve?recipe_id=drama.commentary.rebuild",
                          headers=auth(viewer))
    assert response.status_code == 400


def test_audit_is_admin_only(client, conn, project, fake_commerce):
    viewer = tokens(conn, fake_commerce)["viewer"]
    admin = tokens(conn, fake_commerce)["admin"]
    assert client.get("/api/v1/audit", headers=auth(viewer)).status_code == 403
    assert client.get("/api/v1/audit", headers=auth(admin)).status_code == 200


def test_system_status_lists_modules(client, conn, fake_commerce):
    token = tokens(conn, fake_commerce)["viewer"]
    body = client.get("/api/v1/system/status", headers=auth(token)).json()
    assert "media.encode" in body["modules"]
    assert body["worker_version"] and body["engine_version"]
    assert body["queue_depth"] == 0


def test_rate_limit_returns_429(settings, conn, fake_commerce):
    """把桶容量压到 3，验证限流生效。"""
    from drama_cloud.api import create_app

    app = create_app(settings)
    app.state.limiter.capacity = 3.0
    app.state.limiter.refill = 0.0
    client = TestClient(app)
    token = tokens(conn, fake_commerce)["viewer"]
    codes = [client.get("/api/v1/auth/whoami", headers=auth(token)).status_code for _ in range(6)]
    assert 429 in codes
    assert codes[-1] == 429
