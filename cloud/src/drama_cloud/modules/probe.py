"""媒体探测模块。

管线第一步：把输入素材探测成 `ctx.probe`，后面的计划型模块都依赖它。
"""

from __future__ import annotations

from ..base import StrictModel
from ..recipes.registry import ModuleSpec, register
from .ffmpeg import MediaContext, probe_json, summarize
from .ffmpeg_caps import ffprobe_available


class ProbeParams(StrictModel):
    pass


def implement_probe(ctx: MediaContext, params: ProbeParams) -> dict:
    info = probe_json(ctx.primary, ctx.settings)
    ctx.probe = summarize(info)
    if not ctx.probe.get("has_video"):
        from ..errors import corrupt_input

        raise corrupt_input("输入素材里没有视频轨道")
    ctx.notes.append(
        "探测：{width}x{height} @ {fps}fps，{duration:.2f}s，音轨={has_audio}，"
        "字幕轨={has_subtitle}".format(**ctx.probe)
    )
    return {}


register(
    ModuleSpec(
        name="media.probe",
        title="媒体探测",
        capability="ffprobe",
        requires=frozenset(),
        produces=frozenset({"video", "audio", "metadata"}),
        params_model=ProbeParams,
        kind="plan",
        implement=implement_probe,
        # 探测能力靠的是 ffprobe 这个程序本身，只要它在就成立。
        capability_probe=ffprobe_available,
    )
)
