"""C6 编码处理、C3 字幕工作流、C8 元数据处理。

`media.encode` 是**唯一物化**的模块：它把前面所有计划型模块写进 `ctx.plan` 的东西
（时间线、视频/音频滤镜、字幕、元数据策略、章节）拼成一条 ffmpeg 命令并执行。
"""

from __future__ import annotations

from pathlib import Path
from typing import List, Literal, Optional

from pydantic import Field, model_validator

from ..base import StrictModel
from ..errors import WorkError, ErrorClass, bad_param
from ..recipes.registry import ModuleSpec, register
from .ffmpeg import MediaContext, run_checked
from .ffmpeg_caps import (
    ffmpeg_available,
    missing_encoders,
    requires_all,
    requires_encoders,
    requires_filters,
)

# ---------- C3 字幕 ----------


class SubtitleSource(StrictModel):
    kind: Literal["external", "embedded", "planned"]
    #: kind=external 时：输入素材下标（字幕文件作为一份独立输入）。
    input_index: Optional[int] = Field(None, ge=0)
    #: kind=embedded 时：素材内字幕轨下标。
    stream_index: Optional[int] = Field(None, ge=0)
    #: kind=planned 时：产出字幕的步骤模块名（默认 `subtitle.asr`）。
    #: 这是「识别 → 上字幕」一条管线跑完的关键：字幕不在输入素材里，而是前序步骤刚生成的。
    producer: str = "subtitle.asr"

    @model_validator(mode="after")
    def _check(self):
        if self.kind == "external" and self.input_index is None:
            raise ValueError("external 字幕必须给出 input_index")
        if self.kind == "embedded" and self.stream_index is None:
            raise ValueError("embedded 字幕必须给出 stream_index")
        return self


class SubtitleParams(StrictModel):
    source: SubtitleSource
    #: burn = 烧进画面（不可关闭）；soft = 作为可选择字幕轨封装。
    mode: Literal["burn", "soft"] = "soft"
    language: str = "chi"
    title: str = "中文"


def implement_subtitle(ctx: MediaContext, params: SubtitleParams) -> dict:
    source = params.source
    if source.kind == "planned":
        # 字幕来自同一条管线的前序步骤（ASR 刚产出的 SRT）。找不到就显式报错——
        # 「静默当成没有字幕」会让整条任务在质量门禁之外悄悄少做一件事。
        plan_key = "asr" if source.producer == "subtitle.asr" else source.producer
        produced = ctx.plan.get(plan_key) or {}
        path = produced.get("subtitle")
        if not path or not Path(str(path)).is_file():
            raise bad_param(
                f"source.kind=planned 需要前序步骤 {source.producer} 的产出，"
                f"但计划里没有可用的字幕文件（plan[{plan_key!r}].subtitle）"
            )
        plan = {
            "mode": params.mode,
            "kind": "planned",
            "path": str(path),
            "producer": source.producer,
            "language": params.language,
            "title": params.title,
        }
    elif source.kind == "external":
        index = source.input_index
        if index is None or index >= len(ctx.inputs):
            raise bad_param("外挂字幕的输入素材下标不存在")
        path = ctx.inputs[index]
        plan = {
            "mode": params.mode,
            "kind": "external",
            "path": str(path),
            "language": params.language,
            "title": params.title,
        }
    else:
        plan = {
            "mode": params.mode,
            "kind": "embedded",
            "stream_index": int(source.stream_index),
            "language": params.language,
            "title": params.title,
        }
    ctx.plan["subtitle"] = plan
    ctx.notes.append(f"字幕：{params.mode}（{source.kind}）")
    return plan


# ---------- C8 元数据 ----------


class MetadataParams(StrictModel):
    #: keep = 原样保留；strip = 清除全部可清除字段；edit = 按 set/remove 调整。
    mode: Literal["keep", "strip", "edit"] = "keep"
    set: dict = Field(default_factory=dict)
    remove: List[str] = Field(default_factory=list)
    #: 隐私处理：抹掉定位相关字段（mov/mp4 里的 location / com.apple.quicktime.location.*）。
    strip_location: bool = True

    @model_validator(mode="after")
    def _check_keys(self):
        forbidden = [k for k in self.set if any(ch in k for ch in " \t\n")]
        if forbidden:
            raise ValueError(f"元数据键不能包含空白字符：{forbidden}")
        return self


#: 与定位/设备相关的元数据键，`strip_location` 生效时一律移除。
LOCATION_KEYS = (
    "location",
    "location-eng",
    "com.apple.quicktime.location.ISO6709",
    "com.apple.quicktime.location.accuracy.horizontal",
    "com.android.manufacturer",
    "com.android.model",
)


def implement_metadata(ctx: MediaContext, params: MetadataParams) -> dict:
    policy = {
        "mode": params.mode,
        "set": dict(params.set),
        "remove": list(params.remove),
        "strip_location": params.strip_location,
    }
    if params.strip_location:
        policy["remove"] = sorted({*policy["remove"], *LOCATION_KEYS})
    ctx.plan["metadata_policy"] = policy
    ctx.notes.append(f"元数据策略：{params.mode}（移除 {len(policy['remove'])} 个字段）")
    return policy


# ---------- C6 编码 ----------


class EncodeParams(StrictModel):
    video_codec: Literal["h264", "h265", "vp9", "av1"] = "h264"
    audio_codec: Literal["aac", "mp3", "opus", "none"] = "aac"
    container: Literal["mp4", "mkv", "webm"] = "mp4"
    #: CRF 质量模式（与 rate_control=crf 搭配）。
    crf: int = Field(20, ge=0, le=51)
    rate_control: Literal["crf", "bitrate"] = "crf"
    video_bitrate_kbps: Optional[int] = Field(None, ge=100, le=200000)
    audio_bitrate_kbps: int = Field(128, ge=32, le=512)
    #: 编码 preset（对 h264/h265 生效）。
    preset: Literal["ultrafast", "fast", "medium", "slow"] = "medium"
    #: GOP 长度（秒）；0 表示交给编码器默认。
    gop_seconds: float = Field(2.0, ge=0.0, le=30.0)
    #: 关键帧最大间隔由 GOP 推导；强制场景切换处插入关键帧。
    scene_cut: bool = True
    frame_rate_policy: Literal["preserve", "cfr"] = "preserve"
    pix_fmt: Optional[Literal["yuv420p", "yuv444p"]] = None

    @model_validator(mode="after")
    def _check_bitrate(self):
        if self.rate_control == "bitrate" and self.video_bitrate_kbps is None:
            raise ValueError("rate_control=bitrate 时必须给出 video_bitrate_kbps")
        return self


#: 逻辑编码器名 → ffmpeg 编码器。h265 用 libx265，vp9 用 libvpx-vp9，av1 用本机可用的 libaom-av1。
VIDEO_ENCODERS = {
    "h264": "libx264",
    "h265": "libx265",
    "vp9": "libvpx-vp9",
    "av1": "libaom-av1",
}
AUDIO_ENCODERS = {"aac": "aac", "mp3": "libmp3lame", "opus": "libopus", "none": None}
#: 容器与编码器的兼容性；配方声明不支持的组合时直接拒绝，而不是产出坏文件。
CONTAINER_MATRIX = {
    "mp4": ({"h264", "h265", "av1"}, {"aac", "mp3", "none"}),
    "mkv": ({"h264", "h265", "vp9", "av1"}, {"aac", "mp3", "opus", "none"}),
    "webm": ({"vp9", "av1"}, {"opus", "none"}),
}


def check_container(params: EncodeParams) -> None:
    videos, audios = CONTAINER_MATRIX[params.container]
    if params.video_codec not in videos:
        raise WorkError(
            ErrorClass.TARGET_UNSUPPORTED,
            f"容器 {params.container} 不支持视频编码 {params.video_codec}",
        )
    if params.audio_codec not in audios:
        raise WorkError(
            ErrorClass.TARGET_UNSUPPORTED,
            f"容器 {params.container} 不支持音频编码 {params.audio_codec}",
        )


def ensure_encoders(
    ctx: MediaContext, params: EncodeParams, subtitle_codec: Optional[str]
) -> None:
    """确认这次真正会用到的编码器都在。

    为什么不在参数校验阶段做完：本机装了哪些编码器是**部署事实**，不是参数问题。
    所以这里查一次 `ffmpeg -encoders`，缺了就以 `capability_missing` 失败并列出缺哪个、
    属于什么用途——而不是让 ffmpeg 跑到一半报 "Unknown encoder"（那种报错的分类是
    `target_unsupported`，会把「机器没装」误报成「你的参数不支持」，把排查引到错方向）。

    `media.encode` 的能力探测只钉住默认的 `libx264`；h265/vp9/av1 与音频编码器是配方
    选的，只能在拿到参数后才能确认，所以放在这里补。
    """
    needed = {VIDEO_ENCODERS[params.video_codec]: "视频"}
    if params.audio_codec != "none" and ctx.probe.get("has_audio"):
        audio = AUDIO_ENCODERS[params.audio_codec]
        if audio:
            needed[audio] = "音频"
    if subtitle_codec:
        needed[subtitle_codec] = "字幕"

    missing = missing_encoders(ctx.settings, needed)
    if missing:
        detail = "、".join(f"{name}（{needed[name]}编码）" for name in missing)
        raise WorkError(
            ErrorClass.CAPABILITY_MISSING,
            f"本机 ffmpeg 缺少编码器：{detail}；请在部署侧装齐 ffmpeg，或改用本机已有的编码器",
        )


def _segment_args(ctx: MediaContext, timeline: dict) -> List[str]:
    """把时间线编译成 filter_complex 的 trim/concat 段。"""
    segments = timeline.get("segments") or [{"input_index": 0, "start": 0.0, "end": None}]
    duration = float(ctx.probe.get("duration") or 0.0)
    pieces_v, pieces_a, filters = [], [], []
    for index, segment in enumerate(segments):
        start = float(segment["start"])
        end = segment["end"]
        end = duration if end is None else float(end)
        label = f"s{index}"
        filters.append(
            f"[{segment['input_index']}:v]trim=start={start}:end={end},setpts=PTS-STARTPTS"
            f",settb=AVTB[{label}v]"
        )
        filters.append(
            f"[{segment['input_index']}:a]atrim=start={start}:end={end},asetpts=PTS-STARTPTS"
            f",asettb=AVTB[{label}a]"
        )
        pieces_v.append(f"[{label}v]")
        pieces_a.append(f"[{label}a]")
    if len(segments) > 1:
        filters.append("".join(pieces_v) + f"concat=n={len(segments)}:v=1:a=0[joinedv]")
        filters.append("".join(pieces_a) + f"concat=n={len(segments)}:v=0:a=1[joineda]")
        video_label, audio_label = "[joinedv]", "[joineda]"
    else:
        video_label, audio_label = "[s0v]", "[s0a]"
    return filters, video_label, audio_label


def implement_encode(ctx: MediaContext, params: EncodeParams) -> dict:
    check_container(params)
    timeline = ctx.plan.get("timeline") or {"segments": [{"input_index": 0, "start": 0.0, "end": None}]}
    if timeline.get("pending_review"):
        raise bad_param("时间线尚未通过人工审核，拒绝编码")

    subtitle = ctx.plan.get("subtitle")
    soft_subtitle_codec = (
        ("mov_text" if params.container == "mp4" else "srt")
        if subtitle and subtitle.get("mode") == "soft"
        else None
    )
    ensure_encoders(ctx, params, soft_subtitle_codec)

    filters, video_label, audio_label = _segment_args(ctx, timeline)
    video_filters = ctx.plan.get("video_filters") or []
    audio_filters = ctx.plan.get("audio_filters") or []

    if subtitle and subtitle.get("mode") == "burn":
        if subtitle["kind"] == "external":
            escaped = subtitle["path"].replace("\\", "\\\\").replace(":", "\\:").replace("'", "\\'")
            video_filters = [*video_filters, f"subtitles='{escaped}'"]
        else:
            # 内嵌字幕烧录需要先抽出成临时文件，交给 subtitles 滤镜。
            extracted = ctx.scratch("burn.srt")
            run_checked(
                [
                    ctx.ffmpeg, "-hide_banner", "-nostdin", "-i", str(ctx.primary),
                    "-map", f"0:{subtitle['stream_index']}", "-f", "srt", "-y", str(extracted),
                ],
                timeout=600.0,
            )
            video_filters = [*video_filters, f"subtitles='{extracted}'"]

    if video_filters:
        filters.append(f"{video_label}{','.join(video_filters)}[vout]")
        video_label = "[vout]"
    if audio_filters:
        filters.append(f"{audio_label}{','.join(audio_filters)}[aout]")
        audio_label = "[aout]"

    # 所有输入必须集中在这里声明：ffmpeg 的 -i 是输入选项，出现在输出选项之后就会报
    # "you are trying to apply an input option to an output file or vice versa"。
    args: List[str] = [ctx.ffmpeg, "-hide_banner", "-nostdin", "-y"]
    for path in ctx.inputs:
        args += ["-i", str(path)]

    # 软字幕：把外挂字幕作为额外输入并转成 mov_text（mp4）/ srt（mkv）。
    soft_subtitle = bool(subtitle and subtitle.get("mode") == "soft")
    subtitle_input_index = None
    subtitle_codec = None
    if soft_subtitle:
        if subtitle["kind"] == "external":
            subtitle_input_index = len(ctx.inputs)
            args += ["-i", subtitle["path"]]
        subtitle_codec = soft_subtitle_codec

    # C10：章节元数据文件也是输入，必须在输出选项之前声明。
    chapters = ctx.plan.get("chapters")
    chapters_input_index = None
    if chapters:
        metadata_file = ctx.scratch("chapters.ffmetadata")
        metadata_file.write_text(render_chapters(chapters["ranges"]), encoding="utf-8")
        chapters_input_index = len(ctx.inputs) + (1 if subtitle_input_index is not None else 0)
        args += ["-f", "ffmetadata", "-i", str(metadata_file)]

    args += ["-filter_complex", ";".join(filters), "-map", video_label]
    if params.audio_codec != "none" and ctx.probe.get("has_audio"):
        args += ["-map", audio_label]

    # C6：编码参数
    args += ["-c:v", VIDEO_ENCODERS[params.video_codec]]
    if params.video_codec in ("h264", "h265"):
        args += ["-preset", params.preset]
    if params.rate_control == "crf":
        crf_flag = "-crf" if params.video_codec != "vp9" else "-crf"
        args += [crf_flag, str(params.crf)]
        if params.video_codec == "vp9":
            args += ["-b:v", "0"]
    else:
        args += ["-b:v", f"{params.video_bitrate_kbps}k"]
    if params.gop_seconds > 0:
        fps = float(ctx.probe.get("fps") or 25.0)
        args += ["-g", str(max(1, int(round(fps * params.gop_seconds))))]
    if params.scene_cut and params.video_codec in ("h264", "h265"):
        # 编码器默认已开启场景切换检测；这里只把它显式化，便于配方审查时看到行为。
        args += ["-sc_threshold", "40"]
    if params.frame_rate_policy == "cfr":
        args += ["-r", f"{ctx.probe.get('fps') or 25.0:g}"]
    if params.pix_fmt:
        args += ["-pix_fmt", params.pix_fmt]

    if params.audio_codec != "none" and ctx.probe.get("has_audio"):
        args += ["-c:a", AUDIO_ENCODERS[params.audio_codec], "-b:a", f"{params.audio_bitrate_kbps}k"]

    # C8：元数据策略
    policy = ctx.plan.get("metadata_policy") or {"mode": "keep", "set": {}, "remove": []}
    if policy["mode"] == "strip":
        args += ["-map_metadata", "-1"]
    else:
        args += ["-map_metadata", "0"]
        for key in policy.get("remove") or []:
            args += ["-metadata", f"{key}="]
        for key, value in (policy.get("set") or {}).items():
            args += ["-metadata", f"{key}={value}"]

    # C10：章节标记（放在元数据策略之后，后者会覆盖前者的 -map_metadata）
    if chapters_input_index is not None:
        args += [
            "-map_metadata", str(chapters_input_index),
            "-map_chapters", str(chapters_input_index),
        ]

    if soft_subtitle:
        if subtitle_input_index is not None:
            args += ["-map", f"{subtitle_input_index}:s:0", "-c:s", subtitle_codec,
                     "-metadata:s:s:0", f"language={subtitle['language']}",
                     "-metadata:s:s:0", f"title={subtitle['title']}"]
        else:
            args += ["-map", f"0:{subtitle['stream_index']}", "-c:s", subtitle_codec]

    suffix = Path(ctx.inputs[0]).suffix or ".mp4"
    output = ctx.scratch(f"encoded{'.' + params.container if params.container != 'mp4' else suffix}")
    args += ["-movflags", "+faststart"] if params.container == "mp4" else []
    args += [str(output)]

    run_checked(args, timeout=ctx.settings.task_timeout_seconds)
    if not output.exists() or output.stat().st_size == 0:
        raise WorkError(ErrorClass.INTERNAL, "编码未产出可用文件")

    ctx.encoded = output
    ctx.plan["encode"] = {
        "video_codec": params.video_codec,
        "audio_codec": params.audio_codec,
        "container": params.container,
        "rate_control": params.rate_control,
        "crf": params.crf,
    }
    ctx.notes.append(
        f"编码：{params.video_codec}/{params.audio_codec} → {params.container}"
        f"（{params.rate_control}={params.crf if params.rate_control == 'crf' else str(params.video_bitrate_kbps) + 'kbps'}）"
    )
    return {"output": str(output)}


def render_chapters(ranges: List[dict]) -> str:
    """生成 ffmetadata 章节文件（C10）。"""
    lines = [";FFMETADATA1"]
    for item in ranges:
        lines.append("[CHAPTER]")
        lines.append("TIMEBASE=1/1000")
        lines.append(f"START={int(round(item['start'] * 1000))}")
        lines.append(f"END={int(round(item['end'] * 1000))}")
        lines.append(f"title={item['title']}")
    return "\n".join(lines) + "\n"


def split_chapters(
    source: Path,
    ranges: List[dict],
    destination: Path,
    settings=None,
) -> List[Path]:
    """按章节范围切出多个文件（C10 的「输出多个分集文件」）。

    用 `-c copy` 流拷贝，不重新编码，所以分集产物与主产物画质完全一致。
    """
    binary = getattr(settings, "ffmpeg", "ffmpeg")
    outputs = []
    for index, item in enumerate(ranges):
        target = destination / f"part_{index + 1:02d}.mp4"
        run_checked(
            [
                binary, "-hide_banner", "-nostdin", "-y",
                "-ss", f"{item['start']:g}", "-to", f"{item['end']:g}",
                "-i", str(source), "-c", "copy", "-map", "0", str(target),
            ],
            timeout=1800.0,
        )
        outputs.append(target)
    return outputs


register(
    ModuleSpec(
        name="subtitle.mux",
        title="字幕处理（C3 字幕工作流）",
        capability="subtitles",
        requires=frozenset({"video"}),
        produces=frozenset({"subtitle_plan"}),
        params_model=SubtitleParams,
        implement=implement_subtitle,
        # 烧录走 `subtitles` 滤镜；软字幕转封装要 `mov_text`（mp4）编码器。
        capability_probe=requires_filters("subtitles"),
    )
)
register(
    ModuleSpec(
        name="metadata.policy",
        title="元数据策略（C8）",
        capability="ffmpeg",
        requires=frozenset({"metadata"}),
        produces=frozenset({"metadata_policy"}),
        params_model=MetadataParams,
        implement=implement_metadata,
        capability_probe=ffmpeg_available,
    )
)
register(
    ModuleSpec(
        name="media.encode",
        title="编码与封装（C6）",
        capability="h264_encode",
        requires=frozenset({"video"}),
        produces=frozenset({"encoded"}),
        params_model=EncodeParams,
        implement=implement_encode,
        # 这一条只钉两件「无论配方怎么写都必须成立」的事：默认视频编码器在，以及拼时间线
        # 用的基础滤镜在。h265/vp9/av1 与音频编码器由配方选，只在拿到参数后才能确认，
        # 交给 `implement_encode` 里的 `ensure_encoders` 逐次核对。
        capability_probe=requires_all(
            requires_encoders("libx264"),
            requires_filters("trim", "atrim", "setpts", "asetpts", "settb", "asettb", "concat"),
        ),
    )
)
