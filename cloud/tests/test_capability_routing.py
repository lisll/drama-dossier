"""Tier 1 / Tier 3 云侧收口：动态能力要求、整机能力探测、跨产物戳记校验入口。

三块内容对应三类「以前靠人脑记、现在靠代码判」的结论：

1. **参数相关的能力要求**：同一个模块选了模型后端就多一条 `video_inpaint_model`，
   必须在**计划阶段**拒绝，而不是领了任务跑到一半才发现起不来 worker；
2. **整机能力探测**：`video_inpaint_model` 之类的能力以前只能由部署方写环境变量声明，
   声明错了没人发现；现在由探测函数判定（配了 worker 且清单校验通过才算有）；
3. **跨产物一致性**：`drama-cloud artifacts check` 把产物戳记交给 `dossier-artifacts`，
   一致性规则不在这边重写第二份，退出码原样透传。
"""

from __future__ import annotations

import dataclasses
import json
import stat
from pathlib import Path

import pytest

from drama_cloud.cli import build_parser, cmd_artifacts_check
from drama_cloud.db import connect, now_iso
from drama_cloud.errors import ErrorClass, WorkError
from drama_cloud.modules import inpaint as inpaint_module
from drama_cloud.modules.ffmpeg import classify
from drama_cloud.plan import PlanInput, build_plan
from drama_cloud.recipes import registry
from drama_cloud.recipes.schema import validate_recipe
from drama_cloud.storage import LocalStorage
from drama_cloud.tasks import service

RECIPES_DIR = Path(__file__).resolve().parents[1] / "recipes"


#: media.probe 需要 ffprobe，任何配方都跑不掉，所以测试里的能力集都带上它。
BASE_CAPS = ["ffmpeg", "ffprobe"]


def caps(*extra: str) -> list:
    return BASE_CAPS + list(extra)


def make_executable(path: Path, body: str) -> Path:
    path.write_text(body, encoding="utf-8")
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return path


# ---------- 1. 整机能力探测 ----------


@pytest.fixture()
def extra_capabilities():
    """登记/注销整机能力都会改全局表，测试后必须还原，否则污染别的用例。"""
    registry.ensure_modules_loaded()
    saved = dict(registry._EXTRA_CAPABILITIES)
    try:
        yield registry._EXTRA_CAPABILITIES
    finally:
        registry._EXTRA_CAPABILITIES.clear()
        registry._EXTRA_CAPABILITIES.update(saved)


def test_register_capability_rejects_a_duplicate_tag(extra_capabilities):
    """两条真相比没有真相更糟：重复注册直接报错，而不是后注册的悄悄覆盖。"""
    with pytest.raises(RuntimeError):
        registry.register_capability(inpaint_module.MODEL_CAPABILITY, lambda _settings: True)


def test_registered_capability_probe_decides_availability(settings, extra_capabilities):
    registry.register_capability("test.probe.ok", lambda _settings: True)
    registry.register_capability("test.probe.gone", lambda _settings: False)

    def boom(_settings):
        raise RuntimeError("探测脚本自己挂了")

    registry.register_capability("test.probe.boom", boom)

    available = registry.worker_capabilities(settings)
    assert "test.probe.ok" in available
    # 探测为假 = 没有这个能力；探测抛异常**也**按「没有」处理（不能因为探测炸了就假装有）。
    assert "test.probe.gone" not in available
    assert "test.probe.boom" not in available


def test_model_capability_is_not_declared_without_a_worker(settings):
    """回归：`video_inpaint_model` 曾经是「有模型机器的部署方手写一行环境变量」，现在必须探测。"""
    assert "video_inpaint_model" not in registry.worker_capabilities(settings)


def test_model_capability_needs_both_a_worker_and_a_manifest(settings):
    from drama_cloud.modules import inpaint

    without_worker = dataclasses.replace(settings, inpaint_worker_program=None)
    assert inpaint.model_capability_available(without_worker) is False

    # 只配了 worker、没配清单：只能给弱保证（「配了」不等于「装齐了」）。
    weak = dataclasses.replace(settings, inpaint_worker_program="/tmp/worker.py")
    assert inpaint.model_capability_available(weak) is True


def test_model_capability_is_verified_against_the_runtime_manifest(settings, tmp_path):
    from drama_cloud.modules import inpaint

    runtime = make_executable(
        tmp_path / "dossier-runtime",
        "#!/usr/bin/env bash\n"
        "set -euo pipefail\n"
        'code=0\n'
        'if [ -f "$(dirname "$0")/exit" ]; then code="$(cat "$(dirname "$0")/exit")"; fi\n'
        'exit "$code"\n',
    )
    (tmp_path / "exit").write_text("0", encoding="utf-8")
    manifest = tmp_path / "runtime.toml"
    manifest.write_text("[[component]]\nname = \"e2fgvi\"\n", encoding="utf-8")

    configured = dataclasses.replace(
        settings,
        inpaint_worker_program="/tmp/worker.py",
        runtime_bin=str(runtime),
        runtime_manifest=str(manifest),
    )
    assert inpaint.runtime_manifest_check(configured) is True
    assert inpaint.model_capability_available(configured) is True
    assert "video_inpaint_model" in registry.worker_capabilities(configured)

    # 清单校验不过（权重被删/摘要对不上）→ 能力如实缺席，任务在计划阶段就被挡住。
    (tmp_path / "exit").write_text("1", encoding="utf-8")
    assert inpaint.runtime_manifest_check(configured) is False
    assert inpaint.model_capability_available(configured) is False
    assert "video_inpaint_model" not in registry.worker_capabilities(configured)


def test_model_capability_is_absent_when_the_runtime_tool_cannot_be_found(settings, tmp_path):
    from drama_cloud.modules import inpaint

    configured = dataclasses.replace(
        settings,
        inpaint_worker_program="/tmp/worker.py",
        runtime_bin=str(tmp_path / "no-such-dossier-runtime"),
        runtime_manifest=str(tmp_path / "runtime.toml"),
    )
    # 配了清单却找不到校验工具 = 核不了 = 不能声明能力（宁可少声明，不可假声明）。
    assert inpaint.runtime_manifest_check(configured) is False
    assert inpaint.model_capability_available(configured) is False


def test_a_missing_command_is_classified_as_a_capability_gap():
    """「命令找不到」是能力问题而不是素材问题：分类错了会触发无用的自动重试。"""
    assert classify("bash: rubberband: command not found") is ErrorClass.CAPABILITY_MISSING
    assert classify("no such file or directory: ffmpeg") is ErrorClass.INTERNAL


# ---------- 2. 参数相关的能力要求（计划阶段拦截） ----------


def erase_recipe(inpainter: str, detector: str = "fixed") -> dict:
    payload = json.loads((RECIPES_DIR / "drama-logo-erase.json").read_text(encoding="utf-8"))
    for step in payload["pipeline"]:
        if step["module"] == "video.inpaint":
            step["params"]["inpainter"] = inpainter
            step["params"]["detector"] = detector
    return payload


def video_input() -> PlanInput:
    return PlanInput(asset_id="ast_1", storage_key="assets/ast_1/x.mp4", role="video",
                     sha256="a" * 64)


def test_plan_does_not_ask_for_the_model_with_the_ffmpeg_backend():
    recipe = validate_recipe(erase_recipe("column"))
    plan = build_plan(recipe, [video_input()], caps("h264_encode", "video_inpaint"))
    assert "video_inpaint_model" not in plan.required_capabilities()


def test_plan_rejects_the_model_backend_without_the_model_capability():
    recipe = validate_recipe(erase_recipe("worker"))
    with pytest.raises(WorkError) as error:
        build_plan(recipe, [video_input()], caps("h264_encode", "video_inpaint"))
    assert error.value.error_class is ErrorClass.CAPABILITY_MISSING
    # 错误信息要指到具体的能力标签与步骤，否则排障只能靠猜。
    assert "video_inpaint_model" in str(error.value)
    assert "erase" in str(error.value)


def test_plan_rejects_the_worker_detector_too():
    """检测用模型也一样要模型能力——只卡修复后端是漏的。"""
    recipe = validate_recipe(erase_recipe("column", detector="worker"))
    with pytest.raises(WorkError) as error:
        build_plan(recipe, [video_input()], caps("h264_encode", "video_inpaint"))
    assert error.value.error_class is ErrorClass.CAPABILITY_MISSING


def test_plan_accepts_the_model_backend_when_the_capability_is_present():
    recipe = validate_recipe(erase_recipe("worker"))
    plan = build_plan(
        recipe, [video_input()],
        caps("h264_encode", "video_inpaint", "video_inpaint_model"),
    )
    assert "video_inpaint_model" in plan.required_capabilities()


# ---------- 3. 跨产物戳记校验（CLI 消费端） ----------


def make_finished_task(conn, project_id: str) -> str:
    """直接落一条终态任务行：这条用例只关心产物戳记的搬运，不该被 ffmpeg 拖住。"""
    task_id = service.new_id("tsk")
    now = now_iso()
    conn.execute(
        "INSERT INTO tasks (id, project_id, state, priority, input_asset_ids, recipe_id, "
        "recipe_version, recipe_sha256, target_json, created_at, updated_at) "
        "VALUES (?,?,?,?,?,?,?,?,?,?,?)",
        (task_id, project_id, "SUCCEEDED", 100, "[]", "test.recipe", 1, "a" * 64, "{}", now, now),
    )
    conn.commit()
    return task_id


def add_stamp(conn, storage, task_id: str, payload: dict) -> None:
    path = storage.settings.work_dir / f"{task_id}.stamp.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, ensure_ascii=False), encoding="utf-8")
    service.register_artifact(conn, storage, task_id, "artifact_stamp", path)
    conn.commit()


FAKE_ARTIFACTS = """#!/usr/bin/env bash
set -euo pipefail
code=0
if [ -f "$(dirname "$0")/exit" ]; then code="$(cat "$(dirname "$0")/exit")"; fi
echo "{\\"consistent\\": true, \\"files\\": $(($# - 1))}"
exit "$code"
"""


@pytest.fixture()
def artifacts_tool(tmp_path, monkeypatch):
    tool = make_executable(tmp_path / "dossier-artifacts", FAKE_ARTIFACTS)
    (tmp_path / "exit").write_text("0", encoding="utf-8")
    monkeypatch.setenv("DRAMA_CLOUD_ARTIFACTS_BIN", str(tool))
    return tmp_path


def run_check(settings, *task_ids: str, json_flag: bool = False):
    argv = ["--home", str(settings.home), "artifacts", "check", *task_ids]
    if json_flag:
        argv.append("--json")
    args = build_parser().parse_args(argv)
    return cmd_artifacts_check(args)


def test_artifacts_check_passes_the_stamps_to_the_tool(settings, conn, storage, project,
                                                      artifacts_tool):
    project_id, _, _ = project
    task_id = make_finished_task(conn, project_id)
    add_stamp(conn, storage, task_id, {"pipeline": "drama.logo.erase", "version": "1.0.0"})
    (artifacts_tool / "exit").write_text("0", encoding="utf-8")
    assert run_check(settings, task_id) == 0


def test_artifacts_check_propagates_the_inconsistency_exit_code(settings, conn, storage, project,
                                                                artifacts_tool):
    """工具说「不一致」就是退出码 1——CLI 不许把它翻译成「成功但带警告」。"""
    project_id, _, _ = project
    first = make_finished_task(conn, project_id)
    second = make_finished_task(conn, project_id)
    add_stamp(conn, storage, first, {"pipeline": "drama.logo.erase", "version": "1.0.0"})
    add_stamp(conn, storage, second, {"pipeline": "drama.logo.erase", "version": "1.0.0"})
    (artifacts_tool / "exit").write_text("1", encoding="utf-8")
    assert run_check(settings, first, second) == 1
    (artifacts_tool / "exit").write_text("2", encoding="utf-8")
    assert run_check(settings, first, second) == 2


def test_artifacts_check_reports_tasks_without_stamps(settings, conn, storage, project,
                                                      artifacts_tool, capsys):
    project_id, _, _ = project
    task_id = make_finished_task(conn, project_id)
    # 没有戳记 = 没什么可校验的，必须显式失败（退出码 1），不能静默返回 0。
    assert run_check(settings, task_id) == 1
    assert "没有" in capsys.readouterr().err


def test_artifacts_check_reports_a_missing_tool(settings, conn, storage, project,
                                                artifacts_tool, capsys, monkeypatch):
    project_id, _, _ = project
    task_id = make_finished_task(conn, project_id)
    add_stamp(conn, storage, task_id, {"pipeline": "drama.logo.erase", "version": "1.0.0"})
    from drama_cloud import cli

    monkeypatch.setattr(cli, "resolve_binary", lambda *args, **kwargs: None)
    assert run_check(settings, task_id) == 1
    assert "capability_missing" in capsys.readouterr().err
