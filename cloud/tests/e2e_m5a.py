"""M5a 端到端：真进程 + 真 HTTP + 真 PostgreSQL。

覆盖用户要的那条主线，每一步都断言，任何一步不对立刻退出：

    注册 → 登录 → 充值下单 → 模拟收银台支付 → 余额到账 → 流水
    → 处理配方按集扣费（5 积分/集） → 积分不足被拒（402）
    → 报审包预扣/结算/失败退还 → 邀请奖励 → 重复回调不重复入账

库名随机，跑完自己删掉那个库；不动既有的 drama_commerce / drama_ui_m4b2。
"""

from __future__ import annotations

import base64
import hashlib
import hmac
import json
import os
import re
import secrets
import signal
import socket
import subprocess
import sys
import time
from datetime import datetime, timezone

import psycopg
import urllib.error
import urllib.request

ADMIN_DSN = "postgresql://drama:drama-dev-only@127.0.0.1:5432/postgres"
ROOT = os.path.dirname(os.path.abspath(__file__))
CLOUD = os.path.abspath(os.path.join(ROOT, ".."))
CODE_SECRET = "e2e-code-secret-0123456789abcdefghij"
MOCK_SECRET = "e2e-mock-pay-secret-0123456789"
#: 两个账号各自的机器码（64 位小写十六进制）。
DEVICE_A = "a" * 64
DEVICE_B = "b" * 64
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


class Api:
    def __init__(self, base: str):
        self.base = base
        self.token: str | None = None

    def call(self, method, path, body=None, *, headers=None, raw=False):
        url = path if str(path).startswith(("http://", "https://")) else f"{self.base}{path}"
        data = None
        head = dict(headers or {})
        if body is not None:
            data = json.dumps(body).encode("utf-8")
            head["Content-Type"] = "application/json"
        if self.token and "Authorization" not in head:
            head["Authorization"] = f"Bearer {self.token}"
        request = urllib.request.Request(url, data=data, headers=head, method=method)
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                text = response.read().decode("utf-8")
                return response.status, (text if raw else (json.loads(text) if text else None)), dict(response.headers)
        except urllib.error.HTTPError as error:
            text = error.read().decode("utf-8")
            try:
                parsed = json.loads(text)
            except json.JSONDecodeError:
                parsed = text
            return error.code, parsed, dict(error.headers)


def wait_health(api: Api, timeout: float = 90.0) -> bool:
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            status, body, _ = api.call("GET", "/api/v1/health")
            if status == 200 and body.get("ok"):
                return True
        except Exception:
            pass
        time.sleep(0.5)
    return False


def fetch_code(email: str, after: float, timeout: float = 30.0) -> str:
    """从 Mailpit 取最新一封发给该邮箱的验证码。"""
    deadline = time.time() + timeout
    while time.time() < deadline:
        with urllib.request.urlopen("http://127.0.0.1:8025/api/v1/messages?limit=20", timeout=10) as resp:
            messages = json.loads(resp.read().decode("utf-8"))["messages"]
        for item in messages:
            recipients = [t["Address"] for t in item.get("To", [])]
            if email not in recipients:
                continue
            stamp = item.get("Created", "")
            created = datetime.fromisoformat(stamp.replace("Z", "+00:00")).timestamp()
            if created < after - 5:
                continue
            with urllib.request.urlopen(
                f"http://127.0.0.1:8025/api/v1/message/{item['ID']}", timeout=10
            ) as resp:
                detail = json.loads(resp.read().decode("utf-8"))
            found = re.search(r"验证码是：\s*([0-9]{6})", detail.get("Text", "") or "")
            if found:
                return found.group(1)
            found = re.search(r"([0-9]{6})", detail.get("HTML", "") or "")
            if found:
                return found.group(1)
        time.sleep(0.7)
    raise RuntimeError(f"没在 Mailpit 里找到发给 {email} 的验证码")


def main() -> int:
    db_name = "drama_e2e_" + secrets.token_hex(4)
    key_file = os.path.join("/tmp", f"m5a-{secrets.token_hex(4)}", "license.pem")
    os.makedirs(os.path.dirname(key_file), exist_ok=True)
    port = free_port()
    env = {
        **os.environ,
        "DRAMA_COMMERCE_DSN": f"postgresql://drama:drama-dev-only@127.0.0.1:5432/{db_name}",
        "DRAMA_COMMERCE_CODE_SECRET": CODE_SECRET,
        "DRAMA_COMMERCE_SMTP_HOST": "127.0.0.1",
        "DRAMA_COMMERCE_SMTP_PORT": "1025",
        "DRAMA_COMMERCE_SMTP_SECURITY": "plain",
        "DRAMA_COMMERCE_LICENSE_KEY_FILE": key_file,
        "DRAMA_COMMERCE_MOCK_PAY_SECRET": MOCK_SECRET,
    }
    python = os.path.join(CLOUD, ".venv", "bin", "python")
    server = None
    created_db = False
    try:
        print(f"[1] 建临时库 {db_name}")
        with psycopg.connect(ADMIN_DSN, autocommit=True) as admin:
            admin.execute(f'CREATE DATABASE "{db_name}"')
        created_db = True

        print("[2] 生成签名私钥 + 执行迁移")
        for args in (["drama-commerce", "keygen", key_file], ["drama-commerce", "migrate"]):
            result = subprocess.run(
                [python, "-m", "drama_cloud.commerce.cli", *args[1:]],
                env=env, capture_output=True, text=True, cwd=CLOUD,
            )
            if result.returncode != 0:
                print(result.stdout[-2000:], result.stderr[-2000:])
                raise RuntimeError(f"{args} 失败")

        print("[3] 生成两张 30 天卡密（积分接口要求账号有使用权）")
        cards_dir = os.path.dirname(key_file)
        card_a = make_card(env, python, os.path.join(cards_dir, "card-a.csv"))
        card_b = make_card(env, python, os.path.join(cards_dir, "card-b.csv"))

        print(f"[4] 起服务（127.0.0.1:{port}）")
        log_path = os.path.join(cards_dir, "server.log")
        log_file = open(log_path, "w", encoding="utf-8")
        server = subprocess.Popen(
            [python, "-m", "drama_cloud.commerce.cli", "serve", "--port", str(port), "--log-level", "warning"],
            env=env, cwd=CLOUD, stdout=log_file, stderr=subprocess.STDOUT,
        )
        api = Api(f"http://127.0.0.1:{port}")
        if not wait_health(api):
            log_file.flush()
            with open(log_path, encoding="utf-8") as fh:
                print(fh.read()[-3000:])
            raise RuntimeError("服务没起来")

        run_scenarios(api, card_a, card_b)
    finally:
        if server is not None:
            server.send_signal(signal.SIGINT)
            try:
                server.wait(timeout=15)
            except subprocess.TimeoutExpired:
                server.kill()
        if created_db:
            with psycopg.connect(ADMIN_DSN, autocommit=True) as admin:
                admin.execute(f'DROP DATABASE IF EXISTS "{db_name}"')
            print(f"[清理] 已删除临时库 {db_name}")

    print()
    if FAILURES:
        print(f"失败 {len(FAILURES)} 项：")
        for item in FAILURES:
            print("  -", item)
        return 1
    print("全部通过")
    return 0


def register(api: Api, email: str, password: str, invite_code=None) -> dict:
    status, _, _ = api.call("POST", "/api/v1/auth/codes", {"email": email, "purpose": "register"})
    check(status == 202, f"发验证码 {email}（HTTP {status}）")
    code = fetch_code(email, time.time() - 120)
    payload = {"email": email, "code": code, "password": password, "accept_terms": True}
    if invite_code:
        payload["invite_code"] = invite_code
    status, body, _ = api.call("POST", "/api/v1/auth/register", payload)
    check(status == 201, f"注册 {email}（HTTP {status}）")
    if status != 201:
        raise RuntimeError(f"注册失败：{body}")
    return body


def login(api: Api, email: str, password: str) -> None:
    status, body, _ = api.call("POST", "/api/v1/auth/login", {"email": email, "password": password})
    check(status == 200, f"登录 {email}（HTTP {status}）")
    if status != 200:
        raise RuntimeError(f"登录失败：{body}")
    api.token = body["tokens"]["access_token"]


def balance(api: Api) -> int:
    status, body, _ = api.call("GET", "/api/v1/credits")
    check(status == 200, f"查余额（HTTP {status}）")
    return body["balance"]


def recharge(api: Api, tier_id: str) -> dict:
    status, order, _ = api.call("POST", "/api/v1/payments/orders", {"tier_id": tier_id, "channel": "mock"})
    check(status == 200, f"下单 {tier_id}（HTTP {status}）")
    return order


def mock_notify(api: Api, payload: dict) -> tuple[int, str]:
    """按 Mock 渠道的规则签名后打回调。"""
    body = json.dumps(payload, ensure_ascii=False, sort_keys=True).encode("utf-8")
    signature = hmac.new(MOCK_SECRET.encode(), body, hashlib.sha256).hexdigest()
    return _post_raw(api, body, signature)


def _post_raw(api: Api, body: bytes, signature: str) -> tuple[int, str]:
    request = urllib.request.Request(
        f"{api.base}/api/v1/payments/mock/notify",
        data=body,
        headers={"Content-Type": "application/json", "X-Mock-Signature": signature},
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return response.status, response.read().decode("utf-8")
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode("utf-8")


def make_card(env: dict, python: str, path: str) -> str:
    """用 CLI 生成 1 张 30 天卡，返回明文卡密（数据库里只有摘要）。"""
    result = subprocess.run(
        [python, "-m", "drama_cloud.commerce.cli", "cards", "generate",
         "--days", "30", "--count", "1", "--out", path],
        env=env, capture_output=True, text=True, cwd=CLOUD,
    )
    if result.returncode != 0:
        print(result.stdout[-2000:], result.stderr[-2000:])
        raise RuntimeError("生成卡密失败")
    with open(path, encoding="utf-8") as handle:
        rows = handle.read().strip().splitlines()
    # 第一行是表头，第二行第一列是卡密明文。
    return rows[1].split(",")[0]


def prepare_account(api: Api, email: str, password: str, card: str, device: str) -> dict:
    """注册 → 登录 → 兑换卡密 → 绑定设备。积分相关接口都要求「有使用权且设备已绑定」。"""
    register(api, email, password)
    login(api, email, password)
    status, body, _ = api.call("POST", "/api/v1/cards/redeem", {"code": card})
    check(status == 200, f"{email} 兑换卡密（HTTP {status}）")
    status, body, _ = api.call("POST", "/api/v1/license/activate", {
        "device_id": device,
        "name": "e2e-machine",
        "platform": "linux",
        "app_version": "0.2.1",
    })
    check(status == 200, f"{email} 绑定设备（HTTP {status}）")
    return body


def run_scenarios(api: Api, card_a: str, card_b: str) -> None:
    email = "e2e@example.com"
    password = "correct-horse-1"

    print("\n[A] 注册、登录、兑换卡密、绑定设备")
    prepare_account(api, email, password, card_a, DEVICE_A)

    print("\n[B] 初始余额与档位")
    status, body, _ = api.call("GET", "/api/v1/credits")
    check(status == 200 and body["balance"] == 0, "新账号余额为 0")
    check(body["history"] == [] or len(body["history"]) == 0, "新账号没有流水")
    status, tiers, _ = api.call("GET", "/api/v1/payments/tiers")
    check(status == 200, f"取充值档位（HTTP {status}）")
    ids = [t["id"] for t in tiers["items"]]
    check(len(ids) >= 3, f"档位数 >= 3（实际 {ids}）")

    print("\n[C] 充值：下单 → 回调 → 到账")
    order = recharge(api, "tier-100")
    check(order["amountFen"] == 1000, f"订单金额 1000 分（实际 {order['amountFen']}）")
    check(order["checkout"]["kind"] == "redirect", "返回跳转式收银台")
    check(str(order["checkout"]["payload"]).startswith("http://"),
          "收银台是可直接打开的完整地址（桌面端要把它渲染成链接）")
    print(f"    收银台地址：{order['checkout']['payload']}")

    # 真去打开收银台页面：这是 Mock 闭环里之前缺失的那一环。
    status, page, _ = api.call("GET", order["checkout"]["payload"], raw=True)
    check(status == 200, f"收银台页面可打开（HTTP {status}）")
    check("模拟收银台" in page, "收银台页面内容正确")
    match = re.search(r"data-action='paid'\s+data-body='([A-Za-z0-9+/=]+)'\s+data-signature='([0-9a-f]+)'", page)
    check(match is not None, "收银台页面带「支付成功」按钮")
    if match is None:
        raise RuntimeError("收银台页面没有可用的按钮")
    page_body = base64.b64decode(match.group(1))
    page_sig = match.group(2)
    status, text = _post_raw(api, page_body, page_sig)
    check(status == 200, f"用页面上的按钮付（HTTP {status}）")
    check(balance(api) == 100, "充值 100 积分已到账")

    print("\n[D] 重复回调不重复入账")
    status, text = _post_raw(api, page_body, page_sig)
    check(status == 200, f"重复回调被接受（HTTP {status}）")
    check(balance(api) == 100, "重放同一笔没有再次入账")

    print("\n[E] 伪造签名被拒")
    status, text = _post_raw(api, page_body, "0" * 64)
    check(status == 400, f"错签名被拒（HTTP {status}）")
    check(balance(api) == 100, "错签名没有入账")

    print("\n[F] 流水")
    status, body, _ = api.call("GET", "/api/v1/credits/history")
    check(status == 200, f"取流水（HTTP {status}）")
    items = body["items"]
    check(any(i.get("kind") == "grant" and i.get("delta") == 100 for i in items),
          "流水里有这笔 +100 的入账")

    print("\n[G] 处理配方：按集扣费（5 积分/集）")
    device = DEVICE_A
    task = hashlib.sha256(b"e2e-task").hexdigest()
    status, recipe, _ = api.call("POST", "/api/v1/version-recipes", {
        "device_id": device, "task": task, "platform": "universal",
        "strength": "medium", "episode_count": 3,
    })
    check(status == 200, f"签发配方（HTTP {status}）")
    check(recipe.get("credits") == 15, f"3 集应扣 15 积分（实际 {recipe.get('credits')}）")
    check(balance(api) == 100, "签发配方时还没扣费")

    status, consumed, _ = api.call("POST", f"/api/v1/version-recipes/{recipe['recipe_id']}/consume", {
        "device_id": device,
    })
    check(status == 200, f"消费配方（HTTP {status}）")
    check(consumed.get("credits") == 15, f"消费返回扣了 15（实际 {consumed.get('credits')}）")
    check(balance(api) == 85, f"扣费后余额 85（实际 {balance(api)}）")

    print("\n[H] 积分不足：402")
    # 把余额花到不够一集（5 积分）。
    short_task = hashlib.sha256(b"e2e-short").hexdigest()
    status, big, _ = api.call("POST", "/api/v1/version-recipes", {
        "device_id": device, "task": short_task, "platform": "universal",
        "strength": "medium", "episode_count": 17,  # 17 * 5 = 85，正好花完
    })
    check(status == 200, f"再签发 17 集配方（HTTP {status}）")
    status, _, _ = api.call("POST", f"/api/v1/version-recipes/{big['recipe_id']}/consume", {
        "device_id": device,
    })
    check(status == 200, f"消费 17 集（HTTP {status}）")
    check(balance(api) == 0, f"余额花光到 0（实际 {balance(api)}）")

    third_task = hashlib.sha256(b"e2e-third").hexdigest()
    status, third, _ = api.call("POST", "/api/v1/version-recipes", {
        "device_id": device, "task": third_task, "platform": "universal",
        "strength": "medium", "episode_count": 1,
    })
    check(status == 200, f"余额 0 时仍能签发（签发不扣费，HTTP {status}）")
    status, body, _ = api.call("POST", f"/api/v1/version-recipes/{third['recipe_id']}/consume", {
        "device_id": device,
    })
    check(status == 402, f"扣费时余额不足返回 402（HTTP {status}）")
    check(isinstance(body, dict) and body.get("error") == "insufficient_credits",
          f"错误码是 insufficient_credits（实际 {body.get('error') if isinstance(body, dict) else body}）")
    check(balance(api) == 0, "失败没有被扣成负数")

    print("\n[I] 报审包：预扣 → 成功结算")
    order2 = recharge(api, "tier-500")
    payload = {
        "out_trade_no": order2["outTradeNo"],
        "channel_trade_no": "MOCK-E2E-2",
        "amount_fen": order2["amountFen"],
        "status": "paid",
        "paid_at": datetime.now(timezone.utc).isoformat(),
    }
    status, _ = mock_notify(api, payload)
    check(status == 200, f"第二笔充值回调（HTTP {status}）")
    # 之前剩下 0（被 17 集花光），这次充 550。
    check(balance(api) == 550, f"余额 550（实际 {balance(api)}）")

    print("\n[I2] 报审包：预扣 → 结算；失败则退还")
    before = balance(api)
    episode = {
        "file_name": "ep01.mp4",
        "size_bytes": 1024,
        "sha256": hashlib.sha256(b"ep01").hexdigest(),
        "spec": {"duration_sec": 90.0, "width": 1080, "height": 1920},
    }
    base_package = {
        "device_id": DEVICE_A,
        "drama_name": "端到端测试剧",
        "ai_generated": True,
        "copy_episodes": False,
        "episodes": [episode],
    }
    status, package, _ = api.call("POST", "/api/v1/packages", base_package)
    check(status == 200, f"生成 1 集报审包（HTTP {status}）")
    if status == 200:
        check(package.get("credits") == 5, f"1 集报审包扣 5（实际 {package.get('credits')}）")
        check(balance(api) == before - 5, f"余额扣了 5（{before} → {balance(api)}）")

    # 故意制造失败：扫描件路径不在要盖章的文档里，服务端会拒。
    failed = dict(base_package)
    failed["signer"] = "scan"
    failed["scans"] = {"不存在.pdf": base64.b64encode(b"x").decode()}
    after_package = balance(api)
    status, body, _ = api.call("POST", "/api/v1/packages", failed)
    check(status == 422, f"故意失败的报审包被拒（HTTP {status}）")
    check(balance(api) == after_package, f"失败后退还，余额不变（{after_package} → {balance(api)}）")

    print("\n[J] 邀请奖励：被邀请人首次付费后双方得积分")
    status, invites, _ = api.call("GET", "/api/v1/invites")
    check(status == 200, f"取邀请信息（HTTP {status}）")
    inviter_code = invites["code"]
    check(len(inviter_code) == 10, f"邀请码 10 位（实际 {inviter_code}）")
    print(f"    邀请码：{inviter_code}，奖励配置：邀请人 +{invites['inviterCredits']}，"
          f"被邀请人 +{invites['inviteeCredits']}，上限 {invites['maxRewards']} 次")

    friend_email = "e2e-friend@example.com"
    # 注册时就带上邀请码。
    register(api, friend_email, password, invite_code=inviter_code)
    login(api, friend_email, password)
    status, _, _ = api.call("POST", "/api/v1/cards/redeem", {"code": card_b})
    check(status == 200, f"被邀请人兑换卡密（HTTP {status}）")
    status, _, _ = api.call("POST", "/api/v1/license/activate", {
        "device_id": DEVICE_B, "name": "e2e-friend", "platform": "linux", "app_version": "0.2.1",
    })
    check(status == 200, f"被邀请人绑定设备（HTTP {status}）")
    check(balance(api) == 0, "被邀请人初始余额 0（注册不送积分）")

    order3 = recharge(api, "tier-100")
    status, _ = mock_notify(api, {
        "out_trade_no": order3["outTradeNo"],
        "channel_trade_no": "MOCK-E2E-3",
        "amount_fen": order3["amountFen"],
        "status": "paid",
        "paid_at": datetime.now(timezone.utc).isoformat(),
    })
    check(status == 200, f"被邀请人首次付费（HTTP {status}）")
    friend_balance = balance(api)
    check(friend_balance == 100 + invites["inviteeCredits"],
          f"被邀请人 = 充值 100 + 奖励 {invites['inviteeCredits']}（实际 {friend_balance}）")

    login(api, email, password)
    inviter_before = 545  # 进邀请环节之前：550 充值 - 5 报审包
    inviter_balance = balance(api)
    check(inviter_balance == inviter_before + invites["inviterCredits"],
          f"邀请人 = {inviter_before} + 奖励 {invites['inviterCredits']}（实际 {inviter_balance}）")
    status, invites2, _ = api.call("GET", "/api/v1/invites")
    check(invites2.get("invited") == 1, f"邀请战绩记录 1 人（实际 {invites2.get('invited')}）")
    check(invites2.get("earnedCredits") == invites["inviterCredits"],
          f"已得奖励 {invites2.get('earnedCredits')} 积分")

    print("\n[K] 余额可由流水汇总核对（账本只增不改）")
    status, history, _ = api.call("GET", "/api/v1/credits/history?limit=200")
    total = sum(int(item["delta"]) for item in history["items"])
    check(total == inviter_balance, f"流水求和 {total} == 余额 {inviter_balance}")


if __name__ == "__main__":
    sys.exit(main())
