"""FFmpeg 能力清单探测：滤镜与编码器。

为什么需要这一层：`docs/PROJECT_REVIEW.md` §7.1 的故障根因是「工具在 ≠ 能力在」。
`shutil.which("ffmpeg")` 只能证明**这个程序存在**，证明不了它编译进了 `deflicker`、
`signalstats` 或 `libx264`——发行版裁剪包、老版本、自编译版都会缺东西。缺了却仍然声明能力，
任务会领到手才失败（而且要等跑到那一步）。

所以这里照 Rust 侧（`crates/dossier-runtime/src/probe.rs`）同一套解析规则读
`ffmpeg -filters` / `-encoders`，模块用它做 `capability_probe`：

- 探测结果按「程序名 + 开关」缓存，一次任务只问一次；失败时**按缺能力处理**（返回空集），
  因为「探测不到」和「确实没有」对路由来说是同一个结论；
- 探测本身不处理素材，所以可以安全地在「列出本机能力」时反复调用。

与 Rust 侧的分工：Rust 侧面向单机工具链自检（`dossier-media probe`），
这里面向云端 Worker 的能力路由。两边解析规则保持镜像，避免同一台机器给出两个答案。
"""

from __future__ import annotations

import shutil
import subprocess
from typing import Callable, Dict, FrozenSet, Iterable, List, Optional

from ..config import Settings

#: `ffmpeg -filters` / `-encoders` 的解析结果按程序缓存（同一个二进制答案不会变）。
_LISTING_CACHE: Dict[str, FrozenSet[str]] = {}

_LISTING_TIMEOUT = 20.0


def clear_cache() -> None:
    """清空缓存（单测与「换了 ffmpeg 路径」的场景用）。"""
    _LISTING_CACHE.clear()


def _run_listing(program: str, flag: str) -> Optional[str]:
    try:
        result = subprocess.run(
            [program, "-hide_banner", flag],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=_LISTING_TIMEOUT,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        # 程序不在、跑不起来、超时：一律按「问不出来」处理。
        return None
    if result.returncode != 0:
        return None
    return result.stdout.decode("utf-8", "replace")


def parse_filter_listing(text: str) -> FrozenSet[str]:
    """解析 `ffmpeg -filters` 的名称集合。

    刻意**不依赖分隔线**：滤镜清单没有分隔线（只有编码器清单有）。
    图例行形如 `  T.. = Timeline support`，靠「第三列含 `->`」就能把它们排除掉。
    """
    names = set()
    for line in (text or "").splitlines():
        tokens = line.split()
        if len(tokens) < 3:
            continue
        flags, name, io = tokens[0], tokens[1], tokens[2]
        if len(flags) < 2 or "->" not in io or name == "=":
            continue
        names.add(name)
    return frozenset(names)


def parse_encoder_listing(text: str) -> FrozenSet[str]:
    """解析 `ffmpeg -encoders` 的名称集合（只取视频/音频/字幕三类）。

    编码器清单**有**分隔线，且图例行形如 ` V..... = Video`——不先跳过的话 `=` 会被当成
    编码器名。所以这里照 Rust 侧的做法：见到纯 `-` 行之后才开始解析。
    """
    names = set()
    in_body = False
    for line in (text or "").splitlines():
        trimmed = line.strip()
        if not in_body:
            if len(trimmed) >= 6 and set(trimmed) == {"-"}:
                in_body = True
            continue
        tokens = trimmed.split()
        if len(tokens) < 2:
            continue
        flags, name = tokens[0], tokens[1]
        if len(flags) < 6 or not flags[0].isalpha():
            continue
        if flags[0] in ("V", "A", "S"):
            names.add(name)
    return frozenset(names)


def _listing(program: str, flag: str, parser: Callable[[str], FrozenSet[str]]) -> FrozenSet[str]:
    key = program + "\x00" + flag
    cached = _LISTING_CACHE.get(key)
    if cached is not None:
        return cached
    text = _run_listing(program, flag)
    names = frozenset() if text is None else parser(text)
    _LISTING_CACHE[key] = names
    return names


def ffmpeg_filters(settings: Optional[Settings]) -> FrozenSet[str]:
    if settings is None:
        return frozenset()
    return _listing(getattr(settings, "ffmpeg", "ffmpeg"), "-filters", parse_filter_listing)


def ffmpeg_encoders(settings: Optional[Settings]) -> FrozenSet[str]:
    if settings is None:
        return frozenset()
    return _listing(getattr(settings, "ffmpeg", "ffmpeg"), "-encoders", parse_encoder_listing)


def present(program: str) -> bool:
    """程序能否被解析到（`which` 语义，不执行它）。"""
    return bool(program) and shutil.which(program) is not None


def ffmpeg_available(settings: Optional[Settings]) -> bool:
    return settings is not None and present(getattr(settings, "ffmpeg", "ffmpeg"))


def ffprobe_available(settings: Optional[Settings]) -> bool:
    return settings is not None and present(getattr(settings, "ffprobe", "ffprobe"))


def _check_names(kind: str, names: tuple) -> None:
    """每个参数必须是**单个**滤镜/编码器名。

    曾经出现过 `requires_filters("tmix,rotate,noise,unsharp")`：整串被当成一个名字去查，
    永远查不到，三个去重模块因此永远不会被派发，且没有任何报错。这里在模块注册时就拒绝。
    """
    if not names:
        raise ValueError(f"requires_{kind} 至少需要一个名字")
    for name in names:
        if not isinstance(name, str) or not name or any(ch in name for ch in ", \t"):
            raise ValueError(
                f"requires_{kind} 的每个参数必须是单个名字，收到 {name!r}；"
                f"多个名字请分开传参，例如 requires_{kind}(\"a\", \"b\")"
            )


def requires_filters(*names: str) -> Callable[[Optional[Settings]], bool]:
    """生成一个「这些滤镜都得在」的 `capability_probe`。"""
    _check_names("filters", names)

    def probe(settings: Optional[Settings]) -> bool:
        if not ffmpeg_available(settings):
            return False
        available = ffmpeg_filters(settings)
        return all(name in available for name in names)

    probe.__name__ = "requires_filters__" + "__".join(names)
    return probe


def requires_encoders(*names: str) -> Callable[[Optional[Settings]], bool]:
    """生成一个「这些编码器都得在」的 `capability_probe`。"""
    _check_names("encoders", names)

    def probe(settings: Optional[Settings]) -> bool:
        if not ffmpeg_available(settings):
            return False
        available = ffmpeg_encoders(settings)
        return all(name in available for name in names)

    probe.__name__ = "requires_encoders__" + "__".join(names)
    return probe


def requires_all(*probes: Callable[[Optional[Settings]], bool]) -> Callable[[Optional[Settings]], bool]:
    """把多个探测条件合成一个（全部成立才算成立）。

    需要它是因为一个模块的能力常常同时依赖「程序在」和「滤镜/编码器在」，
    例如 `media.encode` 既要用基础滤镜拼时间线，又要用 `libx264` 出片。
    合成后仍然只暴露一个布尔结论——`worker_capabilities` 的语义不变。
    """
    if not probes:
        raise ValueError("requires_all 至少需要一个探测条件")

    def probe(settings: Optional[Settings]) -> bool:
        return all(item(settings) for item in probes)

    probe.__name__ = "requires_all__" + "__".join(
        getattr(item, "__name__", "probe") for item in probes
    )
    return probe


def missing_filters(settings: Optional[Settings], names: Iterable[str]) -> List[str]:
    """哪些滤镜本机没有（用于在错误信息里说清楚缺什么，而不是只报「不支持」）。"""
    available = ffmpeg_filters(settings)
    return [name for name in names if name not in available]


def missing_encoders(settings: Optional[Settings], names: Iterable[str]) -> List[str]:
    available = ffmpeg_encoders(settings)
    return [name for name in names if name not in available]
