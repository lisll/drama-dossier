"""M7 端到端：真 commerce + 真 PostgreSQL + 真 HTTP。

把管理后台这条链路跑一遍，每一步都断言：

    建管理员（CLI）→ 登录 → 改版本策略（强制更新）→ 发公告 → 关一个开关
    → 用户端公开接口拉到的配置随之变化（且不含用户数据）
    → 错误上报（含敏感字段被拒）→ 后台能看到
    → 普通用户令牌进不了任何管理接口
    → 停用管理员后会话立刻失效

库名随机，跑完自删；不动既有数据库。备份恢复的演练在 M7-7 已单独做过
（见那次汇报），这里不重复。
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

import psycopg

ADMIN_DSN = "postgresql://drama:drama-dev-only@127.0.0.1:5432/postgres"
ROOT = os.path.dirname(os.path.abspath(__file__))
CLOUD = os.path.abspath(os.path.join(ROOT, ".."))
CODE_SECRET = "e2e-m7-code-secret-0123456789abcdef"
ADMIN_PASSWORD = "e2e-admin-password-1234"
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


def call(base, method, path, body=None, token=None):
    url = base + path
    data = None
    head = {}
    if body is not None:
        data = json.dumps(body).encode("utf-8")
        head["Content-Type"] = "application/json"
    if token:
        head["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(url, data=data, headers=head, method=method)
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            text = response.read().decode("utf-8")
            return response.status, (json.loads(text) if text else None)
    except urllib.error.HTTPError as error:
        text = error.read().decode("utf-8")
        try:
            return error.code, json.loads(text)
        except json.JSONDecodeError:
            return error.code, text


def wait_ready(base, timeout=90.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            status, _ = call(base, "GET", "/api/v1/health")
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
    db_name = "drama_e2e_m7_" + secrets.token_hex(4)
    work = os.path.join("/tmp", f"m7-{secrets.token_hex(4)}")
    os.makedirs(work, exist_ok=True)
    port = free_port()
    key_file = os.path.join(work, "license.pem")
    python = os.path.join(CLOUD, ".venv", "bin", "python")
    env = {
        **os.environ,
        "DRAMA_COMMERCE_DSN": f"postgresql://drama:drama-dev-only@127.0.0.1:5432/{db_name}",
        "DRAMA_COMMERCE_CODE_SECRET": CODE_SECRET,
        "DRAMA_COMMERCE_SMTP_HOST": "127.0.0.1",
        "DRAMA_COMMERCE_SMTP_PORT": "1025",
        "DRAMA_COMMERCE_SMTP_SECURITY": "plain",
        "DRAMA_COMMERCE_LICENSE_KEY_FILE": key_file,
        "DRAMA_COMMERCE_MOCK_PAY_SECRET": "e2e-mock-secret",
    }
    server = None
    created = False
    try:
        print(f"[1] 建临时库 {db_name}")
        with psycopg.connect(ADMIN_DSN, autocommit=True) as admin:
            admin.execute(f'CREATE DATABASE "{db_name}"')
        created = True

        print("[2] 生成私钥 + 迁移 + 建管理员")
        for step in (
            ["keygen", key_file],
            ["migrate"],
        ):
            result = subprocess.run(
                [python, "-m", "drama_cloud.commerce.cli", *step],
                env=env, capture_output=True, text=True, cwd=CLOUD,
            )
            if result.returncode != 0:
                print(result.stdout[-1200:], result.stderr[-1200:])
                raise RuntimeError(f"{step} 失败")
        # 密码从 stdin 读（CLI 的约定），所以这里喂进去。
        result = subprocess.run(
            [python, "-m", "drama_cloud.commerce.cli", "admin", "create",
             "--email", "boss@example.com"],
            env=env, input=f"{ADMIN_PASSWORD}\n{ADMIN_PASSWORD}\n",
            capture_output=True, text=True, cwd=CLOUD,
        )
        check(result.returncode == 0, f"CLI 建管理员（退出码 {result.returncode}）")

        print(f"[3] 起服务（:{port}）")
        server = subprocess.Popen(
            [python, "-m", "drama_cloud.commerce.cli", "serve", "--port", str(port),
             "--log-level", "warning"],
            env=env, cwd=CLOUD,
            stdout=open(os.path.join(work, "server.log"), "w"), stderr=subprocess.STDOUT,
        )
        base = f"http://127.0.0.1:{port}"
        if not wait_ready(base):
            raise RuntimeError("服务没起来")

        run_scenarios(base, python, env)
    finally:
        if server is not None:
            server.send_signal(signal.SIGINT)
            try:
                server.wait(timeout=15)
            except subprocess.TimeoutExpired:
                server.kill()
        if created:
            with psycopg.connect(ADMIN_DSN, autocommit=True) as admin:
                admin.execute(f'DROP DATABASE IF EXISTS "{db_name}" WITH (FORCE)')
            print(f"[清理] 已删除临时库 {db_name}")

    print()
    if FAILURES:
        print(f"失败 {len(FAILURES)} 项：")
        for item in FAILURES:
            print("  -", item)
        return 1
    print("全部通过")
    return 0


def admin_auth(token: str) -> dict:
    return {"Authorization": f"Bearer {token}"}


def run_scenarios(base: str, python: str, env: dict) -> None:
    print("\n[A] 管理员登录")
    status, body = call(base, "POST", "/api/v1/admin/login",
                        {"email": "boss@example.com", "password": ADMIN_PASSWORD})
    check(status == 200, f"登录（HTTP {status}）")
    admin_token = body["token"]
    check(admin_token.startswith("adm_"), "管理员令牌有独立前缀")
    status, body = call(base, "GET", "/api/v1/admin/me", token=admin_token)
    check(status == 200 and body["admin"]["email"] == "boss@example.com", "whoami 通过")

    print("\n[B] 默认配置不拦人")
    status, config = call(base, "GET", "/api/v1/client/config")
    check(status == 200, f"公开配置可读（HTTP {status}）")
    check(config["version"]["minSupported"] == "0.0.0", "默认不强制任何人更新")
    check(all(config["flags"].values()), "四个开关默认全开")
    check(config["announcements"] == [], "默认没有公告")

    print("\n[C] 改版本策略 → 客户端配置跟着变")
    status, _ = call(base, "PUT", "/api/v1/admin/version-policy", {
        "min_supported": "1.0.0", "latest": "1.2.0",
        "download_url": "https://example.com/app.exe",
        "download_sha256": "a" * 64, "download_sig": "sig", "notes": "修了些问题",
    }, token=admin_token)
    check(status == 200, f"改版本策略（HTTP {status}）")
    status, config = call(base, "GET", "/api/v1/client/config")
    check(config["version"]["minSupported"] == "1.0.0", "公开配置里的最低版本变了")
    check(config["version"]["latest"] == "1.2.0", "最新版本也变了")

    print("\n[D] 发公告 → 客户端看得到")
    status, created = call(base, "POST", "/api/v1/admin/announcements", {
        "title": "维护通知", "body": "今晚 23 点维护，约 30 分钟。", "level": "warn",
    }, token=admin_token)
    check(status == 201, f"发公告（HTTP {status}）")
    status, config = call(base, "GET", "/api/v1/client/config")
    check(len(config["announcements"]) == 1, "客户端拉到了 1 条公告")
    check(config["announcements"][0]["title"] == "维护通知", "标题正确")
    check(config["announcements"][0]["level"] == "warn", "级别正确")

    print("\n[E] 关一个开关 → 客户端看得到")
    status, _ = call(base, "PUT", "/api/v1/admin/flags/server_processing",
                     {"enabled": False, "note": "临时检修"}, token=admin_token)
    check(status == 200, f"关掉服务端处理（HTTP {status}）")
    status, config = call(base, "GET", "/api/v1/client/config")
    check(config["flags"]["server_processing"] is False, "开关已关闭")
    check(config["flags"]["recharge"] is True, "其它开关不受影响")

    print("\n[F] 公开配置里没有用户数据")
    status, _ = call(base, "POST", "/api/v1/auth/codes",
                     {"email": "payer@example.com", "purpose": "register"})
    code = fetch_code("payer@example.com", time.time() - 120)
    status, user = call(base, "POST", "/api/v1/auth/register",
                        {"email": "payer@example.com", "code": code,
                         "password": "correct-horse-1", "accept_terms": True})
    check(status == 201, f"注册一个普通用户（HTTP {status}）")
    user_token = user["tokens"]["access_token"]
    _, raw = call(base, "GET", "/api/v1/client/config")
    text = json.dumps(raw, ensure_ascii=False)
    check("payer@example.com" not in text, "配置里没有用户邮箱")
    check(user["user"]["id"] not in text, "配置里没有用户 id")

    print("\n[G] 普通用户令牌进不了管理接口")
    for path in ("/api/v1/admin/me", "/api/v1/admin/users", "/api/v1/admin/flags"):
        status, _ = call(base, "GET", path, token=user_token)
        check(status == 401, f"{path} → {status}（应 401）")

    print("\n[H] 错误上报：多传敏感字段被拒")
    status, body = call(base, "POST", "/api/v1/client/errors", {
        "code": "io", "app_version": "0.2.1", "platform": "linux",
    })
    check(status == 202, f"正常上报（HTTP {status}）")
    status, _ = call(base, "POST", "/api/v1/client/errors", {
        "code": "io", "app_version": "0.2.1", "platform": "linux",
        "email": "leak@example.com", "path": "/home/x/剧名/第01集.mp4",
    })
    check(status == 422, f"带敏感字段被拒（HTTP {status}）")
    # 同一错误报 3 次，后台只应看到一行、计数 3。
    for _ in range(2):
        call(base, "POST", "/api/v1/client/errors", {
            "code": "io", "app_version": "0.2.1", "platform": "linux",
        })
    status, listing = call(base, "GET", "/api/v1/admin/client-errors", token=admin_token)
    check(status == 200, f"后台看错误列表（HTTP {status}）")
    check(len(listing["items"]) == 1, f"同一错误只有一行（实际 {len(listing['items'])}）")
    check(listing["items"][0]["occurrences"] == 3,
          f"计数为 3（实际 {listing['items'][0]['occurrences']}）")

    print("\n[I] 停用管理员 → 会话立刻失效")
    # 用 CLI 停用（需要新起进程，环境里要有同一个 DSN）。
    result = subprocess.run(
        [python, "-m", "drama_cloud.commerce.cli", "admin", "disable",
         "--email", "boss@example.com"],
        env=env, capture_output=True, text=True, cwd=CLOUD,
    )
    check(result.returncode == 0, f"CLI 停用管理员（退出码 {result.returncode}）")
    status, _ = call(base, "GET", "/api/v1/admin/me", token=admin_token)
    check(status == 401, f"停用后会话失效（HTTP {status}）")
    status, _ = call(base, "POST", "/api/v1/admin/login",
                     {"email": "boss@example.com", "password": ADMIN_PASSWORD})
    check(status == 401, f"停用后不能登录（HTTP {status}）")


if __name__ == "__main__":
    sys.exit(main())
