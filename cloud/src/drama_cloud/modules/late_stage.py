"""Tier 2 后期能力接入云端（合成渲染 / 变速变调 / 音源分离 / ASR / OCR）。

这五个能力此前只存在于 Rust 侧库与 CLI（`crates/dossier-media`）：库能规划、能拼命令，
云端却一个模块都没有，于是「配方能用到它们」是一句空话。这一层把它们接进白名单注册表，
并沿用与其余模块相同的三条规矩：

1. **能力先探测再路由**：探测依据是 `dossier-media probe --json`（同一套标签），
   缺 rubberband/demucs/whisper/tesseract 的机器不会声明对应能力，任务在计划阶段就被拒绝；
2. **命令由工具的库层拼**：这里只传参数，不自己拼 ffmpeg 命令行——避免第二套实现；
3. **不静默降级**：工具跑失败、或命令成功但产物不在，都显式报错。

产物一律落在任务工作目录（`ctx.scratch`），登记成任务产物后可以下载；
是否替换主管线输入由每个模块自己声明（见各自的 `produces` 与说明）。
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Dict, List, Literal, Optional

from pydantic import Field, model_validator

from ..base import StrictModel
from ..errors import ErrorClass, WorkError, bad_param
from ..recipes.registry import ModuleSpec, register
from .ffmpeg import MediaContext, classify
from .media_cli import (
    media_capabilities,
    requires_any_media,
    requires_media,
    resolve_media_tool,
    run_media,
)

#: 只取 stderr 尾部：外部工具失败时真正的原因在最后几行。
_TAIL_LIMIT = 800


def _tail(text: str) -> str:
    cleaned = text.strip()
    if len(cleaned) <= _TAIL_LIMIT:
        return cleaned
    return "…" + cleaned[-_TAIL_LIMIT:]


def _failure(name: str, result) -> WorkError:
    stderr = result.stderr.decode("utf-8", "replace")
    # 127 是 shell 的「命令没找到」：`dossier-media` 起来了，但它要调的
    # rubberband/demucs/whisper/tesseract 不在。这属于能力缺失，重试多少遍都一样。
    if result.returncode == 127:
        return WorkError(
            ErrorClass.CAPABILITY_MISSING,
            f"{name} 找不到依赖的外部命令（退出码 127）：{_tail(stderr)}",
        )
    return WorkError(
        classify(stderr),
        f"{name} 失败（退出码 {result.returncode}）：{_tail(stderr)}",
    )


def _require_output(path: Path, name: str, result) -> None:
    """命令成功但没有产物 —— 这是最常见也最难查的一类失败，必须在这里拦住。"""
    if not path.is_file() or path.stat().st_size == 0:
        stderr = result.stderr.decode("utf-8", "replace")
        raise WorkError(
            ErrorClass.INTERNAL,
            f"{name} 命令成功但没有产出 {path.name}：{_tail(stderr)}",
        )


def _format_float(value: float) -> str:
    text = f"{value:.4f}".rstrip("0").rstrip(".")
    return text or "0"


def _count_srt_cues(text: str) -> int:
    """数 SRT 的条目数（`-->` 行数就是条数，不依赖空行分隔是否规范）。"""
    return sum(1 for line in text.splitlines() if "-->" in line)


def _extract_json(text: str, opener: str, closer: str) -> Optional[object]:
    """从混合输出里取出第一个完整 JSON 值（工具会把 JSON 与人类可读输出混在一起）。"""
    start = text.find(opener)
    if start < 0:
        return None
    depth = 0
    in_string = False
    escaped = False
    for index in range(start, len(text)):
        char = text[index]
        if in_string:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                in_string = False
            continue
        if char == '"':
            in_string = True
        elif char == opener:
            depth += 1
        elif char == closer:
            depth -= 1
            if depth == 0:
                try:
                    return json.loads(text[start : index + 1])
                except json.JSONDecodeError:
                    return None
    return None


# ---------- 图像合成渲染 ----------


class RenderParams(StrictModel):
    """`video.render`：把文字/角标/字幕合进画面（不改变帧率与时长）。"""

    input_index: int = Field(0, ge=0)
    #: 叠加文字；每条一行（可重复）。
    texts: List[str] = Field(default_factory=list)
    font_size: int = Field(28, ge=8, le=200)
    font_color: str = "white"
    #: 字体文件。中文字幕/水印必须显式给（默认字体没有汉字）；这是只读资源路径，不是命令。
    font_file: Optional[str] = None
    #: 图片叠加，`<路径>[,宽度]`；用于重制角标/台标。
    logos: List[str] = Field(default_factory=list)
    #: 外挂字幕的输入下标（srt/ass）；与 `use_asr_subtitle` 二选一。
    subtitles_input_index: Optional[int] = Field(None, ge=0)
    #: 使用同一条管线里 `subtitle.asr` 产出的字幕（`source: planned`）。
    use_asr_subtitle: bool = False
    subtitle_style: Optional[str] = None
    output_name: str = "rendered.mp4"
    timeout_seconds: float = Field(1800.0, gt=0.0)

    @model_validator(mode="after")
    def _at_least_one_element(self):
        if not self.texts and not self.logos and self.subtitles_input_index is None and not self.use_asr_subtitle:
            raise ValueError("render 至少需要 texts / logos / 字幕 之一，否则只是在重编码")
        if self.subtitles_input_index is not None and self.use_asr_subtitle:
            raise ValueError("字幕来源只能选一个：subtitles_input_index 或 use_asr_subtitle")
        return self


def _resolve_render_subtitle(ctx: MediaContext, params: RenderParams) -> Optional[Path]:
    if params.use_asr_subtitle:
        asr = ctx.plan.get("asr") or {}
        path = asr.get("subtitle")
        if not path or not Path(str(path)).is_file():
            raise WorkError(
                ErrorClass.PARAM_OUT_OF_RANGE,
                "use_asr_subtitle=true，但计划里没有 subtitle.asr 产出的字幕；"
                "请把 subtitle.asr 放在 render 之前",
            )
        return Path(str(path))
    if params.subtitles_input_index is not None:
        if params.subtitles_input_index >= len(ctx.inputs):
            raise bad_param(f"subtitles_input_index={params.subtitles_input_index} 超出输入素材范围")
        return Path(ctx.inputs[params.subtitles_input_index])
    return None


def implement_render(ctx: MediaContext, params: RenderParams) -> dict:
    if params.input_index >= len(ctx.inputs):
        raise bad_param(f"input_index={params.input_index} 超出输入素材范围")
    tool = resolve_media_tool(ctx.settings)
    source = Path(ctx.inputs[params.input_index])
    subtitle = _resolve_render_subtitle(ctx, params)
    if subtitle is not None and "media.render.subtitle_burn" not in media_capabilities(ctx.settings):
        raise WorkError(
            ErrorClass.CAPABILITY_MISSING,
            "本机 ffmpeg 没有 libass（subtitles 滤镜），无法烧录字幕；"
            "请改用软字幕封装（subtitle.mux mode=soft）或安装带 libass 的 ffmpeg",
        )
    output = ctx.scratch(params.output_name)
    args = ["render", "--input", str(source), "--output", str(output)]
    for text in params.texts:
        args += ["--text", text]
    if params.texts:
        args += ["--font-size", str(params.font_size), "--font-color", params.font_color]
        if params.font_file:
            args += ["--font-file", params.font_file]
    for logo in params.logos:
        args += ["--logo", logo]
    if subtitle is not None:
        args += ["--subtitles", str(subtitle)]
        if params.subtitle_style:
            args += ["--subtitle-style", params.subtitle_style]
    args.append("--run")
    result = run_media(tool, args, timeout=params.timeout_seconds)
    if result.returncode != 0:
        raise _failure("dossier-media render", result)
    _require_output(output, "合成渲染", result)

    ctx.inputs[params.input_index] = output
    if Path(ctx.primary) == source:
        ctx.primary = output
    plan = {
        "tool": str(tool),
        "source": str(source),
        "output": str(output),
        "texts": list(params.texts),
        "logos": list(params.logos),
        "subtitle": str(subtitle) if subtitle else None,
    }
    ctx.plan["render"] = plan
    ctx.notes.append(
        f"合成渲染完成：{len(params.texts)} 条文字、{len(params.logos)} 个图片叠加"
        + ("、字幕烧录" if subtitle else "")
    )
    return plan


register(
    ModuleSpec(
        name="video.render",
        title="图像合成渲染（文字/角标/字幕烧录）",
        capability="media.render.compose",
        requires=frozenset({"video"}),
        produces=frozenset({"video"}),
        params_model=RenderParams,
        implement=implement_render,
        capability_probe=requires_media("media.render.compose"),
    )
)


# ---------- 音源分离 ----------


class SeparateParams(StrictModel):
    """`audio.separate`：人声/伴奏分离（Demucs）。产物是旁路音轨，不替换主视频。"""

    input_index: int = Field(0, ge=0)
    mode: Literal["two", "four"] = "two"
    model: str = "htdemucs"
    device: Literal["cpu", "cuda"] = "cpu"
    jobs: int = Field(1, ge=1, le=16)
    timeout_seconds: float = Field(3600.0, gt=0.0)


def _audio_source(ctx: MediaContext, index: int) -> Path:
    if index >= len(ctx.inputs):
        raise bad_param(f"input_index={index} 超出输入素材范围")
    if not ctx.probe.get("has_audio"):
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "该模块要求输入包含音轨")
    return Path(ctx.inputs[index])


def implement_separate(ctx: MediaContext, params: SeparateParams) -> dict:
    tool = resolve_media_tool(ctx.settings)
    source = _audio_source(ctx, params.input_index)
    output_dir = ctx.work_dir / "stems"
    args = [
        "separate",
        "--input",
        str(source),
        "--output-dir",
        str(output_dir),
        "--mode",
        params.mode,
        "--model",
        params.model,
        "--device",
        params.device,
        "--jobs",
        str(params.jobs),
        "--run",
    ]
    result = run_media(tool, args, timeout=params.timeout_seconds)
    if result.returncode != 0:
        raise _failure("dossier-media separate", result)

    # 输出布局由工具保证：`<out>/<model>/<track>/<stem>.wav`；这里按磁盘事实收集，
    # 而不是猜文件名（猜错会得到「跑完了但一个产物都没有」）。
    track_dir = output_dir / params.model / source.stem
    stems = sorted(track_dir.glob("*.wav")) if track_dir.is_dir() else []
    if not stems:
        raise WorkError(
            ErrorClass.INTERNAL,
            f"分离命令成功但没有产出音轨（期望目录 {track_dir}）：{_tail(result.stderr.decode('utf-8', 'replace'))}",
        )
    for stem in stems:
        ctx.artifacts.append(
            {
                "kind": "stem",
                "path": stem,
                "name": stem.name,
                "media": {"model": params.model, "mode": params.mode, "track": source.stem},
            }
        )
    plan = {
        "tool": str(tool),
        "source": str(source),
        "model": params.model,
        "mode": params.mode,
        "stems": [str(stem) for stem in stems],
    }
    ctx.plan["stems"] = plan
    ctx.notes.append(f"音源分离完成：{len(stems)} 条音轨（{params.mode}，{params.model}）")
    return plan


register(
    ModuleSpec(
        name="audio.separate",
        title="音源分离（人声/伴奏）",
        capability="media.audio.separate",
        requires=frozenset({"audio"}),
        produces=frozenset({"stems"}),
        params_model=SeparateParams,
        implement=implement_separate,
        capability_probe=requires_media("media.audio.separate"),
    )
)


# ---------- 变速 / 变调 ----------


class TimeStretchParams(StrictModel):
    """`audio.timestretch`：变速（可选保持/调整音调）。"""

    input_index: int = Field(0, ge=0)
    speed: float = Field(gt=0.0, le=8.0)
    pitch_semitones: float = Field(0.0, ge=-24.0, le=24.0)
    #: 本机没有 rubberband 时，是否接受「变速同时变调」。
    allow_pitch_shift: bool = False
    output_name: str = "stretched.wav"
    timeout_seconds: float = Field(1800.0, gt=0.0)

    @model_validator(mode="after")
    def _something_to_do(self):
        if (self.speed - 1.0).__abs__() < 1e-9 and self.pitch_semitones == 0.0:
            raise ValueError("speed=1 且 pitch=0，这次变速不会改变任何东西")
        return self


def implement_timestretch(ctx: MediaContext, params: TimeStretchParams) -> dict:
    tool = resolve_media_tool(ctx.settings)
    source = _audio_source(ctx, params.input_index)
    capabilities = media_capabilities(ctx.settings)
    # 「保持音调」是硬要求：本机只有 atempo 时必须显式拒绝，而不是悄悄给出变调结果。
    wants_pitched = params.pitch_semitones != 0.0 or (
        abs(params.speed - 1.0) > 1e-9 and not params.allow_pitch_shift
    )
    if wants_pitched and "media.audio.timestretch" not in capabilities:
        raise WorkError(
            ErrorClass.CAPABILITY_MISSING,
            "要求保持音调，但本机没有 rubberband（只有 ffmpeg atempo 会连带变调）；"
            "请安装 rubberband，或显式设置 allow_pitch_shift=true 接受变调",
        )
    output = ctx.scratch(params.output_name)
    args = [
        "speed",
        "--input",
        str(source),
        "--output",
        str(output),
        "--speed",
        _format_float(params.speed),
        "--pitch",
        _format_float(params.pitch_semitones),
    ]
    if params.allow_pitch_shift:
        args.append("--allow-pitch-shift")
    args.append("--run")
    result = run_media(tool, args, timeout=params.timeout_seconds)
    if result.returncode != 0:
        raise _failure("dossier-media speed", result)
    _require_output(output, "变速", result)

    # 输入是音频文件时才替换主管线输入；视频素材的音轨变速属于旁路产物
    # （直接替换成 wav 会让后面的视频步骤拿到一条没有画面的输入）。
    audio_suffixes = {".wav", ".mp3", ".m4a", ".aac", ".flac", ".ogg", ".opus"}
    replaced = source.suffix.lower() in audio_suffixes
    if replaced:
        ctx.inputs[params.input_index] = output
        if Path(ctx.primary) == source:
            ctx.primary = output
    plan = {
        "tool": str(tool),
        "source": str(source),
        "output": str(output),
        "speed": params.speed,
        "pitch_semitones": params.pitch_semitones,
        "replaced_input": replaced,
    }
    ctx.plan["timestretch"] = plan
    ctx.artifacts.append(
        {"kind": "audio", "path": output, "name": output.name, "media": {"speed": params.speed}}
    )
    ctx.notes.append(
        f"变速完成：{params.speed}x，音调 {params.pitch_semitones} 半音"
        + ("" if replaced else "（主视频未替换，音轨作为旁路产物）")
    )
    return plan


register(
    ModuleSpec(
        name="audio.timestretch",
        title="变速/变调（Rubber Band 优先，atempo 降级）",
        # 标签覆盖两种后端：能变速就算具备；要求「保持音调」时在实现里再卡一次。
        capability="media.audio.timestretch",
        requires=frozenset({"audio"}),
        produces=frozenset({"audio"}),
        params_model=TimeStretchParams,
        implement=implement_timestretch,
        capability_probe=requires_any_media(
            "media.audio.timestretch", "media.audio.tempo_only"
        ),
    )
)


# ---------- 语音识别（ASR） ----------


class AsrParams(StrictModel):
    """`subtitle.asr`：语音识别出字幕（Whisper），产物可被 `subtitle.mux` 直接使用。"""

    input_index: int = Field(0, ge=0)
    flavor: Literal["openai", "cpp"] = "openai"
    model: str = "small"
    language: Optional[str] = "zh"
    threads: Optional[int] = Field(None, ge=1, le=128)
    timeout_seconds: float = Field(3600.0, gt=0.0)


def implement_asr(ctx: MediaContext, params: AsrParams) -> dict:
    tool = resolve_media_tool(ctx.settings)
    source = _audio_source(ctx, params.input_index)
    output_dir = ctx.work_dir / "subtitles"
    args = [
        "asr",
        "--input",
        str(source),
        "--output-dir",
        str(output_dir),
        "--flavor",
        params.flavor,
        "--model",
        params.model,
    ]
    if params.language:
        args += ["--language", params.language]
    if params.threads:
        args += ["--threads", str(params.threads)]
    args.append("--run")
    result = run_media(tool, args, timeout=params.timeout_seconds)
    if result.returncode != 0:
        raise _failure("dossier-media asr", result)
    subtitle = output_dir / f"{source.stem}.srt"
    _require_output(subtitle, "语音识别", result)
    cues = _count_srt_cues(subtitle.read_text(encoding="utf-8", errors="replace"))

    ctx.plan["asr"] = {
        "tool": str(tool),
        "source": str(source),
        "subtitle": str(subtitle),
        "cues": cues,
        "model": params.model,
        "flavor": params.flavor,
        "language": params.language,
    }
    ctx.artifacts.append(
        {
            "kind": "subtitle",
            "path": subtitle,
            "name": subtitle.name,
            "media": {"cues": cues, "model": params.model, "language": params.language},
        }
    )
    ctx.notes.append(f"语音识别完成：{cues} 条字幕（{params.model}，{params.flavor}）")
    return ctx.plan["asr"]


register(
    ModuleSpec(
        name="subtitle.asr",
        title="语音识别转字幕（Whisper → SRT）",
        capability="media.asr",
        requires=frozenset({"audio"}),
        produces=frozenset({"subtitle_text"}),
        params_model=AsrParams,
        implement=implement_asr,
        capability_probe=requires_media("media.asr"),
    )
)


# ---------- OCR 文本识别 ----------


class OcrRegionParams(StrictModel):
    """区域用绝对像素表达（原点左上角），因为角标/硬字幕的位置是固定的。"""

    x: int = Field(0, ge=0)
    y: int = Field(0, ge=0)
    width: int = Field(gt=0, le=8192)
    height: int = Field(gt=0, le=8192)


class OcrParams(StrictModel):
    """`subtitle.ocr`：抽区域帧 + 识别文字（Tesseract），用于核对角标/标题文本。"""

    input_index: int = Field(0, ge=0)
    region: OcrRegionParams
    interval: float = Field(1.0, gt=0.0, le=60.0)
    language: str = "chi_sim+eng"
    #: 置信度门槛：低于它的行仍会输出，但只计入 `low_confidence`。
    confidence: float = Field(60.0, ge=0.0, le=100.0)
    #: 最多识别多少帧（角标是静态的，几帧就够；防止长视频把任务拖爆）。
    max_frames: int = Field(12, ge=1, le=500)
    timeout_seconds: float = Field(1800.0, gt=0.0)


def implement_ocr(ctx: MediaContext, params: OcrParams) -> dict:
    tool = resolve_media_tool(ctx.settings)
    if params.input_index >= len(ctx.inputs):
        raise bad_param(f"input_index={params.input_index} 超出输入素材范围")
    source = Path(ctx.inputs[params.input_index])
    frames_dir = ctx.work_dir / "ocr"
    args = [
        "ocr",
        "--input",
        str(source),
        "--output-dir",
        str(frames_dir),
        "--x",
        str(params.region.x),
        "--y",
        str(params.region.y),
        "--width",
        str(params.region.width),
        "--height",
        str(params.region.height),
        "--interval",
        _format_float(params.interval),
        "--language",
        params.language,
        "--run",
    ]
    result = run_media(tool, args, timeout=params.timeout_seconds)
    if result.returncode != 0:
        raise _failure("dossier-media ocr", result)
    frames = sorted(frames_dir.glob("region_*.png"))
    if not frames:
        raise WorkError(
            ErrorClass.INTERNAL,
            f"OCR 命令成功但没有抽出区域图片（区域可能落在画面外）："
            f"{_tail(result.stderr.decode('utf-8', 'replace'))}",
        )

    lines: List[dict] = []
    for index, frame in enumerate(frames[: params.max_frames]):
        recognize_dir = ctx.work_dir / "ocr_recognized"
        recognize_args = [
            "ocr",
            "--image",
            str(frame),
            "--output-dir",
            str(recognize_dir),
            "--language",
            params.language,
            "--run",
            "--json",
        ]
        recognized = run_media(tool, recognize_args, timeout=params.timeout_seconds)
        if recognized.returncode != 0:
            raise _failure("dossier-media ocr --image", recognized)
        payload = _extract_json(
            recognized.stdout.decode("utf-8", "replace"), "[", "]"
        )
        for entry in payload if isinstance(payload, list) else []:
            if isinstance(entry, dict):
                entry = dict(entry)
                entry["frame"] = index + 1
                lines.append(entry)

    confident = [line for line in lines if float(line.get("confidence") or -1) >= params.confidence]
    report_path = ctx.scratch("ocr_report.json")
    report = {
        "source": str(source),
        "region": params.region.model_dump(mode="json"),
        "interval": params.interval,
        "language": params.language,
        "confidence": params.confidence,
        "frames": len(frames),
        "recognized_frames": min(len(frames), params.max_frames),
        "lines": lines,
        "confident": len(confident),
    }
    report_path.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    ctx.plan["ocr"] = report
    ctx.artifacts.append(
        {
            "kind": "ocr_report",
            "path": report_path,
            "name": "ocr_report.json",
            "media": {"lines": len(lines), "confident": len(confident)},
        }
    )
    ctx.notes.append(
        f"OCR 完成：{len(frames)} 帧抽图、识别 {len(lines)} 行（置信度 ≥ {params.confidence:g}：{len(confident)} 行）"
    )
    return report


register(
    ModuleSpec(
        name="subtitle.ocr",
        title="区域 OCR 文本识别（Tesseract）",
        capability="media.ocr",
        requires=frozenset({"video"}),
        produces=frozenset({"ocr_report"}),
        params_model=OcrParams,
        implement=implement_ocr,
        capability_probe=requires_media("media.ocr"),
    )
)


# ---------- 镜头切点检测 ----------


class ShotsParams(StrictModel):
    """`timeline.shots`：检测镜头切点，产出**候选**切点清单（不自动改时间线）。

    云端的 `timeline.user_edits` 有自己的一版「场景检测」（ffmpeg `select` 打分），
    但那一版的结果只活在一次任务的临时目录里、无法审阅，也没有能力标签参与路由。
    这里把 Rust 侧 `dossier-media shots` 接进来：同一套阈值语义、产物登记成任务产物、
    缺 `select` 滤镜的机器在计划阶段就被挡住。它是候选生成，不是语义判断——
    「要不要按这个切点成章」仍然由编排层（或人）决定。
    """

    input_index: int = Field(0, ge=0)
    #: 场景分数阈值；越高越保守。
    threshold: float = Field(0.35, gt=0.0, lt=1.0)
    #: 两个切点之间的最小间隔（秒），用于压掉同一秒内的重复检测。
    min_gap: float = Field(0.2, ge=0.0, le=10.0)
    output_name: str = "shots.json"
    timeout_seconds: float = Field(3600.0, gt=0.0)


def implement_shots(ctx: MediaContext, params: ShotsParams) -> dict:
    tool = resolve_media_tool(ctx.settings)
    if params.input_index >= len(ctx.inputs):
        raise bad_param(f"input_index={params.input_index} 超出输入素材范围")
    source = Path(ctx.inputs[params.input_index])
    duration = ctx.probe.get("duration")
    args = [
        "shots",
        "--input",
        str(source),
        "--threshold",
        _format_float(params.threshold),
        "--min-gap",
        _format_float(params.min_gap),
        "--json",
        "--run",
    ]
    if duration:
        args += ["--duration", _format_float(float(duration))]
    result = run_media(tool, args, timeout=params.timeout_seconds)
    if result.returncode != 0:
        raise _failure("dossier-media shots", result)
    payload = _extract_json(result.stdout.decode("utf-8", "replace"), "{", "}")
    if not isinstance(payload, dict):
        raise WorkError(
            ErrorClass.INTERNAL,
            "镜头检测命令成功，但输出里没有 JSON 报告："
            + _tail(result.stdout.decode("utf-8", "replace")),
        )
    cuts = []
    for entry in payload.get("boundaries") or []:
        if not isinstance(entry, dict):
            continue
        cuts.append(
            {
                "time": float(entry.get("timeSec") or 0.0),
                "score": float(entry.get("score") or 0.0),
            }
        )
    report = {
        "source": str(source),
        "threshold": params.threshold,
        "min_gap": params.min_gap,
        "duration": duration,
        "cuts": cuts,
        "cut_count": len(cuts),
    }
    path = ctx.scratch(params.output_name)
    path.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    ctx.plan["shots"] = {**report, "tool": str(tool), "report": str(path)}
    ctx.artifacts.append(
        {
            "kind": "shot_list",
            "path": path,
            "name": path.name,
            "media": {"cuts": len(cuts), "threshold": params.threshold},
        }
    )
    ctx.notes.append(
        f"镜头检测完成：{len(cuts)} 个候选切点（阈值 {params.threshold:g}，最小间隔 {params.min_gap:g}s）"
    )
    return ctx.plan["shots"]


register(
    ModuleSpec(
        name="timeline.shots",
        title="镜头切点检测（候选切点清单）",
        capability="media.shot.detect",
        requires=frozenset({"video"}),
        produces=frozenset({"shots"}),
        kind="plan",
        params_model=ShotsParams,
        implement=implement_shots,
        capability_probe=requires_media("media.shot.detect"),
    )
)
