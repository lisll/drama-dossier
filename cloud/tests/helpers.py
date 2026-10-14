"""测试公用小工具。"""

from __future__ import annotations

import io

from drama_cloud.tasks import service


def make_asset(conn, storage, project_id: str, name: str = "asset.bin", role: str = "video"):
    """登记一份不需要 ffmpeg 的假素材（内容随便，只用于走通任务链路）。"""
    return service.register_asset(
        conn, storage, project_id, name, io.BytesIO(b"placeholder-bytes"), role=role
    )


def make_task(conn, settings, storage, project_id, recipe_id="drama.no_edit.generic",
              role="video", **kwargs):
    asset = make_asset(conn, storage, project_id, role=role)
    return service.create_task(
        conn, settings, project_id, [asset["id"]], recipe_id=recipe_id, **kwargs
    )


import contextlib
import dataclasses


@contextlib.contextmanager
def patch_module(name: str, implement):
    """临时替换某个模块的实现（用于注入失败）。"""
    from drama_cloud.recipes.registry import REGISTRY, get_module

    original = get_module(name)
    REGISTRY[name] = dataclasses.replace(original, implement=implement)
    try:
        yield
    finally:
        REGISTRY[name] = original
