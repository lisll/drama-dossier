"""擦除模块与确认流：参数加力（分支A）、闸门裁决、以及用假工具跑通的自动重试/转人工。"""

from __future__ import annotations

import dataclasses
import json
import stat
from pathlib import Path

import pytest

from drama_cloud.errors import ErrorClass, WorkError
from drama_cloud.modules.ffmpeg import MediaContext
from drama_cloud.modules.inpaint import (
    EraseVerifyParams,
    InpaintParams,
    build_args,
    implement_erase_verify,
    implement_inpaint,
    judge,
    plan_escalation,
    resolve_tool,
    tool_flags,
)
from drama_cloud.tasks import review as review_service
from drama_cloud.tasks import service
from drama_cloud.tasks.runner import Worker

from .conftest import requires_ffmpeg

#: 假工具：把输入原样拷成输出，并按 `--dilate` 与 sidecar `mode` 文件决定结论。
#: 这样「自动重试加大 mask 之后就通过」这条链路不需要 GPU、模型或真实素材就能测。
FAKE_TOOL = """#!/usr/bin/env bash
set -euo pipefail
mode="strict"
if [ -f "$(dirname "$0")/mode" ]; then mode="$(cat "$(dirname "$0")/mode")"; fi
dilate=0
while [ $# -gt 0 ]; do
  case "$1" in
    --input) input="$2"; shift 2;;
    --output) output="$2"; shift 2;;
    --report-json) report="$2"; shift 2;;
    --dilate) dilate="$2"; shift 2;;
    *) shift;;
  esac
done
cp "$input" "$output"
if [ "$mode" = "noreport" ]; then
  exit 0
fi
verdict=pass
ratio=0.000
code=0
if [ "$mode" = "always" ]; then
  verdict=fail; ratio=0.600; code=2
elif [ "$mode" = "warn" ]; then
  verdict=warn; ratio=0.030; code=3
elif [ "$dilate" -lt 6 ]; then
  verdict=fail; ratio=0.400; code=2
fi
cat > "$report" <<JSON
{"framesIn": 20, "framesOut": 20, "regions": [{"x": 0, "y": 0, "width": 64, "height": 48}],
 "clips": 1, "inpaintedClips": 1, "meanMaskCoverage": 0.08,
 "verification": {"applicable": true, "framesChecked": 20, "maskedPixels": 3072,
  "roi": {"x": 0, "y": 0, "width": 64, "height": 48},
  "residual": {"checked": true, "framesWithResidual": 0, "meanRatio": ${ratio},
               "maxRatio": ${ratio}, "worstFrame": 0, "maxBoxes": 1},
  "flicker": {"samples": 19, "framesOverFail": 0, "meanIndex": 0.1, "maxIndex": 0.2,
              "meanInsideDiff": 0.1, "meanOutsideDiff": 1.0},
  "seam": {"samples": 19, "framesOverFail": 0, "meanScore": 0.1, "maxScore": 0.2,
           "meanBoundaryGrad": 1.0, "meanReferenceGrad": 1.0},
  "consistency": {"samples": 19, "framesBelowFail": 0, "meanPsnrDb": 40.0, "minPsnrDb": 35.0},
  "ssim": {"samples": 19, "framesBelowFail": 0, "meanSsim": 0.97, "minSsim": 0.94},
  "verdict": "${verdict}", "issues": [], "notes": []}}
JSON
echo "fake inpaint: dilate=$dilate verdict=$verdict"
exit $code
"""


def make_help_tool(directory: Path, flags: str = "") -> Path:
    """造一个只回 `--help` 的假工具，用来验证「按能力拼参数」的探测逻辑。"""
    script = directory / "help-inpaint"
    script.write_text(f"#!/usr/bin/env bash\necho '{flags}'\n", encoding="utf-8")
    script.chmod(script.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return script


def make_fake_tool(directory: Path, mode: str = "strict") -> Path:
    script = directory / "fake-inpaint"
    script.write_text(FAKE_TOOL, encoding="utf-8")
    script.chmod(script.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    (directory / "mode").write_text(mode, encoding="utf-8")
    return script


@pytest.fixture()
def context(tmp_path, settings):
    work = tmp_path / "work"
    work.mkdir()
    primary = tmp_path / "in.mp4"
    primary.write_bytes(b"x")
    return MediaContext(
        settings=settings,
        work_dir=work,
        primary=primary,
        inputs=[primary],
        probe={"duration": 2.0, "has_audio": True, "has_video": True,
               "fps": 10.0, "width": 160, "height": 120},
    )


def verify_block(**overrides) -> dict:
    """一份最小可用的校验段落（对应 Rust 侧 `verification` 的 camelCase 字段）。"""
    block = {
        "applicable": True,
        "framesChecked": 20,
        "maskedPixels": 3072,
        "residual": {"checked": True, "maxRatio": 0.0, "meanRatio": 0.0,
                     "framesWithResidual": 0, "worstFrame": None, "maxBoxes": 0},
        "flicker": {"samples": 19, "framesOverFail": 0, "meanIndex": 0.1, "maxIndex": 0.2,
                    "meanInsideDiff": 0.1, "meanOutsideDiff": 1.0},
        "seam": {"samples": 19, "framesOverFail": 0, "meanScore": 0.1, "maxScore": 0.2,
                 "meanBoundaryGrad": 1.0, "meanReferenceGrad": 1.0},
        "consistency": {"samples": 19, "framesBelowFail": 0, "meanPsnrDb": 40.0,
                        "minPsnrDb": 35.0},
        "ssim": {"samples": 19, "framesBelowFail": 0, "meanSsim": 0.97, "minSsim": 0.94},
        "verdict": "pass",
        "issues": [],
        "notes": [],
    }
    block.update(overrides)
    return block


# ---------- 分支A：参数加力 ----------


def test_escalation_keeps_first_attempt_and_boosts_on_retry():
    params = InpaintParams.model_validate({"regions": [{"x": 0, "y": 0, "width": 10, "height": 10}]})
    first = plan_escalation(params, 1, None)
    assert (first.dilate, first.feather, first.inpainter) == (
        params.dilate, params.feather, params.inpainter
    )
    assert first.notes == []

    second = plan_escalation(params, 2, None)
    assert second.dilate == params.dilate + 2
    assert second.feather == params.feather + 1
    assert second.context > params.context
    assert second.notes

    # 关掉加力时重跑不会改任何参数（对照：自动重试必须换参数才有意义）。
    off = plan_escalation(params.model_copy(update={"escalation": "off"}), 3, None)
    assert (off.dilate, off.feather, off.context) == (
        params.dilate, params.feather, params.context
    )


def test_escalation_switches_backend_only_when_worker_is_configured():
    params = InpaintParams.model_validate(
        {"regions": [{"x": 0, "y": 0, "width": 10, "height": 10}], "escalation": "mask+backend"}
    )
    with_worker = plan_escalation(params, 3, {"program": "python3", "args": ["worker.py"]})
    assert with_worker.inpainter == "worker"
    without_worker = plan_escalation(params, 3, None)
    assert without_worker.inpainter == "column"
    assert any("未配置" in note for note in without_worker.notes)


def test_build_args_maps_params_to_cli(context):
    params = InpaintParams.model_validate(
        {
            "regions": [{"x": 1, "y": 2, "width": 3, "height": 4},
                        {"x": 9, "y": 9, "width": 5, "height": 5}],
            "start": 1.5,
            "duration": 2.0,
            "dilate": 6,
            "feather": 3,
        }
    )
    escalation = plan_escalation(params, 1, None)
    args = build_args(
        context, params, escalation,
        source=context.primary, output=context.work_dir / "out.mp4",
        report_path=context.work_dir / "report.json", tool=Path("/bin/true"), worker=None,
    )
    assert args[:2] == ["/bin/true", "--input"]
    assert "--roi" in args and "1,2,3,4" in args and "9,9,5,5" in args
    assert "--dilate" in args and "6" in args
    assert "--start" in args and "--duration" in args


def test_build_args_requires_deployment_worker_config(context):
    params = InpaintParams.model_validate({"detector": "worker"})
    escalation = plan_escalation(params, 1, None)
    with pytest.raises(WorkError) as error:
        build_args(
            context, params, escalation,
            source=context.primary, output=context.work_dir / "out.mp4",
            report_path=context.work_dir / "report.json", tool=Path("/bin/true"), worker=None,
        )
    assert error.value.error_class is ErrorClass.CAPABILITY_MISSING


def test_resolve_tool_prefers_deployment_configuration(settings, tmp_path, monkeypatch):
    fake = make_fake_tool(tmp_path)
    configured = dataclasses.replace(settings, inpaint_bin=str(fake))
    assert resolve_tool(configured) == fake

    from drama_cloud.modules import inpaint as module

    monkeypatch.setattr(module.shutil, "which", lambda name: None)
    monkeypatch.setattr(module, "repo_root", lambda: tmp_path / "no-such-repo")
    with pytest.raises(WorkError) as error:
        resolve_tool(dataclasses.replace(settings, inpaint_bin=None))
    assert error.value.error_class is ErrorClass.CAPABILITY_MISSING


# ---------- 确认流闸门 ----------


def gate_context(context, **verification):
    context.plan["inpaint"] = {
        "attempt": 1,
        "verdict": verification.pop("tool_verdict", "pass"),
        "regions": [],
        "report": {"verification": verify_block(**verification)},
    }
    return context


def test_erase_verify_passes_when_metrics_are_clean(context):
    params = EraseVerifyParams()
    report = implement_erase_verify(gate_context(context), params)
    assert report["verdict"] == "pass"
    assert context.plan.get("review") is None


def test_erase_verify_fail_is_retryable(context):
    params = EraseVerifyParams()
    with pytest.raises(WorkError) as error:
        implement_erase_verify(gate_context(context, residual={"checked": True, "maxRatio": 0.6}),
                               params)
    assert error.value.error_class is ErrorClass.QUALITY_RETRYABLE


def test_erase_verify_warn_requires_human_spot_check(context):
    params = EraseVerifyParams()
    report = implement_erase_verify(
        gate_context(context, residual={"checked": True, "maxRatio": 0.05}), params
    )
    assert report["verdict"] == "warn"
    assert context.plan["review"]["required"] is True
    assert context.plan["review"]["severity"] == "warn"


def test_erase_verify_warn_can_be_refused_by_policy(context):
    params = EraseVerifyParams(accept_warn=False)
    with pytest.raises(WorkError) as error:
        implement_erase_verify(
            gate_context(context, residual={"checked": True, "maxRatio": 0.05}), params
        )
    assert error.value.error_class is ErrorClass.QUALITY_GATE_FAILED


def test_erase_verify_requires_content_check_by_default(context):
    params = EraseVerifyParams()
    report = implement_erase_verify(gate_context(context, residual={"checked": False}), params)
    assert report["verdict"] == "warn"

    lenient = EraseVerifyParams(require_content_check=False)
    assert implement_erase_verify(
        gate_context(context, residual={"checked": False}), lenient
    )["verdict"] == "pass"


def test_erase_verify_rejects_missing_report(context):
    with pytest.raises(WorkError) as error:
        implement_erase_verify(context, EraseVerifyParams())
    assert error.value.error_class is ErrorClass.PARAM_OUT_OF_RANGE


def test_judge_checks_ssim_direction():
    # SSIM 越大越好：结构被抹平时最差帧 SSIM 会掉到失败阈值以下。
    verification = verify_block(ssim={"samples": 19, "framesBelowFail": 4,
                                      "meanSsim": 0.62, "minSsim": 0.40})
    verdict, checks, _ = judge(verification, EraseVerifyParams())
    assert verdict == "fail"
    check = next(c for c in checks if c["name"] == "erase_ssim")
    assert check["level"] == "fail" and check["value"] == pytest.approx(0.40)

    warned = verify_block(ssim={"samples": 19, "framesBelowFail": 0,
                                "meanSsim": 0.80, "minSsim": 0.70})
    assert judge(warned, EraseVerifyParams())[0] == "warn"


def test_build_args_forwards_ssim_thresholds(context, tmp_path):
    params = InpaintParams.model_validate({
        "regions": [{"x": 0, "y": 0, "width": 8, "height": 8}],
        "ssim_warn": 0.9, "ssim_fail": 0.6,
    })
    tool = make_help_tool(tmp_path, "--verify-ssim-warn --verify-ssim-fail")
    args = build_args(
        context, params, plan_escalation(params, 1, None),
        source=context.primary, output=tmp_path / "out.mp4",
        report_path=tmp_path / "r.json", tool=tool, worker=None,
    )
    assert "--verify-ssim-warn" in args and "0.9000" in args
    assert "--verify-ssim-fail" in args and "0.6000" in args


def test_tool_flags_reads_long_flags_from_help(tmp_path):
    tool = make_help_tool(tmp_path, "--input --verify-ssim-warn --verify-seam-warn -v -q")
    flags = tool_flags(tool)
    assert {"--input", "--verify-ssim-warn", "--verify-seam-warn"}.issubset(flags)
    # 单个连字符的短开关不是我们关心的形态（长开关才是稳定契约）。
    assert "-v" not in flags and "-q" not in flags
    # 探测结果按「路径 + 大小 + mtime」缓存：同一份文件不会重复起进程。
    assert tool_flags(tool) == flags


def test_tool_flags_degrades_to_empty_for_unreadable_tool(tmp_path):
    assert tool_flags(tmp_path / "no-such-tool") == frozenset()


def test_build_args_drops_ssim_flags_the_tool_does_not_know(context, tmp_path):
    """旧版工具（不认识 SSIM 开关）也必须能跑通：少传参数，而不是解析失败。"""
    params = InpaintParams.model_validate({
        "regions": [{"x": 0, "y": 0, "width": 8, "height": 8}],
    })
    old_tool = make_help_tool(tmp_path, "--input --output --report-json")
    args = build_args(
        context, params, plan_escalation(params, 1, None),
        source=context.primary, output=tmp_path / "out.mp4",
        report_path=tmp_path / "r.json", tool=old_tool, worker=None,
    )
    assert "--verify-ssim-warn" not in args
    assert "--verify-ssim-fail" not in args
    # 其它参数照常拼出来，降级只影响这一个可选维度。
    assert "--input" in args and "--dilate" in args


def test_judge_checks_consistency_direction():
    verification = verify_block(consistency={"samples": 19, "framesBelowFail": 3,
                                             "meanPsnrDb": 22.0, "minPsnrDb": 20.0})
    verdict, checks, _ = judge(verification, EraseVerifyParams())
    assert verdict == "fail"
    assert any(check["name"] == "erase_consistency" and check["level"] == "fail"
               for check in checks)


# ---------- 与工具的真实调用契约（假可执行文件） ----------


def test_implement_inpaint_reports_missing_log(context, settings, tmp_path):
    fake = make_fake_tool(tmp_path, mode="noreport")
    context.settings = dataclasses.replace(settings, inpaint_bin=str(fake))
    params = InpaintParams.model_validate({"regions": [{"x": 0, "y": 0, "width": 8, "height": 8}]})
    with pytest.raises(WorkError) as error:
        implement_inpaint(context, params)
    assert error.value.error_class is ErrorClass.INTERNAL


# ---------- 端到端：自动重试 → 通过 / 用尽 → 转人工 ----------


def _prepare_task(conn, settings, storage, project, tiny_video, recipe_id="drama.logo.erase"):
    project_id, _, _ = project
    video = service.register_local_asset(
        conn, settings, storage, project_id, tiny_video, name="正片", actor="test"
    )
    return service.create_task(
        conn, settings, project_id, [video["id"]], recipe_id=recipe_id, actor="test"
    )


@requires_ffmpeg
def test_quality_retry_escalates_then_succeeds(conn, settings, storage, project, tiny_video, tmp_path):
    tool = make_fake_tool(tmp_path, mode="strict")
    worker_settings = dataclasses.replace(settings, inpaint_bin=str(tool))
    task = _prepare_task(conn, settings, storage, project, tiny_video)

    assert Worker(worker_settings, worker_id="wrk_a").run_once() is True
    detail = service.get_task(conn, task["id"])
    assert detail["state"] == "RETRY_WAITING"
    assert detail["error"]["class"] == str(ErrorClass.QUALITY_RETRYABLE)
    # 分支A：质量类重试会换参数重算，上一轮的编码检查点必须失效。
    assert service.get_checkpoints(conn, task["id"]) == []
    # 还有重试额度时不进人工队列。
    assert review_service.list_reviews(conn, task_id=task["id"]) == []

    conn.execute("UPDATE tasks SET next_run_at = NULL WHERE id = ?", (task["id"],))
    assert Worker(worker_settings, worker_id="wrk_b").run_once() is True
    detail = service.get_task(conn, task["id"])
    assert detail["state"] == "SUCCEEDED", detail["error"]
    assert detail["attempts"] == 2
    assert all(step["status"] == "executed" for step in detail["result"]["steps"])
    kinds = {artifact["kind"] for artifact in detail["artifacts"]}
    assert "inpaint_report" in kinds and "output" in kinds
    assert detail["review"] is None

    # 处理日志里保留了这次加力后的实际参数，便于事后复盘。
    report = next(a for a in detail["artifacts"] if a["kind"] == "inpaint_report")
    assert report["media"]["verdict"] == "pass"


@requires_ffmpeg
def test_exhausted_quality_retry_goes_to_review_queue(conn, settings, storage, project, tiny_video, tmp_path):
    tool = make_fake_tool(tmp_path, mode="always")
    worker_settings = dataclasses.replace(settings, inpaint_bin=str(tool))
    task = _prepare_task(conn, settings, storage, project, tiny_video)

    for index in range(3):
        assert Worker(worker_settings, worker_id=f"wrk_{index}").run_once() is True
        conn.execute("UPDATE tasks SET next_run_at = NULL WHERE id = ?", (task["id"],))

    detail = service.get_task(conn, task["id"])
    assert detail["state"] == "FAILED"
    assert detail["attempts"] == 3
    assert detail["error"]["class"] == str(ErrorClass.QUALITY_RETRYABLE)
    # 失败的任务不产出成品，但最后一遍擦除结果会留成候选产物：人工复核需要一个
    # 能看的实物，否则「标记通过」无从谈起。
    kinds = {artifact["kind"] for artifact in detail["artifacts"]}
    assert "inpaint_candidate" in kinds
    candidate = next(a for a in detail["artifacts"] if a["kind"] == "inpaint_candidate")
    assert candidate["quality"]["auto_verified"] is False

    pending = review_service.list_reviews(conn, status="pending", task_id=task["id"])
    assert len(pending) == 1
    item = pending[0]
    assert item["reason"] == review_service.REASON_AUTO_FAILED
    assert item["severity"] == "high"
    assert item["artifact_id"] == candidate["id"]
    assert item["detail"]["inpaint"]["summary"]["verdict"] == "fail"
    assert item["detail"]["candidate_unverified"] is True
    assert item["detail"]["suggestions"]

    # 候选产物没有通过自动校验，但人工可以选择放行——放行是人的决定，不是系统默认。
    decided = review_service.decide(
        conn, item["id"], "approve", note="合成素材可接受", actor="test"
    )
    assert decided["status"] == "approved"


@requires_ffmpeg
def test_warn_verdict_produces_output_and_review_item(conn, settings, storage, project, tiny_video, tmp_path):
    tool = make_fake_tool(tmp_path, mode="warn")
    worker_settings = dataclasses.replace(settings, inpaint_bin=str(tool))
    task = _prepare_task(conn, settings, storage, project, tiny_video)

    assert Worker(worker_settings, worker_id="wrk_w").run_once() is True
    detail = service.get_task(conn, task["id"])
    assert detail["state"] == "SUCCEEDED", detail["error"]
    assert detail["review"]["pending"] == 1
    item = review_service.list_reviews(conn, status="pending", task_id=task["id"])[0]
    assert item["reason"] == review_service.REASON_WARN
    assert item["artifact_id"], "warn 放行时必须能定位到待抽查的产物"
    assert review_service.decide(conn, item["id"], "approve", note="人工看过，可交付",
                                 actor="reviewer")["status"] == "approved"


# ---------- C2：能力路由（部署条件决定 Worker 能力清单） ----------


def test_inpaint_capability_is_probed_from_the_deployment(monkeypatch, settings, tmp_path):
    from drama_cloud.modules import inpaint
    from drama_cloud.recipes.registry import worker_capabilities

    fake = make_fake_tool(tmp_path)
    ready = dataclasses.replace(settings, inpaint_bin=str(fake))
    assert inpaint.capability_available(ready) is True
    assert "video_inpaint" in worker_capabilities(ready)

    def missing(_settings):
        raise WorkError(ErrorClass.CAPABILITY_MISSING, "本机未安装擦除工具")

    monkeypatch.setattr(inpaint, "resolve_tool", missing)
    assert inpaint.capability_available(ready) is False
    assert "video_inpaint" not in worker_capabilities(ready)


def test_inpaint_task_is_rejected_at_plan_time_without_the_tool(monkeypatch, settings, tmp_path):
    """C2 的要点：缺工具时任务在「计划阶段」就被拒，而不是领了任务跑一半才失败。"""
    from drama_cloud.modules import inpaint
    from drama_cloud.plan import PlanInput, build_plan
    from drama_cloud.recipes.registry import worker_capabilities
    from drama_cloud.recipes.schema import validate_recipe

    def missing(_settings):
        raise WorkError(ErrorClass.CAPABILITY_MISSING, "本机未安装擦除工具")

    monkeypatch.setattr(inpaint, "resolve_tool", missing)
    available = worker_capabilities(settings)
    assert "video_inpaint" not in available

    recipe = validate_recipe(json.loads(
        (Path(__file__).resolve().parents[1] / "recipes" / "drama-logo-erase.json").read_text(
            encoding="utf-8"
        )
    ))
    item = PlanInput(asset_id="ast_1", storage_key="assets/ast_1/x.mp4", role="video",
                     sha256="a" * 64)
    with pytest.raises(WorkError) as error:
        build_plan(recipe, [item], available)
    assert error.value.error_class is ErrorClass.CAPABILITY_MISSING


def test_gpu_only_model_capability_comes_from_the_deployment(settings):
    """`video_inpaint_model` 由部署方声明（只有装了模型 worker 的机器才有）。"""
    from drama_cloud.recipes.registry import worker_capabilities

    assert "video_inpaint_model" not in worker_capabilities(settings)
    gpu = dataclasses.replace(settings, capabilities=("video_inpaint_model",))
    assert "video_inpaint_model" in worker_capabilities(gpu)


# ---------- 长任务进度流（Tier 3） ----------

#: 认识 `--progress-json` 的假工具：跑的时候先写两条进度、睡一下、再写终结条，
#: 这样「边跑边上报」而不是「跑完才读最后一行」是可被观测的。
PROGRESS_TOOL = r"""#!/usr/bin/env bash
set -euo pipefail
if [ "${1:-}" = "--help" ]; then
  echo "  --input --output --report-json --progress-json --dilate"
  exit 0
fi
progress=""
while [ $# -gt 0 ]; do
  case "$1" in
    --input) input="$2"; shift 2;;
    --output) output="$2"; shift 2;;
    --report-json) report="$2"; shift 2;;
    --progress-json) progress="$2"; shift 2;;
    *) shift;;
  esac
done
emit() { printf '%s\n' "$1" >> "$progress"; }
emit '{"seq":1,"kind":"started","taskId":"t","pipeline":"drama.logo.erase","stage":"locate","stageIndex":0,"stageTotal":2,"unitDone":0,"unitTotal":2}'
emit '{"seq":2,"kind":"progress","taskId":"t","pipeline":"drama.logo.erase","stage":"inpaint","stageIndex":1,"stageTotal":2,"unitDone":1,"unitTotal":2}'
sleep 0.3
emit '{"seq":3,"kind":"finished","taskId":"t","pipeline":"drama.logo.erase","stage":"inpaint","stageIndex":1,"stageTotal":2,"unitDone":2,"unitTotal":2}'
cp "$input" "$output"
cat > "$report" <<JSON
{"framesIn": 2, "framesOut": 2, "regions": [{"x": 0, "y": 0, "width": 8, "height": 8}],
 "clips": 1, "inpaintedClips": 1, "roiJobs": 1, "inpaintBatches": 1, "meanMaskCoverage": 0.1,
 "verification": {"applicable": true, "verdict": "pass", "issues": [], "notes": []}}
JSON
exit 0
"""


def make_progress_tool(directory: Path) -> Path:
    script = directory / "progress-inpaint"
    script.write_text(PROGRESS_TOOL, encoding="utf-8")
    script.chmod(script.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    return script


def test_build_args_only_passes_progress_json_when_asked(context):
    """进度流是可选开关：不给路径就不该出现在命令行里（旧工具不认识它）。"""
    params = InpaintParams.model_validate({"regions": [{"x": 0, "y": 0, "width": 8, "height": 8}]})
    escalation = plan_escalation(params, 1, None)
    common = dict(
        source=context.primary,
        output=context.work_dir / "out.mp4",
        report_path=context.work_dir / "report.json",
        tool=Path("/bin/true"),
        worker=None,
    )
    assert "--progress-json" not in build_args(context, params, escalation, **common)
    args = build_args(
        context, params, escalation, progress_path=context.work_dir / "progress.jsonl", **common
    )
    index = args.index("--progress-json")
    assert args[index + 1].endswith("progress.jsonl")


def test_implement_inpaint_streams_progress_into_the_hook(context, settings, tmp_path):
    """工具报出的每一条进度都要实时交给订阅方，并把末条留在处理日志里。"""
    tool = make_progress_tool(tmp_path)
    context.settings = dataclasses.replace(settings, inpaint_bin=str(tool))
    seen: list = []
    context.progress_hook = seen.append
    params = InpaintParams.model_validate({"regions": [{"x": 0, "y": 0, "width": 8, "height": 8}]})

    implement_inpaint(context, params)

    assert [event["seq"] for event in seen] == [1, 2, 3]
    assert seen[-1]["kind"] == "finished"
    assert context.plan["inpaint"]["progress"]["unitDone"] == 2


def test_missing_progress_support_is_announced_not_fatal(context, settings, tmp_path):
    """旧工具不认 `--progress-json` 时只记一条说明，擦除照常完成。"""
    # 用 warn 结论：本条只用来看「缺进度能力」的处置，不要被自动重试的裁决带跑。
    tool = make_fake_tool(tmp_path, mode="warn")
    context.settings = dataclasses.replace(settings, inpaint_bin=str(tool))
    seen: list = []
    context.progress_hook = seen.append
    params = InpaintParams.model_validate({"regions": [{"x": 0, "y": 0, "width": 8, "height": 8}]})

    implement_inpaint(context, params)

    assert seen == [], "工具没报进度时不该凭空造事件"
    assert context.plan["inpaint"]["progress"] is None
    assert any("--progress-json" in note for note in context.notes)
