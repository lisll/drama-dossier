"""集中限流：计数存在 PostgreSQL 里，多个服务实例共享同一份计数（固定时间窗）。

调用 `hit` 时连接不能处在事务里：计数要立刻提交，被拒绝的请求也算一次。
"""

from __future__ import annotations

from dataclasses import dataclass

from .errors import rate_limited

_HIT = """
INSERT INTO rate_limits AS r (bucket, window_start, hits)
VALUES (
    %(bucket)s,
    to_timestamp((floor(extract(epoch FROM now()) / %(window)s::int) * %(window)s::int)::float8),
    1
)
ON CONFLICT (bucket, window_start) DO UPDATE SET hits = r.hits + 1
RETURNING hits,
          ceil(extract(epoch FROM (window_start + make_interval(secs => %(window)s::int) - now())))::int
              AS retry_after
"""


@dataclass(frozen=True)
class Rule:
    name: str
    limit: int
    window_seconds: int


def hit(conn, rule: Rule, key: str) -> None:
    """记一次访问；本时间窗内超过上限就抛 429，并告诉客户端还要等多少秒。"""
    row = conn.execute(_HIT, {"bucket": f"{rule.name}:{key}", "window": rule.window_seconds}).fetchone()
    if row["hits"] > rule.limit:
        raise rate_limited(row["retry_after"])
