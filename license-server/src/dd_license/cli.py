"""license-admin 命令行：只负责引导（建库、密钥、管理员）、自检与启动服务。

日常发卡、查询、作废都在网页后台完成。
"""

from __future__ import annotations

import argparse
import getpass
import ipaddress
import logging
import os
import stat
import sys
import time
from pathlib import Path

from .admin_auth import AdminAuth
from .config import ConfigError, Settings, load_settings
from .db import Database
from .errors import LicenseError
from .keys import b64url, generate_private_key, key_id, load_private_key, parse_public_keys, public_raw


def build_app(settings: Settings):
    from .api import create_app
    from .service import LicenseService

    key = load_private_key(settings.key_file)
    db = Database(settings.db_path)
    db.migrate()
    service = LicenseService(db, key, settings.token_days, extra_trusted=parse_public_keys(settings.extra_pubkeys))
    return create_app(settings, service, AdminAuth(db))


def _read_password(confirm: bool = True) -> str:
    pw = getpass.getpass("输入密码（至少 16 位，不回显）：")
    if confirm and getpass.getpass("再输入一次：") != pw:
        raise SystemExit("两次输入不一致")
    return pw


def _fmt_ts(ts) -> str:
    return time.strftime("%Y-%m-%d %H:%M", time.localtime(ts)) if ts else "-"


def cmd_init_db(settings: Settings, _args) -> None:
    Database(settings.db_path).migrate()
    if os.name == "posix":
        os.chmod(settings.db_path.parent, 0o700)
    print(f"数据库已就绪：{settings.db_path}")


def cmd_keygen(settings: Settings, args) -> None:
    out = Path(args.out) if args.out else settings.key_file
    key = generate_private_key(out)
    raw = public_raw(key.public_key())
    print(f"私钥已写入 {out}（权限 0600，请立即离线备份；丢失后已发出的授权将无法续签）")
    print(f"公钥（填入客户端 PRODUCTION_KEYS）：{b64url(raw)}")
    print(f"kid：{key_id(raw)}")


def cmd_pubkey(settings: Settings, _args) -> None:
    key = load_private_key(settings.key_file)
    raw = public_raw(key.public_key())
    print(b64url(raw))
    print(f"kid：{key_id(raw)}")


def cmd_admin(settings: Settings, args) -> None:
    db = Database(settings.db_path)
    db.migrate()
    auth = AdminAuth(db)
    if args.admin_cmd == "create":
        auth.create_admin(args.username, _read_password())
        print(f"管理员 {args.username} 已创建")
    elif args.admin_cmd == "passwd":
        auth.set_password(args.username, _read_password())
        print("密码已更新，该管理员的所有会话已失效")
    elif args.admin_cmd == "disable":
        auth.set_disabled(args.username, True)
        print("已禁用，并注销其所有会话")
    elif args.admin_cmd == "enable":
        auth.set_disabled(args.username, False)
        print("已启用")
    elif args.admin_cmd == "unlock":
        auth.unlock(args.username)
        print("已解除登录锁定")
    elif args.admin_cmd == "list":
        for a in auth.list_admins():
            state = "已禁用" if a["disabled_at"] else ("锁定中" if (a["locked_until"] or 0) > time.time() else "正常")
            print(f"{a['id']:>3}  {a['username']:<20} {state:<4}  最近登录 {_fmt_ts(a['last_login_at'])}")


def doctor(settings: Settings, host: str = "127.0.0.1") -> list[tuple[bool, str]]:
    """安全自检。返回 (是否通过, 说明) 列表。"""
    out: list[tuple[bool, str]] = []

    def check(ok: bool, msg: str) -> None:
        out.append((ok, msg))

    if settings.key_file.exists():
        mode = stat.S_IMODE(settings.key_file.stat().st_mode)
        check(not (mode & 0o037), f"签名私钥权限 {oct(mode)}（应为 0600 或 0640）")
    else:
        check(False, f"签名私钥不存在：{settings.key_file}")
    db_dir = settings.db_path.parent
    if db_dir.exists():
        mode = stat.S_IMODE(db_dir.stat().st_mode)
        check(not (mode & 0o077), f"数据库目录 {db_dir} 权限 {oct(mode)}（应为 0700）")
    else:
        check(False, f"数据库目录不存在：{db_dir}（先执行 init-db）")
    check(bool(settings.admin_allow_ips), "已配置后台 IP 白名单 DD_LICENSE_ADMIN_ALLOW_IPS")
    check(not settings.admin_allow_any_ip, "未开启 DD_LICENSE_ADMIN_ALLOW_ANY_IP（开启后任何 IP 都能看到后台）")
    try:
        loopback = ipaddress.ip_address(host).is_loopback
    except ValueError:
        loopback = host == "localhost"
    check(loopback, f"服务只监听本机地址（当前 {host}）")
    check(settings.trust_proxy, "已设置 DD_LICENSE_TRUST_PROXY=1（部署在 Caddy 后面时必须开启，否则白名单拿不到真实 IP）")
    check(settings.cookie_secure, "管理 Cookie 带 Secure 属性（未设置 DD_LICENSE_INSECURE_COOKIE）")
    if hasattr(os, "geteuid"):
        check(os.geteuid() != 0, "不以 root 身份运行")
    if settings.db_path.exists():
        admins = AdminAuth(Database(settings.db_path)).list_admins()
        check(any(a["disabled_at"] is None for a in admins), "至少有一个可用的管理员账号")
    return out


def cmd_doctor(settings: Settings, args) -> None:
    results = doctor(settings, args.host)
    for ok, msg in results:
        print(("  [通过] " if ok else "  [问题] ") + msg)
    failed = sum(1 for ok, _ in results if not ok)
    print(f"\n共 {len(results)} 项，{failed} 项需要处理")
    if failed:
        raise SystemExit(1)


def cmd_serve(settings: Settings, args) -> None:
    import uvicorn

    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s %(message)s")
    log = logging.getLogger("dd_license")
    if not settings.admin_enabled:
        log.warning("未配置 DD_LICENSE_ADMIN_ALLOW_IPS：管理后台整体关闭（返回 404）")
    if settings.admin_allow_any_ip:
        log.warning("已开启 DD_LICENSE_ADMIN_ALLOW_ANY_IP：任何 IP 都能看到后台登录页，强烈建议改用白名单")
    app = build_app(settings)
    # proxy_headers=False：真实 IP 由我们自己按"只信本机代理"的规则解析，不交给 uvicorn
    uvicorn.run(
        app, host=args.host, port=args.port, workers=1, proxy_headers=False,
        server_header=False, date_header=False, access_log=False,
    )


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(prog="license-admin", description="剧备卡密授权服务管理工具")
    sub = parser.add_subparsers(dest="cmd", required=True)
    sub.add_parser("init-db", help="创建或升级数据库")
    p = sub.add_parser("keygen", help="生成 Ed25519 签名私钥")
    p.add_argument("--out", help="输出路径，默认 DD_LICENSE_KEY_FILE")
    sub.add_parser("pubkey", help="打印当前私钥对应的公钥")
    p = sub.add_parser("admin", help="管理员账号")
    asub = p.add_subparsers(dest="admin_cmd", required=True)
    for name in ("create", "passwd", "disable", "enable", "unlock"):
        ap = asub.add_parser(name)
        ap.add_argument("--username", required=True)
    asub.add_parser("list")
    p = sub.add_parser("doctor", help="安全自检")
    p.add_argument("--host", default="127.0.0.1")
    p = sub.add_parser("serve", help="启动服务")
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--port", type=int, default=8200)
    args = parser.parse_args(argv)

    handlers = {
        "init-db": cmd_init_db, "keygen": cmd_keygen, "pubkey": cmd_pubkey,
        "admin": cmd_admin, "doctor": cmd_doctor, "serve": cmd_serve,
    }
    try:
        handlers[args.cmd](load_settings(), args)
    except (ConfigError, LicenseError) as exc:
        print(f"错误：{getattr(exc, 'message', exc)}", file=sys.stderr)
        raise SystemExit(2) from None


if __name__ == "__main__":
    main()
