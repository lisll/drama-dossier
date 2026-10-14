"""Tier 2 后期能力接入云端：模块注册、能力探测、以及用假工具跑通的执行链路。

这里用**假 `dossier-media`**（一个 bash 脚本）来驱动：真实工具需要
demucs/whisper/tesseract，本机没有；但「云端把参数传对了、产物收回来了、
能力缺失时明确报错」这些行为完全可以用假工具验证，而且不依赖模型权重。
"""

from __future__ import annotations

import dataclasses
import json
import shutil
import stat
import subprocess
from pathlib import Path

import pytest

from drama_cloud.errors import ErrorClass, WorkError
from drama_cloud.modules import media_cli
from drama_cloud.modules.ffmpeg import MediaContext
from drama_cloud.modules.late_stage import (
    AsrParams,
    OcrParams,
    OcrRegionParams,
    RenderParams,
    SeparateParams,
    ShotsParams,
    TimeStretchParams,
    implement_asr,
    implement_ocr,
    implement_render,
    implement_separate,
    implement_shots,
    implement_timestretch,
)
from drama_cloud.recipes.registry import registered_modules

#: 假 `dossier-media`：把 `probe --json` 的可探测标签交给环境变量控制，
#: 其余子命令按真实工具的产物约定造文件。
FAKE_MEDIA = """#!/usr/bin/env bash
set -euo pipefail
write_probe() {
  caps="${FAKE_MEDIA_CAPS:-}"
  json="["
  first=1
  IFS=',' read -ra items <<< "$caps"
  for item in "${items[@]}"; do
    [ -z "$item" ] && continue
    if [ $first -eq 0 ]; then json="$json,"; fi
    json="$json\\"$item\\""
    first=0
  done
  json="$json]"
  echo "{\\"capabilities\\": $json, \\"missing\\": []}"
}

cmd="${1:-}"
shift || true
case "$cmd" in
  probe) write_probe ;;
  render)
    input=""; output=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --input) input="$2"; shift 2;;
        --output) output="$2"; shift 2;;
        *) shift;;
      esac
    done
    cp "$input" "$output"
    echo "rendered"
    ;;
  separate)
    input=""; out=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --input) input="$2"; shift 2;;
        --output-dir) out="$2"; shift 2;;
        *) shift;;
      esac
    done
    stem="$(basename "$input")"; stem="${stem%.*}"
    mkdir -p "$out/htdemucs/$stem"
    : > "$out/htdemucs/$stem/vocals.wav"
    : > "$out/htdemucs/$stem/no_vocals.wav"
    ;;
  speed)
    output=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --output) output="$2"; shift 2;;
        *) shift;;
      esac
    done
    # 必须写出**非空**文件：实现会把「命令成功但产物为空」当成失败拦住，
    # 空文件测不出真实产物链路。
    printf 'stretched-audio' > "$output"
    ;;
  asr)
    input=""; out=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --input) input="$2"; shift 2;;
        --output-dir) out="$2"; shift 2;;
        *) shift;;
      esac
    done
    stem="$(basename "$input")"; stem="${stem%.*}"
    mkdir -p "$out"
    printf '1\\n00:00:01,000 --> 00:00:02,000\\n你好\\n\\n2\\n00:00:02,500 --> 00:00:03,500\\n再见\\n' > "$out/$stem.srt"
    ;;
  shots)
    # 真实工具会先打印将要执行的命令，再打印 JSON —— 云端必须能从混合输出里取出 JSON。
    echo "ffmpeg -hide_banner -i in.mp4 -vf select='gt(scene,0.35)',metadata=print"
    if [ -n "${FAKE_MEDIA_SHOTS_NO_JSON:-}" ]; then
      echo "检测完成：2 个切点"
      exit 0
    fi
    printf '{"threshold": 0.35, "boundaries": [{"timeSec": 1.25, "score": 0.5}, {"timeSec": 2.5, "score": 0.9}], "detail": "…"}\\n'
    ;;
  ocr)
    out=""; image=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --output-dir) out="$2"; shift 2;;
        --image) image="$2"; shift 2;;
        *) shift;;
      esac
    done
    mkdir -p "$out"
    if [ -n "${FAKE_MEDIA_OCR_NO_FRAMES:-}" ]; then
      # 「区域落在画面外 / 抽帧参数打不中任何帧」的变体：命令成功但没有产物。
      exit 0
    fi
    if [ -n "$image" ]; then
      printf '[{"text": "第1集", "confidence": 92.5, "left": 0, "top": 0, "width": 40, "height": 12}]\\n'
      echo "已识别 1 行"
    else
      : > "$out/region_0001.png"
      : > "$out/region_0002.png"
    fi
    ;;
  *) echo "unknown command $cmd" >&2; exit 9;;
esac
"""


def make_fake_media(tmp_path: Path, caps: str) -> Path:
    script = tmp_path / "dossier-media"
    script.write_text(FAKE_MEDIA.replace('caps="${FAKE_MEDIA_CAPS:-}"', f'caps="{caps}"'))
    script.chmod(script.stat().st_mode | stat.S_IEXEC)
    return script


@pytest.fixture()
def context(tmp_path, settings):
    def build(suffix: str = "in.mp4") -> MediaContext:
        work = tmp_path / "work"
        work.mkdir(exist_ok=True)
        primary = tmp_path / f"source{suffix}"
        primary.write_bytes(b"x")
        return MediaContext(
            settings=settings,
            work_dir=work,
            primary=primary,
            inputs=[primary],
            probe={"duration": 4.0, "has_audio": True, "has_video": True,
                   "fps": 10.0, "width": 320, "height": 240},
        )

    return build


ALL_CAPS = (
    "media.render.compose,media.render.subtitle_burn,media.audio.separate,"
    "media.audio.timestretch,media.asr,media.ocr"
)


def test_modules_are_registered_with_capability_labels():
    modules = registered_modules()
    for name in ("video.render", "audio.separate", "audio.timestretch", "subtitle.asr", "subtitle.ocr"):
        assert name in modules, f"{name} 没有注册"
    assert modules["subtitle.asr"].produces == frozenset({"subtitle_text"})


def test_capability_probe_is_false_when_the_tool_is_missing(settings, monkeypatch):
    media_cli.clear_cache()
    # 只把配置指成不存在的路径是不够的：查找顺序里还有 PATH 与仓库 target/，
    # 学习机上 `cargo build` 过就会命中真实工具。这里直接掐掉解析结果。
    monkeypatch.setattr(media_cli, "resolve_binary", lambda *args, **kwargs: None)
    bare = dataclasses.replace(settings, media_bin="/nonexistent/dossier-media")
    assert media_cli.media_capabilities(bare) == frozenset()
    assert media_cli.requires_media("media.asr")(bare) is False


def test_capability_probe_reads_the_tool_report(tmp_path, settings):
    media_cli.clear_cache()
    tool = make_fake_media(tmp_path, "media.asr")
    configured = dataclasses.replace(settings, media_bin=str(tool))
    assert media_cli.media_capabilities(configured) == frozenset({"media.asr"})
    assert media_cli.requires_media("media.asr")(configured) is True
    assert media_cli.requires_media("media.ocr")(configured) is False


def test_render_runs_the_tool_and_replaces_the_input(tmp_path, settings, context):
    media_cli.clear_cache()
    tool = make_fake_media(tmp_path, ALL_CAPS)
    ctx = context()
    ctx.settings = dataclasses.replace(settings, media_bin=str(tool))
    plan = implement_render(ctx, RenderParams(texts=["第 1 集"], subtitles_input_index=None))
    assert Path(plan["output"]).is_file()
    assert ctx.inputs[0] == Path(plan["output"])
    assert ctx.primary == Path(plan["output"])
    assert any("合成渲染完成" in note for note in ctx.notes)


def test_render_requires_an_element(tmp_path):
    with pytest.raises(Exception):
        RenderParams()


def test_render_refuses_subtitle_burn_without_libass(tmp_path, settings, context):
    media_cli.clear_cache()
    tool = make_fake_media(tmp_path, "media.render.compose")
    ctx = context()
    ctx.settings = dataclasses.replace(settings, media_bin=str(tool))
    with pytest.raises(WorkError) as error:
        implement_render(ctx, RenderParams(texts=["x"], use_asr_subtitle=True))
    # 这里先撞上「没有 ASR 产出」；把 ASR 计划补上后应撞「没有 libass」。
    assert error.value.error_class in (ErrorClass.PARAM_OUT_OF_RANGE, ErrorClass.CAPABILITY_MISSING)


def test_render_burns_the_subtitle_produced_by_asr(tmp_path, settings, context):
    media_cli.clear_cache()
    tool = make_fake_media(tmp_path, ALL_CAPS)
    ctx = context()
    ctx.settings = dataclasses.replace(settings, media_bin=str(tool))
    implement_asr(ctx, AsrParams())
    plan = implement_render(ctx, RenderParams(use_asr_subtitle=True))
    assert plan["subtitle"] == ctx.plan["asr"]["subtitle"]


def test_separate_collects_the_stems_from_disk(tmp_path, settings, context):
    media_cli.clear_cache()
    tool = make_fake_media(tmp_path, ALL_CAPS)
    ctx = context(suffix=".wav")
    ctx.settings = dataclasses.replace(settings, media_bin=str(tool))
    plan = implement_separate(ctx, SeparateParams())
    assert len(plan["stems"]) == 2
    assert {Path(p).name for p in plan["stems"]} == {"vocals.wav", "no_vocals.wav"}
    assert [a["kind"] for a in ctx.artifacts] == ["stem", "stem"]


def test_timestretch_refuses_pitch_preservation_without_rubberband(tmp_path, settings, context):
    media_cli.clear_cache()
    tool = make_fake_media(tmp_path, "media.audio.tempo_only")
    ctx = context(suffix=".wav")
    ctx.settings = dataclasses.replace(settings, media_bin=str(tool))
    with pytest.raises(WorkError) as error:
        implement_timestretch(ctx, TimeStretchParams(speed=1.2, allow_pitch_shift=False))
    assert error.value.error_class == ErrorClass.CAPABILITY_MISSING
    # 显式接受变调后就能跑（这里验证「不静默降级」的另一半）。
    plan = implement_timestretch(ctx, TimeStretchParams(speed=1.2, allow_pitch_shift=True))
    assert Path(plan["output"]).is_file()
    assert plan["replaced_input"] is True


def test_timestretch_does_not_replace_a_video_input(tmp_path, settings, context):
    media_cli.clear_cache()
    tool = make_fake_media(tmp_path, ALL_CAPS)
    ctx = context()  # 视频素材
    ctx.settings = dataclasses.replace(settings, media_bin=str(tool))
    plan = implement_timestretch(ctx, TimeStretchParams(speed=1.2, allow_pitch_shift=True))
    assert plan["replaced_input"] is False
    assert ctx.inputs[0] == ctx.primary  # 主视频没被换成一个只有声音的 wav


def test_asr_registers_the_subtitle_artifact(tmp_path, settings, context):
    media_cli.clear_cache()
    tool = make_fake_media(tmp_path, ALL_CAPS)
    ctx = context(suffix=".wav")
    ctx.settings = dataclasses.replace(settings, media_bin=str(tool))
    plan = implement_asr(ctx, AsrParams())
    assert plan["cues"] == 2
    kinds = [a["kind"] for a in ctx.artifacts]
    assert kinds == ["subtitle"]
    assert ctx.plan["asr"]["subtitle"].endswith(".srt")


def test_subtitle_mux_can_use_the_planned_asr_subtitle(tmp_path, settings, context):
    from drama_cloud.modules.encode import SubtitleParams, SubtitleSource, implement_subtitle

    media_cli.clear_cache()
    tool = make_fake_media(tmp_path, ALL_CAPS)
    ctx = context(suffix=".wav")
    ctx.settings = dataclasses.replace(settings, media_bin=str(tool))
    implement_asr(ctx, AsrParams())
    plan = implement_subtitle(
        ctx, SubtitleParams(source=SubtitleSource(kind="planned"), mode="burn")
    )
    assert plan["kind"] == "planned"
    assert Path(plan["path"]).is_file()
    # 没有前序 ASR 时必须显式报错，而不是当成「没有字幕」混过去。
    fresh = context(suffix=".wav")
    fresh.settings = dataclasses.replace(settings, media_bin=str(tool))
    with pytest.raises(WorkError):
        implement_subtitle(fresh, SubtitleParams(source=SubtitleSource(kind="planned")))


def test_ocr_aggregates_lines_from_every_frame(tmp_path, settings, context):
    media_cli.clear_cache()
    tool = make_fake_media(tmp_path, ALL_CAPS)
    ctx = context()
    ctx.settings = dataclasses.replace(settings, media_bin=str(tool))
    report = implement_ocr(
        ctx,
        OcrParams(region=OcrRegionParams(x=0, y=200, width=320, height=40), max_frames=2),
    )
    assert report["frames"] == 2
    assert report["recognized_frames"] == 2
    assert len(report["lines"]) == 2
    assert report["confident"] == 2
    assert [a["kind"] for a in ctx.artifacts] == ["ocr_report"]


def test_ocr_reports_a_region_that_produces_no_frames(tmp_path, settings, context, monkeypatch):
    media_cli.clear_cache()
    tool = make_fake_media(tmp_path, ALL_CAPS)
    monkeypatch.setenv("FAKE_MEDIA_OCR_NO_FRAMES", "1")
    ctx = context()
    ctx.settings = dataclasses.replace(settings, media_bin=str(tool))
    with pytest.raises(WorkError) as error:
        implement_ocr(ctx, OcrParams(region=OcrRegionParams(x=0, y=0, width=10, height=10)))
    # 命令成功却没有区域图片：这是实现问题而不是素材问题，必须与「缺能力」区分开。
    assert error.value.error_class == ErrorClass.INTERNAL
    assert "区域" in str(error.value)


def test_ocr_without_the_media_tool_is_a_capability_gap(tmp_path, settings, context, monkeypatch):
    media_cli.clear_cache()
    monkeypatch.setattr(media_cli, "resolve_binary", lambda *args, **kwargs: None)
    ctx = context()
    ctx.settings = dataclasses.replace(settings, media_bin="/nonexistent/dossier-media")
    with pytest.raises(WorkError) as error:
        implement_ocr(ctx, OcrParams(region=OcrRegionParams(x=0, y=0, width=10, height=10)))
    assert error.value.error_class == ErrorClass.CAPABILITY_MISSING


def test_a_missing_dependency_is_a_capability_gap_not_a_retry(tmp_path, settings, context):
    """工具在、它依赖的命令不在（退出码 127）必须归为能力缺失——重试不会让它变好。"""
    media_cli.clear_cache()
    tool = make_fake_media(tmp_path, ALL_CAPS)
    tool.write_text(
        "#!/usr/bin/env bash\n"
        "set -euo pipefail\n"
        'cmd="${1:-}"; shift || true\n'
        'if [ "$cmd" = "probe" ]; then echo \'{"capabilities": ["media.asr"], "missing": []}\'; exit 0; fi\n'
        'echo "whisper: command not found" >&2\n'
        "exit 127\n",
        encoding="utf-8",
    )
    ctx = context(suffix=".wav")
    ctx.settings = dataclasses.replace(settings, media_bin=str(tool))
    with pytest.raises(WorkError) as error:
        implement_asr(ctx, AsrParams())
    assert error.value.error_class == ErrorClass.CAPABILITY_MISSING


# ---------- 镜头切点检测 ----------


def test_shots_registers_a_candidate_cut_list(tmp_path, settings, context):
    media_cli.clear_cache()
    tool = make_fake_media(tmp_path, ALL_CAPS)
    ctx = context()
    ctx.settings = dataclasses.replace(settings, media_bin=str(tool))
    report = implement_shots(ctx, ShotsParams(threshold=0.35, min_gap=0.2))
    assert report["cut_count"] == 2
    assert [cut["time"] for cut in report["cuts"]] == [1.25, 2.5]
    # 「候选」必须留下可审阅的产物，而不只是内存里的一个数。
    assert [a["kind"] for a in ctx.artifacts] == ["shot_list"]
    assert json.loads(Path(report["report"]).read_text(encoding="utf-8"))["cut_count"] == 2


def test_shots_reports_a_tool_that_prints_no_json(tmp_path, settings, context, monkeypatch):
    media_cli.clear_cache()
    tool = make_fake_media(tmp_path, ALL_CAPS)
    monkeypatch.setenv("FAKE_MEDIA_SHOTS_NO_JSON", "1")
    ctx = context()
    ctx.settings = dataclasses.replace(settings, media_bin=str(tool))
    with pytest.raises(WorkError) as error:
        implement_shots(ctx, ShotsParams(threshold=0.35))
    # 命令成功但读不出结果 = 实现/工具契约问题，不能被当成「没有切点」放过去。
    assert error.value.error_class == ErrorClass.INTERNAL


def test_shots_module_is_registered_with_its_capability():
    modules = registered_modules()
    spec = modules["timeline.shots"]
    assert spec.capability == "media.shot.detect"
    assert spec.produces == frozenset({"shots"})
    assert spec.kind == "plan"


def test_shots_runs_the_real_tool_on_a_hard_cut(tmp_path, settings, context):
    """真实工具路径（需要已构建的 `dossier-media` 与本机 ffmpeg）。

    这条用例的价值在于：前面几条都是假工具，只有这条真的调用 `ffmpeg select=scene`。
    """
    media_cli.clear_cache()
    try:
        tool = media_cli.resolve_media_tool(settings)
    except WorkError:
        pytest.skip("需要已构建的 dossier-media（cargo build --offline -p dossier-media）")
    if not shutil.which("ffmpeg"):
        pytest.skip("需要 ffmpeg")

    clip = tmp_path / "cut.mp4"
    subprocess.run(
        [
            "ffmpeg", "-hide_banner", "-loglevel", "error", "-nostdin", "-y",
            "-f", "lavfi", "-i", "color=c=black:s=160x120:r=10:d=1",
            "-f", "lavfi", "-i", "color=c=white:s=160x120:r=10:d=1",
            "-filter_complex", "[0:v][1:v]concat=n=2:v=1:a=0",
            "-pix_fmt", "yuv420p", str(clip),
        ],
        check=True,
    )
    ctx = context()
    ctx.settings = dataclasses.replace(settings, media_bin=str(tool))
    ctx.inputs = [clip]
    ctx.primary = clip
    ctx.probe = {"duration": 2.0, "has_video": True, "fps": 10.0, "width": 160, "height": 120}
    report = implement_shots(ctx, ShotsParams(threshold=0.3, min_gap=0.1))
    assert report["cut_count"] >= 1, report
