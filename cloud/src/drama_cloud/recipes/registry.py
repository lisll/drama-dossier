"""处理模块白名单注册表。

设计文档 §10 要求「配方模块采用白名单注册，参数经过类型、范围和依赖校验；
不执行配方内任意命令」。因此：

- 配方里只能出现**已注册**的模块名，注册表是代码里写死的，配方改不动它；
- 每个模块用 Pydantic 模型声明参数（`extra="forbid"`），未知字段直接拒绝，
  从根上堵掉「往配方里塞 shell 命令 / 可执行路径」的可能；
- 模块声明 `requires` / `produces` 角色，用于校验管线依赖是否闭合。
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Callable, Dict, FrozenSet, Optional

from pydantic import BaseModel


@dataclass(frozen=True)
class ModuleSpec:
    name: str
    title: str
    #: 该模块需要的能力标签；Worker 不支持时任务在计划阶段就失败，不会跑一半才发现。
    capability: Optional[str]
    requires: FrozenSet[str] = field(default_factory=frozenset)
    produces: FrozenSet[str] = field(default_factory=frozenset)
    params_model: type = BaseModel
    #: `transcode` 类模块会改视频本体；`gate` 只读；`plan` 只产出计划数据。
    kind: str = "transform"
    implement: Optional[Callable] = None


REGISTRY: Dict[str, ModuleSpec] = {}

_modules_loaded = False


def ensure_modules_loaded() -> None:
    """确保 `drama_cloud.modules` 已被导入（模块注册是「导入即注册」的副作用）。

    所有要校验配方、列出模块或执行管线的入口都先调用它，这样注册表就不会因为
    「谁先被 import」而时有时无。CLI、API、Worker 走的都是这一条路径。
    """
    global _modules_loaded
    if _modules_loaded:
        return
    _modules_loaded = True
    from .. import modules  # noqa: F401  导入即注册


def register(spec: ModuleSpec) -> ModuleSpec:
    if spec.name in REGISTRY:
        raise RuntimeError(f"模块重复注册：{spec.name}")
    REGISTRY[spec.name] = spec
    return spec


def get_module(name: str) -> ModuleSpec:
    ensure_modules_loaded()
    spec = REGISTRY.get(name)
    if spec is None:
        raise KeyError(name)
    return spec


def registered_modules() -> Dict[str, ModuleSpec]:
    ensure_modules_loaded()
    return dict(REGISTRY)


def parse_params(module: str, params: dict):
    """按模块的参数模型校验并归一化参数。"""
    spec = get_module(module)
    return spec.params_model.model_validate(params or {})


def worker_capabilities(settings=None) -> list:
    """当前 Worker 实际具备的能力标签。

    不传 `settings` 时按默认运行目录读取（并因此带上部署声明的额外能力）。
    """
    ensure_modules_loaded()
    if settings is None:
        from ..config import load_settings

        settings = load_settings()
    extra = settings.capabilities
    capabilities = {"ffmpeg", "ffprobe"}
    for spec in REGISTRY.values():
        if spec.capability:
            capabilities.add(spec.capability)
    capabilities.update(extra)
    return sorted(capabilities)
