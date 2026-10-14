"""共享的严格 Pydantic 基类。

`extra="forbid"` 是配方安全边界的一部分：任何未在模块参数模型里声明的字段都会被拒绝，
所以配方无法夹带额外指令或路径。
"""

from __future__ import annotations

from pydantic import BaseModel, ConfigDict


class StrictModel(BaseModel):
    model_config = ConfigDict(extra="forbid", protected_namespaces=())
