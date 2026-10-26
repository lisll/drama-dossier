"""卡密：生成、规范化、摘要。

- 字符集 32 个（去掉易混的 0/O/1/I），16 位 ≈ 80 bit 随机量；
- 只用 `secrets`（CSPRNG）生成；
- 数据库只存摘要，明文只在生成那一刻出现一次。
"""

from __future__ import annotations

import hashlib
import re
import secrets

ALPHABET = "23456789ABCDEFGHJKLMNPQRSTUVWXYZ"
CODE_LEN = 16
PREFIX = "DDK"
_DIGEST_CONTEXT = b"drama-dossier/card/v1\n"
_VALID = re.compile(rf"^[{ALPHABET}]{{{CODE_LEN}}}$")


def generate_code() -> str:
    return "".join(secrets.choice(ALPHABET) for _ in range(CODE_LEN))


def display(code: str) -> str:
    groups = [code[i : i + 4] for i in range(0, CODE_LEN, 4)]
    return PREFIX + "-" + "-".join(groups)


def normalize(raw: str) -> str | None:
    """用户输入 → 16 位规范卡密；格式不对返回 None。

    忽略大小写、空白、短横线和 DDK 前缀。
    """
    if not isinstance(raw, str) or len(raw) > 64:
        return None
    text = re.sub(r"[\s\-]", "", raw).upper()
    if text.startswith(PREFIX) and len(text) == CODE_LEN + len(PREFIX):
        text = text[len(PREFIX) :]
    return text if _VALID.match(text) else None


def digest(code: str) -> str:
    return hashlib.sha256(_DIGEST_CONTEXT + code.encode("ascii")).hexdigest()


def prefix_of_digest(dig: str) -> str:
    """审计里只记摘要前 8 位，足以人工对照，又不足以反推。"""
    return dig[:8]


def generate_batch(count: int) -> list[str]:
    if not 1 <= count <= 10000:
        raise ValueError("单批数量必须在 1 到 10000 之间")
    codes: set[str] = set()
    while len(codes) < count:
        codes.add(generate_code())
    return sorted(codes)
