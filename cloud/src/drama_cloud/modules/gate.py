"""质量门禁（设计文档 §10）。

门禁是任务成功与失败的分界线：只有它的全部检查通过，产物才会被登记为最终产物。
覆盖文档点名的检查项：可解码性、目标分辨率/帧率/编码、时长、音画同步、音轨/字幕要求、
产物哈希，以及文件大小。
"""

from __future__ import annotations

from typing import List, Optional

from pydantic import Field

from ..base import StrictModel
from ..errors import ErrorClass, WorkError
from ..recipes.registry import ModuleSpec, register
from .ffmpeg import MediaContext, probe_json, summarize
from .ffmpeg_caps import ffprobe_available
from .timeline import timeline_duration


class QualityGateParams(StrictModel):
    """未给出的字段回落到配方的 `quality_policy`。"""

    require_decodable_output: Optional[bool] = None
    check_audio_video_sync: Optional[bool] = None
    validate_target_profile: Optional[bool] = None
    max_av_sync_ms: Optional[float] = Field(None, gt=0.0)
    min_duration_seconds: Optional[float] = Field(None, ge=0.0)
    #: 时长偏差容差（秒）。剪辑/重采样会带来零头，给一个显式容差而不是「差不多」。
    duration_tolerance_seconds: float = Field(1.0, ge=0.0, le=30.0)


def _policy(ctx: MediaContext, params: QualityGateParams) -> dict:
    policy = dict(ctx.plan.get("quality_policy") or {})
    defaults = {
        "require_decodable_output": True,
        "check_audio_video_sync": True,
        "validate_target_profile": True,
        "max_av_sync_ms": 200.0,
        "min_duration_seconds": 0.5,
    }
    merged = {**defaults, **policy}
    for field in (
        "require_decodable_output",
        "check_audio_video_sync",
        "validate_target_profile",
        "max_av_sync_ms",
        "min_duration_seconds",
    ):
        override = getattr(params, field)
        if override is not None:
            merged[field] = override
    return merged


def implement_quality_gate(ctx: MediaContext, params: QualityGateParams) -> dict:
    if ctx.encoded is None:
        raise WorkError(ErrorClass.QUALITY_GATE_FAILED, "没有可检验的产物：编码步骤未产出文件")

    policy = _policy(ctx, params)
    checks: List[dict] = []

    def record(name: str, ok: bool, detail: str = "") -> bool:
        checks.append({"name": name, "ok": bool(ok), "detail": detail})
        return ok

    encoded = ctx.encoded
    size = encoded.stat().st_size if encoded.exists() else 0
    record("file_exists", encoded.exists() and size > 0, f"{size} bytes")

    # 可解码性：能探测出视频轨才算可解码。
    summary = {}
    decodable = False
    try:
        info = probe_json(encoded, ctx.settings)
        summary = summarize(info)
        decodable = bool(summary.get("has_video")) and summary.get("duration", 0) > 0
    except WorkError as error:
        record("decodable", False, error.message)
    if not decodable and not any(c["name"] == "decodable" for c in checks):
        record("decodable", False, "产物没有可解码的视频轨")
    elif decodable:
        record("decodable", True, f"{summary['width']}x{summary['height']} @ {summary['fps']}fps")

    encode_plan = ctx.plan.get("encode") or {}
    if policy.get("validate_target_profile") and decodable:
        expected_video = {
            "h264": "h264",
            "h265": "hevc",
            "vp9": "vp9",
            "av1": "av1",
        }.get(encode_plan.get("video_codec"), None)
        if expected_video:
            record(
                "target_video_codec",
                summary.get("video_codec") == expected_video,
                f"期望 {expected_video}，实际 {summary.get('video_codec')}",
            )
        target_resolution = ctx.plan.get("target_resolution") or {}
        if target_resolution and summary.get("width"):
            expected_w = target_resolution.get("width")
            expected_h = target_resolution.get("height")
            actual_w, actual_h = summary["width"], summary["height"]
            # `fit`/`pad` 模式下只给一边时另一边按比例推导，所以按「不超过框」判断。
            ok = True
            if expected_w and actual_w > expected_w:
                ok = False
            if expected_h and actual_h > expected_h:
                ok = False
            if target_resolution.get("mode") == "fill" and (
                (expected_w and actual_w != expected_w) or (expected_h and actual_h != expected_h)
            ):
                ok = False
            record(
                "target_resolution",
                ok,
                f"期望 ≤{expected_w}x{expected_h}，实际 {actual_w}x{actual_h}",
            )

        expected_container = encode_plan.get("container")
        if expected_container:
            names = (summary.get("format_name") or "").split(",")
            # ffprobe 对 mp4 报 mov,mp4,m4a,3gp,3g2,mj2；只要交集非空即视为匹配。
            expected_names = {"mp4": {"mp4", "mov"}, "mkv": {"matroska", "webm"}, "webm": {"webm"}}
            record(
                "target_container",
                bool(expected_names.get(expected_container, set()) & set(names)),
                f"期望 {expected_container}，实际 {summary.get('format_name')}",
            )

    # 时长：与时间线期望值比对。
    expected_duration = timeline_duration(ctx)
    if decodable and expected_duration > 0:
        delta = abs(summary["duration"] - expected_duration)
        record(
            "duration",
            delta <= params.duration_tolerance_seconds,
            f"期望 {expected_duration:.3f}s，实际 {summary['duration']:.3f}s，偏差 {delta:.3f}s",
        )
        record(
            "min_duration",
            summary["duration"] >= float(policy.get("min_duration_seconds") or 0.0),
            f"{summary['duration']:.3f}s",
        )

    # 音画同步
    if policy.get("check_audio_video_sync") and decodable:
        if summary.get("has_audio"):
            drift = abs(float(summary.get("av_sync_ms") or 0.0))
            record(
                "av_sync",
                drift <= float(policy.get("max_av_sync_ms") or 200.0),
                f"偏差 {drift:.1f}ms（上限 {policy.get('max_av_sync_ms')}ms）",
            )
        else:
            record("av_sync", True, "产物无音轨，跳过同步检查")

    # 音轨要求：源有音轨且未被显式丢弃时，产物必须保留音轨。
    if decodable:
        audio_expected = bool(ctx.probe.get("has_audio")) and encode_plan.get("audio_codec") != "none"
        record(
            "audio_track",
            summary.get("has_audio") == audio_expected,
            f"期望音轨={audio_expected}，实际={summary.get('has_audio')}",
        )

    # 字幕要求：软字幕必须真的封装进去。
    subtitle = ctx.plan.get("subtitle")
    if decodable and subtitle and subtitle.get("mode") == "soft":
        record(
            "subtitle_track",
            bool(summary.get("has_subtitle")),
            f"期望字幕轨，实际 {summary.get('subtitle_streams')} 条",
        )

    # 产物哈希：门禁通过后登记产物时要复用这个值。
    try:
        digest = ctx.settings and _sha256(encoded)
        record("artifact_hash", bool(digest), digest[:16] + "…")
    except OSError as error:
        digest = None
        record("artifact_hash", False, str(error))

    failed = [c for c in checks if not c["ok"]]
    report = {
        "ok": not failed,
        "checks": checks,
        "encoded": str(encoded),
        "sha256": digest,
        "size_bytes": size,
        "media": summary,
    }
    ctx.plan["quality_report"] = report

    if failed:
        detail = "；".join(f"{c['name']}: {c['detail']}" for c in failed)
        raise WorkError(ErrorClass.QUALITY_GATE_FAILED, f"质量门禁未通过 → {detail}")

    ctx.notes.append(f"质量门禁通过（{len(checks)} 项检查）")
    return report


def _sha256(path) -> str:
    import hashlib

    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


register(
    ModuleSpec(
        name="media.quality_gate",
        title="质量门禁",
        capability="ffprobe",
        requires=frozenset({"encoded"}),
        produces=frozenset({"quality_report"}),
        params_model=QualityGateParams,
        implement=implement_quality_gate,
        capability_probe=ffprobe_available,
    )
)
