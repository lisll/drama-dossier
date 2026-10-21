"""处理模块集合。

导入本包即完成全部模块注册。注册表是**代码里的白名单**：配方只能引用这里出现过的模块。
"""

from __future__ import annotations

from . import (  # noqa: F401  导入即注册
    audio,
    dedup,
    encode,
    filters,
    gate,
    inpaint,
    late_stage,
    overlay,
    probe,
    timeline,
)
from .ffmpeg import MediaContext, probe_json, run_checked, summarize

__all__ = [
    "MediaContext",
    "probe_json",
    "run_checked",
    "summarize",
]
