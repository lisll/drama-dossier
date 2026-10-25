"""V5 式两轮编码端到端验证 CLI（技术验证，对照 Grove Max 逆向文档 §6）。

Grove Max 的两轮编码架构：first-encode 产出「母版」（去重画面+音频链），overlay
（水印/AI 角标）叠在母版上，final-encode 产出成片（精修链+机型伪装元数据）；每步
产物以 JSON 信封登记（schemaVersion/provenance/inputs/outputs 的 sha256），
final-manifest 汇总全链哈希。本 CLI 在本地复现这条链路（不联网、不签配方）：

    python -m drama_cloud.demo_dedup --input 样片.mp4 --output-dir out \
        --seed 42 --series 剧名 --episodes 1 \
        --badge "内容由AI生成，纯属虚构" --watermark 剧名 \
        --time-stretch auto

输出目录结构（对照 Grove workflows/<id>/pipeline-v5/ 的精简版）：

    <output-dir>/
      recipe.json               # seed 签发的完整去重配方
      first-encode.mp4          # 第一轮母版
      first-encode-envelope.json
      overlay/ai-badge.png      # 角标/水印 PNG
      overlay-envelope.json
      final-encode.mp4          # 第二轮成片
      final-manifest.json       # 全链 sha256 汇总
      checkpoint.json           # 断点续跑进度（对照 Grove §12.2 母版续跑）
      episode-<ep>.log          # per-episode 运行日志（Grove 命名规范）

断点续跑语义（Grove §12.2）：每步完成即写 checkpoint（记录产物 sha256）；
重跑时「配置一致 + 产物文件存在且 sha256 匹配」才跳过该步；配置变化
（seed/输入/文案/变调后端）或 checkpoint 损坏视为全新任务全量重跑；
某步重做后其下游步骤记录一律作废（下游产物由上游派生）。
"""
from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import sys
import uuid
from datetime import datetime
from pathlib import Path
from typing import Callable, Dict, List, Optional, Tuple

from .modules.dedup import (
    compile_dedup_audio,
    compile_dedup_finish,
    compile_dedup_video,
)
from .modules.overlay import OverlayParams, compile_overlay_expr, find_font, render_text_png
from .recipes.dedup import generate_dedup_recipe

SCHEMA_VERSION = 1

CHECKPOINT_NAME = "checkpoint.json"
#: 步骤依赖顺序：下游产物由上游派生，上游重做后下游记录作废
STAGE_ORDER = ("firstEncode", "overlay", "finalEncode")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def probe_media(ffprobe: str, source: Path) -> dict:
    result = subprocess.run(
        [ffprobe, "-v", "error", "-print_format", "json", "-show_streams", "-show_format",
         str(source)],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False,
    )
    if result.returncode != 0:
        raise SystemExit(f"ffprobe 失败：{result.stderr.decode('utf-8', 'replace')[:400]}")
    return json.loads(result.stdout.decode("utf-8", "replace"))


def write_envelope(output_dir: Path, name: str, envelope: dict) -> None:
    (output_dir / name).write_text(
        json.dumps(envelope, ensure_ascii=False, indent=2), encoding="utf-8"
    )


def _encode_args(encoding: dict, bframes: int = 0) -> List[str]:
    """配方 encoding 段 → libx264 CBR 参数（照抄 Grove compile_reference 的 encoderArgs）。"""
    target_bps = str(encoding["bitrateTargetBps"])
    interval = encoding["keyframeInterval"]
    return [
        "-c:v", encoding["encoder"], "-preset", encoding["preset"], "-profile:v",
        encoding["profile"], "-b:v", target_bps, "-minrate", target_bps,
        "-maxrate", target_bps, "-bufsize", str(encoding["bitrateBufferBps"]),
        "-x264-params",
        (f"keyint={interval}:min-keyint={encoding['keyframeMinInterval']}"
         f":scenecut=40:ref=4:bframes={bframes}:aq-mode={encoding['aqMode']}"
         f":nal-hrd={encoding['nalHrd']}:force-cfr=1:filler=1"),
    ]


def _source_sha_envelope(source: Path) -> dict:
    return {"path": str(source), "sha256": sha256_file(source)}


def finalize_encoder_tag(path: Path, expected_encoder: str) -> None:
    """把 mdta keys 表里的占位 ``gmencod`` 原位等长改写为 ``encoder``。

    语义对照 Grove ``reference_metadata.py``（finalize_encoder_tag）：mp4 muxer
    总是把 ``encoder`` 覆盖成 ``Lavf…``（暴露真实工具链），因此编码阶段先写自定义
    占位 key ``gmencod``=<伪装串>，收尾时二进制改名——两者都是 7 字节，atom 尺寸
    与媒体数据完全不动。前置条件：编码命令带 ``-fflags +bitexact``（不写 Lavf 串）
    且 ``-movflags use_metadata_tags``（允许任意 mdta key），占位 key 全文件唯一。
    """
    data = path.read_bytes()
    if data.count(b"gmencod") != 1:
        raise SystemExit("encoder 占位 key 不唯一或缺失，拒绝改写")
    needle = b"\x00\x00\x00\x0fmdtagmencod"
    index = data.find(needle)
    if index < 0:
        raise SystemExit("未找到 mdta keys 里的 gmencod 占位（结构不符）")
    name_offset = index + len(needle) - len(b"gmencod")
    if data[name_offset : name_offset + 7] != b"gmencod":
        raise SystemExit("占位 key 结构校验失败")
    expected = expected_encoder.encode("utf-8")
    if expected not in data:
        raise SystemExit(f"占位值与配方不符：{expected_encoder}")
    rewritten = data[:name_offset] + b"encoder" + data[name_offset + 7 :]
    path.write_bytes(rewritten)
    with path.open("r+b") as stream:
        stream.flush()
        import os

        os.fsync(stream.fileno())


# ---------------------------------------------------------------------------
# per-episode 日志与断点续跑
# ---------------------------------------------------------------------------


def _make_logger(output_dir: Path, episode: int) -> Tuple[Callable[[str], None], Path]:
    """per-episode 日志（对照 Grove workflows/<id>/pipeline-v5/episode-<ep>.log 命名）。

    stdout 与落盘双写；追加模式，断点续跑的日志接在同一文件之后，便于回溯整段历史。
    """
    log_path = output_dir / f"episode-{episode}.log"

    def log(line: str) -> None:
        stamp = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
        print(f"[{stamp}] {line}", flush=True)
        with log_path.open("a", encoding="utf-8") as stream:
            stream.write(f"[{stamp}] {line}\n")

    return log, log_path


def detect_rubberband(ffmpeg: str) -> bool:
    """探测 ffmpeg 是否带 rubberband 滤镜（Grove §12.3：rubberband 优先、atempo 兜底）。"""
    result = subprocess.run(
        [ffmpeg, "-hide_banner", "-filters"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False,
    )
    return b"rubberband" in result.stdout


def _load_checkpoint(output_dir: Path, config: dict, log: Callable[[str], None]) -> dict:
    """读 checkpoint；缺失/损坏/配置不一致都视为全新任务（全量重跑）。"""
    fresh = {"schemaVersion": SCHEMA_VERSION, "config": config, "stages": {}}
    path = output_dir / CHECKPOINT_NAME
    if not path.is_file():
        return fresh
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        log("checkpoint.json 读取失败，按全新任务全量重跑")
        return fresh
    if (not isinstance(data, dict) or not isinstance(data.get("config"), dict)
            or not isinstance(data.get("stages"), dict)):
        log("checkpoint.json 结构异常，按全新任务全量重跑")
        return fresh
    if data["config"] != config:
        changed = [key for key in sorted(set(data["config"]) | set(config))
                   if data["config"].get(key) != config.get(key)]
        log(f"配置与上次运行不一致（{', '.join(changed)}），全量重跑")
        return fresh
    log("checkpoint 校验通过，已完成步骤将断点跳过")
    return data


def _stage_valid(output_dir: Path, entry, expected_names: Tuple[str, ...]) -> bool:
    """步骤有效 = 标记 done + 产物清单一致 + 每个产物文件存在且 sha256 匹配。"""
    if not isinstance(entry, dict) or entry.get("status") != "done":
        return False
    outputs = entry.get("outputs")
    if not isinstance(outputs, dict) or set(outputs) != set(expected_names):
        return False
    for name, digest in outputs.items():
        path = output_dir / name
        if not path.is_file() or sha256_file(path) != digest:
            return False
    return True


def _record_stage(output_dir: Path, checkpoint: dict, name: str,
                  outputs: Dict[str, str], log: Callable[[str], None]) -> None:
    """登记已完成步骤并落盘 checkpoint；同时作废其下游步骤记录。"""
    checkpoint["stages"][name] = {"status": "done", "outputs": outputs}
    index = STAGE_ORDER.index(name)
    for later in STAGE_ORDER[index + 1:]:
        checkpoint["stages"].pop(later, None)
    (output_dir / CHECKPOINT_NAME).write_text(
        json.dumps(checkpoint, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    log(f"checkpoint 已更新：{name} 完成")


def run_pipeline(
    ffmpeg: str,
    ffprobe: str,
    source: Path,
    output_dir: Path,
    seed: int,
    series_name: str,
    episode: int,
    badge_text: Optional[str],
    watermark_text: Optional[str],
    font_path: Optional[str],
    time_stretch: str = "atempo",
) -> Path:
    output_dir.mkdir(parents=True, exist_ok=True)
    log, log_path = _make_logger(output_dir, episode)

    # 变调后端解析：auto 探测 ffmpeg 滤镜能力（rubberband 优先、atempo 兜底，§12.3）
    backend = time_stretch
    if backend == "auto":
        backend = "rubberband" if detect_rubberband(ffmpeg) else "atempo"
        log(f"变调后端 auto 探测：选用 {backend}")
    elif backend == "rubberband" and not detect_rubberband(ffmpeg):
        log("警告：指定 rubberband 但 ffmpeg 未带该滤镜，回落 atempo 链")
        backend = "atempo"

    log(f"任务启动：episode={episode} seed={seed} 变调后端={backend} 日志={log_path.name}")
    log(f"输入：{source}")
    probe = probe_media(ffprobe, source)
    video_stream = next((s for s in probe.get("streams", []) if s.get("codec_type") == "video"), None)
    if video_stream is None:
        log("失败：输入没有视频轨")
        raise SystemExit("输入没有视频轨")
    width, height = int(video_stream["width"]), int(video_stream["height"])
    has_audio = any(s.get("codec_type") == "audio" for s in probe.get("streams", []))
    log(f"媒体信息：{width}x{height} 音轨={'有' if has_audio else '无'}")

    input_sha = sha256_file(source)
    config = {
        "seed": seed,
        "episode": episode,
        "series": series_name,
        "badge": badge_text or "",
        "watermark": watermark_text or "",
        "timeStretch": backend,
        "inputSha256": input_sha,
    }
    checkpoint = _load_checkpoint(output_dir, config, log)
    run_id = checkpoint.setdefault("runId", uuid.uuid4().hex[:32])
    (output_dir / CHECKPOINT_NAME).write_text(
        json.dumps(checkpoint, ensure_ascii=False, indent=2), encoding="utf-8"
    )

    # 1) 签发配方（同 seed 可复现；代价低，每次重算保证与 checkpoint 校验同源）
    recipe = generate_dedup_recipe(
        seed=seed, episode_count=max(episode, 1), width=width, height=height,
        series_name=series_name,
    )
    variant = recipe["variants"][episode - 1]
    (output_dir / "recipe.json").write_text(
        json.dumps(recipe, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    log(f"配方已签发：referenceSha256={recipe['referenceSha256'][:16]}…")

    # 2) 第一轮编码：母版（去重画面链 + 音频去重链）
    master = output_dir / "first-encode.mp4"
    if _stage_valid(output_dir, checkpoint["stages"].get("firstEncode"), ("first-encode.mp4",)):
        log("第 1/3 步 母版编码：checkpoint 校验通过，跳过")
    else:
        log("第 1/3 步 母版编码（第一轮：去重画面链 + 音频链）…")
        args = [ffmpeg, "-hide_banner", "-nostdin", "-y", "-i", str(source)]
        filters = ["[0:v]" + ",".join(compile_dedup_video(
            # VideoDedupParams 只在模块层校验；CLI 直接走编译纯函数
            _SimpleParams(width, height, recipe),  # type: ignore[arg-type]
        )) + "[vout]"]
        if has_audio:
            filters.append("[0:a]" + ",".join(compile_dedup_audio(
                recipe["audio"], variant, backend=backend)) + "[aout]")
            args += ["-filter_complex", ";".join(filters), "-map", "[vout]", "-map", "[aout]",
                     "-c:a", "aac", "-b:a", str(recipe["audio"]["bitrateBps"] // 1000) + "k"]
        else:
            args += ["-filter_complex", ";".join(filters), "-map", "[vout]"]
        args += _encode_args(recipe["encoding"], int(variant.get("bFrames", 0)))
        args += ["-movflags", "+faststart", str(master)]
        result = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
        if result.returncode != 0:
            detail = result.stderr.decode("utf-8", "replace")[-800:]
            log(f"第 1/3 步 母版编码失败：{detail}")
            raise SystemExit(f"第一轮编码失败：{detail}")
        write_envelope(output_dir, "first-encode-envelope.json", {
            "schemaVersion": SCHEMA_VERSION,
            "stage": "first-encode",
            "runId": run_id,
            "seed": seed,
            "referenceSha256": recipe["referenceSha256"],
            "provenance": {
                "inputs": [_source_sha_envelope(source)],
                "outputs": [_source_sha_envelope(master)],
                "command": args,
            },
        })
        _record_stage(output_dir, checkpoint, "firstEncode",
                      {"first-encode.mp4": sha256_file(master)}, log)

    # 3) overlay：角标/水印 PNG（位置/漂移参数来自配方 boxes 语义）。
    #    x/y 表达式由 seed 配方确定性推导，跳过渲染时同样要用于成片轮 overlay。
    overlay_dir = output_dir / "overlay"
    overlay_dir.mkdir(exist_ok=True)
    font = find_font(font_path)
    overlay_specs = []  # (相对路径, 渲染参数, x 表达式, y 表达式, kind)
    boxes = variant.get("boxes") or []
    for kind, text, anchor_y, box in (
        ("ai-badge", badge_text, "top", boxes[0] if len(boxes) > 0 else None),
        ("title-watermark", watermark_text, "bottom", boxes[1] if len(boxes) > 1 else None),
    ):
        if not text:
            continue
        params = OverlayParams(
            text=text,
            anchor_x="left" if box is None else box["xAnchor"],
            anchor_y=anchor_y,
            margin=24,
            opacity=float(box["opacity"]) if box else 0.85,
            drift_x=float(box["xWave"][1]) if box else 0.0,
            drift_period_x=float(box["xWave"][2]) if box else 300.0,
        )
        x_expr, y_expr = compile_overlay_expr(
            params.anchor_x, params.anchor_y, params.margin,
            params.drift_x, params.drift_period_x, params.drift_y, params.drift_period_y,
        )
        overlay_specs.append((f"overlay/{kind}.png", params, x_expr, y_expr, kind))
    overlays = [
        {"kind": kind, "path": str(output_dir / rel), "x": x_expr, "y": y_expr}
        for rel, _params, x_expr, y_expr, kind in overlay_specs
    ]
    if _stage_valid(output_dir, checkpoint["stages"].get("overlay"),
                    tuple(spec[0] for spec in overlay_specs)):
        log("第 2/3 步 overlay 渲染：checkpoint 校验通过，跳过")
    else:
        log(f"第 2/3 步 overlay 渲染（{len(overlay_specs)} 张 PNG）…")
        for rel, params, _x, _y, _kind in overlay_specs:
            render_text_png(params.text, output_dir / rel, font, params.font_size,
                            params.text_color, params.bg_color, params.opacity, params.padding)
        write_envelope(output_dir, "overlay-envelope.json", {
            "schemaVersion": SCHEMA_VERSION,
            "stage": "overlay",
            "runId": run_id,
            "provenance": {
                "inputs": [_source_sha_envelope(master)],
                "outputs": [{"kind": item["kind"], **_source_sha_envelope(Path(item["path"]))}
                            for item in overlays],
            },
        })
        _record_stage(output_dir, checkpoint, "overlay",
                      {rel: sha256_file(output_dir / rel) for rel, *_ in overlay_specs}, log)

    # 4) 第二轮编码：成片（精修链 + overlay + 机型伪装元数据）
    #    -fflags +bitexact：不写 Lavf 串（防工具链暴露）；use_metadata_tags：允许
    #    写任意 mdta key（make/software 在 mp4 muxer 白名单之外）；encoder 走
    #    gmencod 占位 + finalize_encoder_tag 等长改写（对照 Grove reference_metadata）。
    #    -map_metadata -1：成片不继承母版 tags（否则母版的 Lavf 串等会跟伪装串
    #    双写并存）；随后只写配方签发的伪装 tags。
    final = output_dir / "final-encode.mp4"
    episode_meta = recipe["metadata"]["episodes"][episode - 1]
    if _stage_valid(output_dir, checkpoint["stages"].get("finalEncode"), ("final-encode.mp4",)):
        log("第 3/3 步 成片编码：checkpoint 校验通过，跳过")
    else:
        log("第 3/3 步 成片编码（精修链 + overlay + 机型伪装元数据）…")
        args = [ffmpeg, "-hide_banner", "-nostdin", "-y", "-i", str(master)]
        for item in overlays:
            args += ["-i", item["path"]]
        filters = ["[0:v]" + ",".join(compile_dedup_finish(variant)) + "[vpre]"]
        current = "vpre"
        for offset, item in enumerate(overlays):
            nxt = f"vov{offset}"
            filters.append(
                f"[{current}][{offset + 1}:v]overlay=x='{item['x']}':y='{item['y']}'[{nxt}]"
            )
            current = nxt
        args += ["-filter_complex", ";".join(filters), "-map", f"[{current}]",
                 "-map_metadata", "-1"]
        if has_audio:
            args += ["-map", "0:a?", "-c:a", "copy"]
        args += ["-fflags", "+bitexact", "-flags:v", "+bitexact"]
        args += _encode_args(recipe["encoding"], int(variant.get("bFrames", 0)))
        for key, value in (
            ("title", f"{series_name}-第{episode}集"),
            ("creation_time", recipe["metadata"]["creationTime"]),
            ("comment", episode_meta["comment"]),
            ("copyright", episode_meta["copyright"]),
            ("make", episode_meta["make"]),
            ("software", episode_meta.get("software", "")),
            ("gmencod", recipe["metadata"]["encoder"]),
        ):
            if value:
                args += ["-metadata", f"{key}={value}"]
        args += ["-movflags", "use_metadata_tags+faststart", str(final)]
        result = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
        if result.returncode != 0:
            detail = result.stderr.decode("utf-8", "replace")[-800:]
            log(f"第 3/3 步 成片编码失败：{detail}")
            raise SystemExit(f"第二轮编码失败：{detail}")
        finalize_encoder_tag(final, recipe["metadata"]["encoder"])
        _record_stage(output_dir, checkpoint, "finalEncode",
                      {"final-encode.mp4": sha256_file(final)}, log)

    # 5) final-manifest：全链哈希汇总（对照 Grove final-manifest.json；代价低，每次重写）
    ffmpeg_sha = sha256_file(Path(ffmpeg)) if Path(ffmpeg).is_file() else None
    manifest = {
        "schemaVersion": SCHEMA_VERSION,
        "stage": "final-manifest",
        "runId": run_id,
        "seed": seed,
        "referenceSha256": recipe["referenceSha256"],
        "episode": episode,
        "stages": {
            "firstEncode": {"output": _source_sha_envelope(master)},
            "overlay": [{"kind": item["kind"], **_source_sha_envelope(Path(item["path"]))}
                        for item in overlays],
            "finalEncode": {"output": _source_sha_envelope(final)},
        },
        "toolchain": {"ffmpeg": str(ffmpeg), "ffmpegSha256": ffmpeg_sha,
                      "ffprobe": str(ffprobe)},
        "metadataTags": {
            "make": episode_meta["make"],
            "encoder": recipe["metadata"]["encoder"],
            "creationTime": recipe["metadata"]["creationTime"],
        },
    }
    write_envelope(output_dir, "final-manifest.json", manifest)
    log("final-manifest 已汇总，全链 3 步完成")
    log(f"完成：{final}")
    return final


class _SimpleParams:
    """CLI 直连编译纯函数用的最小参数壳（不走 pydantic 校验，配方已自检过）。"""

    def __init__(self, width: int, height: int, recipe: dict):
        self.width = width
        self.height = height
        self.pixelFormat = recipe["encoding"]["pixelFormat"]
        self.scaleFactor = recipe["scaleFactor"]
        self.temporalWeights = recipe["temporalWeights"]
        self.variant = recipe["variants"][0]


def main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(description="V5 式两轮去重编码端到端验证")
    parser.add_argument("--input", required=True, help="输入视频")
    parser.add_argument("--output-dir", required=True, help="产物目录")
    parser.add_argument("--seed", type=int, default=42)
    parser.add_argument("--series", default="示例剧")
    parser.add_argument("--episodes", type=int, default=1, help="签发配方的分集数量")
    parser.add_argument("--episode", type=int, default=1, help="本次编码取第几集配方")
    parser.add_argument("--badge", default=None, help="AI 声明角标文案")
    parser.add_argument("--watermark", default=None, help="剧名水印文案")
    parser.add_argument("--font", default=None, help="中文字体路径（默认自动探测）")
    parser.add_argument(
        "--time-stretch", choices=("atempo", "rubberband", "auto"), default="atempo",
        help="音频变调后端：atempo 兜底 / rubberband 优先 / auto 探测（默认 atempo）",
    )
    parser.add_argument("--ffmpeg", default="ffmpeg")
    parser.add_argument("--ffprobe", default="ffprobe")
    args = parser.parse_args(argv)

    final = run_pipeline(
        ffmpeg=args.ffmpeg, ffprobe=args.ffprobe, source=Path(args.input),
        output_dir=Path(args.output_dir), seed=args.seed, series_name=args.series,
        episode=args.episode, badge_text=args.badge, watermark_text=args.watermark,
        font_path=args.font, time_stretch=args.time_stretch,
    )
    print(f"完成：{final}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
