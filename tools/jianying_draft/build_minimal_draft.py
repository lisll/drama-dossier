#!/usr/bin/env python3
"""剪映真草稿生成：把一段成片铺成「视频/音频/文本」三轨的可编辑草稿。

用途（技术验证口径）：验证「去重成片 → 剪映草稿」这条链路。生成物是剪映
5.x/6.x 草稿目录（draft_content.json + 草稿元信息），可以直接被剪映打开二次创作。

与 Grove 参考实现（engine__sidecar__jianying_draft__build_draft.py，已证实）的对应关系：
- 依赖同一套 pyJianYingDraft 库生成 draft_content.json 骨架（Grove L1721-1739），
  不手写 JSON——剪映对草稿字段校验严格，手写骨架极易开不出来。
- 每集一个草稿、主轨吸附（Grove L1741-1749 `create_draft(..., maintrack_adsorb=True)`）。
- 视频段与音轨分离：给 `--audio` 时视频段静音、原声单独铺音轨，方便剪辑师独立调音
  （Grove L1896-1901 视频段 `volume=0.0` + 独立 stem 音轨同款做法）。

与 Grove 的刻意简化（均为有意取舍，非遗漏）：
- 不把素材拷进 `Resources/local`，草稿直接引用源文件绝对路径（本机打开即可）；
  Grove 拷贝是为了草稿目录自包含可搬运，验证阶段不需要。
- 不做转场/滤镜/特效/贴纸/参考文本等装饰轨，只保留三轨核心结构。
- 无 SRT 时放一条 3 秒剧名占位标题，验证文本轨可编辑。

运行方式（复用 cloud venv，pyJianYingDraft 已装入）：
    cloud/.venv/bin/python tools/jianying_draft/build_minimal_draft.py \
        --video /path/to/final-encode.mp4 [--audio /path/to/audio.m4a] \
        [--srt /path/to/subs.srt] [--output-root /path/to/jianying_drafts]

已知待确认项（后续适配 Windows 时验证）：
- pymediainfo 依赖 MediaInfo 动态库；macOS 的 wheel 自带，Windows 需要随包
  分发 MediaInfo.dll（待确认发行方式）。
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

try:
    from pyJianYingDraft import (
        AudioMaterial,
        AudioSegment,
        DraftFolder,
        TextSegment,
        Timerange,
        TrackSpec,
        TrackType,
        VideoMaterial,
        VideoSegment,
    )
except ImportError:
    print(
        "缺少 pyJianYingDraft 依赖：请用 cloud/.venv/bin/python 运行，"
        "或先 `pip install pyJianYingDraft`（对应 Grove 的 JIANYING_DRAFT_DEPENDENCY_MISSING）。",
        file=sys.stderr,
    )
    sys.exit(2)


def build_minimal_draft(
    video_path: Path,
    *,
    draft_name: str,
    output_root: Path,
    audio_path: Path | None = None,
    srt_path: Path | None = None,
    fps: int = 30,
) -> dict:
    """生成一个三轨草稿目录，返回摘要（供调用方打印或断言）。

    结构（对照 Grove 的分轨命名习惯）：
    - 视频主轨「主视频」：成片全长单段；
    - 音频轨「原声」：仅当提供 `--audio`；与视频对齐铺满，超出视频长度的部分截断；
    - 文本轨「字幕」：SRT 可编辑字幕，或 3 秒剧名占位标题。
    """
    if not video_path.is_file():
        raise FileNotFoundError(f"成片不存在：{video_path}")

    # 素材探测（pymediainfo 读取时长/宽高；macOS wheel 自带 MediaInfo 库）
    video_material = VideoMaterial(str(video_path), draft_name)
    duration_us = video_material.duration
    if duration_us <= 0:
        raise ValueError(f"成片时长探测为 0，无法建草稿：{video_path}")

    output_root.mkdir(parents=True, exist_ok=True)  # DraftFolder 要求根目录已存在（Grove L1713 同款）
    folder = DraftFolder(str(output_root))
    # allow_replace=True：技术验证阶段允许覆盖同名草稿，便于反复跑
    script = folder.create_draft(
        draft_name,
        video_material.width,
        video_material.height,
        fps,
        maintrack_adsorb=True,
        allow_replace=True,
    )

    # 三轨一次建齐（Grove L1863-1877：先建轨再加段，固定展示顺序）
    script.append_track(TrackSpec(TrackType.video, "主视频"))
    if audio_path is not None:
        script.append_track(TrackSpec(TrackType.audio, "原声"))
    script.append_track(TrackSpec(TrackType.text, "字幕"))

    # 视频主轨：整段铺满。提供独立音轨时静音（Grove 同款 volume=0.0）
    script.add_segment(
        VideoSegment(
            video_material,
            Timerange(0, duration_us),
            source_timerange=Timerange(0, duration_us),
            volume=0.0 if audio_path is not None else 1.0,
        ),
        "主视频",
    )

    # 原声音轨：与视频对齐，超出部分截断
    if audio_path is not None:
        if not audio_path.is_file():
            raise FileNotFoundError(f"音轨不存在：{audio_path}")
        audio_material = AudioMaterial(str(audio_path), "原声")
        audio_duration_us = min(audio_material.duration, duration_us)
        if audio_duration_us <= 0:
            raise ValueError(f"音轨时长探测为 0：{audio_path}")
        script.add_segment(
            AudioSegment(
                audio_material,
                Timerange(0, audio_duration_us),
                source_timerange=Timerange(0, audio_duration_us),
                volume=1.0,
            ),
            "原声",
        )

    # 文本轨：SRT 优先（import_srt 自动建段，片段在剪映里可直接改字）；
    # 没有 SRT 时放一条 3 秒占位标题，验证文本轨结构
    if srt_path is not None:
        if not srt_path.is_file():
            raise FileNotFoundError(f"字幕不存在：{srt_path}")
        script.import_srt(str(srt_path), "字幕")
    else:
        script.add_segment(TextSegment(draft_name, Timerange(0, 3_000_000)), "字幕")

    script.save()
    # create_draft 的草稿目录固定是 根目录/草稿名（pyJianYingDraft draft_folder.py 源码已证实）
    draft_path = output_root / draft_name

    # 读回 draft_content.json 打结构摘要，确认三轨确实落盘
    content = json.loads((draft_path / "draft_content.json").read_text(encoding="utf-8"))
    tracks = content.get("tracks", [])
    return {
        "draft_name": draft_name,
        "draft_path": str(draft_path),
        "duration_seconds": duration_us / 1_000_000,
        "canvas": {"width": video_material.width, "height": video_material.height, "fps": fps},
        "tracks": [
            {"type": t.get("type"), "name": t.get("name"), "segments": len(t.get("segments", []))}
            for t in tracks
        ],
        "materials": {
            "videos": len(content.get("materials", {}).get("videos", [])),
            "audios": len(content.get("materials", {}).get("audios", [])),
            "texts": len(content.get("materials", {}).get("texts", [])),
        },
    }


def default_draft_name(video_path: Path) -> str:
    """默认草稿名：直接用成片文件名（含集号），剪映时间线标签即用它。"""
    return video_path.stem


def main() -> int:
    parser = argparse.ArgumentParser(description="生成剪映三轨真草稿（技术验证）")
    parser.add_argument("--video", required=True, help="成片视频路径")
    parser.add_argument("--audio", help="可选：独立原声音轨（提供时视频段静音）")
    parser.add_argument("--srt", help="可选：SRT 字幕（缺省时放 3 秒剧名占位标题）")
    parser.add_argument("--name", help="草稿名（默认用成片文件名）")
    parser.add_argument(
        "--output-root",
        default=str(Path.cwd() / "jianying_drafts"),
        help="剪映草稿根目录（默认 ./jianying_drafts）",
    )
    parser.add_argument("--fps", type=int, default=30, help="草稿帧率（默认 30，只影响吸附粒度）")
    args = parser.parse_args()

    video_path = Path(args.video).expanduser().resolve()
    summary = build_minimal_draft(
        video_path,
        draft_name=args.name or default_draft_name(video_path),
        output_root=Path(args.output_root).expanduser().resolve(),
        audio_path=Path(args.audio).expanduser().resolve() if args.audio else None,
        srt_path=Path(args.srt).expanduser().resolve() if args.srt else None,
        fps=args.fps,
    )
    print(json.dumps(summary, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
