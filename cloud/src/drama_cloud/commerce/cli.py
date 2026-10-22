"""账号与授权服务命令行。配置见 cloud/docs/COMMERCE.md。

    drama-commerce migrate | serve | cleanup
    drama-commerce keygen PATH | keyinfo PATH
    drama-commerce cards generate | list | revoke | status
    drama-commerce credits balance | grant | history | expire
"""

from __future__ import annotations

import argparse
import csv
import getpass
import hashlib
import json
import os
import secrets
import sys
import uuid
from datetime import datetime
from decimal import Decimal
from pathlib import Path
from typing import List, Optional

import psycopg

from . import cards, credits
from .config import ConfigError, load_dsn, load_settings
from .db import MigrationError, connect, migrate
from .license_token import Signer, b64url_encode, generate_key_file

MAX_CARDS_PER_BATCH = 10000


class UsageError(ValueError):
    """命令行参数不对。"""


def cmd_migrate(args) -> int:
    executed = migrate(load_dsn())
    print("已执行迁移：" + "、".join(executed) if executed else "数据库已是最新，没有要执行的迁移")
    return 0


def cmd_serve(args) -> int:
    import uvicorn

    from .api import create_app

    uvicorn.run(create_app(load_settings()), host=args.host, port=args.port, log_level=args.log_level)
    return 0


def cmd_cleanup(args) -> int:
    from .accounts import purge_expired

    with connect(load_dsn()) as conn:
        counts = purge_expired(conn)
    print(json.dumps(counts, ensure_ascii=False))
    return 0


# ---------- 授权签名密钥 ----------


def _print_key(signer: Signer, path) -> None:
    info = {"kid": signer.kid, "public_key": b64url_encode(signer.public_key), "path": str(path)}
    print(json.dumps(info, ensure_ascii=False))


def cmd_keygen(args) -> int:
    signer = generate_key_file(args.path)
    _print_key(signer, args.path)
    print("私钥已写入（权限 600）：只放在服务器上，不要提交进仓库，丢了要重新发版桌面端。"
          "public_key 是公钥，要内置到桌面端。", file=sys.stderr)
    return 0


def cmd_keyinfo(args) -> int:
    _print_key(Signer.from_pem_file(args.path), args.path)
    return 0


# ---------- 卡密 ----------


def _operator(args) -> str:
    if args.by:
        return args.by
    try:
        return getpass.getuser()
    except Exception:  # 容器里可能查不到当前用户名
        return "cli"


def cmd_cards_generate(args) -> int:
    if not 1 <= args.count <= MAX_CARDS_PER_BATCH:
        raise UsageError(f"--count 要在 1 到 {MAX_CARDS_PER_BATCH} 之间")
    days = None if args.lifetime else args.days
    if days is not None and not 1 <= days <= 3650:
        raise UsageError("--days 要在 1 到 3650 之间")
    price_fen: Optional[int] = None
    if args.price_yuan is not None:
        if not 0 <= args.price_yuan <= 1_000_000:
            raise UsageError("--price-yuan 要在 0 到 1000000 之间")
        # 界面层按元输入，存储按分（整数）：先用 Decimal 限制两位小数再取整，
        # 避免 19.99 * 100 的浮点误差把 1999 分写成 1998 分。
        price_fen = int(Decimal(str(args.price_yuan)) * 100)
        if Decimal(price_fen) / 100 != Decimal(str(args.price_yuan)):
            raise UsageError("--price-yuan 最多两位小数（分是最小单位）")
    kind = "lifetime" if days is None else "timed"
    label = "永久" if days is None else f"{days}天"
    name = (args.name if args.name is not None else f"{label}-{datetime.now():%Y%m%d-%H%M%S}").strip()
    if not 1 <= len(name) <= 100:
        raise UsageError("--name 要在 1 到 100 个字符之间")
    out = Path(args.out)
    if out.exists():
        raise UsageError(f"{out} 已存在，不会覆盖")
    codes = set()
    while len(codes) < args.count:
        codes.add(cards.generate_code())
    codes = list(codes)

    # 先把明文写进同目录的临时文件（权限 600）并落盘，再写数据库，最后改成正式文件名：
    # 写库失败就删掉临时文件；写库成功后改名失败，明文还在临时文件里，不会丢。
    temp = out.with_name(f".{out.name}.{secrets.token_hex(4)}.tmp")
    fd = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(fd, "w", encoding="utf-8-sig", newline="") as handle:
            writer = csv.writer(handle)
            writer.writerow(["code", "kind", "days", "price_fen", "batch_name"])
            for code in codes:
                writer.writerow([
                    cards.display(code), kind, "" if days is None else days,
                    "" if price_fen is None else price_fen, name,
                ])
            handle.flush()
            os.fsync(handle.fileno())
        with connect(load_dsn()) as conn:
            batch_id = cards.create_batch(
                conn, name=name, days=days, codes=codes, operator=_operator(args),
                price_fen=price_fen,
            )
    except BaseException:
        temp.unlink(missing_ok=True)
        raise
    try:
        os.link(temp, out)  # 目标已存在时会失败，不会覆盖别人刚建的文件
    except OSError as error:
        print(f"错误：卡密已写入数据库（批次 {batch_id}），但没能保存为 {out}（{error}）。"
              f"明文在 {temp}，请手动改名并妥善保管", file=sys.stderr)
        return 1
    temp.unlink()
    summary = {"batch_id": batch_id, "name": name, "kind": kind, "days": days,
               "price_fen": price_fen, "count": len(codes), "file": str(out)}
    print(json.dumps(summary, ensure_ascii=False))
    print("CSV 是卡密明文的唯一副本（数据库里只有摘要）：丢了找不回来；泄露了请用 cards revoke 作废整批。",
          file=sys.stderr)
    return 0


def _when(value) -> Optional[str]:
    return value.isoformat() if value else None


def cmd_cards_list(args) -> int:
    if not 1 <= args.limit <= 1000:
        raise UsageError("--limit 要在 1 到 1000 之间")
    with connect(load_dsn()) as conn:
        batches = cards.list_batches(conn, args.limit)
    for row in batches:
        print(json.dumps({
            "batch_id": str(row["id"]), "name": row["name"], "kind": row["kind"], "days": row["days"],
            "price_fen": row["price_fen"],
            "total": row["total"], "redeemed": row["redeemed"], "revoked_at": _when(row["revoked_at"]),
            "created_by": row["created_by"], "created_at": _when(row["created_at"]),
        }, ensure_ascii=False))
    if not batches:
        print("还没有卡密批次", file=sys.stderr)
    return 0


def cmd_cards_revoke(args) -> int:
    try:
        batch_id = str(uuid.UUID(args.batch))
    except ValueError as error:
        raise UsageError("--batch 应该是批次 ID（cards list 里的 batch_id）") from error
    if not args.yes:
        raise UsageError("作废后这一批里没兑换的卡密全部不能再用，而且不能撤销。确认要作废请加 --yes")
    with connect(load_dsn()) as conn:
        result = cards.revoke_batch(conn, batch_id, _operator(args))
    if result is None:
        raise UsageError(f"没有这个批次：{batch_id}")
    print(json.dumps({"batch_id": batch_id, "already_revoked": result["already"], "unused": result["unused"]},
                     ensure_ascii=False))
    return 0


def cmd_cards_status(args) -> int:
    code = cards.normalize(args.code)
    if code is None:
        raise UsageError(cards.FORMAT_MESSAGE)
    with connect(load_dsn()) as conn:
        row = cards.status(conn, code)
    if row is None:
        print(json.dumps({"state": "not_found"}, ensure_ascii=False))
        return 1
    if row["redeemed_by"] is not None:
        state = "redeemed"
    elif row["revoked_at"] is not None:
        state = "revoked"
    else:
        state = "unused"
    print(json.dumps({
        "state": state, "batch_id": str(row["batch_id"]), "batch_name": row["batch_name"],
        "kind": row["kind"], "days": row["days"],
        "redeemed_by": str(row["redeemed_by"]) if row["redeemed_by"] else None,
        "redeemed_at": _when(row["redeemed_at"]), "batch_revoked_at": _when(row["revoked_at"]),
    }, ensure_ascii=False))
    return 0


def cmd_credits_balance(args) -> int:
    with connect(load_dsn()) as conn:
        user_id = _user_id_of(conn, args)
        current = credits.balance(conn, user_id)
    print(json.dumps({"user_id": user_id, "balance": current}, ensure_ascii=False))
    return 0


def cmd_credits_grant(args) -> int:
    """管理员手动加积分（M5a 阶段没有充值界面时用它发测试积分）。"""
    if args.amount <= 0:
        raise UsageError("--amount 必须大于 0")
    operator = _operator(args)
    with connect(load_dsn()) as conn:
        user_id = _user_id_of(conn, args)
        # 幂等键由操作人 + 用户 + 金额 + 备注组成：同一条命令重复执行不会重复加分，
        # 不同人给同一用户加同样金额也不会被误判成重复。
        key = f"admin:{operator}:{user_id}:{args.amount}:{args.memo or ''}"
        current = credits.grant(
            conn, user_id, args.amount, idempotency_key=key, memo=args.memo or "", actor=operator
        )
    print(json.dumps({"user_id": user_id, "balance": current}, ensure_ascii=False))
    return 0


def cmd_credits_history(args) -> int:
    with connect(load_dsn()) as conn:
        user_id = _user_id_of(conn, args)
        rows = credits.history(conn, user_id, args.limit)
    for row in rows:
        print(json.dumps(row, ensure_ascii=False))
    return 0


def cmd_credits_expire(args) -> int:
    """回收悬挂过久的预扣（也可以交给 cleanup 定时跑）。"""
    with connect(load_dsn()) as conn:
        released = credits.release_stale_holds(conn)
    print(json.dumps({"released": released}, ensure_ascii=False))
    return 0


# ---------- 管理员（M7-1） ----------
#
# 管理后台没有注册入口：注册入口本身就是攻击面。管理员只能从这里创建。
# 密码**从标准输入读**，不走命令行参数——命令行参数会进 shell 历史与进程列表。

def _read_password(prompt: str, confirm: bool) -> str:
    import getpass
    import sys

    if not sys.stdin.isatty():
        # 非交互（脚本/CI）：从 stdin 读一行。文档里要写明这一点，
        # 免得有人以为管道输入也行却看不到提示。
        line = sys.stdin.readline().strip()
        if not line:
            raise UsageError("没有从标准输入读到密码")
        return line
    first = getpass.getpass(prompt)
    if confirm:
        again = getpass.getpass("再输一次：")
        if first != again:
            raise UsageError("两次输入的密码不一致")
    return first


def cmd_release_sign(args) -> int:
    """给升级包签名（M7-4）。

    输出三样东西（打印成 JSON，好让部署脚本直接取）：
    `sha256`、`signature`、`size_bytes`。把它们填进管理后台的版本策略即可。

    为什么要单独一条命令：客户端校验升级包时会验 Ed25519 签名，
    签不出来就无法提供可用的升级包。让人「自己算」等于把最要紧的一步交给手工。
    """
    from .license_token import Signer

    key_file = load_settings().license_key_file
    path = Path(args.file)
    try:
        payload = path.read_bytes()
    except OSError as error:
        raise UsageError(f"读不到升级包 {path}（{error.strerror}）") from error
    signer = Signer.from_pem_file(key_file)
    digest = hashlib.sha256(payload).hexdigest()
    signature = signer.sign_detached("release", payload)
    print(json.dumps(
        {
            "file": str(path),
            "size_bytes": len(payload),
            "sha256": digest,
            "signature": signature,
            "key_id": signer.kid,
        },
        ensure_ascii=False,
        indent=2,
    ))
    return 0


def cmd_admin_create(args) -> int:
    from .admins import AdminService

    password = _read_password("管理员密码（至少 12 个字符）：", confirm=True)
    operator = _operator(args)
    with connect(load_dsn()) as conn:
        admin_id = AdminService().create(conn, args.email, password, actor=operator)
    print(json.dumps({"admin_id": admin_id, "email": args.email.lower()}, ensure_ascii=False))
    return 0


def cmd_admin_disable(args) -> int:
    from .admins import AdminService

    operator = _operator(args)
    with connect(load_dsn()) as conn:
        AdminService().disable(conn, args.email, actor=operator)
    print(json.dumps({"disabled": args.email.lower()}, ensure_ascii=False))
    return 0


def cmd_admin_enable(args) -> int:
    from .admins import AdminService

    operator = _operator(args)
    with connect(load_dsn()) as conn:
        AdminService().enable(conn, args.email, actor=operator)
    print(json.dumps({"enabled": args.email.lower()}, ensure_ascii=False))
    return 0


def cmd_admin_list(args) -> int:
    with connect(load_dsn()) as conn:
        rows = conn.execute(
            "SELECT id, email, created_at, disabled_at FROM admins ORDER BY created_at"
        ).fetchall()
    print(json.dumps(
        [
            {
                "id": str(row["id"]),
                "email": row["email"],
                "created_at": row["created_at"].isoformat(),
                "disabled": row["disabled_at"] is not None,
            }
            for row in rows
        ],
        ensure_ascii=False,
        indent=2,
    ))
    return 0


def _user_id_of(conn, args) -> str:
    """把 --email / --user-id 解析成用户 ID；两种都没给就报用法错误。"""
    if args.user_id:
        return args.user_id
    if args.email:
        row = conn.execute(
            "SELECT id FROM users WHERE email = %s AND deleted_at IS NULL", (args.email.lower(),)
        ).fetchone()
        if row is None:
            raise UsageError(f"找不到这个邮箱对应的账号：{args.email}")
        return str(row["id"])
    raise UsageError("需要 --email 或 --user-id 指定账号")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="drama-commerce", description="剧备账号与授权服务")
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser("migrate", help="执行数据库迁移（只需要 DRAMA_COMMERCE_DSN）")
    p.set_defaults(func=cmd_migrate)

    p = sub.add_parser("serve", help="启动 HTTP 服务")
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--port", type=int, default=8100)
    p.add_argument("--log-level", default="info")
    p.set_defaults(func=cmd_serve)

    p = sub.add_parser("cleanup", help="清理过期的验证码、令牌、会话和限流计数（建议每天一次）")
    p.set_defaults(func=cmd_cleanup)

    p = sub.add_parser("keygen", help="生成授权签名私钥（Ed25519），打印公钥；文件已存在就拒绝")
    p.add_argument("path")
    p.set_defaults(func=cmd_keygen)

    p = sub.add_parser("keyinfo", help="打印私钥文件对应的 kid 和公钥")
    p.add_argument("path")
    p.set_defaults(func=cmd_keyinfo)

    card_parser = sub.add_parser("cards", help="卡密：生成、列出批次、作废批次、查询单张")
    card_sub = card_parser.add_subparsers(dest="cards_command", required=True)

    p = card_sub.add_parser("generate", help="生成一批卡密，明文导出成 CSV（数据库只存摘要）")
    group = p.add_mutually_exclusive_group(required=True)
    group.add_argument("--days", type=int, help="按天计的卡，例如月卡 --days 30")
    group.add_argument("--lifetime", action="store_true", help="永久卡")
    p.add_argument("--count", type=int, required=True)
    p.add_argument("--out", required=True, help="导出的 CSV 路径，文件不能已存在")
    p.add_argument("--name", help="批次名称，默认按类型和时间生成")
    p.add_argument(
        "--price-yuan", type=float,
        help="这批卡的售价（元）。只记录在批次上供对账，不参与兑换；"
             "不填表示这批不标价。0 表示赠品卡",
    )
    p.add_argument("--by", help="操作人，记进审计日志（默认是当前系统用户）")
    p.set_defaults(func=cmd_cards_generate)

    p = card_sub.add_parser("list", help="列出最近的批次和兑换数量（每行一个 JSON）")
    p.add_argument("--limit", type=int, default=50)
    p.set_defaults(func=cmd_cards_list)

    p = card_sub.add_parser("revoke", help="作废一个批次里还没兑换的卡密")
    p.add_argument("--batch", required=True, help="批次 ID")
    p.add_argument("--yes", action="store_true", help="确认作废")
    p.add_argument("--by", help="操作人，记进审计日志（默认是当前系统用户）")
    p.set_defaults(func=cmd_cards_revoke)

    p = card_sub.add_parser("status", help="查一张卡密的状态（未用 / 已兑换 / 已作废）")
    p.add_argument("code")
    p.set_defaults(func=cmd_cards_status)

    credit_parser = sub.add_parser("credits", help="积分：查余额、手动加、查流水、回收悬挂预扣")
    credit_sub = credit_parser.add_subparsers(dest="credits_command", required=True)

    def add_account_args(parser: argparse.ArgumentParser) -> None:
        # 二选一：邮箱更好记（人工操作用），用户 ID 更精确（脚本用）。
        group = parser.add_mutually_exclusive_group(required=True)
        group.add_argument("--email", help="账号邮箱")
        group.add_argument("--user-id", help="账号 UUID")

    p = credit_sub.add_parser("balance", help="查一个账号的可用积分")
    add_account_args(p)
    p.set_defaults(func=cmd_credits_balance)

    p = credit_sub.add_parser("grant", help="手动给账号加积分（幂等：同一条命令重复执行不会重复加）")
    add_account_args(p)
    p.add_argument("--amount", type=int, required=True, help="加多少积分（正整数）")
    p.add_argument("--memo", help="备注，会写进流水与审计日志")
    p.add_argument("--by", help="操作人，默认是当前系统用户")
    p.set_defaults(func=cmd_credits_grant)

    p = credit_sub.add_parser("history", help="列出一个账号最近的积分流水（新到旧）")
    add_account_args(p)
    p.add_argument("--limit", type=int, default=50)
    p.set_defaults(func=cmd_credits_history)

    p = credit_sub.add_parser("expire", help="回收悬挂超过 2 小时的预扣（也可以交给 cleanup 定时跑）")
    p.set_defaults(func=cmd_credits_expire)

    admin_parser = sub.add_parser("admin", help="管理员：创建、停用、列出（管理后台没有注册入口）")
    admin_sub = admin_parser.add_subparsers(dest="admin_command", required=True)

    p = admin_sub.add_parser("create", help="新建管理员（密码从标准输入读，不走命令行参数）")
    p.add_argument("--email", required=True, help="管理员邮箱（小写）")
    p.add_argument("--by", help="操作人，记进审计日志（默认是当前系统用户）")
    p.set_defaults(func=cmd_admin_create)

    p = admin_sub.add_parser("disable", help="停用管理员（不删除，审计日志还要引用）")
    p.add_argument("--email", required=True, help="管理员邮箱")
    p.add_argument("--by", help="操作人")
    p.set_defaults(func=cmd_admin_disable)

    p = admin_sub.add_parser("enable", help="恢复被停用的管理员")
    p.add_argument("--email", required=True, help="管理员邮箱")
    p.add_argument("--by", help="操作人")
    p.set_defaults(func=cmd_admin_enable)

    p = admin_sub.add_parser("list", help="列出全部管理员")
    p.set_defaults(func=cmd_admin_list)

    p = sub.add_parser("release", help="升级包：给安装包签名（M7 自动更新用）")
    release_sub = p.add_subparsers(dest="release_command", required=True)
    sign_parser = release_sub.add_parser(
        "sign", help="给一个安装包算 sha256 并签名，输出可直接填进版本策略"
    )
    sign_parser.add_argument("file", help="安装包路径（.exe / .msi / .dmg / .pkg）")
    sign_parser.set_defaults(func=cmd_release_sign)
    return parser


def main(argv: Optional[List[str]] = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return args.func(args)
    except (ConfigError, MigrationError, UsageError) as error:
        print(f"错误：{error}", file=sys.stderr)
        return 2
    except psycopg.OperationalError as error:
        # libpq 的报错第一行包含主机、端口和原因，不包含密码。
        first_line = (str(error).strip().splitlines() or ["未知原因"])[0]
        print(f"错误：无法连接数据库（{first_line}）", file=sys.stderr)
        return 2
    except OSError as error:
        print(f"错误：{error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
