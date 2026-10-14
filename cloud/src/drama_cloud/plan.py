"""配方 → ProcessingPlan（设计文档 §6、§7.3）。

任务是围绕「一份不可变配方 + 一组输入素材」展开的，但配方本身只是声明；
真正能执行的东西是 ProcessingPlan：

- 把每个 `PipelineStep` 解析成注册表里的 `ModuleSpec`，参数按模块模型校验并冻结成 plain dict；
- 做**能力检查**：Worker 没有声明的能力时，在计划阶段就失败（`capability_missing`），
  绝不跑到一半才发现；
- 做**输入角色分配**：谁是主视频、谁是外挂字幕、谁提供转写；
- 计算计划指纹（配方哈希 + 输入哈希 + Worker 版本），写进检查点，用来判断「能不能接着上次跑」。

计划是纯数据，不碰磁盘、不跑 ffmpeg，因此可以在 API 进程里直接构造与校验。
"""

from __future__ import annotations

import hashlib
from dataclasses import dataclass, field
from typing import Dict, Iterable, List, Sequence

from .errors import ErrorClass, WorkError
from .recipes.registry import ModuleSpec, get_module
from .recipes.schema import Recipe

#: Worker 自身版本；配方用 `compatibility.min_worker_version` 声明最低要求。
WORKER_VERSION = "1.0.0"
#: 执行引擎版本，写进检查点，用于判断「换了 ffmpeg 就重跑」。
ENGINE_VERSION = "ffmpeg-4"

#: 输入的语义角色。配方里的 `input_index` 指的都是这个列表的下标。
INPUT_ROLES = ("video", "audio", "subtitle", "metadata")


def parse_version(text: str) -> tuple:
    """把 `1.2.3` 解析成可比较的元组；解析不了的部分按 0 处理。"""
    parts = []
    for chunk in str(text or "").split("."):
        digits = ""
        for char in chunk:
            if char.isdigit():
                digits += char
            else:
                break
        parts.append(int(digits) if digits else 0)
    return tuple(parts + [0] * (3 - len(parts))) if len(parts) < 3 else tuple(parts)


@dataclass(frozen=True)
class PlanInput:
    """一份参与本次任务的输入素材。"""

    asset_id: str
    storage_key: str
    role: str = "video"
    sha256: str = ""
    name: str = ""


@dataclass(frozen=True)
class PlannedStep:
    id: str
    module: str
    params: dict
    spec: ModuleSpec


@dataclass
class ProcessingPlan:
    recipe_id: str
    recipe_version: int
    recipe_sha256: str
    mode: str
    target: dict
    quality_policy: dict
    steps: List[PlannedStep]
    inputs: List[PlanInput]
    capabilities: List[str]
    worker_version: str = WORKER_VERSION
    engine_version: str = ENGINE_VERSION
    warnings: List[str] = field(default_factory=list)

    @property
    def primary(self) -> PlanInput:
        for item in self.inputs:
            if item.role == "video":
                return item
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "计划里没有视频输入")

    def module_names(self) -> List[str]:
        return [step.module for step in self.steps]

    def required_capabilities(self) -> List[str]:
        caps = set()
        for step in self.steps:
            if step.spec.capability:
                caps.add(step.spec.capability)
        return sorted(caps)

    def fingerprint(self) -> str:
        """计划指纹：任一要素变化都必须重跑（对应检查点的失效判断）。"""
        digest = hashlib.sha256()
        digest.update(self.recipe_sha256.encode())
        digest.update(f"|{self.worker_version}|{self.engine_version}".encode())
        for item in self.inputs:
            digest.update(f"|{item.role}:{item.sha256 or item.asset_id}".encode())
        for step in self.steps:
            digest.update(f"|{step.id}:{step.module}".encode())
            digest.update(repr(sorted(step.params.items(), key=lambda kv: kv[0])).encode())
        return digest.hexdigest()

    def to_dict(self) -> dict:
        return {
            "recipe_id": self.recipe_id,
            "recipe_version": self.recipe_version,
            "recipe_sha256": self.recipe_sha256,
            "mode": self.mode,
            "target": self.target,
            "quality_policy": self.quality_policy,
            "worker_version": self.worker_version,
            "engine_version": self.engine_version,
            "fingerprint": self.fingerprint(),
            "capabilities": self.capabilities,
            "required_capabilities": self.required_capabilities(),
            "warnings": list(self.warnings),
            "inputs": [
                {"asset_id": i.asset_id, "role": i.role, "sha256": i.sha256, "name": i.name}
                for i in self.inputs
            ],
            "steps": [
                {"id": s.id, "module": s.module, "title": s.spec.title, "kind": s.spec.kind,
                 "params": s.params}
                for s in self.steps
            ],
        }


def _normalize_params(step_module: str, raw: dict) -> dict:
    """按模块的参数模型校验并导出 plain dict（冻结，避免后续被改动）。"""
    from .recipes.registry import parse_params

    model = parse_params(step_module, raw)
    return model.model_dump(mode="json")


def build_plan(
    recipe: Recipe,
    inputs: Sequence[PlanInput],
    capabilities: Iterable[str],
) -> ProcessingPlan:
    """构造可执行计划，做能力检查与输入角色校验。"""
    capability_set = {str(c) for c in capabilities}

    missing = sorted(set(recipe.compatibility.required_capabilities) - capability_set)
    if missing:
        raise WorkError(
            ErrorClass.CAPABILITY_MISSING,
            "Worker 缺少配方声明的能力：" + ", ".join(missing),
        )

    minimum = parse_version(recipe.compatibility.min_worker_version)
    if parse_version(WORKER_VERSION) < minimum:
        raise WorkError(
            ErrorClass.CAPABILITY_MISSING,
            f"Worker {WORKER_VERSION} 低于配方要求的最低版本 "
            f"{recipe.compatibility.min_worker_version}",
        )

    plan_inputs = list(inputs)
    if not plan_inputs:
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "任务没有输入素材")
    for item in plan_inputs:
        if item.role not in INPUT_ROLES:
            raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, f"未知输入角色：{item.role}")
    if not any(i.role == "video" for i in plan_inputs):
        raise WorkError(ErrorClass.PARAM_OUT_OF_RANGE, "任务缺少 video 角色的输入素材")

    steps: List[PlannedStep] = []
    warnings: List[str] = []
    available_caps = set(capability_set)
    for step in recipe.pipeline:
        spec = get_module(step.module)
        if spec.capability and spec.capability not in available_caps:
            raise WorkError(
                ErrorClass.CAPABILITY_MISSING,
                f"步骤 {step.id}（{step.module}）需要能力 {spec.capability}，"
                "当前 Worker 不支持；请改为具备该能力的 Worker 或调整配方",
            )
        steps.append(PlannedStep(step.id, step.module, _normalize_params(step.module, step.params), spec))

    _validate_input_references(steps, plan_inputs)

    if not any(s.module == "media.encode" for s in steps):
        warnings.append("配方没有 media.encode 步骤，本次任务不会产出视频文件")

    return ProcessingPlan(
        recipe_id=recipe.recipe_id,
        recipe_version=recipe.version,
        recipe_sha256=recipe.content_sha256(),
        mode=recipe.preset.mode,
        target=recipe.target.model_dump(mode="json"),
        quality_policy=recipe.quality_policy.model_dump(mode="json"),
        steps=steps,
        inputs=plan_inputs,
        capabilities=sorted(capability_set),
        warnings=warnings,
    )


def _validate_input_references(steps: Sequence[PlannedStep], inputs: Sequence[PlanInput]) -> None:
    """配方参数里引用的输入下标必须真实存在，且角色对得上。"""
    for step in steps:
        params = step.params
        if step.module == "subtitle.mux":
            source = params.get("source") or {}
            if source.get("kind") == "external":
                index = source.get("input_index")
                if index is None or not (0 <= index < len(inputs)):
                    raise WorkError(
                        ErrorClass.PARAM_OUT_OF_RANGE,
                        f"步骤 {step.id} 引用了不存在的外挂字幕输入 #{index}",
                    )
                if inputs[index].role != "subtitle":
                    raise WorkError(
                        ErrorClass.PARAM_OUT_OF_RANGE,
                        f"步骤 {step.id} 引用的输入 #{index} 角色为 {inputs[index].role}，"
                        "不是 subtitle",
                    )
        if step.module in ("timeline.user_edits", "commentary.rebuild", "video.inpaint"):
            indexes = []
            if step.module == "timeline.user_edits":
                indexes = [s.get("input_index", 0) for s in params.get("segments") or []]
            elif step.module == "video.inpaint":
                # 擦除会把该路输入替换成擦除结果，下标必须真实存在。
                indexes = [params.get("input_index", 0)]
            else:
                indexes = [params.get("commentary_input_index", 0)]
            for index in indexes:
                if index is None or not (0 <= index < len(inputs)):
                    raise WorkError(
                        ErrorClass.PARAM_OUT_OF_RANGE,
                        f"步骤 {step.id} 引用了不存在的输入素材 #{index}",
                    )


def plan_from_snapshot(snapshot: dict, inputs: Sequence[PlanInput], capabilities: Iterable[str]) -> ProcessingPlan:
    """从任务里保存的配方快照重建计划（任务执行期间不重新解析发布指针）。"""
    from .recipes.schema import validate_recipe

    recipe = validate_recipe(snapshot)
    return build_plan(recipe, inputs, capabilities)


def defaults_for_target(target: dict) -> Dict[str, str]:
    """把配方的 target 映射成编码模块的默认参数，供 `recipe_to_pipeline` 使用。"""
    return {
        "container": target.get("container", "mp4"),
        "video_codec": target.get("video_codec", "h264"),
        "audio_codec": target.get("audio_codec", "aac"),
    }
