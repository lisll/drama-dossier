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
    #: 能力可用性判定：`None` 表示「代码里注册了就一定具备」；给了函数则按部署配置判定
    #: （例如 `dossier-inpaint` 可执行文件是否真的存在、模型 worker 是否已配置）。
    #: 判定为假时该能力**不会**出现在 Worker 的能力清单里，不兼容的任务在计划阶段就被
    #: 路由到别的 Worker，而不是领了任务再发现跑不动。
    capability_probe: Optional[Callable] = None
    #: 参数相关的额外能力要求：有些模块「选哪个后端」会带来不同的依赖
    #: （例如擦除选了模型修复后端就需要 `video_inpaint_model`）。
    #: 入参是**已校验**的参数 dict，返回需要的能力标签集合；计划阶段一并检查。
    dynamic_capabilities: Optional[Callable[[dict], FrozenSet[str]]] = None


REGISTRY: Dict[str, ModuleSpec] = {}

#: 不属于任何模块、但又确实存在的整机能力（例如「模型权重装齐了」）。
#: 以前这类能力只能靠部署方写 `DRAMA_CLOUD_CAPABILITIES` 手工声明——声明错了没人发现；
#: 现在改为注册一个探测函数：探测通过才出现在能力清单里。
_EXTRA_CAPABILITIES: Dict[str, Callable[[Optional[object]], bool]] = {}


def register_capability(tag: str, probe: Callable[[Optional[object]], bool]) -> None:
    """注册一条「整机能力」及其探测函数（重复注册直接报错，避免两份真相）。"""
    if tag in _EXTRA_CAPABILITIES:
        raise RuntimeError(f"能力重复注册：{tag}")
    _EXTRA_CAPABILITIES[tag] = probe


def registered_extra_capabilities() -> Dict[str, Callable[[Optional[object]], bool]]:
    ensure_modules_loaded()
    return dict(_EXTRA_CAPABILITIES)

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

    `ffmpeg` / `ffprobe` 这两条**也要探测**：以前它们是无条件写死的，于是没装 ffmpeg 的
    机器照样声明「我会 ffmpeg」，`metadata.policy` 这类只要求 `ffmpeg` 的步骤会被排进来，
    直到执行时才炸。能力标签的含义是「本机真的能做这件事」，所以这里改成如实探测。
    部署侧确有其他手段提供该能力时，仍可用 `settings.capabilities` 显式声明覆盖。
    """
    ensure_modules_loaded()
    if settings is None:
        from ..config import load_settings

        settings = load_settings()
    # 局部导入：registry 在 recipes 包内，而 modules 包导入时会反向 import registry，
    # 顶层导入会形成环。
    from ..modules.ffmpeg_caps import ffmpeg_available, ffprobe_available

    extra = settings.capabilities
    capabilities = set()
    if ffmpeg_available(settings):
        capabilities.add("ffmpeg")
    if ffprobe_available(settings):
        capabilities.add("ffprobe")
    for spec in REGISTRY.values():
        if not spec.capability:
            continue
        probe = spec.capability_probe
        if probe is not None and not probe(settings):
            # 部署侧条件不成立（缺可执行文件/模型），能力如实缺席。
            continue
        capabilities.add(spec.capability)
    for tag, probe in _EXTRA_CAPABILITIES.items():
        try:
            if probe(settings):
                capabilities.add(tag)
        except Exception:  # noqa: BLE001  探测异常按「没有这个能力」处理
            continue
    capabilities.update(extra)
    return sorted(capabilities)
