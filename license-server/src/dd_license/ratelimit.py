"""进程内滑动窗口限流（单 worker 部署，见 README）。"""

from __future__ import annotations

import threading
import time
from collections import defaultdict, deque

from .errors import LicenseError

# 每隔多久清理一次过期的计数（秒）。清理是全量扫描，不能每个请求都做。
GC_EVERY = 60.0


class RateLimiter:
    def __init__(self, clock=time.monotonic):
        self._hits: dict[str, deque] = defaultdict(deque)
        # 每个 key 自己的窗口长度：不同接口窗口可能不同，清理时不能按当前请求的窗口误删别人的计数
        self._windows: dict[str, int] = {}
        self._lock = threading.Lock()
        self._clock = clock
        self._next_gc = clock() + GC_EVERY

    def hit(self, key: str, limit: int, window: int) -> None:
        """记一次；超限抛 429。"""
        t = self._clock()
        with self._lock:
            if t >= self._next_gc:
                self._gc(t)
                self._next_gc = t + GC_EVERY
            q = self._hits[key]
            self._windows[key] = window
            while q and q[0] <= t - window:
                q.popleft()
            if len(q) >= limit:
                retry = max(1, int(q[0] + window - t) + 1)
                raise LicenseError("rate_limited", f"操作过于频繁，请 {retry} 秒后再试", 429, retry_after=retry)
            q.append(t)

    def _gc(self, t: float) -> None:
        stale = [k for k, q in self._hits.items() if not q or q[-1] <= t - self._windows.get(k, 0)]
        for k in stale:
            del self._hits[k]
            self._windows.pop(k, None)

    def size(self) -> int:
        with self._lock:
            return len(self._hits)

    def reset(self) -> None:
        with self._lock:
            self._hits.clear()
            self._windows.clear()
