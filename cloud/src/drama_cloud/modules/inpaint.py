"""C12 硬字幕 / 角标擦除 与 擦除确认流（技术学习用途）。

覆盖处理管线的三步：

1. **标识检测 + 时序 Mask 生成**：由 `dossier-inpaint`（Rust）内部完成（ROI 固定区域或
   DBNet worker 检测 + 逐帧掩码 + 时序平滑）；
2. **视频 Inpaint 擦除**：`video.inpaint` 调用该工具，产出擦除后的视频并把结构化处理日志
   （残留 / 闪烁 / 接缝 / 一致性指标）写进 `ctx.plan["inpaint"]`；
3. **结果自动校验（确认流闸门）**：`video.erase_verify` 读同一份指标，按配方自己的阈值
   重新裁决。

分工刻意如此：**工具负责测量，配方负责裁决**。调阈值不需要重跑擦除，也不会把
「测出来是什么」和「能不能交付」两个决定混在一起。

失败走向（§7.2 重试规则 + 确认流）：

- 结论 `fail` → `quality_retryable`：调度器按次数自动重试（分支A），重试时本模块会
  逐级加大 mask 膨胀/羽化、必要时切换更强修复后端；次数用尽转人工复核队列（分支B）。
- 结论 `warn` → 产物照常产出，但登记一条人工抽查记录（不自动放行）。
- 结论 `pass` → 自动确认通过。

合规边界：只用于处理自有素材（含自有素材的角标/台标清理），不做 DRM 绕过，
也不处理他人作品的版权标识。
"""

from __future__ import annotations

import json
import os
import re
import shutil
from pathlib import Path
from typing import Dict, FrozenSet, List, Literal, Optional, Tuple

from pydantic import Field, model_validator

from ..base import StrictModel
from ..config import Settings
from ..errors import ErrorClass, WorkError, bad_param, quality_retry, unsupported
from ..recipes.registry import ModuleSpec, register, register_capability
from ..toolchain import resolve_binary
from .ffmpeg import MediaContext, classify, read_progress_events, run, run_streaming

#: 工具输出的最大验收长度（错误信息只需要尾部）。
_STDERR_TAIL = 800

#: 结论严重度排序：数字越大越严重。
_RANK = {"pass": 0, "warn": 1, "fail": 2}

#: 自动重试的力度升级幅度。
_DILATE_STEP = 2
_FEATHER_STEP = 1
_CONTEXT_STEP = 0.1
_MAX_DILATE = 32
_MAX_FEATHER = 12
_MAX_CONTEXT = 4.0


class Region(StrictModel):
    """固定标识区域（像素，原点左上角）。多个区域可以同时擦除字幕条与角标。"""

    x: int = Field(ge=0)
    y: int = Field(ge=0)
    width: int = Field(gt=0, le=8192)
    height: int = Field(gt=0, le=8192)

    def as_cli(self) -> str:
        return f"{self.x},{self.y},{self.width},{self.height}"


class InpaintParams(StrictModel):
    """`video.inpaint` 参数。

    只允许调「怎么做擦除」的数值与后端选择；可执行文件、解释器、worker 参数一律由
    部署配置（`Settings.inpaint_*`）给出，配方改不动它们（§10 不执行配方内任意命令）。
    """

    #: 要处理的输入下标（`ctx.inputs` 下标）；擦除后该输入会被替换成擦除结果。
    input_index: int = Field(0, ge=0)
    #: 固定区域；`detector=fixed` 时必填，`detector=worker` 时作为「只在区域内采纳检测框」的约束。
    regions: List[Region] = Field(default_factory=list)
    #: 只处理该时间点之后的画面（秒）；之前的帧原样输出。
    start: Optional[float] = Field(None, ge=0.0)
    duration: Optional[float] = Field(None, gt=0.0)
    start_frame: Optional[int] = Field(None, ge=0)
    end_frame: Optional[int] = Field(None, ge=1)
    #: 检测框落在区域内的判定外扩量（像素）。
    region_pad: int = Field(8, ge=0, le=256)
    #: 掩码时序平滑半径（帧）：相邻帧掩码取并集，抑制漏检闪烁。
    temporal_smooth: int = Field(1, ge=0, le=30)
    #: 检测后端：fixed（需 regions）或 worker（模型检测）。
    detector: Literal["fixed", "worker"] = "fixed"
    #: 修复后端：column（本地基线，无模型）或 worker（时序视频修复模型）。
    inpainter: Literal["column", "worker"] = "column"
    #: 掩码膨胀半径（像素）。太小会残留 logo 轮廓，太大伤正常画面。
    dilate: int = Field(4, ge=0, le=64)
    #: 回贴羽化半径（像素）。
    feather: int = Field(2, ge=0, le=64)
    #: ROI 相对检测框的外扩比例。
    context: float = Field(1.6, ge=1.0, le=8.0)
    #: 检测置信度下限。
    min_score: float = Field(0.5, ge=0.0, le=1.0)
    clip_frames: int = Field(30, ge=1, le=600)
    probe_frames: int = Field(60, ge=1, le=2000)
    #: 覆盖素材帧率（素材帧率不可信时使用）。
    fps: Optional[float] = Field(None, gt=0.0)
    vcodec: str = "libx264"
    crf: int = Field(16, ge=0, le=51)
    preset: str = "medium"
    #: 输出像素格式。角标这类彩色小目标对色度二次抽样敏感，可用 yuv444p。
    pix_fmt: str = "yuv444p"
    #: 掩码内结构相似度（SSIM）均值警告阈值（0–1，越大越像）。工具自检用。
    ssim_warn: float = Field(0.85, ge=0.0, le=1.0)
    #: 掩码内 SSIM 最低失败阈值（0–1）。结构被抹平（涂抹/糊块）会先在这里暴露。
    ssim_fail: float = Field(0.55, ge=0.0, le=1.0)
    #: 是否把工具内部的结论交给 `video.erase_verify` 裁决（true 时工具结论只记录不拦截）。
    defer_verdict: bool = False
    #: 自动重试的加力方式：off / mask（加大膨胀羽化）/ mask+backend（再加换后端）。
    escalation: Literal["off", "mask", "mask+backend"] = "mask"
    timeout_seconds: float = Field(1800.0, gt=0.0)

    @model_validator(mode="after")
    def _check_backends(self):
        if self.detector == "fixed" and not self.regions:
            raise ValueError("detector=fixed 时至少要给出一个 regions 区域")
        if self.start_frame is not None and self.end_frame is not None:
            if self.end_frame <= self.start_frame:
                raise ValueError("end_frame 必须大于 start_frame")
        if self.ssim_fail > self.ssim_warn:
            raise ValueError("ssim_fail 不能大于 ssim_warn")
        return self


class EraseVerifyParams(StrictModel):
    """`video.erase_verify`：确认流闸门（只裁决，不重新测量）。

    阈值语义与工具内一致，只是判据换成本配方声明的值；因此同一个实现既能做
    「严格交付」也能做「宽松预览」，而不用重跑一次擦除。
    """

    #: 必须存在上一步的擦除报告；缺失视为配方写错（而不是静默放行）。
    require_report: bool = True
    #: 没有内容级检测结果（例如 `detector=fixed`）时，是否仍要求人工确认。
    require_content_check: bool = True
    #: 结论为 warn 时放行但转人工抽查；设为 false 表示「警告也不放行」（转人工复核）。
    accept_warn: bool = True
    residual_ratio_fail: float = Field(0.10, ge=0.0, le=1.0)
    residual_ratio_warn: float = Field(0.02, ge=0.0, le=1.0)
    #: 闪烁指数（掩码内帧间差 ÷ 掩码外参考帧间差）。
    flicker_fail: float = Field(3.0, ge=0.0)
    flicker_warn: float = Field(1.2, ge=0.0)
    #: 接缝分数（掩码边界梯度 ÷ 参考带梯度）。
    seam_fail: float = Field(2.0, ge=0.0)
    seam_warn: float = Field(1.0, ge=0.0)
    #: 掩码内一致性 PSNR（dB）；无干净参考时的真值代理。
    psnr_fail_db: float = Field(24.0, ge=0.0)
    psnr_warn_db: float = Field(30.0, ge=0.0)
    #: 掩码内结构相似度（SSIM）最低失败阈值（0–1）；PSNR 看不见的「抹平/糊块」由它兜底。
    ssim_fail: float = Field(0.55, ge=0.0, le=1.0)
    #: 掩码内 SSIM 均值警告阈值（0–1）。
    ssim_warn: float = Field(0.85, ge=0.0, le=1.0)

    @model_validator(mode="after")
    def _check_ordering(self):
        if self.ssim_fail > self.ssim_warn:
            raise ValueError("ssim_fail 不能大于 ssim_warn")
        return self


# ---------- 工具定位 ----------


def repo_root() -> Path:
    """仓库根目录。

    委托给 `drama_cloud.toolchain`：三个 Rust 工具的查找顺序必须一致，
    但**这个模块级的名字要留着**——单测通过替换 `module.repo_root` 来隔离真实仓库。
    """
    from ..toolchain import repo_root as _repo_root

    return _repo_root()


def resolve_tool(settings: Optional[Settings]) -> Path:
    """定位 `dossier-inpaint` 可执行文件。

    查找顺序：部署配置 → PATH → 仓库内的 cargo 构建产物。找不到时明确按
    `capability_missing` 失败，而不是静默跳过擦除（那会交付一份「没擦干净」的成品）。
    """
    configured = getattr(settings, "inpaint_bin", None) if settings is not None else None
    candidates: List[Path] = [Path(str(item)) for item in (configured,) if item]
    found = shutil.which("dossier-inpaint")
    if found:
        candidates.append(Path(found))
    root = repo_root()
    candidates.append(root / "target" / "release" / "dossier-inpaint")
    candidates.append(root / "target" / "debug" / "dossier-inpaint")
    for candidate in candidates:
        if candidate.is_file() and os.access(str(candidate), os.X_OK):
            return candidate
    raise WorkError(
        ErrorClass.CAPABILITY_MISSING,
        "找不到 dossier-inpaint 可执行文件；请设置 DRAMA_CLOUD_INPAINT_BIN，"
        "或在仓库根目录执行 cargo build --release -p dossier-inpaint",
    )


#: 模型修复后端所需的能力标签。以前它只能由部署方写 `DRAMA_CLOUD_CAPABILITIES`
#: 手工声明（声明错了没人发现）；现在改为注册探测函数，见 `model_capability_available`。
MODEL_CAPABILITY = "video_inpaint_model"

#: `dossier-runtime check` 的超时（清单校验要读文件摘要，给足时间但不无限等）。
_RUNTIME_CHECK_TIMEOUT = 120.0


def runtime_bin(settings: Optional[Settings]) -> Optional[Path]:
    """定位 `dossier-runtime`（用于核对运行时组件清单）。"""
    configured = getattr(settings, "runtime_bin", None) if settings is not None else None
    return resolve_binary(configured, "dossier-runtime")


def runtime_manifest_check(settings: Optional[Settings]) -> Optional[bool]:
    """按清单核对模型组件是否装齐。

    返回 `None` 表示「没法核对」（没配清单），与 `False`（核对了、但不齐）是两种结论：
    前者只能给出弱保证，后者必须让能力缺席。
    """
    manifest = getattr(settings, "runtime_manifest", None) if settings is not None else None
    if not manifest:
        return None
    tool = runtime_bin(settings)
    if tool is None:
        return False
    try:
        result = run(
            [str(tool), "check", "--manifest", str(manifest)],
            timeout=_RUNTIME_CHECK_TIMEOUT,
        )
    except WorkError:
        return False
    return result.returncode == 0


def model_capability_available(settings: Optional[Settings]) -> bool:
    """`video_inpaint_model` 是否成立。

    两级判定：
    1. 必须配置了模型 worker（`DRAMA_CLOUD_INPAINT_WORKER`）——没配就不用谈模型；
    2. 配了清单（`DRAMA_CLOUD_RUNTIME_MANIFEST`）时，还要 `dossier-runtime check`
       通过（组件齐全、摘要对得上）。没配清单时只能给出弱保证：「配了 worker」。
    这比「靠环境变量手工声明」强的地方在于：第 2 级是**真的去核对文件**，
    文件被删、权重换了、装错版本都会被探测到。
    """
    if settings is None:
        return False
    program = getattr(settings, "inpaint_worker_program", None)
    if not program:
        return False
    return runtime_manifest_check(settings) is not False


#: 参数相关的额外能力要求：选了模型后端（检测或修复）就必须有 `video_inpaint_model`。
#: 这是 Tier 1「模型端到端可用性」的关键一步——不做这层检查，配方完全可以在
#: 一台没有模型的机器上通过计划校验，然后跑到一半才发现 worker 起不来。
def required_inpaint_capabilities(params: dict) -> FrozenSet[str]:
    caps = set()
    if params.get("inpainter") == "worker" or params.get("detector") == "worker":
        caps.add(MODEL_CAPABILITY)
    return frozenset(caps)


register_capability(MODEL_CAPABILITY, model_capability_available)

#: `--help` 探测结果的缓存键是「路径 + 大小 + mtime」，同一份可执行文件只问一次。
_TOOL_FLAGS: Dict[Tuple[str, int, int], FrozenSet[str]] = {}

#: 只取长开关；短开关在帮助文本里噪声太大。
_FLAG_RE = re.compile(r"--[a-z0-9][a-z0-9-]*")


def tool_flags(tool: Path) -> FrozenSet[str]:
    """探测可执行文件认识哪些长开关（读 `--help`，不跑任何实际处理）。

    云端发布与工具版本可能不同步：一个只带了旧指标的工具遇到新开关会直接
    解析失败。所以**按能力拼参数**——探测不到就退回旧参数，让流程跑完并在
    处理日志里说明，而不是因为一个可选开关让整条擦除流程失败。

    探测不到或读不出帮助文本时按「空集合」处理，即只使用最保守的参数。
    """
    try:
        stamp = tool.stat()
    except OSError:
        return frozenset()
    key = (str(tool), stamp.st_size, stamp.st_mtime_ns)
    cached = _TOOL_FLAGS.get(key)
    if cached is not None:
        return cached
    try:
        result = run([str(tool), "--help"], timeout=30)
    except WorkError:
        flags: FrozenSet[str] = frozenset()
    else:
        text = (result.stdout + result.stderr).decode("utf-8", "replace")
        flags = frozenset(_FLAG_RE.findall(text))
    _TOOL_FLAGS[key] = flags
    return flags


def capability_available(settings: Optional[Settings]) -> bool:
    """`video_inpaint` 能力是否成立：工具可执行文件真的存在。

    这是纯探测（不执行工具），因此可以安全地在「列出本机能力」时反复调用。
    """
    try:
        resolve_tool(settings)
    except WorkError:
        return False
    return True


def worker_settings(settings: Optional[Settings]) -> Optional[Dict[str, object]]:
    program = getattr(settings, "inpaint_worker_program", None) if settings is not None else None
    if not program:
        return None
    args = getattr(settings, "inpaint_worker_args", ()) or ()
    return {"program": str(program), "args": [str(a) for a in args]}


# ---------- 分支A：按尝试次数加力 ----------


class Escalation(StrictModel):
    """一次重试实际采用的加力结果。"""

    dilate: int
    feather: int
    context: float
    temporal_smooth: int
    inpainter: str
    attempt: int
    notes: List[str] = Field(default_factory=list)


def plan_escalation(
    params: InpaintParams,
    attempt: int,
    worker: Optional[Dict[str, object]],
) -> Escalation:
    """把「第几次尝试」换算成实际的擦除力度。

    这是分支A的核心：自动重试必须换参数，否则重跑只会得到同一个结论。
    - 第 2 次：掩码向外多扩几像素、羽化加大、ROI 外扩比例略微放大（对付 logo 边缘残留）；
    - 第 3 次及以后：在 `mask+backend` 模式下再切换更强的修复后端（如果部署方配置了）。
    """
    level = max(0, int(attempt) - 1)
    escalation = Escalation(
        dilate=params.dilate,
        feather=params.feather,
        context=params.context,
        temporal_smooth=params.temporal_smooth,
        inpainter=params.inpainter,
        attempt=max(1, int(attempt)),
    )
    if params.escalation == "off" or level == 0:
        return escalation

    escalation.dilate = min(params.dilate + _DILATE_STEP * level, _MAX_DILATE)
    escalation.feather = min(params.feather + _FEATHER_STEP * level, _MAX_FEATHER)
    escalation.context = min(params.context * (1.0 + _CONTEXT_STEP * level), _MAX_CONTEXT)
    # 掩码变大后，时序平滑也跟着放宽一档，避免漏检帧留下闪动的空洞。
    escalation.temporal_smooth = min(params.temporal_smooth + level, 5)
    escalation.notes.append(
        f"第 {escalation.attempt} 次尝试自动加力：dilate {params.dilate}→{escalation.dilate}，"
        f"feather {params.feather}→{escalation.feather}，"
        f"context {params.context:.2f}→{escalation.context:.2f}"
    )
    if params.escalation == "mask+backend" and escalation.attempt >= 3:
        if escalation.inpainter != "worker" and worker:
            escalation.notes.append("第 3 次尝试切换更强的时序修复后端：column → worker")
            escalation.inpainter = "worker"
        elif worker:
            escalation.notes.append("已在使用 worker 修复后端，维持不变")
        else:
            escalation.notes.append(
                "本应切换更强的修复后端，但部署方未配置 DRAMA_CLOUD_INPAINT_WORKER，"
                "本次仅加力掩码"
            )
    return escalation


# ---------- 命令行拼装 ----------


def build_args(
    ctx: MediaContext,
    params: InpaintParams,
    escalation: Escalation,
    *,
    source: Path,
    output: Path,
    report_path: Path,
    tool: Path,
    worker: Optional[Dict[str, object]],
    progress_path: Optional[Path] = None,
) -> List[str]:
    args: List[str] = [
        str(tool),
        "--input", str(source),
        "--output", str(output),
        "--report-json", str(report_path),
        "--detector", params.detector,
        "--inpainter", escalation.inpainter,
        "--dilate", str(escalation.dilate),
        "--feather", str(escalation.feather),
        "--context", f"{escalation.context:.3f}",
        "--region-pad", str(params.region_pad),
        "--temporal-smooth", str(escalation.temporal_smooth),
        "--clip-frames", str(params.clip_frames),
        "--probe-frames", str(params.probe_frames),
        "--min-score", f"{params.min_score:.3f}",
        "--vcodec", params.vcodec,
        "--crf", str(params.crf),
        "--preset", params.preset,
        "--pix-fmt", params.pix_fmt,
        # 用与控制面同一个 ffmpeg/ffprobe，避免同一台机器上出现两套行为。
        "--ffmpeg", ctx.ffmpeg,
        "--ffprobe", ctx.ffprobe,
    ]
    if progress_path is not None:
        args += ["--progress-json", str(progress_path)]
    if "--verify-ssim-warn" in tool_flags(tool):
        # SSIM 真值代理：配方声明的阈值同时给工具自检与 video.erase_verify 裁决用。
        args += [
            "--verify-ssim-warn", f"{params.ssim_warn:.4f}",
            "--verify-ssim-fail", f"{params.ssim_fail:.4f}",
        ]
    for region in params.regions:
        args += ["--roi", region.as_cli()]
    if params.start is not None:
        args += ["--start", f"{params.start:.3f}"]
    if params.duration is not None:
        args += ["--duration", f"{params.duration:.3f}"]
    if params.start_frame is not None:
        args += ["--start-frame", str(params.start_frame)]
    if params.end_frame is not None:
        args += ["--end-frame", str(params.end_frame)]
    if params.fps is not None:
        args += ["--fps", f"{params.fps:.6f}"]
    if params.detector == "worker" or escalation.inpainter == "worker":
        if not worker:
            raise WorkError(
                ErrorClass.CAPABILITY_MISSING,
                "本步骤要求模型 worker，但部署方未配置 DRAMA_CLOUD_INPAINT_WORKER",
            )
        args += ["--worker-program", str(worker["program"])]
        for item in worker["args"]:
            args += ["--worker-args", str(item)]
    return args


# ---------- 报告读取 ----------


def read_report(path: Path) -> dict:
    if not path.exists():
        raise WorkError(
            ErrorClass.INTERNAL,
            f"擦除工具未写出处理日志：{path}（无法自动确认结果，已按失败处理）",
        )
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise WorkError(ErrorClass.INTERNAL, f"处理日志不是合法 JSON：{error}") from error
    if not isinstance(payload, dict):
        raise WorkError(ErrorClass.INTERNAL, "处理日志顶层必须是对象")
    return payload


def report_verdict(report: dict) -> str:
    verification = report.get("verification") or {}
    verdict = str(verification.get("verdict") or "pass")
    return verdict if verdict in _RANK else "pass"


def report_summary(report: dict) -> dict:
    """把工具报告压成便于写进任务进度的摘要。"""
    verification = report.get("verification") or {}
    residual = verification.get("residual") or {}
    flicker = verification.get("flicker") or {}
    seam = verification.get("seam") or {}
    consistency = verification.get("consistency") or {}
    return {
        "applicable": bool(verification.get("applicable")),
        "verdict": report_verdict(report),
        "frames_checked": verification.get("framesChecked"),
        "masked_pixels": verification.get("maskedPixels"),
        "residual_max_ratio": residual.get("maxRatio"),
        "residual_checked": bool(residual.get("checked")),
        "flicker_max_index": flicker.get("maxIndex"),
        "seam_max_score": seam.get("maxScore"),
        "psnr_mean_db": consistency.get("meanPsnrDb"),
        "ssim_mean": (verification.get("ssim") or {}).get("meanSsim"),
        "ssim_min": (verification.get("ssim") or {}).get("minSsim"),
        "issues": verification.get("issues") or [],
        "notes": verification.get("notes") or [],
    }


def _tail(text: str) -> str:
    text = (text or "").strip()
    return text if len(text) <= _STDERR_TAIL else "…" + text[-_STDERR_TAIL:]


# ---------- 模块实现 ----------


def implement_inpaint(ctx: MediaContext, params: InpaintParams) -> dict:
    if params.input_index >= len(ctx.inputs):
        raise bad_param(f"input_index={params.input_index} 超出输入素材范围")

    tool = resolve_tool(ctx.settings)
    worker = worker_settings(ctx.settings)
    escalation = plan_escalation(params, ctx.attempt, worker)
    flags = tool_flags(tool)
    if "--verify-ssim-warn" not in flags:
        escalation.notes.append(
            "已安装的 dossier-inpaint 不认识 --verify-ssim-warn，本次校验沿用旧指标"
            "（残留 / 闪烁 / 接缝 / 一致性）；如需 SSIM 请升级该工具"
        )
    # 进度流是可选能力：旧工具不认这个开关，硬加会让整条擦除失败。
    follow_progress = "--progress-json" in flags
    if not follow_progress:
        escalation.notes.append(
            "已安装的 dossier-inpaint 不认识 --progress-json，本次不产出逐帧进度"
            "（任务进度仍按阶段上报）；如需实时进度请升级该工具"
        )

    source = Path(ctx.inputs[params.input_index])
    suffix = source.suffix or ".mp4"
    output = ctx.scratch(f"inpainted{suffix}")
    report_path = ctx.scratch("inpaint-report.json")
    progress_path = ctx.scratch("inpaint.progress.jsonl")
    if output.exists():
        # 上一次尝试的产物不能被误当成本次结果（尤其是加力后仍有残留的情况）。
        output.unlink()
    if progress_path.exists():
        # 上一次尝试的进度同理：留着会让跟随者读到旧任务的事件。
        progress_path.unlink()

    args = build_args(
        ctx,
        params,
        escalation,
        source=source,
        output=output,
        report_path=report_path,
        tool=tool,
        worker=worker,
        progress_path=progress_path if follow_progress else None,
    )
    result = run_streaming(
        args,
        progress_path=progress_path if follow_progress else None,
        on_progress=ctx.progress_hook if follow_progress else None,
        timeout=params.timeout_seconds,
    )
    stderr = result.stderr.decode("utf-8", "replace")
    if result.returncode not in (0, 2, 3):
        raise WorkError(
            classify(stderr),
            f"dossier-inpaint 退出码 {result.returncode}：{_tail(stderr)}",
        )
    if not output.exists():
        raise WorkError(
            ErrorClass.INTERNAL,
            f"擦除工具没有产出视频（退出码 {result.returncode}）：{_tail(stderr)}",
        )

    report = read_report(report_path)
    verdict = report_verdict(report)
    summary = report_summary(report)
    last_progress: Optional[dict] = None
    if follow_progress:
        events, _ = read_progress_events(progress_path, 0)
        if events:
            last_progress = events[-1]

    ctx.plan["inpaint"] = {
        "tool": str(tool),
        "output": str(output),
        "input_index": params.input_index,
        "source": str(source),
        "attempt": escalation.attempt,
        "regions": [region.model_dump(mode="json") for region in params.regions],
        "detected_regions": [r for r in (report.get("regions") or []) if isinstance(r, dict)],
        "escalation": escalation.model_dump(mode="json"),
        "report": report,
        "summary": summary,
        "verdict": verdict,
        # 逐帧进度流的**末条**事件（含阶段与帧计数）。过程事件已实时上报给任务进度，
        # 这里留一份终态是为了让处理日志能自答「这次擦除到底跑了多少帧」。
        "progress": last_progress,
    }

    # 用擦除结果替换该路输入：后续步骤（画面处理 / 编码 / 门禁）看到的就是干净画面。
    ctx.inputs[params.input_index] = output
    if Path(ctx.primary) == source:
        ctx.primary = output

    for note in escalation.notes:
        ctx.notes.append(note)
    ctx.notes.append(
        f"擦除完成（{escalation.inpainter} 修复）：结论 {verdict}，"
        f"残留峰值 {(summary.get('residual_max_ratio') or 0.0) * 100:.2f}%，"
        f"闪烁峰值 {summary.get('flicker_max_index') or 0.0:.2f}，"
        f"接缝峰值 {summary.get('seam_max_score') or 0.0:.2f}"
    )
    ctx.artifacts.append(
        {
            "kind": "inpaint_report",
            "path": report_path,
            "name": "inpaint_report.json",
            "media": summary,
        }
    )
    # 产物戳记（Tier 3）：工具默认写在 `<输出>.stamp.json`，里面是管线版本、工具版本、
    # 参数与输入摘要。它是「这批产物是不是同一套流程出的」的唯一依据，所以必须
    # 跟着产物一起存下来；旧工具不写戳记时明确记一条说明，而不是静默当它不存在。
    stamp_path = Path(f"{output}.stamp.json")
    if stamp_path.is_file():
        # 管线名与版本**从戳记里读**，不在这里再写一份常量：版本号只有一个来源
        # （Rust 侧 `dossier_core::pipeline_version`），否则两边会各自漂移。
        stamp_media: dict = {"output": output.name}
        try:
            payload = json.loads(stamp_path.read_text(encoding="utf-8"))
            stamp_media["pipeline"] = payload.get("pipeline")
            stamp_media["pipeline_version"] = payload.get("pipelineVersion")
            stamp_media["tool_version"] = (payload.get("tool") or {}).get("version")
        except (OSError, ValueError):
            stamp_media["unreadable"] = True
        ctx.artifacts.append(
            {
                "kind": "artifact_stamp",
                "path": stamp_path,
                "name": stamp_path.name,
                "media": stamp_media,
            }
        )
        ctx.plan["inpaint"]["stamp"] = str(stamp_path)
    else:
        ctx.notes.append(
            "本次擦除没有产出产物戳记（已安装的 dossier-inpaint 较旧，或不支持写法）；"
            "升级该工具后可用 `drama-cloud artifacts check` 做跨产物一致性校验"
        )

    def require_review(severity: str, reason: str) -> None:
        review = ctx.plan.setdefault("review", {"required": False, "severity": "info", "reasons": []})
        review["required"] = True
        review["reasons"] = list(review.get("reasons") or []) + [reason]
        if _RANK.get(severity, 0) >= _RANK.get(str(review.get("severity") or "info"), -1):
            review["severity"] = severity
        review["reason"] = review["reasons"][0]

    if verdict == "fail":
        detail = "；".join(
            str(issue.get("message") or issue.get("code"))
            for issue in summary.get("issues") or []
        ) or "擦除结果未通过自动校验"
        if params.defer_verdict:
            ctx.notes.append("工具内部结论为 fail，按配方声明交由 video.erase_verify 裁决")
            require_review("high", f"擦除自检 fail：{detail}")
        else:
            raise quality_retry(
                f"擦除结果未通过自动校验（第 {escalation.attempt} 次尝试）→ {detail}"
            )
    elif verdict == "warn":
        detail = "；".join(
            str(issue.get("message") or issue.get("code"))
            for issue in summary.get("issues") or []
        ) or "擦除结果仅为 warn"
        ctx.notes.append(f"擦除自检为 warn，需人工抽查 → {detail}")
        require_review("warn", f"擦除自检 warn：{detail}")

    return {
        "verdict": verdict,
        "attempt": escalation.attempt,
        "escalation": escalation.model_dump(mode="json"),
        "summary": summary,
    }


def judge(verification: dict, params: EraseVerifyParams) -> tuple:
    """按配方阈值重算结论；返回 (verdict, checks, notes)。"""
    checks: List[dict] = []
    notes: List[str] = []
    verdict = "pass"

    def record(name: str, value: float, warn: float, fail: float, detail: str) -> None:
        nonlocal verdict
        if value > fail:
            level = "fail"
        elif value > warn:
            level = "warn"
        else:
            level = "pass"
        checks.append({"name": name, "ok": level != "fail", "level": level, "value": value,
                       "detail": detail})
        if _RANK[level] > _RANK[verdict]:
            verdict = level

    residual = verification.get("residual") or {}
    if residual.get("checked"):
        value = float(residual.get("maxRatio") or 0.0)
        record(
            "erase_residual",
            value,
            params.residual_ratio_warn,
            params.residual_ratio_fail,
            f"掩码内残留文本像素占比峰值 {value * 100:.2f}%"
            f"（警告 {params.residual_ratio_warn * 100:.2f}%，失败 {params.residual_ratio_fail * 100:.2f}%）",
        )
    else:
        notes.append("残留检查未执行（fixed 检测器无法判断画面内容）")
        if params.require_content_check:
            checks.append(
                {
                    "name": "erase_residual",
                    "ok": True,
                    "level": "warn",
                    "value": None,
                    "detail": "没有内容级检测结果，无法自动确认残留已清除；按要求转人工抽查",
                }
            )
            verdict = "warn" if _RANK[verdict] < _RANK["warn"] else verdict

    flicker = verification.get("flicker") or {}
    if float(flicker.get("samples") or 0) > 0:
        value = float(flicker.get("maxIndex") or 0.0)
        record(
            "erase_flicker",
            value,
            params.flicker_warn,
            params.flicker_fail,
            f"掩码内闪烁指数峰值 {value:.2f}（警告 {params.flicker_warn}，失败 {params.flicker_fail}）",
        )
    seam = verification.get("seam") or {}
    if float(seam.get("samples") or 0) > 0:
        value = float(seam.get("maxScore") or 0.0)
        record(
            "erase_seam",
            value,
            params.seam_warn,
            params.seam_fail,
            f"掩码边界接缝分数峰值 {value:.2f}（警告 {params.seam_warn}，失败 {params.seam_fail}）",
        )
    consistency = verification.get("consistency") or {}
    if float(consistency.get("samples") or 0) > 0:
        value = float(consistency.get("minPsnrDb") or 0.0)
        # PSNR 越大越好，所以判据方向与上面三个指标相反。
        if value < params.psnr_fail_db:
            level = "fail"
        elif value < params.psnr_warn_db:
            level = "warn"
        else:
            level = "pass"
        checks.append(
            {
                "name": "erase_consistency",
                "ok": level != "fail",
                "level": level,
                "value": value,
                "detail": f"掩码内一致性 PSNR 最低 {value:.1f} dB"
                f"（警告低于 {params.psnr_warn_db}，失败低于 {params.psnr_fail_db}）",
            }
        )
        if _RANK[level] > _RANK[verdict]:
            verdict = level
    ssim = verification.get("ssim") or {}
    if float(ssim.get("samples") or 0) > 0:
        # 用最差帧的 SSIM（结构保真度越低越糟），判据方向与残留/闪烁/接缝相反。
        value = float(ssim.get("minSsim") or 1.0)
        if value < params.ssim_fail:
            level = "fail"
        elif value < params.ssim_warn:
            level = "warn"
        else:
            level = "pass"
        checks.append(
            {
                "name": "erase_ssim",
                "ok": level != "fail",
                "level": level,
                "value": value,
                "detail": f"掩码内结构相似度最低 {value:.3f}"
                f"（警告低于 {params.ssim_warn}，失败低于 {params.ssim_fail}）",
            }
        )
        if _RANK[level] > _RANK[verdict]:
            verdict = level
    return verdict, checks, notes


def implement_erase_verify(ctx: MediaContext, params: EraseVerifyParams) -> dict:
    section = ctx.plan.get("inpaint")
    if not section:
        raise bad_param("没有可校验的擦除报告；video.erase_verify 必须跟在 video.inpaint 之后")
    verification = (section.get("report") or {}).get("verification")
    if not verification:
        if params.require_report:
            raise bad_param("擦除报告里没有校验段落（工具运行时可能关闭了自动校验）")
        ctx.notes.append("擦除报告没有校验段落，按配方声明跳过确认流")
        return {"applicable": False, "verdict": "pass", "skipped": True}
    if not verification.get("applicable"):
        ctx.notes.append("擦除报告显示没有可校验的掩码区域，确认流跳过")
        return {"applicable": False, "verdict": "pass", "skipped": True}

    verdict, checks, notes = judge(verification, params)
    report = {
        "verdict": verdict,
        "checks": checks,
        "tool_verdict": section.get("verdict"),
        "attempt": section.get("attempt"),
        "notes": notes + list(verification.get("notes") or []),
    }
    ctx.plan["erase_verify"] = report
    for note in notes:
        ctx.notes.append(note)
    failed = [c for c in checks if c["level"] == "fail"]
    warned = [c for c in checks if c["level"] == "warn"]

    if verdict == "fail":
        detail = "；".join(f"{c['name']}: {c['detail']}" for c in failed) or "指标超过失败阈值"
        raise quality_retry(
            f"擦除确认流未通过（第 {section.get('attempt')} 次尝试）→ {detail}"
        )
    if verdict == "warn":
        detail = "；".join(f"{c['name']}: {c['detail']}" for c in warned) or "指标进入警告区间"
        if not params.accept_warn:
            raise WorkError(
                ErrorClass.QUALITY_GATE_FAILED,
                f"擦除确认流出现警告且配方要求不放行 → {detail}",
            )
        review = ctx.plan.setdefault("review", {"required": False, "severity": "info", "reasons": []})
        review["required"] = True
        review["severity"] = "warn" if review.get("severity") in (None, "info") else review["severity"]
        review["reasons"] = list(review.get("reasons") or []) + [f"确认流 warn：{detail}"]
        review["reason"] = review["reasons"][0]
        ctx.notes.append(f"擦除确认流为 warn，已登记人工抽查 → {detail}")
    else:
        ctx.notes.append(f"擦除确认流通过（{len(checks)} 项指标）")
    return report


register(
    ModuleSpec(
        name="video.inpaint",
        title="硬字幕/角标擦除（C12）",
        capability="video_inpaint",
        requires=frozenset({"video"}),
        produces=frozenset({"video"}),
        params_model=InpaintParams,
        kind="transcode",
        implement=implement_inpaint,
        # 没装/没构建 dossier-inpaint 的部署不会声明该能力，任务在计划阶段就被拒绝，
        # 而不是跑起来才发现工具缺失（见 docs/PROJECT_REVIEW.md C2）。
        capability_probe=capability_available,
        # 选了模型后端时，还要有真的可用的模型（见 required_inpaint_capabilities）。
        dynamic_capabilities=required_inpaint_capabilities,
    )
)
register(
    ModuleSpec(
        name="video.erase_verify",
        title="擦除结果确认流（C12）",
        capability="video_inpaint",
        requires=frozenset({"video"}),
        produces=frozenset({"erase_verification"}),
        params_model=EraseVerifyParams,
        kind="gate",
        implement=implement_erase_verify,
        capability_probe=capability_available,
    )
)
