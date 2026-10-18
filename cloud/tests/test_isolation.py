"""用户隔离的越权矩阵（M5b-3）。

**这是 M5b 最重要的一份测试。** 「一个用户看不到、也取不到别人的任务和产物」
是总方案给 M5b 的完成标准之一；漏一个接口就是一条数据泄露。

做法：造两个用户 A 与 B，A 名下有一套完整的资源（项目、素材、上传会话、任务、
检查点、产物、复核记录），然后让 B 拿**每一个真实存在的 id** 去逐个接口敲门。
每个都必须回 404（不是 403——不透露资源是否存在）。

另外三个方向也一起钉住：
* A 自己能拿到（否则不是隔离，是坏掉）；
* A 的列表里看不到 B 的东西；
* 过了短时下载令牌的 owner 校验（令牌是无状态签名，必须绑 owner）。
"""

from __future__ import annotations

import hashlib

import pytest
from fastapi.testclient import TestClient

from drama_cloud.api import create_app
from drama_cloud.tasks import review as review_service

from .helpers import make_asset, make_task


@pytest.fixture()
def client(settings, conn, fake_commerce):
    return TestClient(create_app(settings))


def auth(token: str) -> dict:
    return {"Authorization": f"Bearer {token}"}


@pytest.fixture()
def two_users(client, conn, fake_commerce, settings, storage):
    """两个用户，A 名下有一套完整资源。返回访问它们所需的全部 id 与令牌。"""
    from drama_cloud.auth import principal_for_email
    from drama_cloud.tasks import service

    # 发内置配方：建任务需要已发布的配方版本。
    from .conftest import _seed_recipes

    _seed_recipes(conn)
    a_id, a_token = fake_commerce.register("alice@example.com")
    b_id, b_token = fake_commerce.register("bob@example.com")
    # 建影子行（真实链路里由认证时的 principal_for_email 完成）。
    principal_for_email(conn, a_id, "alice@example.com")
    principal_for_email(conn, b_id, "bob@example.com")

    project_a = service.ensure_default_project(conn, a_id, name="A 的项目")
    asset = make_asset(conn, storage, project_a, name="a.mp4")
    task = make_task(conn, settings, storage, project_a, recipe_id="drama.no_edit.generic")

    # 人为放一条产物与一条复核记录，免得依赖真的跑完任务。
    artifact_id = "af_a_" + hashlib.sha256(b"a").hexdigest()[:16]
    conn.execute(
        "INSERT INTO artifacts (id, task_id, kind, storage_key, sha256, size_bytes, created_at)"
        " VALUES (%s,%s,'output',%s,%s,%s,%s)",
        (artifact_id, task["id"], f"artifacts/{task['id']}/out.mp4",
         "0" * 64, 123, "2026-01-01T00:00:00+00:00"),
    )
    review = review_service.create_review(
        conn, task["id"], review_service.REASON_AUTO_FAILED, severity="high", actor="worker"
    )

    # A 的一个分片上传会话
    upload = client.post(
        "/api/v1/assets/uploads",
        headers=auth(a_token),
        json={
            "name": "第 1 集", "role": "video", "filename": "a.mp4",
            "size_bytes": 1024, "sha256": "0" * 64, "chunk_size": 1024,
        },
    ).json()

    return {
        "a": {"id": a_id, "token": a_token, "project": project_a},
        "b": {"id": b_id, "token": b_token},
        "asset": asset["id"],
        "task": task["id"],
        "artifact": artifact_id,
        "review": review["id"],
        "upload": upload["upload_id"],
    }


#: B 拿着 A 的 id 去敲的每一个接口。用 (方法, 路径, 额外参数) 表示。
ATTACKS = [
    ("get", "/api/v1/tasks/{task}", {}),
    ("post", "/api/v1/tasks/{task}/cancel", {}),
    ("post", "/api/v1/tasks/{task}/retry", {}),
    ("get", "/api/v1/artifacts/{artifact}", {}),
    ("get", "/api/v1/artifacts/{artifact}/download", {}),
    ("get", "/api/v1/reviews/{review}", {}),
    ("post", "/api/v1/reviews/{review}/claim", {}),
    ("post", "/api/v1/reviews/{review}/decision", {"json": {"decision": "reject"}}),
    ("get", "/api/v1/assets/uploads/{upload}", {}),
    ("put", "/api/v1/assets/uploads/{upload}/chunks/0",
     {"content": b"x" * 1024, "headers": {"Content-Type": "application/octet-stream"}}),
    ("post", "/api/v1/assets/uploads/{upload}/complete", {}),
    ("delete", "/api/v1/assets/uploads/{upload}", {}),
]


@pytest.mark.parametrize("method,path,extra", ATTACKS, ids=[a[1] for a in ATTACKS])
def test_b_cannot_reach_a_resources(client, two_users, method, path, extra):
    """B 拿 A 的真实 id 逐个敲门：每一条都必须 404。"""
    a, b = two_users["a"], two_users["b"]
    url = path.format(
        task=two_users["task"],
        artifact=two_users["artifact"],
        review=two_users["review"],
        upload=two_users["upload"],
    )
    headers = auth(b["token"])
    if "headers" in extra:
        headers = {**headers, **extra["headers"]}
        extra = {k: v for k, v in extra.items() if k != "headers"}
    response = getattr(client, method)(url, headers=headers, **extra)
    assert response.status_code == 404, (
        f"{method.upper()} {url} 返回了 {response.status_code}，应当是 404（{response.text[:200]}）"
    )


def test_a_can_reach_his_own_resources(client, two_users):
    """反向确认：不是全都被挡住了，A 自己必须能拿到。"""
    a = two_users["a"]
    headers = auth(a["token"])
    assert client.get(f"/api/v1/tasks/{two_users['task']}", headers=headers).status_code == 200
    assert client.get(f"/api/v1/artifacts/{two_users['artifact']}", headers=headers).status_code == 200
    assert client.get(f"/api/v1/reviews/{two_users['review']}", headers=headers).status_code == 200
    assert client.get(
        f"/api/v1/assets/uploads/{two_users['upload']}", headers=headers
    ).status_code == 200


def test_the_task_list_only_contains_my_own(client, two_users):
    a, b = two_users["a"], two_users["b"]
    mine = client.get("/api/v1/tasks", headers=auth(a["token"])).json()["tasks"]
    assert [task["id"] for task in mine] == [two_users["task"]]
    theirs = client.get("/api/v1/tasks", headers=auth(b["token"])).json()["tasks"]
    assert theirs == []


def test_the_asset_list_only_contains_my_own(client, two_users):
    """A 的列表里只出现自己的素材；B 的列表里一个都没有。

    不假设「A 只有一个素材」——造任务时也会登记素材，那是正常的。
    关键是**B 看不到 A 的任何东西**。
    """
    a, b = two_users["a"], two_users["b"]
    mine = client.get("/api/v1/assets", headers=auth(a["token"])).json()["assets"]
    assert mine, "A 至少应当看到自己登记的素材"
    assert two_users["asset"] in [asset["id"] for asset in mine]

    theirs = client.get("/api/v1/assets", headers=auth(b["token"])).json()["assets"]
    assert theirs == []


def test_the_review_list_only_contains_my_own(client, two_users):
    a, b = two_users["a"], two_users["b"]
    mine = client.get("/api/v1/reviews", headers=auth(a["token"])).json()["reviews"]
    assert [row["id"] for row in mine] == [two_users["review"]]
    theirs = client.get("/api/v1/reviews", headers=auth(b["token"])).json()["reviews"]
    assert theirs == []


def test_b_cannot_create_a_task_in_a_project(client, two_users):
    """B 拿着 A 的 project_id 建任务：必须 404（不能往别人的项目里塞任务）。"""
    a, b = two_users["a"], two_users["b"]
    response = client.post(
        "/api/v1/tasks",
        headers=auth(b["token"]),
        json={
            "asset_ids": [two_users["asset"]],
            "recipe_id": "drama.no_edit.generic",
            "project_id": a["project"],
        },
    )
    assert response.status_code == 404, response.text


def test_b_cannot_open_an_upload_in_a_project(client, two_users):
    """B 拿 A 的 project_id 开分片上传：必须 404。"""
    a, b = two_users["a"], two_users["b"]
    response = client.post(
        "/api/v1/assets/uploads",
        headers=auth(b["token"]),
        json={
            "name": "x", "role": "video", "filename": "x.mp4",
            "size_bytes": 1024, "sha256": "0" * 64, "chunk_size": 1024,
            "project_id": a["project"],
        },
    )
    assert response.status_code == 404, response.text


def test_the_download_token_is_bound_to_its_owner(client, two_users):
    """短时下载令牌是无状态签名：丢了谁拿到都能用，所以必须绑 owner。

    这里直接拿 A 签发的令牌去下载（不带 Bearer），确认能下；
    再伪造一个「别人的」令牌去下 A 的产物，确认下不到。
    """
    a = two_users["a"]
    info = client.get(
        f"/api/v1/artifacts/{two_users['artifact']}", headers=auth(a["token"])
    ).json()
    # A 自己的令牌能下（文件内容是假的，但请求本身应当被放行）。
    download = client.get(info["download_url"])
    assert download.status_code in (200, 410)  # 410 = 占位产物没有真实文件

    # 伪造一个 owner 不是 A 的令牌：sign_payload 用测试进程里的真实密钥签，
    # 所以签名有效、只有 owner 不对——这正是要挡住的情况。
    from drama_cloud.storage import LocalStorage

    storage = LocalStorage(client.app.state.settings)
    forged = storage.sign_payload(
        {"scope": "artifact", "artifact_id": two_users["artifact"], "owner": "someone-else"},
        ttl_seconds=900,
    )
    denied = client.get(
        f"/api/v1/artifacts/{two_users['artifact']}/download?token={forged}"
    )
    assert denied.status_code == 404, denied.text


def test_an_admin_does_not_override_isolation_for_tasks(client, two_users, conn, fake_commerce):
    """admin 也不跨用户读任务。

    这是刻意的：常规任务接口是「我的东西」的地方。运维需要的跨用户视图
    应当走单独的审计/管理接口，而不是把别人的任务混进这个列表——
    否则「我看到的都是我的」这个前提就不成立了。
    """
    admin_token = fake_commerce.user(conn, "root@example.com", "admin")
    response = client.get(f"/api/v1/tasks/{two_users['task']}", headers=auth(admin_token))
    assert response.status_code == 404, response.text
    listing = client.get("/api/v1/tasks", headers=auth(admin_token)).json()["tasks"]
    assert listing == []
