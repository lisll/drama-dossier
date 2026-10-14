"""计划型模块单测：只校验写进 ctx.plan 的意图，不碰磁盘、不跑 ffmpeg。"""

from __future__ import annotations

import pytest

from drama_cloud.errors import ErrorClass, WorkError
from drama_cloud.modules import encode as encode_module
from drama_cloud.modules.audio import AudioQualityParams, implement_audio_quality
from drama_cloud.modules.encode import (
    EncodeParams,
    MetadataParams,
    SubtitleParams,
    check_container,
    implement_metadata,
    implement_subtitle,
)
from drama_cloud.modules.ffmpeg import MediaContext
from drama_cloud.modules.filters import (
    DeflickerParams,
    FpsConvertParams,
    VideoQualityParams,
    implement_deflicker,
    implement_fps_convert,
    implement_video_quality,
)
from drama_cloud.modules.timeline import (
    ChaptersParams,
    TimelineParams,
    implement_chapters,
    implement_timeline,
)


@pytest.fixture()
def context(tmp_path):
    work = tmp_path / "work"
    work.mkdir()
    primary = tmp_path / "in.mp4"
    primary.write_bytes(b"x")
    return MediaContext(
        settings=None,
        work_dir=work,
        primary=primary,
        inputs=[primary],
        probe={"duration": 6.0, "has_audio": True, "has_video": True, "fps": 25.0,
               "width": 1280, "height": 720},
    )


def test_video_quality_builds_filter_chain_and_records_resolution(context):
    params = VideoQualityParams.model_validate(
        {"scale": {"width": 1080, "height": 1920, "mode": "pad"},
         "color": {"saturation": 0.1}, "sharpen": 0.5, "denoise": "light"}
    )
    implement_video_quality(context, params)
    chain = context.plan["video_filters"]
    assert any("scale=1080:1920" in item for item in chain)
    assert any("pad=1080:1920" in item for item in chain)
    assert any("unsharp" in item for item in chain)
    assert any("hqdn3d" in item for item in chain)
    assert context.plan["target_resolution"]["height"] == 1920


def test_video_quality_scale_height_only_uses_even_auto_width(context):
    implement_video_quality(context, VideoQualityParams.model_validate({"scale": {"height": 1080}}))
    assert context.plan["video_filters"] == ["scale=-2:1080:force_original_aspect_ratio=decrease"]


def test_fps_and_deflicker_only_append_filters(context):
    implement_fps_convert(context, FpsConvertParams.model_validate({"fps": 30, "mode": "cfr"}))
    implement_deflicker(context, DeflickerParams.model_validate({"size": 5, "mode": "am"}))
    chain = context.plan["video_filters"]
    assert chain[0] == "fps=30.0" and "deflicker" in chain[1]


def test_timeline_defaults_to_whole_clip(context):
    result = implement_timeline(context, TimelineParams.model_validate({}))
    assert context.plan["timeline"]["segments"] == [{"input_index": 0, "start": 0.0, "end": None}]
    assert result["duration_seconds"] == 6.0
    assert context.plan["timeline"]["pending_review"] is False


def test_timeline_rejects_out_of_range_input_index(context):
    with pytest.raises(WorkError) as error:
        implement_timeline(
            context,
            TimelineParams.model_validate({"segments": [{"input_index": 5, "start": 0, "end": 1}]}),
        )
    assert error.value.error_class is ErrorClass.PARAM_OUT_OF_RANGE


def test_timeline_enforces_max_output_seconds(context):
    with pytest.raises(WorkError):
        implement_timeline(
            context,
            TimelineParams.model_validate({"segments": [], "max_output_seconds": 1.0}),
        )


def test_chapters_mode_ignores_episode_floor(context):
    result = implement_chapters(context, ChaptersParams.model_validate({"mode": "chapters"}))
    assert result["count"] == 1
    assert context.plan["chapters"]["ranges"][0]["end"] == 6.0


def test_episode_mode_merges_short_tail(context):
    context.probe["duration"] = 6.02
    result = implement_chapters(
        context,
        ChaptersParams.model_validate(
            {"mode": "episodes", "episode_seconds": 3.0, "min_segment_seconds": 1.0}
        ),
    )
    assert result["count"] == 2
    assert result["ranges"][-1]["end"] == 6.02
    assert "并入上一集" in " ".join(context.notes)


def test_episode_mode_rejects_too_short_segments(context):
    context.probe["duration"] = 2.0
    with pytest.raises(WorkError) as error:
        implement_chapters(
            context,
            ChaptersParams.model_validate(
                {"mode": "episodes", "episode_seconds": 60.0, "min_segment_seconds": 30.0}
            ),
        )
    assert "短于下限" in error.value.message


def test_metadata_policy_strips_location_fields(context):
    policy = implement_metadata(context, MetadataParams.model_validate({"mode": "edit"}))
    assert "location" in policy["remove"]
    assert "com.apple.quicktime.location.ISO6709" in policy["remove"]


def test_subtitle_external_needs_input_index(context):
    with pytest.raises(Exception):
        SubtitleParams.model_validate({"source": {"kind": "external"}})
    with pytest.raises(WorkError):
        implement_subtitle(
            context, SubtitleParams.model_validate({"source": {"kind": "external", "input_index": 9}})
        )
    plan = implement_subtitle(
        context,
        SubtitleParams.model_validate(
            {"source": {"kind": "external", "input_index": 0}, "mode": "burn"}
        ),
    )
    assert plan["mode"] == "burn" and plan["kind"] == "external"


def test_audio_quality_skips_without_audio_track(context):
    context.probe["has_audio"] = False
    result = implement_audio_quality(context, AudioQualityParams.model_validate({}))
    assert result == {"skipped": True}
    assert "跳过" in " ".join(context.notes)


@pytest.mark.parametrize(
    "container,video,audio",
    [("mp4", "h264", "aac"), ("mkv", "vp9", "opus"), ("webm", "vp9", "opus")],
)
def test_container_matrix_accepts_valid_combinations(container, video, audio):
    check_container(EncodeParams(video_codec=video, audio_codec=audio, container=container))


@pytest.mark.parametrize(
    "container,video,audio",
    [("webm", "h264", "aac"), ("mp4", "vp9", "aac"), ("webm", "vp9", "aac")],
)
def test_container_matrix_rejects_invalid_combinations(container, video, audio):
    with pytest.raises(WorkError) as error:
        check_container(EncodeParams(video_codec=video, audio_codec=audio, container=container))
    assert error.value.error_class is ErrorClass.TARGET_UNSUPPORTED


def test_bitrate_mode_requires_bitrate():
    with pytest.raises(Exception):
        EncodeParams.model_validate({"rate_control": "bitrate"})


def test_encode_refuses_unapproved_timeline(context):
    context.plan["timeline"] = {"segments": [], "pending_review": True}
    with pytest.raises(WorkError) as error:
        encode_module.implement_encode(context, EncodeParams())
    assert "尚未通过人工审核" in error.value.message
