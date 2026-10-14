"""配方数据模型与校验（设计文档 §6）。

配方**只声明允许的处理模块、参数与依赖**：模块名走白名单注册表，参数走每个模块自己的
Pydantic 模型，管线依赖走 `requires`/`produces` 角色闭包检查。
"""

from __future__ import annotations

import hashlib
from typing import Dict, List, Literal, Optional

from pydantic import Field, field_validator

from ..base import StrictModel
from ..db import json_dump
from ..errors import invalid_recipe
from .registry import REGISTRY, ensure_modules_loaded, get_module, parse_params

SCHEMA_VERSION = "1.0"
#: 管线起点的既有角色：任何配方都能读到这三样。
BASE_ROLES = frozenset({"video", "audio", "metadata"})

PipelineMode = Literal["no_edit", "subtitle", "full_edit"]
RecipeStatus = Literal["draft", "reviewing", "published", "deprecated", "revoked"]


class Target(StrictModel):
    platform_profile: str
    container: str
    video_codec: str
    audio_codec: str
    #: 目标分辨率（可选）。声明了就要求配方里真的按它做分辨率归一化。
    width: Optional[int] = Field(None, ge=16, le=7680)
    height: Optional[int] = Field(None, ge=16, le=4320)


class Compatibility(StrictModel):
    min_worker_version: str = "1.0.0"
    required_capabilities: List[str] = Field(default_factory=list)


class Preset(StrictModel):
    mode: PipelineMode = "no_edit"


class PipelineStep(StrictModel):
    id: str
    module: str
    params: Dict = Field(default_factory=dict)

    @field_validator("id")
    @classmethod
    def _id_shape(cls, value: str) -> str:
        if not value or not value.replace("_", "").replace("-", "").isalnum():
            raise ValueError("步骤 id 只能是字母、数字、下划线与连字符")
        return value


class QualityPolicy(StrictModel):
    require_decodable_output: bool = True
    check_audio_video_sync: bool = True
    validate_target_profile: bool = True
    #: 音画同步容差（毫秒）。文档要求检查音画同步，但没给数值；这里给出显式默认值。
    max_av_sync_ms: float = 200.0
    min_duration_seconds: float = 0.5


class Recipe(StrictModel):
    schema_version: str = SCHEMA_VERSION
    recipe_id: str
    version: int = Field(ge=1)
    status: RecipeStatus = "draft"
    target: Target
    compatibility: Compatibility = Field(default_factory=Compatibility)
    preset: Preset = Field(default_factory=Preset)
    pipeline: List[PipelineStep]
    quality_policy: QualityPolicy = Field(default_factory=QualityPolicy)

    # ---------- 派生信息 ----------

    def content_dict(self) -> dict:
        return self.model_dump(mode="json")

    def content_sha256(self) -> str:
        """配方内容哈希（不含 status，发布/撤回不应改变内容指纹）。"""
        payload = self.content_dict()
        payload.pop("status", None)
        return hashlib.sha256(json_dump(payload).encode("utf-8")).hexdigest()


def recipe_json_schema() -> dict:
    """导出 JSON Schema，供客户端与外部工具做静态校验。"""
    return Recipe.model_json_schema()


def validate_recipe(payload: dict) -> Recipe:
    """解析 + 白名单 + 参数 + 依赖校验，任何一项不过都抛 `recipe_schema_invalid`。"""
    ensure_modules_loaded()
    try:
        recipe = Recipe.model_validate(payload)
    except Exception as error:  # pydantic ValidationError 及其子类
        raise invalid_recipe(f"配方结构不合法：{error}") from error

    if recipe.schema_version != SCHEMA_VERSION:
        raise invalid_recipe(
            f"配方 schema_version={recipe.schema_version} 不受支持（当前 {SCHEMA_VERSION}）"
        )

    seen_ids = set()
    for step in recipe.pipeline:
        if step.id in seen_ids:
            raise invalid_recipe(f"步骤 id 重复：{step.id}")
        seen_ids.add(step.id)
        if step.module not in REGISTRY:
            raise invalid_recipe(
                f"步骤 {step.id} 使用了未注册模块 {step.module}；白名单："
                + ", ".join(sorted(REGISTRY))
            )
        try:
            parse_params(step.module, step.params)
        except Exception as error:
            raise invalid_recipe(f"步骤 {step.id}（{step.module}）参数不合法：{error}") from error

    _validate_dependencies(recipe)
    _validate_policy(recipe)
    return recipe


def _validate_dependencies(recipe: Recipe) -> None:
    """校验管线依赖闭合：每一步的 requires 必须已被前面的步骤或输入素材满足。"""
    available = set(BASE_ROLES)
    for step in recipe.pipeline:
        spec = get_module(step.module)
        missing = set(spec.requires) - available
        if missing:
            raise invalid_recipe(
                f"步骤 {step.id}（{step.module}）缺少前置产出：{', '.join(sorted(missing))}"
            )
        available |= set(spec.produces)


def _validate_policy(recipe: Recipe) -> None:
    modules = [step.module for step in recipe.pipeline]
    if not modules:
        raise invalid_recipe("管线不能为空")
    if modules[0] != "media.probe":
        # 必须先探测，后面的模块才能拿到时长/轨道信息。
        raise invalid_recipe("管线第一步必须是 media.probe")
    if recipe.quality_policy.require_decodable_output and modules[-1] != "media.quality_gate":
        raise invalid_recipe("要求质量门禁时，管线最后一步必须是 media.quality_gate")
    if "subtitle" in recipe.preset.mode and "subtitle.mux" not in modules:
        # 字幕工作流至少要有一个明确处理字幕的步骤，避免“模式”变成一句空话。
        raise invalid_recipe("subtitle 模式必须包含 subtitle.mux 步骤")
    _validate_target_matches_encode(recipe)


def _validate_target_matches_encode(recipe: Recipe) -> None:
    """`target` 与 `media.encode` 的实际参数必须一致。

    两者是同一件事的两种表达（一个是给客户端的「目标规格」，一个是给编码器的参数），
    不一致会让质量门禁在跑完整个编码之后才发现规格不对——所以提前在配方校验阶段拦住。
    """
    encode_step = next((s for s in recipe.pipeline if s.module == "media.encode"), None)
    if encode_step is not None:
        params = parse_params("media.encode", encode_step.params)
        mismatches = []
        if params.container != recipe.target.container:
            mismatches.append(
                f"container：target={recipe.target.container} encode={params.container}"
            )
        if params.video_codec != recipe.target.video_codec:
            mismatches.append(
                f"video_codec：target={recipe.target.video_codec} encode={params.video_codec}"
            )
        if params.audio_codec != "none" and params.audio_codec != recipe.target.audio_codec:
            mismatches.append(
                f"audio_codec：target={recipe.target.audio_codec} encode={params.audio_codec}"
            )
        if mismatches:
            raise invalid_recipe(
                f"步骤 {encode_step.id} 的编码参数与 target 不一致 → " + "；".join(mismatches)
            )

    if recipe.target.width or recipe.target.height:
        scale_step = next(
            (
                s
                for s in recipe.pipeline
                if s.module == "video.quality" and (s.params or {}).get("scale")
            ),
            None,
        )
        if scale_step is None:
            raise invalid_recipe(
                "target 声明了分辨率，但配方里没有 video.quality 的 scale 步骤；"
                "分辨率不会自动生效"
            )
        scale = scale_step.params["scale"]
        if recipe.target.width and scale.get("width") != recipe.target.width:
            raise invalid_recipe(
                f"target.width={recipe.target.width} 与 scale.width={scale.get('width')} 不一致"
            )
        if recipe.target.height and scale.get("height") != recipe.target.height:
            raise invalid_recipe(
                f"target.height={recipe.target.height} 与 scale.height={scale.get('height')} 不一致"
            )
