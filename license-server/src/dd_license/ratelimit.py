"""进程内滑动窗口限流（单 worker 部署，见 README）。"""

from __future__ import annotations

import threading
import time
from collections import defaultdict, deque

from .errors import LicenseError


class RateLimiter:
    def __init__(self, clock=time.monotonic):
        self._hits: dict[str, deque] = defaultdict(deque)
        self._lock = threading.Lock()
        self._clock = clock

    def hit(self, key: str, limit: int, window: int) -> None:
        """记一次；超限抛 429。"""
        t = self._clock()
        with self._lock:
            q = self._hits[key]
            while q and q[0] <= t - window:
                q.popleft()
            if len(q) >= limit:
                retry = max(1, int(q[0] + window - t) + 1)
                raise LicenseError("rate_limited", f"操作过于频繁，请 {retry} 秒后再试", 429, retry_after=retry)
            q.append(t)
            if len(self._hits) > 100_000:  # 防内存被大量随机 key 撑爆
                self._gc(t, window)

    def _gc(self, t: float, window: int) -> None:
        for k in [k for k, q in self._hits.items() if not q or q[-1] <= t - window]:
            del self._hits[k]

    def reset(self) -> None:
        with self._lock:
            self._hits.clear()
