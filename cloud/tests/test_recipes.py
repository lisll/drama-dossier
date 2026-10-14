"""配方 Schema、白名单、依赖闭包与版本策略。"""

from __future__ import annotations

import copy
import json
from pathlib import Path

import pytest

from drama_cloud.errors import ErrorClass, WorkError
from drama_cloud.recipes.schema import SCHEMA_VERSION, validate_recipe
from drama_cloud.tasks import service

RECIPES_DIR = Path(__file__).resolve().parents[1] / "recipes"


def base_recipe() -> dict:
    return {
        "schema_version": SCHEMA_VERSION,
        "recipe_id": "test.recipe",
        "version": 1,
        "target": {
            "platform_profile": "test",
            "container": "mp4",
            "video_codec": "h264",
            "audio_codec": "aac",
        },
        "compatibility": {"min_worker_version": "1.0.0", "required_capabilities": ["h264_encode"]},
        "preset": {"mode": "no_edit"},
        "pipeline": [
            {"id": "probe", "module": "media.probe", "params": {}},
            {"id": "encode", "module": "media.encode", "params": {}},
            {"id": "gate", "module": "media.quality_gate", "params": {}},
        ],
    }


def test_builtin_recipes_all_validate():
    files = sorted(RECIPES_DIR.glob("*.json"))
    assert files, "内置配方目录不应为空"
    for path in files:
        payload = json.loads(path.read_text(encoding="utf-8"))
        recipe = validate_recipe(payload)
        assert recipe.recipe_id
        assert recipe.pipeline[0].module == "media.probe"


def test_unknown_module_is_rejected():
    payload = base_recipe()
    payload["pipeline"].insert(1, {"id": "evil", "module": "shell.exec", "params": {}})
    with pytest.raises(WorkError) as error:
        validate_recipe(payload)
    assert error.value.error_class is ErrorClass.RECIPE_SCHEMA_INVALID
    assert "白名单" in error.value.message


def test_unknown_param_field_is_rejected():
    payload = base_recipe()
    payload["pipeline"][1]["params"] = {"command": "rm -rf /"}
    with pytest.raises(WorkError) as error:
        validate_recipe(payload)
    assert error.value.error_class is ErrorClass.RECIPE_SCHEMA_INVALID


def test_param_range_and_container_matrix_are_enforced_at_runtime():
    payload = base_recipe()
    payload["target"]["container"] = "webm"
    payload["target"]["video_codec"] = "vp9"
    payload["target"]["audio_codec"] = "opus"
    payload["pipeline"][1]["params"] = {
        "container": "webm", "video_codec": "vp9", "audio_codec": "opus", "crf": 99,
    }
    with pytest.raises(WorkError):
        validate_recipe(payload)


def test_dependency_closure_is_enforced():
    payload = base_recipe()
    # 去掉 probe，后面的 media.encode 就缺 video 角色了。
    payload["pipeline"] = [step for step in payload["pipeline"] if step["module"] != "media.probe"]
    with pytest.raises(WorkError) as error:
        validate_recipe(payload)
    assert "第一步必须是 media.probe" in error.value.message


def test_last_step_must_be_quality_gate_when_required():
    payload = base_recipe()
    payload["pipeline"] = payload["pipeline"][:-1]
    with pytest.raises(WorkError) as error:
        validate_recipe(payload)
    assert "最后一步必须是 media.quality_gate" in error.value.message


def test_subtitle_mode_requires_subtitle_step():
    payload = base_recipe()
    payload["preset"]["mode"] = "subtitle"
    with pytest.raises(WorkError) as error:
        validate_recipe(payload)
    assert "subtitle.mux" in error.value.message


def test_target_must_match_encode_params():
    payload = base_recipe()
    payload["target"]["video_codec"] = "h265"
    with pytest.raises(WorkError) as error:
        validate_recipe(payload)
    assert "与 target 不一致" in error.value.message


def test_declared_resolution_requires_scale_step():
    payload = base_recipe()
    payload["target"]["width"] = 1080
    payload["target"]["height"] = 1920
    with pytest.raises(WorkError) as error:
        validate_recipe(payload)
    assert "分辨率不会自动生效" in error.value.message


def test_duplicate_step_ids_are_rejected():
    payload = base_recipe()
    payload["pipeline"].insert(1, {"id": "probe", "module": "media.probe", "params": {}})
    with pytest.raises(WorkError) as error:
        validate_recipe(payload)
    assert "重复" in error.value.message


def test_content_hash_ignores_status():
    payload = base_recipe()
    published = validate_recipe({**payload, "status": "published"})
    draft = validate_recipe({**payload, "status": "draft"})
    assert published.content_sha256() == draft.content_sha256()


# ---------- 版本策略 ----------


def test_published_version_is_immutable(conn, seeded):
    payload = json.loads((RECIPES_DIR / "drama-no-edit-generic.json").read_text(encoding="utf-8"))
    with pytest.raises(WorkError) as error:
        service.publish_recipe(conn, payload, actor="test")
    assert "不可修改" in error.value.message


def test_new_version_and_rollback(conn, seeded):
    payload = json.loads((RECIPES_DIR / "drama-no-edit-generic.json").read_text(encoding="utf-8"))
    bumped = copy.deepcopy(payload)
    bumped["version"] = 2
    bumped["pipeline"][1]["params"]["crf"] = 26
    service.publish_recipe(conn, bumped, actor="test")

    resolved = service.resolve_recipe(conn, "drama.no_edit.generic")
    assert resolved["version"] == 2

    service.rollback_to(conn, "drama.no_edit.generic", 1, actor="test")
    assert service.resolve_recipe(conn, "drama.no_edit.generic")["version"] == 1


def test_revoke_falls_back_to_previous_published_version(conn, seeded):
    payload = json.loads((RECIPES_DIR / "drama-no-edit-generic.json").read_text(encoding="utf-8"))
    bumped = copy.deepcopy(payload)
    bumped["version"] = 2
    service.publish_recipe(conn, bumped, actor="test")
    service.set_recipe_status(conn, "drama.no_edit.generic", 2, "revoked", actor="test")
    assert service.resolve_recipe(conn, "drama.no_edit.generic")["version"] == 1
    with pytest.raises(WorkError):
        service.resolve_recipe(conn, "drama.no_edit.generic", 2)


def test_revoked_version_cannot_be_created(conn, seeded):
    service.set_recipe_status(conn, "drama.no_edit.generic", 1, "revoked", actor="test")
    with pytest.raises(WorkError) as error:
        service.resolve_recipe(conn, "drama.no_edit.generic", 1)
    assert "已revoked" in error.value.message or "已 revoked" in error.value.message


def test_rollout_percent_gates_creation(conn, seeded):
    payload = json.loads((RECIPES_DIR / "drama-no-edit-generic.json").read_text(encoding="utf-8"))
    payload["version"] = 2
    service.publish_recipe(conn, payload, actor="test", rollout_percent=10)
    assert service.resolve_recipe(conn, "drama.no_edit.generic", 2, rollout_bucket=5)["version"] == 2
    with pytest.raises(WorkError):
        service.resolve_recipe(conn, "drama.no_edit.generic", 2, rollout_bucket=99)
