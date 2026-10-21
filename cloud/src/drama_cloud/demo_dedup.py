"""V5 式两轮编码端到端验证 CLI（技术验证，对照 Grove Max 逆向文档 §6）。

Grove Max 的两轮编码架构：first-encode 产出「母版」（去重画面+音频链），overlay
（水印/AI 角标）叠在母版上，final-encode 产出成片（精修链+机型伪装元数据）；每步
产物以 JSON 信封登记（schemaVersion/provenance/inputs/outputs 的 sha256），
final-manifest 汇总全链哈希。本 CLI 在本地复现这条链路（不联网、不签配方）：

    python -m drama_cloud.demo_dedup --input 样片.mp4 --output-dir out \
        --seed 42 --series 剧名 --episodes 1 \
        --badge "内容由AI生成，纯属虚构" --watermark 剧名

输出目录结构（对照 Grove workflows/<id>/pipeline-v5/ 的精简版）：

    <output-dir>/
      recipe.json               # seed 签发的完整去重配方
      first-encode.mp4          # 第一轮母版
      first-encode-envelope.json
      overlay/ai-badge.png      # 角标/水印 PNG
      overlay-envelope.json
      final-encode.mp4          # 第二轮成片
      final-manifest.json       # 全链 sha256 汇总
"""
from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import sys
import uuid
from pathlib import Path
from typing import List, Optional

from .modules.dedup import (
    compile_dedup_audio,
    compile_dedup_finish,
    compile_dedup_video,
)
from .modules.overlay import OverlayParams, compile_overlay_expr, find_font, render_text_png
from .recipes.dedup import generate_dedup_recipe

SCHEMA_VERSION = 1


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
) -> Path:
    output_dir.mkdir(parents=True, exist_ok=True)
    run_id = uuid.uuid4().hex[:32]
    probe = probe_media(ffprobe, source)
    video_stream = next((s for s in probe.get("streams", []) if s.get("codec_type") == "video"), None)
    if video_stream is None:
        raise SystemExit("输入没有视频轨")
    width, height = int(video_stream["width"]), int(video_stream["height"])
    has_audio = any(s.get("codec_type") == "audio" for s in probe.get("streams", []))

    # 1) 签发配方（同 seed 可复现）
    recipe = generate_dedup_recipe(
        seed=seed, episode_count=max(episode, 1), width=width, height=height,
        series_name=series_name,
    )
    variant = recipe["variants"][episode - 1]
    (output_dir / "recipe.json").write_text(
        json.dumps(recipe, ensure_ascii=False, indent=2), encoding="utf-8"
    )

    # 2) 第一轮编码：母版（去重画面链 + 音频去重链）
    master = output_dir / "first-encode.mp4"
    args = [ffmpeg, "-hide_banner", "-nostdin", "-y", "-i", str(source)]
    filters = ["[0:v]" + ",".join(compile_dedup_video(
        # VideoDedupParams 只在模块层校验；CLI 直接走编译纯函数
        _SimpleParams(width, height, recipe),  # type: ignore[arg-type]
    )) + "[vout]"]
    if has_audio:
        filters.append("[0:a]" + ",".join(compile_dedup_audio(recipe["audio"], variant)) + "[aout]")
        args += ["-filter_complex", ";".join(filters), "-map", "[vout]", "-map", "[aout]",
                 "-c:a", "aac", "-b:a", str(recipe["audio"]["bitrateBps"] // 1000) + "k"]
    else:
        args += ["-filter_complex", ";".join(filters), "-map", "[vout]"]
    args += _encode_args(recipe["encoding"], int(variant.get("bFrames", 0)))
    args += ["-movflags", "+faststart", str(master)]
    result = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    if result.returncode != 0:
        raise SystemExit(f"第一轮编码失败：{result.stderr.decode('utf-8', 'replace')[-800:]}")
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

    # 3) overlay：角标/水印 PNG（位置/漂移参数来自配方 boxes 语义）
    overlay_dir = output_dir / "overlay"
    overlay_dir.mkdir(exist_ok=True)
    font = find_font(font_path)
    overlays = []
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
        target = overlay_dir / f"{kind}.png"
        render_text_png(params.text, target, font, params.font_size, params.text_color,
                        params.bg_color, params.opacity, params.padding)
        x_expr, y_expr = compile_overlay_expr(
            params.anchor_x, params.anchor_y, params.margin,
            params.drift_x, params.drift_period_x, params.drift_y, params.drift_period_y,
        )
        overlays.append({"kind": kind, "path": str(target), "x": x_expr, "y": y_expr})
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

    # 4) 第二轮编码：成片（精修链 + overlay + 机型伪装元数据）
    #    -fflags +bitexact：不写 Lavf 串（防工具链暴露）；use_metadata_tags：允许
    #    写任意 mdta key（make/software 在 mp4 muxer 白名单之外）；encoder 走
    #    gmencod 占位 + finalize_encoder_tag 等长改写（对照 Grove reference_metadata）。
    #    -map_metadata -1：成片不继承母版 tags（否则母版的 Lavf 串等会跟伪装串
    #    双写并存）；随后只写配方签发的伪装 tags。
    final = output_dir / "final-encode.mp4"
    episode_meta = recipe["metadata"]["episodes"][episode - 1]
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
        raise SystemExit(f"第二轮编码失败：{result.stderr.decode('utf-8', 'replace')[-800:]}")
    finalize_encoder_tag(final, recipe["metadata"]["encoder"])

    # 5) final-manifest：全链哈希汇总（对照 Grove final-manifest.json）
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
    parser.add_argument("--ffmpeg", default="ffmpeg")
    parser.add_argument("--ffprobe", default="ffprobe")
    args = parser.parse_args(argv)

    final = run_pipeline(
        ffmpeg=args.ffmpeg, ffprobe=args.ffprobe, source=Path(args.input),
        output_dir=Path(args.output_dir), seed=args.seed, series_name=args.series,
        episode=args.episode, badge_text=args.badge, watermark_text=args.watermark,
        font_path=args.font,
    )
    print(f"完成：{final}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
