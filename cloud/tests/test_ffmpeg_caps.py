"""FFmpeg 能力清单解析与能力路由单测。

这一层的存在理由（`docs/PROJECT_REVIEW.md` §7.1）：**工具在 ≠ 能力在**。
所以测试重点不是「解析器能跑」，而是几类会误导路由的输入：
清单里的图例行、缺分隔线的编码器清单、探测失败/非零退出、以及「缺一个滤镜就不许声明能力」。
"""

from __future__ import annotations

import dataclasses
import types
from pathlib import Path

import pytest

from drama_cloud.errors import ErrorClass, WorkError
from drama_cloud.modules import encode as encode_module
from drama_cloud.modules import ffmpeg_caps as caps
from drama_cloud.modules.encode import EncodeParams
from drama_cloud.modules.ffmpeg import MediaContext
from drama_cloud.recipes.registry import worker_capabilities

from .conftest import requires_ffmpeg

#: 真实 `ffmpeg -filters` 的前几行 + 两个真实条目（图例行与正文行混在一起）。
FILTER_SAMPLE = """Filters:
  T.. = Timeline support
  .S. = Slice threading
  ..C = Command support
  A = Audio input/output
  V = Video input/output
  N = Dynamic number and/or type of input/output
  | = Source or sink filter
 ... abench            A->A       Benchmark part of a filtergraph.
 ... trim              V->V       Pick one continuous section from the input.
 ... concat            N->N       Concatenate audio and video streams.
 ... subtitles         V->V       Render text subtitles using the libass library.
 ... atrim             A->A       Pick one continuous section from the input.
 ... setpts            V->V       Set PTS for the output video frame.
 ... asetpts           A->A       Set PTS for the output audio frame.
 ... settb             V->V       Set timebase for the video output link.
 ... asettb            A->A       Set timebase for the audio output link.
"""

#: 真实 `ffmpeg -encoders` 的表头（含分隔线）与正文。
ENCODER_SAMPLE = """Encoders:
 V..... = Video
 A..... = Audio
 S..... = Subtitle
 .F.... = Frame-level multithreading
 ..S... = Slice-level multithreading
 ...X.. = Codec is experimental
 ....B. = Supports draw_horiz_band
 .....D = Supports direct rendering method 1
 ------
 V..... a64multi             Multicolor charset for Commodore 64 (codec a64_multi)
 V..... libx264              libx264 H.264 / AVC (codec h264)
 A..... aac                  AAC (Advanced Audio Coding)
 S..... mov_text             3GPP Timed Text subtitle
"""


@pytest.fixture(autouse=True)
def _clean_cache():
    """每个用例都从空缓存开始：否则上一个用例的假清单会污染下一个。"""
    caps.clear_cache()
    yield
    caps.clear_cache()


def _patch_listing(monkeypatch, filters=None, encoders=None):
    def fake(program, flag):
        if flag == "-filters":
            return FILTER_SAMPLE if filters is None else filters
        return ENCODER_SAMPLE if encoders is None else encoders

    monkeypatch.setattr(caps, "_run_listing", fake)


# ---------- 解析器 ----------


def test_filter_parser_ignores_legend_and_keeps_real_names():
    names = caps.parse_filter_listing(FILTER_SAMPLE)
    assert {"trim", "concat", "subtitles"} <= set(names)
    for legend in ("T..", "A", "V", "|", "="):
        assert legend not in names, f"图例行被当成了滤镜名：{legend}"


def test_encoder_parser_ignores_legend_and_keeps_real_names():
    names = caps.parse_encoder_listing(ENCODER_SAMPLE)
    assert {"libx264", "aac", "mov_text", "a64multi"} <= set(names)
    # 分隔线之前的 `V..... = Video` 这类图例行不能被当成编码器。
    for legend in ("=", "V.....", "A.....", "S....."):
        assert legend not in names, f"图例行被当成了编码器名：{legend}"


def test_encoder_parser_ignores_prefix_before_the_separator_line():
    """没有分隔线时**不能**开始解析——否则表头就是一批假编码器。"""
    header_only = "\n".join(ENCODER_SAMPLE.splitlines()[:8])
    assert caps.parse_encoder_listing(header_only) == frozenset()


def test_parsers_tolerate_empty_and_garbage_input():
    for parser in (caps.parse_filter_listing, caps.parse_encoder_listing):
        assert parser("") == frozenset()
        assert parser("not a listing at all\n\n") == frozenset()


# ---------- 探测失败一律按「缺能力」处理 ----------


def test_listing_failure_is_treated_as_missing(monkeypatch, settings):
    monkeypatch.setattr(caps, "_run_listing", lambda program, flag: None)
    assert caps.requires_filters("scale")(settings) is False
    assert caps.ffmpeg_filters(settings) == frozenset()


def test_nonzero_exit_is_treated_as_missing(monkeypatch, settings):
    def boom(*_args, **_kwargs):
        return types.SimpleNamespace(returncode=1, stdout=b"ffmpeg version 4.2.4\n")

    monkeypatch.setattr(caps.subprocess, "run", boom)
    assert caps.requires_filters("scale")(settings) is False


def test_spawn_failure_is_treated_as_missing(monkeypatch, settings):
    def boom(*_args, **_kwargs):
        raise OSError("no such program")

    monkeypatch.setattr(caps.subprocess, "run", boom)
    assert caps.ffmpeg_encoders(settings) == frozenset()
    assert caps.requires_encoders("libx264")(settings) is False


def test_probes_are_false_without_settings():
    """没有部署配置时不能猜：一律按「没有这个能力」返回。"""
    assert caps.ffmpeg_filters(None) == frozenset()
    assert caps.ffmpeg_available(None) is False
    assert caps.requires_filters("scale")(None) is False


def test_probes_are_false_when_the_program_does_not_exist(settings):
    absent = dataclasses.replace(settings, ffmpeg="definitely-not-an-ffmpeg-binary")
    assert caps.ffmpeg_available(absent) is False
    assert caps.requires_filters("scale")(absent) is False
    assert caps.requires_encoders("libx264")(absent) is False


# ---------- 单个能力探测 ----------


def test_requires_filters_needs_every_named_filter(monkeypatch, settings):
    _patch_listing(monkeypatch)
    assert caps.requires_filters("trim", "concat")(settings) is True
    assert caps.requires_filters("trim", "deflicker")(settings) is False


def test_requires_encoders_needs_every_named_encoder(monkeypatch, settings):
    _patch_listing(monkeypatch)
    assert caps.requires_encoders("libx264", "aac")(settings) is True
    assert caps.requires_encoders("libx264", "libx265")(settings) is False


def test_requires_all_needs_every_condition(monkeypatch, settings):
    _patch_listing(monkeypatch)
    both = caps.requires_all(caps.requires_encoders("libx264"), caps.requires_filters("trim"))
    assert both(settings) is True
    one_missing = caps.requires_all(
        caps.requires_encoders("libx264"), caps.requires_filters("deflicker")
    )
    assert one_missing(settings) is False


def test_requires_all_rejects_an_empty_condition_list():
    """空条件会退化成「永远成立」，这类静默放行必须直接拒绝。"""
    with pytest.raises(ValueError):
        caps.requires_all()


def test_missing_helpers_name_only_the_absent_entries(monkeypatch, settings):
    _patch_listing(monkeypatch)
    assert caps.missing_filters(settings, ["trim", "deflicker"]) == ["deflicker"]
    assert caps.missing_encoders(settings, ["libx264", "libx265"]) == ["libx265"]


def test_cache_is_reused_until_cleared(monkeypatch, settings):
    calls = []

    def counting(program, flag):
        calls.append(flag)
        return FILTER_SAMPLE if flag == "-filters" else ENCODER_SAMPLE

    monkeypatch.setattr(caps, "_run_listing", counting)
    caps.ffmpeg_filters(settings)
    caps.ffmpeg_filters(settings)
    assert calls == ["-filters"]
    caps.clear_cache()
    caps.ffmpeg_filters(settings)
    assert calls == ["-filters", "-filters"]


# ---------- 能力路由：缺滤镜就不许声明能力 ----------


def test_h264_encode_capability_is_probed(monkeypatch, settings, tmp_path):
    _patch_listing(monkeypatch)
    fake = tmp_path / "ffmpeg"
    fake.write_bytes(b"stub")
    # `present()` 是 `which` 语义：不是可执行文件就当没有。
    fake.chmod(0o755)
    ready = dataclasses.replace(settings, ffmpeg=str(fake))
    assert "h264_encode" in worker_capabilities(ready)

    # 基础滤镜少一个（concat 被裁掉）→ 能力如实缺席，而不是领了任务再失败。
    trimmed = FILTER_SAMPLE.replace(
        " ... concat            N->N       Concatenate audio and video streams.\n", ""
    )
    _patch_listing(monkeypatch, filters=trimmed)
    # 上一步已经按「程序名+开关」缓存过清单，换了清单就必须清缓存（真实场景同理：
    # 只有部署换了 ffmpeg 才需要，测试里是换了假的解析结果）。
    caps.clear_cache()
    assert "h264_encode" not in worker_capabilities(ready)


def test_h264_encode_is_absent_when_ffmpeg_is_missing(settings):
    absent = dataclasses.replace(settings, ffmpeg="definitely-not-an-ffmpeg-binary")
    capabilities = worker_capabilities(absent)
    assert "h264_encode" not in capabilities
    assert "ffmpeg" not in capabilities


@pytest.mark.parametrize("bad", ["tmix,rotate", "eq hue", "", "a\tb"])
def test_probe_builders_reject_joined_or_blank_names(bad):
    """曾把多个滤镜名用逗号拼成一个参数：整串永远查不到，模块永远不派发且没有报错。"""
    with pytest.raises(ValueError):
        caps.requires_filters(bad)
    with pytest.raises(ValueError):
        caps.requires_encoders(bad)
    with pytest.raises(ValueError):
        caps.requires_filters()


def test_dedup_and_overlay_modules_follow_the_real_listing(monkeypatch, settings):
    """滤镜齐全时去重/角标模块要能被派发；缺了要如实缺席。"""
    full = FILTER_SAMPLE + "".join(
        f" ... {name:<17} V->V       sample\n"
        for name in [
            "tmix", "rotate", "noise", "unsharp", "eq", "hue",
            "asetrate", "atempo", "equalizer", "acompressor", "overlay",
        ]
    )
    _patch_listing(monkeypatch, filters=full)
    from drama_cloud.recipes.registry import ensure_modules_loaded, registered_modules

    ensure_modules_loaded()
    modules = registered_modules()
    for name in ["video.dedup", "video.dedup_finish", "audio.dedup", "overlay.badge", "overlay.watermark"]:
        assert modules[name].capability_probe(settings) is True, name
    assert modules["metadata.device_pool"].capability_probe(settings) is True

    caps.clear_cache()
    _patch_listing(monkeypatch, filters=FILTER_SAMPLE)
    for name in ["video.dedup", "overlay.badge"]:
        assert modules[name].capability_probe(settings) is False, name


def test_capability_probe_names_are_stable_for_diagnostics(settings):
    """探测函数的名字会出现在 Worker 自省输出里，所以它要能说明「缺什么条件」。"""
    probe = caps.requires_all(
        caps.requires_encoders("libx264"), caps.requires_filters("trim", "concat")
    )
    assert "libx264" in probe.__name__ and "trim" in probe.__name__


# ---------- 编码前的编码器预检（参数决定的那部分） ----------


def _context(settings, tmp_path):
    work = tmp_path / "work"
    work.mkdir(exist_ok=True)
    primary = tmp_path / "in.mp4"
    primary.write_bytes(b"x")
    return MediaContext(
        settings=settings,
        work_dir=work,
        primary=primary,
        inputs=[primary],
        probe={"duration": 6.0, "has_audio": True, "has_video": True, "fps": 25.0},
    )


def test_encoder_preflight_passes_when_every_needed_encoder_exists(monkeypatch, settings, tmp_path):
    _patch_listing(monkeypatch)
    encode_module.ensure_encoders(_context(settings, tmp_path), EncodeParams(), None)


def test_encoder_preflight_reports_the_encoder_of_the_chosen_codec(
    monkeypatch, settings, tmp_path
):
    """h265 是配方选的，能力探测钉不住它，所以必须在编码前逐次核对并说清缺哪个。"""
    _patch_listing(monkeypatch)
    params = EncodeParams(video_codec="h265")
    with pytest.raises(WorkError) as error:
        encode_module.ensure_encoders(_context(settings, tmp_path), params, None)
    assert error.value.error_class is ErrorClass.CAPABILITY_MISSING
    assert "libx265" in error.value.message


def test_encoder_preflight_covers_the_audio_and_subtitle_encoders(monkeypatch, settings, tmp_path):
    slim = ENCODER_SAMPLE.replace(
        " A..... aac                  AAC (Advanced Audio Coding)\n", ""
    ).replace(" S..... mov_text             3GPP Timed Text subtitle\n", "")
    _patch_listing(monkeypatch, encoders=slim)
    with pytest.raises(WorkError) as error:
        encode_module.ensure_encoders(_context(settings, tmp_path), EncodeParams(), "mov_text")
    assert "aac" in error.value.message and "mov_text" in error.value.message


def test_encode_stops_before_running_ffmpeg_when_an_encoder_is_absent(
    monkeypatch, settings, tmp_path
):
    """回归：缺编码器要在**开始编码之前**以 capability_missing 失败。"""
    _patch_listing(monkeypatch)
    context = _context(settings, tmp_path)
    context.plan["timeline"] = {"segments": [{"input_index": 0, "start": 0.0, "end": None}]}
    with pytest.raises(WorkError) as error:
        encode_module.implement_encode(
            context, EncodeParams(video_codec="vp9", container="webm", audio_codec="opus")
        )
    assert error.value.error_class is ErrorClass.CAPABILITY_MISSING
    assert "libvpx-vp9" in error.value.message


# ---------- 真实 ffmpeg 的冒烟测试 ----------


@requires_ffmpeg
def test_real_ffmpeg_supports_the_probed_capabilities(settings):
    """这台机器上模块声明的滤镜/编码器必须真的在——否则声明就是假的。"""
    assert caps.ffmpeg_available(settings) is True
    required_filters = (
        "scale", "eq", "unsharp", "hqdn3d", "fps", "deflicker", "signalstats",
        "metadata", "loudnorm", "equalizer", "acompressor", "dynaudnorm", "aformat",
        "subtitles", "trim", "atrim", "setpts", "asetpts", "settb", "asettb", "concat",
    )
    assert caps.missing_filters(settings, required_filters) == []
    required_encoders = (
        "libx264", "libx265", "libvpx-vp9", "libaom-av1", "aac", "libmp3lame",
        "libopus", "mov_text", "srt",
    )
    assert caps.missing_encoders(settings, required_encoders) == []
