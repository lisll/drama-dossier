"""外部可执行文件的解析：部署配置 → PATH → 仓库构建产物。

云端要调三个 Rust 侧工具（`dossier-inpaint` / `dossier-media` / `dossier-artifacts`），
它们的查找顺序必须**一致**：部署配置优先（生产环境显式指定），否则查 PATH，
最后回落到仓库的 `target/release`、`target/debug`（学习环境里 `cargo build` 完就能用）。

以前这段逻辑只存在于 `modules/inpaint.py` 里，第二个、第三个工具出现时就会各写一遍；
所以这里收敛成一处，并用**显式失败**代替「找不到就当没有这个步骤」。
"""

from __future__ import annotations

import os
import shutil
from pathlib import Path
from typing import Iterable, Optional, Tuple

from .errors import ErrorClass, WorkError


def repo_root() -> Path:
    """仓库根目录（`cloud/src/drama_cloud/toolchain.py` 往上三级）。"""
    return Path(__file__).resolve().parents[3]


def search_candidates(
    configured: Optional[str],
    name: str,
    subdirs: Iterable[str] = ("release", "debug"),
) -> Tuple[Path, ...]:
    """按固定顺序给出候选路径（不判断存在性，便于测试与诊断输出）。"""
    candidates = []
    if configured:
        candidates.append(Path(str(configured)))
    found = shutil.which(name)
    if found:
        candidates.append(Path(found))
    root = repo_root()
    for subdir in subdirs:
        candidates.append(root / "target" / subdir / name)
    return tuple(candidates)


def resolve_binary(
    configured: Optional[str],
    name: str,
    subdirs: Iterable[str] = ("release", "debug"),
) -> Optional[Path]:
    """找到可执行文件就返回它；找不到返回 `None`（由调用方决定是拒绝还是降级）。"""
    for candidate in search_candidates(configured, name, subdirs):
        if candidate.is_file() and os.access(str(candidate), os.X_OK):
            return candidate
    return None


def require_binary(configured: Optional[str], name: str, hint: str) -> Path:
    """必需的工具找不到时，按 `capability_missing` 报错并给出构建/安装提示。"""
    resolved = resolve_binary(configured, name)
    if resolved is not None:
        return resolved
    searched = " → ".join(str(path) for path in search_candidates(configured, name))
    raise WorkError(
        ErrorClass.CAPABILITY_MISSING,
        f"找不到可执行文件 {name}（查找顺序：{searched}）。{hint}",
    )
