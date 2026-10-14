"""C5 时间线处理、C10 分集重组、C11 解说重剪。

这三个功能都属于「编排」：它们**只产出计划**（时间线、章节、候选时间线），
真正的剪辑由 `media.encode` 物化。好处是编排结果可以单独导出、审查、复现——
文档对 C10/C11 的要求正是「时间线可审查、可重现」「允许人工审核」。
"""

from __future__ import annotations

import difflib
import json
import math
from typing import List, Literal, Optional

from pydantic import Field, model_validator

from ..base import StrictModel
from ..errors import bad_param
from ..recipes.registry import ModuleSpec, register
from .ffmpeg import MediaContext, run_checked

# ---------- C5 时间线 ----------


class Segment(StrictModel):
    """一个保留片段。`end=None` 表示到素材结尾。"""

    input_index: int = Field(0, ge=0)
    start: float = Field(0.0, ge=0.0)
    end: Optional[float] = Field(None, gt=0.0)

    @model_validator(mode="after")
    def _ordered(self):
        if self.end is not None and self.end <= self.start:
            raise ValueError("片段 end 必须大于 start")
        return self


class TimelineParams(StrictModel):
    #: 保留片段，按给定顺序拼接。留空表示整片不裁剪。
    segments: List[Segment] = Field(default_factory=list)
    #: 场景检测：只产出候选切点，供人工审核，不会自动套用。
    scene_detect: bool = False
    scene_threshold: float = Field(0.35, gt=0.0, le=1.0)
    #: 场景检测最多分析多少秒，避免长片耗时失控。
    scene_max_analyze_seconds: float = Field(1800.0, gt=0.0)
    #: 目标时长上限（秒）；超出时拒绝，避免误提交整片。
    max_output_seconds: Optional[float] = Field(None, gt=0.0)


def detect_scenes(ctx: MediaContext, threshold: float, max_seconds: float) -> List[dict]:
    report = ctx.scratch("scene_detect.txt")
    run_checked(
        [
            ctx.ffmpeg,
            "-hide_banner",
            "-nostdin",
            "-t",
            f"{max_seconds:g}",
            "-i",
            str(ctx.primary),
            "-vf",
            f"select='gt(scene,{threshold})',metadata=print:file={report}",
            "-an",
            "-f",
            "null",
            "-",
        ],
        timeout=1800.0,
    )
    text = report.read_text(encoding="utf-8", errors="replace") if report.exists() else ""
    cuts, pending = [], None
    for line in text.splitlines():
        if line.startswith("frame:"):
            pending = None
            for token in line.split():
                if token.startswith("pts_time:"):
                    try:
                        pending = float(token.split(":", 1)[1])
                    except ValueError:
                        pending = None
        elif line.startswith("lavfi.scene_score=") and pending is not None:
            try:
                score = float(line.split("=", 1)[1])
            except ValueError:
                continue
            cuts.append({"time": round(pending, 3), "score": round(score, 4)})
            pending = None
    return cuts


def timeline_duration(ctx: MediaContext) -> float:
    timeline = ctx.plan.get("timeline")
    if not timeline or not timeline.get("segments"):
        return float(ctx.probe.get("duration") or 0.0)
    total = 0.0
    for segment in timeline["segments"]:
        end = segment["end"]
        if end is None:
            end = float(ctx.probe.get("duration") or 0.0)
        total += max(0.0, end - segment["start"])
    return round(total, 3)


def implement_timeline(ctx: MediaContext, params: TimelineParams) -> dict:
    for segment in params.segments:
        if segment.input_index >= len(ctx.inputs):
            raise bad_param(f"片段引用了不存在的输入素材 #{segment.input_index}")

    segments = [s.model_dump() for s in params.segments]
    if not segments:
        segments = [{"input_index": 0, "start": 0.0, "end": None}]
        ctx.notes.append("未指定片段，按整片处理")

    ctx.plan["timeline"] = {
        "segments": segments,
        "source": "user_edits",
        # 剪辑决策由用户给出，`pending_review` 只在 C11 候选时间线上为真。
        "pending_review": False,
    }

    if params.scene_detect:
        cuts = detect_scenes(ctx, params.scene_threshold, params.scene_max_analyze_seconds)
        ctx.plan["scene_candidates"] = {"threshold": params.scene_threshold, "cuts": cuts}
        ctx.notes.append(f"场景检测到 {len(cuts)} 个候选切点（需人工确认后才应使用）")

    total = timeline_duration(ctx)
    if params.max_output_seconds is not None and total > params.max_output_seconds:
        raise bad_param(
            f"时间线时长 {total:.2f}s 超过上限 {params.max_output_seconds:g}s，请检查剪辑范围"
        )
    ctx.notes.append(f"时间线：{len(segments)} 段，合计 {total:.2f}s")
    return {"segments": len(segments), "duration_seconds": total}


# ---------- C10 分集重组 ----------


class ChaptersParams(StrictModel):
    mode: Literal["chapters", "episodes"] = "chapters"
    #: 每集目标时长（秒）；mode=episodes 时必填。
    episode_seconds: Optional[float] = Field(None, gt=1.0)
    #: 边界检查：短于该时长的分集判为不合格。
    min_segment_seconds: float = Field(20.0, gt=0.0)
    titles: List[str] = Field(default_factory=list)
    #: 是否真的输出多个分集文件；false 时只写章节标记。
    split: bool = False

    @model_validator(mode="after")
    def _require_episode_seconds(self):
        if self.mode == "episodes" and self.episode_seconds is None:
            raise ValueError("mode=episodes 时必须给出 episode_seconds")
        return self


def implement_chapters(ctx: MediaContext, params: ChaptersParams) -> dict:
    total = timeline_duration(ctx)
    if total <= 0:
        raise bad_param("时间线时长为 0，无法分集")

    if params.mode == "chapters":
        # 章节模式：只是容器里的章节标记，不切文件，也不受「单集最短时长」约束。
        count = max(1, len(params.titles) or max(1, int(round(total / 300.0))))
        step = total / count
        ranges = [
            {"title": (params.titles[i] if i < len(params.titles) else f"章节 {i + 1}"),
             "start": round(i * step, 3), "end": round(min((i + 1) * step, total), 3)}
            for i in range(count)
        ]
    else:
        step = float(params.episode_seconds)
        count = max(1, math.ceil(total / step))
        ranges = [
            {
                "title": params.titles[i] if i < len(params.titles) else f"第 {i + 1} 集",
                "start": round(i * step, 3),
                "end": round(min((i + 1) * step, total), 3),
            }
            for i in range(count)
        ]
        # 整除不尽时末尾会剩一个几秒的碎片集。与其报「分集过短」，不如并进上一集——
        # 这才是分集真正的语义（最后一集短一点是正常的，碎成 0.02s 不是）。
        if len(ranges) > 1:
            tail = ranges[-1]
            if tail["end"] - tail["start"] < params.min_segment_seconds:
                ranges.pop()
                ranges[-1]["end"] = tail["end"]
                ctx.notes.append(
                    f"末尾余量 {tail['end'] - tail['start']:.3f}s 并入上一集（{tail['title']}）"
                )

    # 章节只是容器里的标记，不受「单集最短时长」约束；只有真的切成多集时才检查边界。
    too_short = (
        [r for r in ranges if r["end"] - r["start"] < params.min_segment_seconds]
        if params.mode == "episodes"
        else []
    )
    if too_short:
        raise bad_param(
            "以下分集短于下限 {min:g}s，请调整规则：".format(min=params.min_segment_seconds)
            + "、".join(f"{r['title']}({r['end'] - r['start']:.1f}s)" for r in too_short)
        )

    ctx.plan["chapters"] = {"ranges": ranges, "split": params.split}
    ctx.notes.append(f"分集/章节：{len(ranges)} 段（{'输出多个文件' if params.split else '仅写章节标记'}）")
    return {"count": len(ranges), "ranges": ranges}


# ---------- C11 解说重剪 ----------


class TranscriptCue(StrictModel):
    start: float = Field(ge=0.0)
    end: float = Field(gt=0.0)
    text: str


class CommentaryParams(StrictModel):
    #: 解说音轨所在的输入素材下标（0 = 主素材）。
    commentary_input_index: int = Field(0, ge=0)
    #: 语音转写结果。本机没有可用的 ASR 模型，因此要求外部提供（文档 §12 也把
    #: 「字幕来源」列为开发前须确认项）。
    transcript: List[TranscriptCue] = Field(default_factory=list)
    #: 解说脚本，一句一行，按顺序匹配转写片段。
    script: List[str] = Field(default_factory=list)
    match_threshold: float = Field(0.45, ge=0.0, le=1.0)
    #: 人工审核开关。为 false 时只产出候选时间线，不改变本次输出。
    approve: bool = False


def _similarity(a: str, b: str) -> float:
    return difflib.SequenceMatcher(None, a, b).ratio()


def implement_commentary(ctx: MediaContext, params: CommentaryParams) -> dict:
    if params.commentary_input_index >= len(ctx.inputs):
        raise bad_param(f"解说素材下标 {params.commentary_input_index} 超出输入数量")
    if not params.script:
        raise bad_param("解说重剪需要提供 script（解说脚本）")
    if not params.transcript:
        # 明确失败而不是产出空时间线：无声转写就没法把脚本对齐到时间轴。
        raise bad_param("解说重剪需要提供 transcript（转写时间戳）；本机未内置 ASR")

    used, segments, matched = set(), [], []
    for index, line in enumerate(params.script):
        best, best_score = None, 0.0
        for cue_index, cue in enumerate(params.transcript):
            if cue_index in used:
                continue
            score = _similarity(line, cue.text)
            if score > best_score:
                best, best_score = cue_index, score
        if best is not None and best_score >= params.match_threshold:
            used.add(best)
            cue = params.transcript[best]
            segments.append(
                {
                    "input_index": params.commentary_input_index,
                    "start": round(cue.start, 3),
                    "end": round(cue.end, 3),
                }
            )
            matched.append(
                {"script": line, "cue": cue.text, "score": round(best_score, 3),
                 "start": cue.start, "end": cue.end}
            )
        else:
            matched.append({"script": line, "cue": None, "score": round(best_score, 3),
                            "start": None, "end": None})

    candidate_seconds = round(sum(m["end"] - m["start"] for m in matched if m["start"] is not None), 3)
    original_seconds = timeline_duration(ctx)
    candidate = {
        "source": "commentary_candidate",
        "segments": segments,
        "matched": matched,
        "candidate_seconds": candidate_seconds,
        "original_seconds": original_seconds,
        "dropped_seconds": round(max(0.0, original_seconds - candidate_seconds), 3),
        "approved": bool(params.approve),
    }

    report_path = ctx.scratch("commentary_candidate.json")
    report_path.write_text(json.dumps(candidate, ensure_ascii=False, indent=2), encoding="utf-8")
    ctx.artifacts.append(
        {
            "kind": "commentary_candidate",
            "path": report_path,
            "name": "commentary_candidate.json",
            "media": {"segments": len(segments), "candidate_seconds": candidate_seconds},
        }
    )
    ctx.plan["commentary_candidate"] = candidate

    if not segments:
        raise bad_param(
            "脚本与转写没有任何片段达到匹配阈值，请检查 transcript 或降低 match_threshold"
        )

    if params.approve:
        ctx.plan["timeline"] = {
            "segments": segments,
            "source": "commentary_candidate",
            "pending_review": False,
        }
        ctx.notes.append(f"解说候选时间线已人工审核通过并应用：{len(segments)} 段")
    else:
        # 不改变本次输出，只留下候选供人工审核——这是文档对 C11 的硬要求。
        ctx.plan_section("review", {})["commentary"] = "pending"
        ctx.notes.append(
            f"已生成解说候选时间线（{len(segments)} 段 / {candidate_seconds:.2f}s），"
            "approve=false 故未应用，需人工审核后重跑"
        )
    return {"segments": len(segments), "approved": bool(params.approve)}


register(
    ModuleSpec(
        name="timeline.user_edits",
        title="时间线编辑（C5）",
        capability="ffmpeg",
        requires=frozenset({"video", "audio"}),
        produces=frozenset({"timeline"}),
        params_model=TimelineParams,
        implement=implement_timeline,
    )
)
register(
    ModuleSpec(
        name="chapters.reorg",
        title="分集重组（C10）",
        capability="ffmpeg",
        requires=frozenset({"timeline"}),
        produces=frozenset({"chapters"}),
        params_model=ChaptersParams,
        implement=implement_chapters,
    )
)
register(
    ModuleSpec(
        name="commentary.rebuild",
        title="解说重剪（C11）",
        capability="ffmpeg",
        # 转写内容由配方参数提供（本机无 ASR），所以依赖的是音轨存在，而不是某个前置产出。
        requires=frozenset({"audio"}),
        produces=frozenset({"timeline"}),
        params_model=CommentaryParams,
        implement=implement_commentary,
    )
)
