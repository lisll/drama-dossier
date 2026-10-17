"""任务哈希（version-task@1）的 Python 参考实现：和服务端无关，只用于和 Rust 侧对拍。

生产实现在 crates/dossier-license/src/version_task.rs；两边必须逐字节一致。
规则见 cloud/docs/COMMERCE.md「任务哈希」一节：
SHA-256( 规范化 JSON )，键按字母序、无空白、非 ASCII 字符按 UTF-8 原样输出。
"""

from __future__ import annotations

import hashlib
import json

TASK_SCHEMA = "version-task@1"


def canonical_json(value) -> bytes:
    """和 Rust 侧 write_canonical 一致：json.dumps(sort_keys=True, separators=(",", ":"), ensure_ascii=False)。

    两者对数字的表示有约定：整数原样；浮点数用各自语言的最短表示。对拍向量里只用整数和
    保留 4 位小数以内的浮点（derive_params 的 round(..., 4)），两边表示一致。
    """
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")


def template_hash(template: dict) -> str:
    return hashlib.sha256(canonical_json(template)).hexdigest()


def version_task_hash(episodes, rights: str, template, strength: str) -> str:
    """episodes: [(name, sha256)]，调用前按文件名升序排好；template 传模板对象（不是哈希）。"""
    task = {
        "episodes": [{"name": name, "sha256": digest} for name, digest in episodes],
        "rights": rights,
        "schema": TASK_SCHEMA,
        "strength": strength,
        "template": template_hash(template),
    }
    return hashlib.sha256(canonical_json(task)).hexdigest()
