"""账号服务命令行：`drama-commerce migrate | serve | cleanup`。配置见 cloud/docs/COMMERCE.md。"""

from __future__ import annotations

import argparse
import json
import sys
from typing import List, Optional

import psycopg

from .config import ConfigError, load_dsn, load_settings
from .db import MigrationError, connect, migrate


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


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="drama-commerce", description="剧备账号服务")
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
    return parser


def main(argv: Optional[List[str]] = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return args.func(args)
    except (ConfigError, MigrationError) as error:
        print(f"错误：{error}", file=sys.stderr)
        return 2
    except psycopg.OperationalError as error:
        # libpq 的报错第一行包含主机、端口和原因，不包含密码。
        first_line = (str(error).strip().splitlines() or ["未知原因"])[0]
        print(f"错误：无法连接数据库（{first_line}）", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
