"""签名用途前缀的跨语言对拍（M7-4）。

为什么单独一条：Rust 的 `Purpose::context()` 与 Python 的
`license_token.PURPOSE_CONTEXTS` 必须是**逐字节相同**的串。多一个字符、
少一个换行，签名就验不过，而症状只是「客户端说签名无效」——极难定位到
「两边字符串不一致」。

这条测试把两边的值都读出来比对；加新用途时必须两边同时改，否则这里会红。
"""

from __future__ import annotations

import re
from pathlib import Path

from drama_cloud.commerce import license_token

RUST_ENVELOPE = (
    Path(__file__).resolve().parents[3] / "crates" / "dossier-license" / "src" / "envelope.rs"
)

#: 用途名 → Rust 里的枚举名。加新用途时两边都要加。
RUST_NAMES = {
    "license": "License",
    "package-manifest": "PackageManifest",
    "recipe": "Recipe",
    "release": "Release",
}


def _rust_contexts() -> dict:
    """从 envelope.rs 里抠出 `Purpose::X => b"..."` 的映射。"""
    text = RUST_ENVELOPE.read_text(encoding="utf-8")
    found = {}
    for match in re.finditer(r'Purpose::(\w+)\s*=>\s*b"((?:[^"\\]|\\.)*)"', text):
        # Rust 的字节串里转义只有 \n 与 \"；这里只需要 \n。
        value = match.group(2).replace("\\n", "\n").replace('\\"', '"')
        found[match.group(1)] = value.encode("utf-8")
    return found


def test_every_python_purpose_has_a_rust_counterpart():
    rust = _rust_contexts()
    for name in license_token.PURPOSE_CONTEXTS:
        assert name in RUST_NAMES, f"Python 加了用途 {name}，但对拍表里没有对应的 Rust 名"
        assert RUST_NAMES[name] in rust, f"Rust 的 Purpose::{RUST_NAMES[name]} 找不到"


def test_the_context_bytes_are_identical_on_both_sides():
    """逐字节比对：多一个字符就验不过，而症状很难定位到这里。"""
    rust = _rust_contexts()
    for name, value in license_token.PURPOSE_CONTEXTS.items():
        other = rust[RUST_NAMES[name]]
        assert value == other, (
            f"用途 {name} 的上下文串两边不一致：\n"
            f"  Python: {value!r}\n"
            f"  Rust  : {other!r}"
        )


def test_every_rust_purpose_is_covered():
    """Rust 那边也不能有孤儿用途（加了却没人签它，等于永远验不过）。"""
    rust = _rust_contexts()
    covered = {RUST_NAMES[name] for name in license_token.PURPOSE_CONTEXTS}
    assert set(rust) == covered, f"Rust 与 Python 的用途集合不一致：{set(rust) ^ covered}"


def test_contexts_are_prefixed_and_end_with_a_newline():
    """前缀必须以用途名开头、以换行结尾——这是「用途隔离」的实现方式：
    不同用途的签名不会互相冒充，因为上下文串不同。"""
    for name, value in license_token.PURPOSE_CONTEXTS.items():
        assert value.startswith(b"drama-dossier/"), name
        assert value.endswith(b"\n"), name
        assert name.encode() in value, f"用途 {name} 的上下文串里没有用途名"
