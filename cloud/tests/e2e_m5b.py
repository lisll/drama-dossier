"""M5b 端到端：真 commerce + 真媒体控制面 + 真 PostgreSQL + 真 ffmpeg。

把 M5b 的六件事串起来跑一遍，每一步都断言：

    注册/登录 → 同一个令牌进两个服务
    → 分片上传（传一半 → 查缺哪块 → 只补缺的 → 完成）
    → 建任务（预扣积分）→ Worker 真跑 → 成功结算
    → 取产物下载
    → 另一个用户拿同一个 id 去敲门 → 全 404
    → 造过期素材 → cleanup → 文件真的没了、元数据还在

库名随机，跑完自删；不动既有数据库。
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import secrets
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime, timedelta, timezone

import psycopg

ADMIN_DSN = "postgresql://drama:drama-dev-only@127.0.0.1:5432/postgres"
ROOT = os.path.dirname(os.path.abspath(__file__))
CLOUD = os.path.abspath(os.path.join(ROOT, ".."))
CODE_SECRET = "e2e-m5b7-code-secret-0123456789abcdef"
INTERNAL_SECRET = "e2e-m5b7-internal-secret-0123456789"
FAILURES: list[str] = []


def check(condition, label):
    if condition:
        print(f"  ✓ {label}")
    else:
        print(f"  ✗ {label}")
        FAILURES.append(label)


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def call(base, method, path, body=None, token=None, headers=None, raw=False):
    url = path if path.startswith("http") else base + path
    data = None
    head = dict(headers or {})
    if body is not None:
        data = json.dumps(body).encode("utf-8")
        head["Content-Type"] = "application/json"
    if token:
        head["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(url, data=data, headers=head, method=method)
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            text = response.read()
            if raw:
                return response.status, text
            return response.status, (json.loads(text.decode("utf-8")) if text else None)
    except urllib.error.HTTPError as error:
        text = error.read()
        if raw:
            return error.code, text
        try:
            return error.code, json.loads(text.decode("utf-8"))
        except json.JSONDecodeError:
            return error.code, text.decode("utf-8", "replace")


def put_raw(base, path, data: bytes, token: str):
    request = urllib.request.Request(
        base + path, data=data, method="PUT",
        headers={"Authorization": f"Bearer {token}", "Content-Type": "application/octet-stream"},
    )
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            return response.status, json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode("utf-8", "replace")


def wait_ready(base, path, timeout=120.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            status, _ = call(base, "GET", path)
            if status < 500:
                return True
        except Exception:
            pass
        time.sleep(0.5)
    return False


def fetch_code(email: str, after: float, timeout: float = 40.0) -> str:
    deadline = time.time() + timeout
    while time.time() < deadline:
        with urllib.request.urlopen("http://127.0.0.1:8025/api/v1/messages?limit=20", timeout=10) as resp:
            messages = json.loads(resp.read().decode("utf-8"))["messages"]
        for item in messages:
            if email not in [t["Address"] for t in item.get("To", [])]:
                continue
            created = datetime.fromisoformat(item["Created"].replace("Z", "+00:00")).timestamp()
            if created < after - 5:
                continue
            with urllib.request.urlopen(
                f"http://127.0.0.1:8025/api/v1/message/{item['ID']}", timeout=10
            ) as resp:
                detail = json.loads(resp.read().decode("utf-8"))
            match = re.search(r"验证码是：\s*([0-9]{6})", detail.get("Text", "") or "")
            if match:
                return match.group(1)
        time.sleep(0.7)
    raise RuntimeError(f"没找到发给 {email} 的验证码")


def main() -> int:
    db_name = "drama_e2e_m5b7_" + secrets.token_hex(4)
    # 两个库：commerce 与媒体控制面各有自己的表结构，按计划「两套库并存」。
    # 混进一个库会撞在 `users.id` 的类型上（commerce 是 uuid，媒体侧是 text），
    # 这正是下面 [1b] 要钉住的事。
    media_db = db_name + "_media"
    work = os.path.join("/tmp", f"m5b7-{secrets.token_hex(4)}")
    os.makedirs(work, exist_ok=True)
    commerce_port, media_port = free_port(), free_port()
    key_file = os.path.join(work, "license.pem")
    media_home = os.path.join(work, "media-home")
    python = os.path.join(CLOUD, ".venv", "bin", "python")

    commerce_dsn = f"postgresql://drama:drama-dev-only@127.0.0.1:5432/{db_name}"
    media_dsn = f"postgresql://drama:drama-dev-only@127.0.0.1:5432/{media_db}"
    common = {
        **os.environ,
        "DRAMA_CLOUD_DSN": media_dsn,
        "DRAMA_CLOUD_COMMERCE_URL": f"http://127.0.0.1:{commerce_port}",
        "DRAMA_CLOUD_INTERNAL_SECRET": INTERNAL_SECRET,
        "DRAMA_CLOUD_HOME": media_home,
        # 素材保留期调成 1 天，方便下面验证「到期真删」。
        "DRAMA_CLOUD_ASSET_RETENTION_DAYS": "1",
    }
    commerce_env = {
        **common,
        "DRAMA_COMMERCE_DSN": commerce_dsn,
        "DRAMA_COMMERCE_CODE_SECRET": CODE_SECRET,
        "DRAMA_COMMERCE_SMTP_HOST": "127.0.0.1",
        "DRAMA_COMMERCE_SMTP_PORT": "1025",
        "DRAMA_COMMERCE_SMTP_SECURITY": "plain",
        "DRAMA_COMMERCE_LICENSE_KEY_FILE": key_file,
        "DRAMA_COMMERCE_MOCK_PAY_SECRET": "e2e-mock-secret",
        "DRAMA_COMMERCE_INTERNAL_SECRET": INTERNAL_SECRET,
    }
    media_env = {**common, "DRAMA_CLOUD_DSN": media_dsn}

    commerce = media = None
    created_db = False
    try:
        print(f"[1] 建两个临时库：{db_name}（commerce）、{media_db}（媒体控制面）")
        with psycopg.connect(ADMIN_DSN, autocommit=True) as admin:
            admin.execute(f'CREATE DATABASE "{db_name}"')
            admin.execute(f'CREATE DATABASE "{media_db}"')
        created_db = True

        print("[2] commerce 迁移 + 媒体控制面迁移 + 生成私钥")
        steps = [
            (commerce_env, (python, "-m", "drama_cloud.commerce.cli", "keygen", key_file)),
            (commerce_env, (python, "-m", "drama_cloud.commerce.cli", "migrate")),
            (media_env, (python, "-m", "drama_cloud.cli", "initdb")),
            (media_env, (python, "-m", "drama_cloud.cli", "seed")),
        ]
        for step_env, step in steps:
            result = subprocess.run(step, env=step_env, capture_output=True, text=True, cwd=CLOUD)
            if result.returncode != 0:
                print(result.stdout[-1500:], result.stderr[-1500:])
                raise RuntimeError(f"{step[-1]} 失败")

        print(f"[3] 起 commerce(:{commerce_port}) 与媒体控制面(:{media_port})")
        commerce = subprocess.Popen(
            [python, "-m", "drama_cloud.commerce.cli", "serve", "--port", str(commerce_port),
             "--log-level", "warning"],
            env=commerce_env, cwd=CLOUD,
            stdout=open(os.path.join(work, "commerce.log"), "w"), stderr=subprocess.STDOUT,
        )
        commerce_base = f"http://127.0.0.1:{commerce_port}"
        if not wait_ready(commerce_base, "/api/v1/health"):
            raise RuntimeError("commerce 没起来")

        media = subprocess.Popen(
            [python, "-m", "drama_cloud.cli", "serve", "--host", "127.0.0.1",
             "--port", str(media_port), "--log-level", "warning"],
            env=media_env, cwd=CLOUD,
            stdout=open(os.path.join(work, "media.log"), "w"), stderr=subprocess.STDOUT,
        )
        media_base = f"http://127.0.0.1:{media_port}"
        if not wait_ready(media_base, "/api/v1/health"):
            raise RuntimeError("媒体控制面没起来")

        run_scenarios(
            commerce_base, media_base, media_home, media_dsn, media_env,
            python, work, commerce_env,
        )
    finally:
        for process in (media, commerce):
            if process is not None:
                process.send_signal(signal.SIGINT)
                try:
                    process.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    process.kill()
        if created_db:
            with psycopg.connect(ADMIN_DSN, autocommit=True) as admin:
                admin.execute(f'DROP DATABASE IF EXISTS "{media_db}" WITH (FORCE)')
                admin.execute(f'DROP DATABASE IF EXISTS "{db_name}" WITH (FORCE)')
            print(f"[清理] 已删除临时库 {db_name} 与 {media_db}")

    print()
    if FAILURES:
        print(f"失败 {len(FAILURES)} 项：")
        for item in FAILURES:
            print("  -", item)
        return 1
    print("全部通过")
    return 0


def register(commerce_base, email, password):
    call(commerce_base, "POST", "/api/v1/auth/codes", {"email": email, "purpose": "register"})
    code = fetch_code(email, time.time() - 180)
    status, body = call(commerce_base, "POST", "/api/v1/auth/register",
                        {"email": email, "code": code, "password": password, "accept_terms": True})
    check(status == 201, f"注册 {email}（HTTP {status}）")
    status, body = call(commerce_base, "POST", "/api/v1/auth/login",
                        {"email": email, "password": password})
    check(status == 200, f"登录 {email}（HTTP {status}）")
    return body["tokens"]["access_token"], body["user"]["id"]


def grant_credits(commerce_base, python, env, user_id, amount, tag):
    """用 commerce 的 CLI 给账号发积分（模拟「已经充过值」）。

    CLI 的 `credits grant` 自带幂等键（同一账号+备注重复执行不会重复加），
    所以这里用不同的 `--memo` 区分两个账号。
    """
    result = subprocess.run(
        [python, "-m", "drama_cloud.commerce.cli", "credits", "grant",
         "--user-id", user_id, "--amount", str(amount), "--memo", f"端到端测试充值 {tag}"],
        env=env, capture_output=True, text=True, cwd=CLOUD,
    )
    return result.returncode == 0, result.stdout + result.stderr


def run_scenarios(commerce_base, media_base, media_home, media_dsn, env, python, work,
                  commerce_env) -> None:
    password = "correct-horse-1"
    make_video(os.path.join(work, "ep01.mp4"), seconds=1)

    print("\n[A] 两个账号，各充值 100 积分")
    token_a, user_a = register(commerce_base, "a@example.com", password)
    token_b, user_b = register(commerce_base, "b@example.com", password)
    for user_id, label in ((user_a, "A"), (user_b, "B")):
        # 充值走 commerce 的 CLI，必须用 commerce 的环境（含它自己的 DSN）。
        ok, output = grant_credits(commerce_base, python, commerce_env, user_id, 100, label)
        check(ok, f"{label} 充值成功" + ("" if ok else f"：{output[-200:]}"))

    print("\n[B] 同一个令牌进两个服务")
    status, who = call(media_base, "GET", "/api/v1/auth/whoami", token=token_a)
    check(status == 200, f"媒体控制面 whoami（HTTP {status}）")
    check(who.get("user_id") == user_a, "媒体控制面认同一个 user_id")

    print("\n[C] 分片上传：传一半 → 查缺哪块 → 只补缺的")
    video = open(os.path.join(work, "ep01.mp4"), "rb").read()
    sha = hashlib.sha256(video).hexdigest()
    chunk = max(1024, len(video) // 3 + 1)
    status, session = call(media_base, "POST", "/api/v1/assets/uploads", {
        "name": "第 1 集", "role": "video", "filename": "ep01.mp4",
        "size_bytes": len(video), "sha256": sha, "chunk_size": chunk,
    }, token=token_a)
    check(status == 201, f"开上传会话（HTTP {status}）")
    upload_id = session["upload_id"]
    total = session["total_chunks"]
    check(total >= 2, f"分成 {total} 块")

    # 只传第 0 块，模拟中途断线。
    status, _ = put_raw(media_base, f"/api/v1/assets/uploads/{upload_id}/chunks/0",
                        video[:chunk], token_a)
    check(status == 200, f"传第 0 块（HTTP {status}）")

    status, state = call(media_base, "GET", f"/api/v1/assets/uploads/{upload_id}", token=token_a)
    check(status == 200 and state["received"] == [0], f"已收到 [0]（实际 {state.get('received')}）")
    check(state["missing"] == list(range(1, total)), f"还缺 {state['missing']}")

    # 只补缺的块。
    for index in state["missing"]:
        piece = video[index * chunk:(index + 1) * chunk]
        status, _ = put_raw(
            media_base, f"/api/v1/assets/uploads/{upload_id}/chunks/{index}", piece, token_a
        )
        check(status == 200, f"补第 {index} 块（HTTP {status}）")

    status, done = call(media_base, "POST", f"/api/v1/assets/uploads/{upload_id}/complete",
                        {}, token=token_a)
    check(status == 200, f"完成上传（HTTP {status}）")
    asset_id = done["asset_id"]

    print("\n[D] 建任务：预扣积分")
    status, task = call(media_base, "POST", "/api/v1/tasks", {
        "asset_ids": [asset_id], "recipe_id": "drama.no_edit.generic",
    }, token=token_a)
    check(status == 201, f"建任务（HTTP {status}）")
    task_id = task["id"]

    print("\n[E] Worker 真跑 → 成功结算")
    worker = subprocess.run(
        [python, "-m", "drama_cloud.cli", "run-worker", "--once"],
        env=env, capture_output=True, text=True, cwd=CLOUD,
    )
    check(worker.returncode == 0, f"Worker 跑完（退出码 {worker.returncode}）")

    status, detail = call(media_base, "GET", f"/api/v1/tasks/{task_id}", token=token_a)
    check(status == 200 and detail["state"] == "SUCCEEDED",
          f"任务成功（实际 {detail.get('state') if status == 200 else status}）")

    print("\n[F] 取产物并下载")
    artifacts = [a for a in detail.get("artifacts", []) if a["kind"] == "output"]
    check(bool(artifacts), f"有输出产物（{len(artifacts)} 个）")
    if artifacts:
        artifact_id = artifacts[0]["id"]
        status, info = call(media_base, "GET", f"/api/v1/artifacts/{artifact_id}", token=token_a)
        check(status == 200, f"取产物信息（HTTP {status}）")
        status, payload = call(media_base, "GET", info["download_url"], raw=True)
        check(status == 200 and len(payload) > 0, f"下载产物（HTTP {status}，{len(payload)} 字节）")

    print("\n[G] 越权：B 拿 A 的所有 id 敲门 → 全 404")
    for method, path in (
        ("GET", f"/api/v1/tasks/{task_id}"),
        ("GET", f"/api/v1/assets/uploads/{upload_id}"),
    ):
        status, _ = call(media_base, method, path, token=token_b)
        check(status == 404, f"B 访问 {path} → {status}（应 404）")
    status, listing = call(media_base, "GET", "/api/v1/tasks", token=token_b)
    check(status == 200 and listing["tasks"] == [], "B 的任务列表是空的")

    print("\n[H] 到期删除：造过期素材 → cleanup → 文件真没了")
    with psycopg.connect(env["DRAMA_CLOUD_DSN"], autocommit=True) as conn:
        row = conn.execute(
            "SELECT storage_key FROM assets WHERE id = %s", (asset_id,)
        ).fetchone()
        storage_key = row[0]
        old = (datetime.now(timezone.utc) - timedelta(days=5)).replace(microsecond=0).isoformat()
        conn.execute("UPDATE assets SET created_at = %s WHERE id = %s", (old, asset_id))

    file_path = os.path.join(media_home, "storage", storage_key)
    check(os.path.exists(file_path), "清理前素材文件在磁盘上")

    cleanup = subprocess.run(
        [python, "-m", "drama_cloud.cli", "cleanup"],
        env=env, capture_output=True, text=True, cwd=CLOUD,
    )
    check(cleanup.returncode == 0, f"cleanup 跑完（退出码 {cleanup.returncode}）")
    check(not os.path.exists(file_path), "过期素材的文件真的被删掉了")

    with psycopg.connect(env["DRAMA_CLOUD_DSN"], autocommit=True) as conn:
        row = conn.execute(
            "SELECT lifecycle, sha256, name FROM assets WHERE id = %s", (asset_id,)
        ).fetchone()
    check(row[0] == "deleted", f"素材标为 deleted（实际 {row[0]}）")
    check(row[1] == sha, "哈希保留（对账用）")
    check(row[2] == "", "文件名已清空（隐私）")

    print("\n[I] 结算后余额正确（扣的就是 commerce 那本账）")
    status, balance = call(
        env["DRAMA_CLOUD_COMMERCE_URL"], "POST", "/internal/credits/balance",
        {"user_id": user_a}, headers={"X-Internal-Secret": INTERNAL_SECRET},
    )
    check(status == 200, f"查 A 的余额（HTTP {status}）")
    check(balance["balance"] == 95, f"余额 = 100 - 5（实际 {balance.get('balance')}）")


def make_video(path: str, seconds: int = 1) -> None:
    """造一小段真视频（1 秒，够跑 no_edit 配方）。"""
    subprocess.run(
        ["ffmpeg", "-hide_banner", "-loglevel", "error", "-nostdin", "-y",
         "-f", "lavfi", "-i", f"testsrc2=size=160x120:rate=10:duration={seconds}",
         "-f", "lavfi", "-i", f"sine=frequency=440:duration={seconds}",
         "-c:v", "libx264", "-preset", "ultrafast", "-crf", "34", "-pix_fmt", "yuv420p",
         "-c:a", "aac", "-b:a", "64k", "-shortest", path],
        check=True,
    )


if __name__ == "__main__":
    sys.exit(main())
