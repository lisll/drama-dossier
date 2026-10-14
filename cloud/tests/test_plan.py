"""ProcessingPlan：能力检查、输入角色、计划指纹。"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from drama_cloud.errors import ErrorClass, WorkError
from drama_cloud.plan import PlanInput, build_plan, parse_version
from drama_cloud.recipes.schema import validate_recipe

RECIPES_DIR = Path(__file__).resolve().parents[1] / "recipes"


#: media.probe 需要 ffprobe，任何配方都跑不掉，所以测试里的能力集都带上它。
BASE_CAPS = ["ffmpeg", "ffprobe"]


def caps(*extra: str) -> list:
    return BASE_CAPS + list(extra)


def load(name: str) -> dict:
    return json.loads((RECIPES_DIR / name).read_text(encoding="utf-8"))


def video_input(role: str = "video", asset_id: str = "ast_1", sha256: str = "a" * 64) -> PlanInput:
    return PlanInput(asset_id=asset_id, storage_key=f"assets/{asset_id}/x.mp4", role=role, sha256=sha256)


def test_plan_expands_steps_and_orders_them():
    recipe = validate_recipe(load("drama-full-edit.json"))
    plan = build_plan(recipe, [video_input()], caps("h264_encode", "video_filters", "audio_filters"))
    assert plan.module_names() == [
        "media.probe", "timeline.user_edits", "video.quality", "audio.quality",
        "metadata.policy", "chapters.reorg", "media.encode", "media.quality_gate",
    ]
    assert plan.recipe_sha256 == recipe.content_sha256()
    assert plan.mode == "full_edit"


def test_declared_capability_shortfall_fails_at_plan_time():
    """配方自己声明的 required_capabilities 不满足时立刻失败。"""
    recipe = validate_recipe(load("drama-full-edit.json"))
    with pytest.raises(WorkError) as error:
        build_plan(recipe, [video_input()], caps("h264_encode"))
    assert error.value.error_class is ErrorClass.CAPABILITY_MISSING
    assert "缺少配方声明的能力" in error.value.message


def test_step_level_capability_is_also_checked():
    """即使配方没声明，模块自身的能力标签也会被逐个检查。"""
    payload = load("drama-no-edit-generic.json")
    payload["compatibility"]["required_capabilities"] = []
    payload["pipeline"].insert(
        1,
        {"id": "video", "module": "video.quality",
         "params": {"scale": {"height": 240}}},
    )
    recipe = validate_recipe(payload)
    with pytest.raises(WorkError) as error:
        build_plan(recipe, [video_input()], caps("h264_encode"))
    assert error.value.error_class is ErrorClass.CAPABILITY_MISSING
    assert "video.quality" in error.value.message


def test_worker_version_gate():
    payload = load("drama-no-edit-generic.json")
    payload["compatibility"]["min_worker_version"] = "99.0.0"
    recipe = validate_recipe(payload)
    with pytest.raises(WorkError) as error:
        build_plan(recipe, [video_input()], caps("h264_encode"))
    assert "最低版本" in error.value.message


def test_plan_requires_video_input():
    recipe = validate_recipe(load("drama-no-edit-generic.json"))
    audio_only = PlanInput(asset_id="ast_2", storage_key="assets/ast_2/a.mp3", role="audio")
    with pytest.raises(WorkError):
        build_plan(recipe, [audio_only], caps("h264_encode"))


def test_external_subtitle_index_must_point_at_subtitle_role():
    recipe = validate_recipe(load("drama-subtitle-burn.json"))
    inputs = [video_input(), video_input(role="video", asset_id="ast_2", sha256="b" * 64)]
    with pytest.raises(WorkError) as error:
        build_plan(recipe, inputs, caps("h264_encode", "video_filters", "subtitles"))
    assert "不是 subtitle" in error.value.message

    ok = [
        video_input(),
        PlanInput(asset_id="ast_3", storage_key="assets/ast_3/a.srt", role="subtitle", sha256="c" * 64),
    ]
    plan = build_plan(recipe, ok, caps("h264_encode", "video_filters", "subtitles"))
    assert [i.role for i in plan.inputs] == ["video", "subtitle"]


def test_timeline_input_index_is_bounds_checked():
    payload = load("drama-no-edit-generic.json")
    payload["pipeline"].insert(1, {
        "id": "timeline",
        "module": "timeline.user_edits",
        "params": {"segments": [{"input_index": 3, "start": 0.0, "end": 1.0}]},
    })
    recipe = validate_recipe(payload)
    with pytest.raises(WorkError) as error:
        build_plan(recipe, [video_input()], caps("h264_encode"))
    assert "不存在的输入素材" in error.value.message


def test_fingerprint_changes_with_inputs_and_params():
    recipe = validate_recipe(load("drama-no-edit-generic.json"))
    capabilities = caps("h264_encode")
    base = build_plan(recipe, [video_input()], capabilities)
    same = build_plan(recipe, [video_input()], capabilities)
    other_input = build_plan(recipe, [video_input(sha256="b" * 64)], capabilities)
    assert base.fingerprint() == same.fingerprint()
    assert base.fingerprint() != other_input.fingerprint()


def test_parse_version_handles_short_and_dirty_values():
    assert parse_version("1.0.0") == (1, 0, 0)
    assert parse_version("2.1") == (2, 1, 0)
    assert parse_version("1.2.3-rc1") == (1, 2, 3)
    assert parse_version("") == (0, 0, 0)
