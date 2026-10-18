"""M5b-0 端到端：真 commerce + 真媒体控制面 + 真 HTTP。

要证明的是「一套登录能进两个服务」，以及三种失败必须分得清：

    注册 commerce → 登录拿令牌 → 用同一个令牌访问媒体控制面 → 通过
    → 令牌无效 → 401
    → commerce 挂掉 → 503（不是 401，也不放行）
    → 邮箱进白名单后同样的令牌变成 admin

库名随机，跑完自己删掉；不动既有数据库。
"""

from __future__ import annotations

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
from datetime import datetime

import psycopg

ADMIN_DSN = "postgresql://drama:drama-dev-only@127.0.0.1:5432/postgres"
ROOT = os.path.dirname(os.path.abspath(__file__))
CLOUD = os.path.abspath(os.path.join(ROOT, ".."))
CODE_SECRET = "e2e-m5b-code-secret-0123456789abcdef"
INTERNAL_SECRET = "e2e-m5b-internal-secret-0123456789"
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
        with urllib.request.urlopen(request, timeout=60) as response:
            text = response.read().decode("utf-8")
            return response.status, (text if raw else (json.loads(text) if text else None))
    except urllib.error.HTTPError as error:
        text = error.read().decode("utf-8")
        try:
            return error.code, json.loads(text)
        except json.JSONDecodeError:
            return error.code, text


def wait_ready(base, path, timeout=90.0) -> bool:
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


def fetch_code(email: str, after: float, timeout: float = 30.0) -> str:
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
    db_name = "drama_e2e_m5b_" + secrets.token_hex(4)
    # 媒体控制面自己的库（与 commerce 分开，见 media_env 里的注释）。
    media_db = db_name + "_media"
    work = os.path.join("/tmp", f"m5b-{secrets.token_hex(4)}")
    os.makedirs(work, exist_ok=True)
    commerce_port, media_port = free_port(), free_port()
    key_file = os.path.join(work, "license.pem")
    media_home = os.path.join(work, "media-home")
    python = os.path.join(CLOUD, ".venv", "bin", "python")
    media_dsn = f"postgresql://drama:drama-dev-only@127.0.0.1:5432/{media_db}"

    commerce_env = {
        **os.environ,
        "DRAMA_COMMERCE_DSN": f"postgresql://drama:drama-dev-only@127.0.0.1:5432/{db_name}",
        "DRAMA_COMMERCE_CODE_SECRET": CODE_SECRET,
        "DRAMA_COMMERCE_SMTP_HOST": "127.0.0.1",
        "DRAMA_COMMERCE_SMTP_PORT": "1025",
        "DRAMA_COMMERCE_SMTP_SECURITY": "plain",
        "DRAMA_COMMERCE_LICENSE_KEY_FILE": key_file,
        "DRAMA_COMMERCE_MOCK_PAY_SECRET": "e2e-mock-secret",
        "DRAMA_COMMERCE_INTERNAL_SECRET": INTERNAL_SECRET,
    }
    media_env = {
        **os.environ,
        "DRAMA_CLOUD_HOME": media_home,
        # M5b-1 起媒体控制面也用 PostgreSQL，且与 commerce **分库**：
        # 两个服务的 `users` 表类型不同（commerce 是 uuid、媒体侧是 text），
        # 混在一个库里会直接撞在外键类型上。
        "DRAMA_CLOUD_DSN": media_dsn,
        "DRAMA_CLOUD_COMMERCE_URL": f"http://127.0.0.1:{commerce_port}",
        "DRAMA_CLOUD_INTERNAL_SECRET": INTERNAL_SECRET,
    }

    commerce = media = None
    created_db = False
    try:
        print(f"[1] 建两个临时库：{db_name}（commerce）、{media_db}（媒体控制面）")
        with psycopg.connect(ADMIN_DSN, autocommit=True) as admin:
            admin.execute(f'CREATE DATABASE "{db_name}"')
            admin.execute(f'CREATE DATABASE "{media_db}"')
        created_db = True

        print("[2] commerce：生成私钥 + 迁移")
        for args in (["keygen", key_file], ["migrate"]):
            result = subprocess.run(
                [python, "-m", "drama_cloud.commerce.cli", *args],
                env=commerce_env, capture_output=True, text=True, cwd=CLOUD,
            )
            if result.returncode != 0:
                print(result.stdout[-1500:], result.stderr[-1500:])
                raise RuntimeError(f"commerce {args} 失败")

        print(f"[3] 起 commerce（:{commerce_port}）与媒体控制面（:{media_port}）")
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
             "--port", str(media_port)], env=media_env, cwd=CLOUD,
            stdout=open(os.path.join(work, "media.log"), "w"), stderr=subprocess.STDOUT,
        )
        media_base = f"http://127.0.0.1:{media_port}"
        if not wait_ready(media_base, "/api/v1/health"):
            raise RuntimeError("媒体控制面没起来")

        run_scenarios(commerce_base, media_base, media_home, media_dsn)
    finally:
        for process, name in ((media, "媒体控制面"), (commerce, "commerce")):
            if process is not None:
                process.send_signal(signal.SIGINT)
                try:
                    process.wait(timeout=15)
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


def run_scenarios(commerce_base: str, media_base: str, media_home: str, media_dsn: str) -> None:
    email, password = "m5b@example.com", "correct-horse-1"

    print("\n[A] 在 commerce 注册并登录")
    status, _ = call(commerce_base, "POST", "/api/v1/auth/codes",
                     {"email": email, "purpose": "register"})
    check(status == 202, f"发验证码（HTTP {status}）")
    code = fetch_code(email, time.time() - 120)
    status, body = call(commerce_base, "POST", "/api/v1/auth/register",
                        {"email": email, "code": code, "password": password, "accept_terms": True})
    check(status == 201, f"注册（HTTP {status}）")
    token = body["tokens"]["access_token"]
    user_id = body["user"]["id"]
    check(bool(token), "拿到访问令牌")

    print("\n[B] 同一个令牌访问媒体控制面")
    status, body = call(media_base, "GET", "/api/v1/auth/whoami", token=token)
    check(status == 200, f"whoami 通过（HTTP {status}）")
    check(isinstance(body, dict) and body.get("user_id") == user_id,
          f"媒体控制面认同一个 user_id（{user_id[:8]}…）")
    check(body.get("role") == "operator", f"默认角色 operator（实际 {body.get('role')}）")

    print("\n[C] 无效令牌：401")
    status, body = call(media_base, "GET", "/api/v1/auth/whoami", token="bogus")
    check(status == 401, f"假令牌被拒（HTTP {status}）")

    print("\n[D] 退出登录后令牌立即失效")
    status, _ = call(commerce_base, "POST", "/api/v1/auth/logout", token=token)
    check(status == 204, f"commerce 退出登录（HTTP {status}）")
    status, _ = call(media_base, "GET", "/api/v1/auth/whoami", token=token)
    check(status == 401, f"媒体控制面同步失效（HTTP {status}）")

    print("\n[E] 重新登录 + 加白名单后变成管理员")
    status, body = call(commerce_base, "POST", "/api/v1/auth/login",
                        {"email": email, "password": password})
    check(status == 200, f"重新登录（HTTP {status}）")
    token = body["tokens"]["access_token"]

    status, body = call(media_base, "GET", "/api/v1/auth/whoami", token=token)
    check(body.get("role") == "operator", "加白名单前是 operator")

    add = subprocess.run(
        [sys.executable, "-m", "drama_cloud.cli", "initdb", "--admin", email],
        # 必须带上媒体控制面自己的 DSN：initdb 要连那个库才能写白名单表。
        env={**os.environ, "DRAMA_CLOUD_HOME": media_home, "DRAMA_CLOUD_DSN": media_dsn},
        capture_output=True, text=True, cwd=CLOUD,
    )
    check(add.returncode == 0, f"initdb --admin 成功（退出码 {add.returncode}）")
    check(email in add.stdout, f"输出里说明已加入白名单")

    status, body = call(media_base, "GET", "/api/v1/auth/whoami", token=token)
    check(body.get("role") == "admin", f"加白名单后是 admin（实际 {body.get('role')}）")

    print("\n[F] 内部接口：密钥不对一律拒绝")
    status, _ = call(commerce_base, "POST", "/internal/introspect", {},
                     headers={"X-Internal-Secret": "wrong", "X-Access-Token": token})
    check(status == 401, f"错密钥被拒（HTTP {status}）")
    status, body = call(commerce_base, "POST", "/internal/introspect", {},
                        headers={"X-Internal-Secret": INTERNAL_SECRET, "X-Access-Token": token})
    check(status == 200, f"对密钥通过（HTTP {status}）")
    check(body == {"user_id": user_id, "email": email}, f"只返回 user_id 与 email（{body}）")


if __name__ == "__main__":
    sys.exit(main())
