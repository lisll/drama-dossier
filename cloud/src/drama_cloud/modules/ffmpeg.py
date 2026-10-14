"""FFmpeg / ffprobe 封装与处理上下文。

模块只拿 `MediaContext` 干活：读当前视频、往 `ctx.plan` 里写「要做什么」，
由 `media.encode` 统一物化。这样 C4/C5/C7/C8/C9 这些「计划型」模块天然可单测
（不碰磁盘、不跑 ffmpeg），而真正调用外部进程的地方只有这一个文件。
"""

from __future__ import annotations

import json
import shutil
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, List, Optional

from ..config import Settings
from ..errors import ErrorClass, WorkError

#: 把 ffmpeg/ffprobe 的常见报错映射到错误分类（§7.2 重试规则）。
_PATTERNS = (
    ("invalid data found when processing input", ErrorClass.INPUT_CORRUPT),
    ("moov atom not found", ErrorClass.INPUT_CORRUPT),
    ("error opening input", ErrorClass.INPUT_CORRUPT),
    ("no space left on device", ErrorClass.STORAGE_UNAVAILABLE),
    ("cannot allocate memory", ErrorClass.RESOURCE_CONTENTION),
    ("resource temporarily unavailable", ErrorClass.RESOURCE_CONTENTION),
    ("unknown encoder", ErrorClass.TARGET_UNSUPPORTED),
    ("encoder not found", ErrorClass.TARGET_UNSUPPORTED),
    ("unknown decoder", ErrorClass.TARGET_UNSUPPORTED),
    ("unsupported codec", ErrorClass.TARGET_UNSUPPORTED),
    ("does not support", ErrorClass.TARGET_UNSUPPORTED),
    ("invalid argument", ErrorClass.PARAM_OUT_OF_RANGE),
    ("no such filter", ErrorClass.PARAM_OUT_OF_RANGE),
)


def classify(stderr: str) -> ErrorClass:
    text = (stderr or "").lower()
    for needle, error_class in _PATTERNS:
        if needle in text:
            return error_class
    return ErrorClass.INTERNAL


def _tail(text: str, limit: int = 800) -> str:
    text = (text or "").strip()
    return text if len(text) <= limit else "…" + text[-limit:]


@dataclass
class MediaContext:
    """一次任务执行期间的共享状态。"""

    settings: Settings
    work_dir: Path
    primary: Path
    inputs: List[Path] = field(default_factory=list)
    probe: Dict = field(default_factory=dict)
    plan: Dict = field(default_factory=dict)
    encoded: Optional[Path] = None
    #: 登记为产物的 (kind, 路径, 媒体信息) 列表。
    artifacts: List[dict] = field(default_factory=list)
    notes: List[str] = field(default_factory=list)
    #: 用户本次任务的运行期参数（覆盖配方参数时使用）。
    user_params: Dict = field(default_factory=dict)
    #: 当前是第几次尝试（从 1 开始）。质量类重试靠它逐级加力：
    #: 第 2 次加大 mask 膨胀/羽化，第 3 次可切换更强的修复后端。
    attempt: int = 1

    @property
    def ffmpeg(self) -> str:
        return self.settings.ffmpeg

    @property
    def ffprobe(self) -> str:
        return self.settings.ffprobe

    def scratch(self, name: str) -> Path:
        target = self.work_dir / name
        target.parent.mkdir(parents=True, exist_ok=True)
        return target

    def plan_section(self, key: str, default):
        return self.plan.setdefault(key, default)


# ---------- 进程调用 ----------


def run(args: List[str], timeout: float = 3600.0, cwd: Optional[Path] = None) -> subprocess.CompletedProcess:
    try:
        return subprocess.run(
            args,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout,
            check=False,
            cwd=str(cwd) if cwd else None,
        )
    except FileNotFoundError as error:
        raise WorkError(ErrorClass.CAPABILITY_MISSING, f"找不到可执行文件：{args[0]}") from error
    except subprocess.TimeoutExpired as error:
        raise WorkError(
            ErrorClass.RESOURCE_CONTENTION, f"命令超时（{timeout:.0f}s）：{args[0]}"
        ) from error


def run_checked(args: List[str], timeout: float = 3600.0, cwd: Optional[Path] = None) -> None:
    result = run(args, timeout=timeout, cwd=cwd)
    if result.returncode != 0:
        stderr = result.stderr.decode("utf-8", "replace")
        raise WorkError(
            classify(stderr),
            f"{Path(args[0]).name} 退出码 {result.returncode}：{_tail(stderr)}",
        )


def probe_json(path: Path, settings: Settings) -> dict:
    result = run(
        [
            settings.ffprobe,
            "-hide_banner",
            "-loglevel",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            str(path),
        ],
        timeout=120.0,
    )
    if result.returncode != 0:
        stderr = result.stderr.decode("utf-8", "replace")
        raise WorkError(classify(stderr), f"探测失败：{_tail(stderr)}")
    try:
        return json.loads(result.stdout.decode("utf-8", "replace") or "{}")
    except json.JSONDecodeError as error:
        raise WorkError(ErrorClass.INTERNAL, "ffprobe 输出不是合法 JSON") from error


# ---------- 探测结果归一化 ----------


def _fraction(value: Optional[str]) -> float:
    if not value:
        return 0.0
    if "/" in value:
        num, _, den = value.partition("/")
        try:
            denominator = float(den)
            return float(num) / denominator if denominator else 0.0
        except ValueError:
            return 0.0
    try:
        return float(value)
    except ValueError:
        return 0.0


def summarize(info: dict) -> dict:
    """把 ffprobe 原始输出压成质量门禁与计划需要的字段。"""
    streams = info.get("streams") or []
    fmt = info.get("format") or {}
    video = next((s for s in streams if s.get("codec_type") == "video"), None)
    audios = [s for s in streams if s.get("codec_type") == "audio"]
    subtitles = [s for s in streams if s.get("codec_type") == "subtitle"]
    summary = {
        "duration": float(fmt.get("duration") or 0.0),
        "size_bytes": int(fmt.get("size") or 0),
        "format_name": fmt.get("format_name") or "",
        "nb_streams": len(streams),
        "has_video": video is not None,
        "has_audio": bool(audios),
        "has_subtitle": bool(subtitles),
        "audio_streams": len(audios),
        "subtitle_streams": len(subtitles),
        "width": 0,
        "height": 0,
        "fps": 0.0,
        "video_codec": None,
        "audio_codec": audios[0].get("codec_name") if audios else None,
    }
    if video is not None:
        summary.update(
            {
                "width": int(video.get("width") or 0),
                "height": int(video.get("height") or 0),
                "fps": round(_fraction(video.get("avg_frame_rate") or video.get("r_frame_rate")), 4),
                "video_codec": video.get("codec_name"),
                "pix_fmt": video.get("pix_fmt"),
            }
        )
    summary.update(_timing(streams, summary["duration"]))
    return summary


def _timing(streams: List[dict], fallback_duration: float) -> dict:
    """音视频时间基准，用于音画同步检查。"""
    video = next((s for s in streams if s.get("codec_type") == "video"), None)
    audio = next((s for s in streams if s.get("codec_type") == "audio"), None)

    def start(stream) -> float:
        if not stream:
            return 0.0
        try:
            return float(stream.get("start_time") or 0.0)
        except ValueError:
            return 0.0

    def duration(stream) -> float:
        if not stream:
            return 0.0
        for key in ("duration", "duration_ts"):
            raw = stream.get(key)
            if raw in (None, "N/A"):
                continue
            try:
                value = float(raw)
            except ValueError:
                continue
            if key == "duration_ts":
                value = value / 1_000_000.0
            return value
        return fallback_duration

    video_start, audio_start = start(video), start(audio)
    return {
        "video_start": video_start,
        "audio_start": audio_start,
        "video_duration": duration(video),
        "audio_duration": duration(audio),
        # 同步偏差 = 起始时间差 + 时长差的综合；单侧缺失时按 0 计。
        "av_sync_ms": round(
            (audio_start - video_start + (duration(audio) - duration(video))) * 1000.0, 2
        )
        if audio is not None and video is not None
        else 0.0,
    }


def which(program: str) -> Optional[str]:
    return shutil.which(program)


def ffmpeg_version(settings: Settings) -> str:
    result = run([settings.ffmpeg, "-hide_banner", "-version"], timeout=30.0)
    if result.returncode != 0:
        return "unknown"
    first = result.stdout.decode("utf-8", "replace").splitlines()[:1]
    return (first[0] if first else "unknown").strip()
