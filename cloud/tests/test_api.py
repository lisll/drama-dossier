"""API 控制面：8 个端点、认证与角色、幂等、上传会话与下载授权。"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from fastapi.testclient import TestClient

from drama_cloud.api import create_app
from drama_cloud.auth import create_user
from drama_cloud.tasks.runner import Worker

from .conftest import requires_ffmpeg

RECIPES_DIR = Path(__file__).resolve().parents[1] / "recipes"


@pytest.fixture()
def client(settings, conn):
    return TestClient(create_app(settings))


def tokens(conn) -> dict:
    return {
        "admin": create_user(conn, "admin", role="admin", actor="test"),
        "operator": create_user(conn, "op", role="operator", actor="test"),
        "viewer": create_user(conn, "viewer", role="viewer", actor="test"),
    }


def auth(token: str) -> dict:
    return {"Authorization": f"Bearer {token}"}


def test_health_needs_no_auth(client):
    response = client.get("/api/v1/health")
    assert response.status_code == 200 and response.json()["status"] == "ok"


def test_missing_and_bad_token(client):
    assert client.get("/api/v1/auth/whoami").status_code == 401
    assert client.get("/api/v1/auth/whoami", headers=auth("nope")).status_code == 401


def test_whoami(client, conn):
    token = tokens(conn)["operator"]
    body = client.get("/api/v1/auth/whoami", headers=auth(token)).json()
    assert body["role"] == "operator"


def test_viewer_cannot_create_task(client, conn):
    token = tokens(conn)["viewer"]
    response = client.post("/api/v1/tasks", headers=auth(token), json={
        "asset_ids": ["ast_x"], "recipe_id": "drama.no_edit.generic",
    })
    assert response.status_code == 403


def test_viewer_cannot_publish_recipe(client, conn):
    token = tokens(conn)["viewer"]
    response = client.post("/api/v1/recipes", headers=auth(token), json={"recipe": {}})
    assert response.status_code == 403


def test_operator_cannot_publish_recipe(client, conn):
    token = tokens(conn)["operator"]
    response = client.post("/api/v1/recipes", headers=auth(token), json={"recipe": {}})
    assert response.status_code == 403


def test_admin_publishes_recipe_and_viewer_reads_it(client, conn):
    token = tokens(conn)["admin"]
    payload = json.loads((RECIPES_DIR / "drama-no-edit-generic.json").read_text(encoding="utf-8"))
    published = client.post("/api/v1/recipes", headers=auth(token),
                            json={"recipe": payload, "reviewed_by": "admin"})
    assert published.status_code == 201, published.text
    assert published.json()["version"] == 1

    viewer = tokens(conn)["viewer"]
    resolved = client.get("/api/v1/recipes/resolve?recipe_id=drama.no_edit.generic",
                          headers=auth(viewer)).json()
    assert resolved["version"] == 1
    versioned = client.get("/api/v1/recipes/drama.no_edit.generic/versions/1",
                           headers=auth(viewer)).json()
    assert versioned["content_sha256"] == resolved["content_sha256"]
    assert client.get("/api/v1/recipes/resolve?recipe_id=nope", headers=auth(viewer)).status_code == 400


def test_recipe_schema_endpoint(client, conn):
    token = tokens(conn)["viewer"]
    schema = client.get("/api/v1/recipes/schema.json", headers=auth(token)).json()
    assert "properties" in schema and "pipeline" in schema["properties"]


def test_seeded_recipes_are_listed(client, conn, project):
    token = tokens(conn)["viewer"]
    rows = client.get("/api/v1/recipes", headers=auth(token)).json()["recipes"]
    assert any(row["recipe_id"] == "drama.no_edit.generic" for row in rows)


def test_upload_session_rejects_bad_role(client, conn):
    token = tokens(conn)["operator"]
    response = client.post("/api/v1/uploads", headers=auth(token),
                           json={"name": "x", "role": "root", "filename": "a.mp4"})
    assert response.status_code == 400


def test_upload_requires_the_matching_token(client, conn):
    token = tokens(conn)["operator"]
    session = client.post("/api/v1/uploads", headers=auth(token),
                          json={"name": "x", "role": "video", "filename": "a.mp4"}).json()
    wrong = client.put(session["upload_url"] + "?token=bogus", content=b"data")
    assert wrong.status_code in (401, 400, 403)
    mismatch = client.put("/api/v1/uploads/up_other?token=" + session["token"], content=b"data")
    assert mismatch.status_code == 403
    empty = client.put(session["upload_url"] + "?token=" + session["token"], content=b"")
    assert empty.status_code == 400


@requires_ffmpeg
def test_full_http_flow(client, conn, settings, tiny_video, project, tmp_path):
    """上传 → 建任务 → 跑 Worker → 查状态 → 取产物 → 下载。"""
    operator = tokens(conn)["operator"]
    viewer = tokens(conn)["viewer"]

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

    detail = client.get(f"/api/v1/tasks/{task_id}", headers=auth(viewer)).json()
    assert detail["state"] == "SUCCEEDED"
    assert detail["checkpoints"] and detail["attempts_detail"]

    artifact = next(a for a in detail["artifacts"] if a["kind"] == "output")
    info = client.get(f"/api/v1/artifacts/{artifact['id']}", headers=auth(viewer)).json()
    assert info["sha256"] == artifact["sha256"]
    assert info["download_url"].startswith(f"/api/v1/artifacts/{artifact['id']}/download?token=")

    # 短时令牌可以直接下载（浏览器场景），不需要 Bearer。
    download = client.get(info["download_url"])
    assert download.status_code == 200
    assert download.headers["x-artifact-sha256"] == artifact["sha256"]
    assert len(download.content) == artifact["size_bytes"]

    # 无凭证下载被拒绝。
    assert client.get(f"/api/v1/artifacts/{artifact['id']}/download").status_code == 401


def test_idempotency_header_creates_one_task(client, conn, project):
    operator = tokens(conn)["operator"]
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


def test_task_creation_rejects_missing_asset(client, conn, project):
    operator = tokens(conn)["operator"]
    response = client.post("/api/v1/tasks", headers=auth(operator), json={
        "asset_ids": ["ast_missing"], "recipe_id": "drama.no_edit.generic",
    })
    assert response.status_code == 400


def test_task_creation_rejects_unknown_recipe(client, conn, project):
    operator = tokens(conn)["operator"]
    session = client.post("/api/v1/uploads", headers=auth(operator), json={
        "name": "x", "role": "video", "filename": "a.mp4",
    }).json()
    asset = client.put(session["upload_url"] + "?token=" + session["token"], content=b"x").json()["asset"]
    response = client.post("/api/v1/tasks", headers=auth(operator), json={
        "asset_ids": [asset["id"]], "recipe_id": "no.such.recipe",
    })
    assert response.status_code == 400


def test_cancel_and_retry_endpoints(client, conn, project):
    operator = tokens(conn)["operator"]
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


def test_draft_recipe_is_not_resolvable(client, conn, project):
    viewer = tokens(conn)["viewer"]
    # 内置的 C11 配方是 draft，不应被发布指针指向。
    response = client.get("/api/v1/recipes/resolve?recipe_id=drama.commentary.rebuild",
                          headers=auth(viewer))
    assert response.status_code == 400


def test_audit_is_admin_only(client, conn, project):
    viewer = tokens(conn)["viewer"]
    admin = tokens(conn)["admin"]
    assert client.get("/api/v1/audit", headers=auth(viewer)).status_code == 403
    assert client.get("/api/v1/audit", headers=auth(admin)).status_code == 200


def test_system_status_lists_modules(client, conn):
    token = tokens(conn)["viewer"]
    body = client.get("/api/v1/system/status", headers=auth(token)).json()
    assert "media.encode" in body["modules"]
    assert body["worker_version"] and body["engine_version"]
    assert body["queue_depth"] == 0


def test_rate_limit_returns_429(settings, conn):
    """把桶容量压到 3，验证限流生效。"""
    from drama_cloud.api import create_app

    app = create_app(settings)
    app.state.limiter.capacity = 3.0
    app.state.limiter.refill = 0.0
    client = TestClient(app)
    token = tokens(conn)["viewer"]
    codes = [client.get("/api/v1/auth/whoami", headers=auth(token)).status_code for _ in range(6)]
    assert 429 in codes
    assert codes[-1] == 429


def test_web_client_has_csp_and_no_inline_script(client):
    """令牌存在 localStorage：页面必须带 CSP 禁止内联脚本，脚本只能来自 /static/app.js。"""
    page = client.get("/")
    assert page.status_code == 200
    csp = page.headers["content-security-policy"]
    assert "script-src 'self'" in csp and "frame-ancestors 'none'" in csp
    assert "unsafe-inline" not in csp.split("script-src", 1)[1].split(";", 1)[0]
    assert page.headers["x-content-type-options"] == "nosniff"
    html = page.text
    assert '<script src="/static/app.js"></script>' in html
    assert "<script>" not in html, "不得再有内联脚本"

    script = client.get("/static/app.js")
    assert script.status_code == 200
    assert "content-security-policy" in script.headers
    assert "function esc(" in script.text
    # 接口响应只带 nosniff，不带 CSP（CSP 只管 Web 客户端页面）
    health = client.get("/api/v1/health")
    assert health.headers["x-content-type-options"] == "nosniff"
    assert "content-security-policy" not in health.headers


def test_api_docs_page_is_not_blocked_by_csp(client):
    """/docs 要从 CDN 加载 Swagger 脚本并带内联初始化脚本，不能套上 Web 客户端的 CSP。"""
    docs = client.get("/docs")
    assert docs.status_code == 200
    assert "content-security-policy" not in docs.headers
    assert docs.headers["x-content-type-options"] == "nosniff"


# ---------- app.js 的 innerHTML 静态检查 ----------
#
# 规则：`innerHTML =` 右边（到语句结束）的每一个组成部分只能是
#   - 字符串字面量（'...' / "..."），
#   - 模板字符串，且其中每个 ${...} 本身也满足这条规则，
#   - esc(...) 调用，
#   - 三元表达式 `条件 ? A : B`，其中 A、B 满足这条规则（条件部分不进入 HTML，不检查），
# 各部分之间用 `+` 拼接。整个文件禁止 insertAdjacentHTML / outerHTML 赋值 / document.write。

_QUOTES = "'\"`"


def _skip_literal(text, i):
    """i 指向开引号，返回闭引号之后的下标；模板里的 ${...} 整体跳过。"""
    quote, i = text[i], i + 1
    while i < len(text):
        ch = text[i]
        if ch == "\\":
            i += 2
            continue
        if quote == "`" and text.startswith("${", i):
            i = _match_brace(text, i + 1) + 1
            continue
        if ch == quote:
            return i + 1
        i += 1
    raise AssertionError("字符串没有闭合")


def _match_brace(text, i):
    """i 指向 `{`，返回与之配对的 `}` 下标。"""
    depth = 0
    while i < len(text):
        ch = text[i]
        if ch in _QUOTES:
            i = _skip_literal(text, i)
            continue
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    raise AssertionError("花括号没有闭合")


def _top_level(text, stop):
    """逐字符走 text，跳过字符串/模板/括号内部；遇到顶层的 stop 字符时产出其下标。"""
    depth, i = 0, 0
    while i < len(text):
        ch = text[i]
        if ch in _QUOTES:
            i = _skip_literal(text, i)
            continue
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
        elif ch in stop and depth == 0:
            yield i
        i += 1


def _statement_end(text, start):
    """从 start 起到顶层 `;` 的下标（没有就到末尾）。字符串里的 `;` 不算。"""
    for index in _top_level(text[start:], ";"):
        return start + index
    return len(text)


def _split_top(text, sep):
    parts, last = [], 0
    for index in _top_level(text, sep):
        parts.append(text[last:index])
        last = index + 1
    parts.append(text[last:])
    return parts


def _wrapped_by_parens(expr):
    """expr 是否整体被一对括号包住（而不是 `(a) + (b)` 这种）。"""
    if not (expr.startswith("(") and expr.endswith(")")):
        return False
    depth, i = 0, 0
    while i < len(expr):
        ch = expr[i]
        if ch in _QUOTES:
            i = _skip_literal(expr, i)
            continue
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
            if depth == 0:
                return i == len(expr) - 1
        i += 1
    return False


def _unsafe_parts(expr):
    """返回 expr 中不安全的组成部分（空列表表示安全）。"""
    expr = expr.strip()
    while _wrapped_by_parens(expr):
        expr = expr[1:-1].strip()
    if not expr:
        return []
    question = _split_top(expr, "?")
    if len(question) > 1:
        # 条件 ? A : B —— 只检查两个分支
        branches = _split_top("?".join(question[1:]), ":")
        if len(branches) != 2:
            return [expr]
        return _unsafe_parts(branches[0]) + _unsafe_parts(branches[1])
    pieces = _split_top(expr, "+")
    if len(pieces) > 1:
        return [bad for piece in pieces for bad in _unsafe_parts(piece)]
    if expr[0] in "'\"" and _skip_literal(expr, 0) == len(expr):
        return []
    if expr[0] == "`" and _skip_literal(expr, 0) == len(expr):
        bad, i = [], 1
        while i < len(expr) - 1:
            if expr[i] == "\\":
                i += 2
                continue
            if expr.startswith("${", i):
                close = _match_brace(expr, i + 1)
                bad += _unsafe_parts(expr[i + 2:close])
                i = close + 1
                continue
            i += 1
        return bad
    if expr.startswith("esc(") and _wrapped_by_parens(expr[3:]):
        return []
    return [expr]


# 正则字面量只可能出现在这些字符之后（表达式开始处）；出现在别处的 `/` 是除号。
_REGEX_PRECEDERS = "(,=:[!&|?{};+-*%<>~^"


def _skip_regex(source, i):
    """i 指向正则字面量开头的 `/`，返回 flags 之后的下标；字符类里的 `/` 不结束正则。"""
    in_class, j = False, i + 1
    while j < len(source):
        ch = source[j]
        if ch == "\\":
            j += 2
            continue
        if ch == "\n":
            break
        if ch == "[":
            in_class = True
        elif ch == "]":
            in_class = False
        elif ch == "/" and not in_class:
            j += 1
            while j < len(source) and source[j].isalpha():
                j += 1
            return j
        j += 1
    raise AssertionError("正则字面量没有闭合")


def _strip_comments(source):
    """去掉 // 与 /* */ 注释，把正则字面量换成占位（其中的引号不能被当成字符串开头）。"""
    out, i = [], 0
    while i < len(source):
        ch = source[i]
        if ch in _QUOTES:
            end = _skip_literal(source, i)
            out.append(source[i:end])
            i = end
            continue
        if ch == "/" and not source.startswith(("//", "/*"), i):
            previous = "".join(out).rstrip()[-1:]
            if previous == "" or previous in _REGEX_PRECEDERS:
                i = _skip_regex(source, i)
                out.append("/re/")
                continue
        if source.startswith("//", i):
            newline = source.find("\n", i)
            i = len(source) if newline < 0 else newline
            continue
        if source.startswith("/*", i):
            i = source.index("*/", i) + 2
            continue
        out.append(ch)
        i += 1
    return "".join(out)


def innerhtml_violations(source):
    import re

    code = _strip_comments(source)
    problems = [f"禁止使用 {sink}" for sink in ("insertAdjacentHTML", "document.write")
                if sink in code]
    if re.search(r"\.outerHTML\s*=(?!=)", code):
        problems.append("禁止给 outerHTML 赋值")
    for match in re.finditer(r"\.innerHTML\s*=(?!=)", code):
        rhs = code[match.end():_statement_end(code, match.end())]
        problems += [f"innerHTML 里有未转义的部分：{part}" for part in _unsafe_parts(rhs)]
    return problems


def test_innerhtml_checker_catches_unsafe_samples():
    """先证明检查器真的能抓到问题，再拿它检查 app.js。"""
    unsafe = [
        "el.innerHTML = `<td>${name}</td>`;",
        "el.innerHTML = '<b>' + error.message + '</b>';",
        "el.innerHTML = `<td>${esc(a) + b}</td>`;",
        "el.innerHTML = ok ? `<i>${x}</i>` : '';",
        "el.innerHTML = '<p>&amp;</p>' + raw;",
        "el.innerHTML = (esc(a)) + (b);",
        "el.insertAdjacentHTML('beforeend', html);",
        "el.outerHTML = html;",
    ]
    for sample in unsafe:
        assert innerhtml_violations(sample), f"应报出问题：{sample}"
    safe = [
        "el.innerHTML = '';",
        "el.innerHTML = `<td>${esc(a)}</td>` + '<td>&amp;</td>';",
        "el.innerHTML = '<span>' + esc(error.message) + '</span>';",
        "el.innerHTML = (task.error ? `<b>${esc(task.error.class)}</b>` : '') + '</div>';",
        "el.textContent = `${raw}`; // el.innerHTML = raw",
        "if (el.innerHTML === raw) {}",
        "const t = String(v).replace(/[&<>\"']/g, f); el.innerHTML = '<b>' + esc(v) + '</b>';",
        "el.innerHTML = `<td>${esc((a / 1048576).toFixed(1))} MB</td>`;",
    ]
    for sample in safe:
        assert innerhtml_violations(sample) == [], f"不应报错：{sample} → {innerhtml_violations(sample)}"


def test_web_client_escapes_everything_written_to_inner_html():
    """静态检查 app.js：写进 innerHTML 的动态值都必须经过 esc()。"""
    source = (Path(__file__).resolve().parents[1] / "src/drama_cloud/web/app.js").read_text(
        encoding="utf-8"
    )
    assert ".innerHTML" in source
    assert innerhtml_violations(source) == []
