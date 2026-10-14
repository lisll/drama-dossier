"""Tier 2 后期能力：解析 `dossier-media` 并读回它的能力清单。

与 `ffmpeg_caps.py` 的分工：那边按 `ffmpeg -filters/-encoders` 判断**滤镜级**能力；
这里判断**后期域**能力（合成渲染 / 变速变调 / 音源分离 / ASR / OCR），它们还要看
rubberband / demucs / whisper / tesseract 有没有装。判断依据是 `dossier-media probe --json`
的输出——那一份报告与 Rust 侧 `crates/dossier-media/src/tools.rs` 是同一套标签，
所以「本机到底会不会这个」在两边只有一个答案。

探测失败（工具不在、跑不起来、超时、输出解析不了）一律**按缺能力处理**：
「探测不到」和「确实没有」对路由来说是同一个结论（与 `ffmpeg_caps.py` 保持一致）。
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Callable, Dict, FrozenSet, List, Optional, Tuple

from ..config import Settings
from ..errors import ErrorClass, WorkError
from ..toolchain import resolve_binary
from .ffmpeg import run

#: 探测结果按「工具路径 + 大小 + mtime」缓存：同一个二进制只问一次。
_CAPABILITY_CACHE: Dict[Tuple[str, int, int], FrozenSet[str]] = {}

_PROBE_TIMEOUT = 60.0


def clear_cache() -> None:
    """清空缓存（单测与「换了工具路径」的场景用）。"""
    _CAPABILITY_CACHE.clear()


def resolve_media_tool(settings: Optional[Settings]) -> Path:
    """定位 `dossier-media`；找不到时显式失败并给出构建方式。"""
    configured = getattr(settings, "media_bin", None) if settings is not None else None
    resolved = resolve_binary(configured, "dossier-media")
    if resolved is None:
        raise WorkError(
            ErrorClass.CAPABILITY_MISSING,
            "找不到 dossier-media 可执行文件；请设置 DRAMA_CLOUD_MEDIA_BIN，"
            "或在仓库根目录执行 cargo build --release -p dossier-media",
        )
    return resolved


def probe_media_tool(settings: Optional[Settings]) -> Optional[dict]:
    """跑一次 `dossier-media probe --json`；任何失败都返回 `None`（= 问不出来）。"""
    try:
        tool = resolve_media_tool(settings)
    except WorkError:
        return None
    try:
        result = run([str(tool), "probe", "--json"], timeout=_PROBE_TIMEOUT)
    except WorkError:
        return None
    if result.returncode != 0:
        return None
    text = result.stdout.decode("utf-8", "replace")
    try:
        payload = json.loads(text)
    except json.JSONDecodeError:
        return None
    return payload if isinstance(payload, dict) else None


def media_capabilities(settings: Optional[Settings]) -> FrozenSet[str]:
    """本机后期能力标签集合；探测不到时返回空集。"""
    try:
        tool = resolve_media_tool(settings)
        stamp = tool.stat()
    except (WorkError, OSError):
        return frozenset()
    key = (str(tool), stamp.st_size, stamp.st_mtime_ns)
    cached = _CAPABILITY_CACHE.get(key)
    if cached is not None:
        return cached
    payload = probe_media_tool(settings)
    capabilities: FrozenSet[str] = frozenset()
    if payload is not None:
        items = payload.get("capabilities") or []
        if isinstance(items, list):
            capabilities = frozenset(str(item) for item in items)
    _CAPABILITY_CACHE[key] = capabilities
    return capabilities


def requires_media(*tags: str) -> Callable[[Optional[Settings]], bool]:
    """构造一个 `capability_probe`：这些能力标签必须**全部**成立。"""

    def probe(settings: Optional[Settings]) -> bool:
        available = media_capabilities(settings)
        return all(tag in available for tag in tags)

    return probe


def run_media(
    tool: Path,
    args: List[str],
    timeout: float,
    cwd: Optional[Path] = None,
):
    """调用 `dossier-media`；失败信息按同一套规则分类（超时 → 资源争用）。"""
    return run([str(tool), *args], timeout=timeout, cwd=cwd)


def requires_any_media(*tags: str) -> Callable[[Optional[Settings]], bool]:
    """构造一个 `capability_probe`：这些能力标签**任一**成立即可。

    用于「有两条实现路径」的模块：变速不变调要 rubberband，纯变速用 ffmpeg `atempo`
    就行——后者也算「本机会变速」，只是音调会跟着变（这一点在模块实现里显式说明，
    不用能力标签去掩盖）。
    """

    def probe(settings: Optional[Settings]) -> bool:
        available = media_capabilities(settings)
        return any(tag in available for tag in tags)

    return probe
