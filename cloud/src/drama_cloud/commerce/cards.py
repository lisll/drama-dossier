"""卡密：批量生成、作废、查询，以及兑换。

卡密是 16 位，字符集 23456789ABCDEFGHJKLMNPQRSTUVWXYZ（32 个字符，去掉了容易看错的 0/1/I/O），
每张 80 位随机数，靠猜是猜不中的。展示和导出写成 DD-XXXX-XXXX-XXXX-XXXX；
兑换时不分大小写，空格、短横线和 DD 前缀都会被忽略。
数据库只存摘要，明文只出现在生成时导出的 CSV 里。
"""

from __future__ import annotations

import hashlib
import re
import secrets
from typing import List, Optional

from . import entitlements
from .db import audit, lock_user
from .errors import CommerceError

ALPHABET = "23456789ABCDEFGHJKLMNPQRSTUVWXYZ"
CODE_LENGTH = 16
PREFIX = "DD"
FORMAT_MESSAGE = "卡密是 16 位，由数字 2-9 和除 I、O 以外的大写字母组成，请检查是否输错或漏输"
_CODE = re.compile(f"[{ALPHABET}]{{{CODE_LENGTH}}}")
_SEPARATORS = re.compile(r"[\s-]+")


def normalize(raw: str) -> Optional[str]:
    """把用户输入整理成 16 位卡密；格式不对返回 None。"""
    text = _SEPARATORS.sub("", raw)
    if not text.isascii():
        return None
    text = text.upper()
    if len(text) == CODE_LENGTH + len(PREFIX) and text.startswith(PREFIX):
        text = text[len(PREFIX):]
    return text if _CODE.fullmatch(text) else None


def generate_code() -> str:
    return "".join(secrets.choice(ALPHABET) for _ in range(CODE_LENGTH))


def display(code: str) -> str:
    groups = [code[index:index + 4] for index in range(0, CODE_LENGTH, 4)]
    return "-".join([PREFIX, *groups])


def digest(code: str) -> bytes:
    return hashlib.sha256(b"drama-dossier/card/v1\n" + code.encode("ascii")).digest()


def create_batch(
    conn,
    *,
    name: str,
    days: Optional[int],
    codes: List[str],
    operator: str,
    price_fen: Optional[int] = None,
) -> str:
    """一个事务写入一批卡密（只写摘要）。days 为 None 表示永久卡。返回批次 ID。

    `price_fen` 是这批卡的售价（单位：分，可空）——只记录在批次上，
    不参与兑换逻辑；见迁移 0013。
    """
    kind = "lifetime" if days is None else "timed"
    with conn.transaction():
        batch_id = conn.execute(
            "INSERT INTO card_batches (name, kind, days, created_by, price_fen)"
            " VALUES (%s, %s, %s, %s, %s) RETURNING id",
            (name, kind, days, operator, price_fen),
        ).fetchone()["id"]
        with conn.cursor() as cursor, cursor.copy("COPY cards (code_digest, batch_id) FROM STDIN") as copy:
            for code in codes:
                copy.write_row((digest(code), batch_id))
        audit(conn, operator, "cards.batch_create", str(batch_id),
              {"count": len(codes), "kind": kind, "days": days, "price_fen": price_fen})
    return str(batch_id)


def revoke_batch(conn, batch_id: str, operator: str) -> Optional[dict]:
    """作废一个批次里还没兑换的卡密；已经兑换的不受影响。批次不存在返回 None。"""
    with conn.transaction():
        row = conn.execute(
            "SELECT revoked_at FROM card_batches WHERE id = %s::uuid FOR UPDATE", (batch_id,)
        ).fetchone()
        if row is None:
            return None
        unused = conn.execute(
            "SELECT count(*) AS n FROM cards WHERE batch_id = %s::uuid AND redeemed_by IS NULL", (batch_id,)
        ).fetchone()["n"]
        if row["revoked_at"] is not None:
            return {"already": True, "unused": unused}
        conn.execute("UPDATE card_batches SET revoked_at = now() WHERE id = %s::uuid", (batch_id,))
        audit(conn, operator, "cards.batch_revoke", batch_id, {"unused": unused})
    return {"already": False, "unused": unused}


def list_batches(conn, limit: int = 50) -> list:
    return conn.execute(
        """
        SELECT b.id, b.name, b.kind, b.days, b.price_fen, b.created_by, b.created_at, b.revoked_at,
               count(c.code_digest) AS total, count(c.redeemed_by) AS redeemed
        FROM card_batches b LEFT JOIN cards c ON c.batch_id = b.id
        GROUP BY b.id ORDER BY b.created_at DESC LIMIT %s
        """,
        (limit,),
    ).fetchall()


def status(conn, code: str) -> Optional[dict]:
    return conn.execute(
        """
        SELECT c.redeemed_by, c.redeemed_at, b.id AS batch_id, b.name AS batch_name,
               b.kind, b.days, b.revoked_at
        FROM cards c JOIN card_batches b ON b.id = c.batch_id
        WHERE c.code_digest = %s
        """,
        (digest(code),),
    ).fetchone()


def _card_view(card) -> dict:
    return {"kind": card["kind"], "days": card["days"]}


def redeem(conn, user_id: str, code: str) -> dict:
    """兑换卡密。同一个人重复兑换同一张卡：原样返回当前授权，不会重复加时间。

    先锁用户行、再锁卡密行，顺序固定，不会互相等死：
    同一个人同时兑换几张卡会排队执行；两个人同时兑换同一张卡，只有一个能成功。
    """
    with conn.transaction():
        lock_user(conn, user_id)
        card = conn.execute(
            """
            SELECT c.code_digest, c.redeemed_by, b.id AS batch_id, b.kind, b.days, b.revoked_at
            FROM cards c JOIN card_batches b ON b.id = c.batch_id
            WHERE c.code_digest = %s
            FOR UPDATE OF c
            """,
            (digest(code),),
        ).fetchone()
        if card is None:
            raise CommerceError(404, "card_not_found", "卡密不存在，请检查是否输错")
        if card["redeemed_by"] is not None:
            if str(card["redeemed_by"]) != user_id:
                raise CommerceError(409, "card_used", "这张卡密已经被使用过了")
            return {
                "entitlement": entitlements.current(conn, user_id).view(),
                "card": _card_view(card),
                "already_redeemed": True,
            }
        if card["revoked_at"] is not None:
            raise CommerceError(409, "card_revoked", "这张卡密已作废，请联系发卡方")
        before = entitlements.current(conn, user_id)
        # 永久卡的 days 是 NULL（表上有约束保证），grant 收到 None 就按永久处理。
        entitlements.grant(conn, user_id, card["days"])
        conn.execute(
            "UPDATE cards SET redeemed_by = %s::uuid, redeemed_at = now() WHERE code_digest = %s",
            (user_id, card["code_digest"]),
        )
        after = entitlements.current(conn, user_id)
        audit(conn, user_id, "card.redeem", user_id, {
            "batch": str(card["batch_id"]),
            "kind": card["kind"],
            "days": card["days"],
            "before": before.view(),
            "after": after.view(),
        })
    return {"entitlement": after.view(), "card": _card_view(card), "already_redeemed": False}
