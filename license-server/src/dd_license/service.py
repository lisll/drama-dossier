"""业务规则：客户端激活/续签 + 管理端生成/作废/查询。

网页后台与命令行共用这一份，规则只写一次。
授权与电脑**固定绑定**：不提供换机迁移，换电脑需要重新购卡。
"""

from __future__ import annotations

import re
import sqlite3
from dataclasses import dataclass

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey, Ed25519PublicKey

from . import cards
from .db import Database, audit, now
from .errors import LicenseError
from .keys import key_id, public_raw
from .plans import PLANS, get_plan
from .token import Claims, TokenError, sign, verify

MID_RE = re.compile(r"^[0-9a-f]{64}$")
DAY = 86400


def check_mid(mid: str) -> str:
    if not isinstance(mid, str) or not MID_RE.match(mid):
        raise LicenseError("bad_machine_id", "机器码格式不正确", 422)
    return mid


@dataclass
class Issued:
    token: str
    plan: str
    until: int | None
    exp: int
    already_redeemed: bool = False

    def to_dict(self) -> dict:
        return {
            "token": self.token,
            "plan": self.plan,
            "until": self.until,
            "exp": self.exp,
            "already_redeemed": self.already_redeemed,
        }


class LicenseService:
    def __init__(
        self,
        db: Database,
        signing_key: Ed25519PrivateKey,
        token_days: int = 7,
        extra_trusted: list[Ed25519PublicKey] | None = None,
    ):
        self.db = db
        self.key = signing_key
        self.pub = signing_key.public_key()
        self.kid = key_id(public_raw(self.pub))
        self.token_ttl = token_days * DAY
        self.trusted = [self.pub, *(extra_trusted or [])]

    # ------------------------------------------------------------------ 签发

    def _issue(self, machine: sqlite3.Row, at: int) -> tuple[str, int]:
        until = machine["until"]
        exp = at + self.token_ttl
        if until is not None:
            exp = min(exp, until)
        claims = Claims(self.kid, machine["id"], machine["mid"], machine["plan"], until, at, exp)
        return sign(self.key, claims), exp

    @staticmethod
    def _require_usable(machine: sqlite3.Row, at: int) -> None:
        if machine["revoked_at"] is not None:
            raise LicenseError("machine_revoked", "这台电脑的授权已被停用，请联系客服", 403)
        if machine["plan"] is None:
            raise LicenseError("not_activated", "这台电脑尚未激活", 403)
        if machine["until"] is not None and machine["until"] <= at:
            raise LicenseError("license_expired", "授权已到期，请兑换新的卡密续期", 403)

    # ------------------------------------------------------------------ 客户端

    def activate(self, raw_card: str, mid: str, app_version: str = "", ip: str = "") -> Issued:
        mid = check_mid(mid)
        code = cards.normalize(raw_card)
        if code is None:
            raise LicenseError("card_invalid", "卡密格式不正确，请检查后重新输入", 422)
        dig = cards.digest(code)
        try:
            return self._activate(dig, mid, app_version, ip)
        except LicenseError as exc:
            # 失败审计必须在业务事务之外单独提交：业务事务抛错会整体回滚，
            # 写在里面的审计会一起消失，后台就看不到"有人在猜卡"。
            with self.db.write() as conn:
                audit(conn, "client", "activate.fail", ip, mid, dig, reason=exc.code)
            raise

    def _activate(self, dig: str, mid: str, app_version: str, ip: str) -> Issued:
        at = now()
        with self.db.write() as conn:
            card = conn.execute(
                "SELECT c.*, b.revoked_at AS batch_revoked FROM cards c JOIN batches b ON b.id = c.batch_id WHERE c.digest = ?",
                (dig,),
            ).fetchone()
            if card is None:
                raise LicenseError("card_invalid", "卡密不存在，请检查后重新输入", 404)
            # 作废只针对未兑换的卡：已兑换的卡由"作废电脑"来停用
            if card["redeemed_mid"] is None and (card["revoked_at"] is not None or card["batch_revoked"] is not None):
                raise LicenseError("card_revoked", "这张卡密已作废", 409)

            machine = conn.execute("SELECT * FROM machines WHERE mid = ?", (mid,)).fetchone()

            if card["redeemed_mid"] is not None:
                if card["redeemed_mid"] != mid:
                    raise LicenseError("card_used", "这张卡密已被其他电脑使用", 409)
                # 本机重复兑换同一张卡：幂等，重新签发
                self._require_usable(machine, at)
                conn.execute(
                    "UPDATE machines SET last_seen_at = ?, last_app_version = ? WHERE id = ?",
                    (at, app_version[:32], machine["id"]),
                )
                machine = conn.execute("SELECT * FROM machines WHERE id = ?", (machine["id"],)).fetchone()
                token, exp = self._issue(machine, at)
                return Issued(token, machine["plan"], machine["until"], exp, already_redeemed=True)

            if machine is None:
                conn.execute(
                    "INSERT INTO machines(mid, last_seen_at, last_app_version) VALUES (?,?,?)",
                    (mid, at, app_version[:32]),
                )
                machine = conn.execute("SELECT * FROM machines WHERE mid = ?", (mid,)).fetchone()
            if machine["revoked_at"] is not None:
                raise LicenseError("machine_revoked", "这台电脑的授权已被停用，请联系客服", 403)
            if machine["plan"] == "lifetime":
                raise LicenseError("already_lifetime", "这台电脑已是永久授权，无需再兑换", 409)
            if card["plan"] == "trial" and machine["first_activated_at"] is not None:
                raise LicenseError("trial_not_allowed", "试用卡只能在从未激活过的电脑上使用", 409)

            if card["plan"] == "lifetime":
                new_plan, new_until = "lifetime", None
            else:
                base = max(at, machine["until"] or 0)
                new_plan, new_until = card["plan"], base + card["days"] * DAY

            changed = conn.execute(
                "UPDATE cards SET redeemed_mid = ?, redeemed_at = ? "
                "WHERE digest = ? AND redeemed_mid IS NULL AND revoked_at IS NULL",
                (mid, at, dig),
            ).rowcount
            if changed != 1:  # 理论上 BEGIN IMMEDIATE 下不会发生；发生就是 bug，绝不能静默放行
                raise LicenseError("card_used", "这张卡密已被使用", 409)
            conn.execute(
                "UPDATE machines SET plan = ?, until = ?, first_activated_at = COALESCE(first_activated_at, ?), "
                "last_seen_at = ?, last_app_version = ? WHERE id = ?",
                (new_plan, new_until, at, at, app_version[:32], machine["id"]),
            )
            audit(
                conn, "client", "activate", ip, mid, dig,
                plan=card["plan"], before=machine["until"], after=new_until,
            )
            machine = conn.execute("SELECT * FROM machines WHERE id = ?", (machine["id"],)).fetchone()
            token, exp = self._issue(machine, at)
        return Issued(token, new_plan, new_until, exp)

    def refresh(self, token: str, mid: str, app_version: str = "", ip: str = "") -> Issued:
        mid = check_mid(mid)
        try:
            claims = verify(token, self.trusted)
        except TokenError:
            raise LicenseError("token_invalid", "授权凭证无效，请重新激活", 401) from None
        if claims.mid != mid:
            raise LicenseError("token_invalid", "授权凭证与本机不符，请重新激活", 401)
        at = now()
        with self.db.write() as conn:
            machine = conn.execute("SELECT * FROM machines WHERE mid = ?", (mid,)).fetchone()
            if machine is None or machine["id"] != claims.lid:
                raise LicenseError("token_invalid", "授权记录不存在，请重新激活", 401)
            self._require_usable(machine, at)
            conn.execute(
                "UPDATE machines SET last_seen_at = ?, last_app_version = ? WHERE id = ?",
                (at, app_version[:32], machine["id"]),
            )
            token_new, exp = self._issue(machine, at)
        return Issued(token_new, machine["plan"], machine["until"], exp)

    # ------------------------------------------------------------------ 管理端

    def create_batch(self, plan_code: str, count: int, note: str, actor: str, ip: str = "") -> tuple[int, list[str]]:
        plan = get_plan(plan_code)
        note = (note or "").strip()[:200]
        codes = cards.generate_batch(count)
        at = now()
        with self.db.write() as conn:
            cur = conn.execute(
                "INSERT INTO batches(plan, days, count, note, created_by, created_at) VALUES (?,?,?,?,?,?)",
                (plan.code, plan.days, count, note, actor, at),
            )
            batch_id = cur.lastrowid
            conn.executemany(
                "INSERT INTO cards(digest, batch_id, plan, days, created_at) VALUES (?,?,?,?,?)",
                [(cards.digest(c), batch_id, plan.code, plan.days, at) for c in codes],
            )
            audit(conn, actor, "batch.create", ip, batch=batch_id, plan=plan.code, count=count, note=note)
        return batch_id, [cards.display(c) for c in codes]

    def revoke_batch(self, batch_id: int, actor: str, ip: str = "") -> int:
        at = now()
        with self.db.write() as conn:
            row = conn.execute("SELECT * FROM batches WHERE id = ?", (batch_id,)).fetchone()
            if row is None:
                raise LicenseError("not_found", "批次不存在", 404)
            if row["revoked_at"] is not None:
                raise LicenseError("already_revoked", "该批次已作废", 409)
            conn.execute("UPDATE batches SET revoked_at = ? WHERE id = ?", (at, batch_id))
            affected = conn.execute(
                "UPDATE cards SET revoked_at = ? WHERE batch_id = ? AND redeemed_mid IS NULL AND revoked_at IS NULL",
                (at, batch_id),
            ).rowcount
            audit(conn, actor, "batch.revoke", ip, batch=batch_id, unredeemed=affected)
        return affected

    def lookup_card(self, raw_card: str) -> dict:
        code = cards.normalize(raw_card)
        if code is None:
            raise LicenseError("card_invalid", "卡密格式不正确", 422)
        dig = cards.digest(code)
        row = self.db.read().execute(
            "SELECT c.*, b.note, b.revoked_at AS batch_revoked, m.id AS machine_id "
            "FROM cards c JOIN batches b ON b.id = c.batch_id LEFT JOIN machines m ON m.mid = c.redeemed_mid "
            "WHERE c.digest = ?",
            (dig,),
        ).fetchone()
        if row is None:
            raise LicenseError("not_found", "没有这张卡密", 404)
        if row["redeemed_mid"]:
            status = "redeemed"
        elif row["revoked_at"] or row["batch_revoked"]:
            status = "revoked"
        else:
            status = "unused"
        return {
            "display": cards.display(code),
            "status": status,
            "plan": row["plan"],
            "days": row["days"],
            "batch_id": row["batch_id"],
            "batch_note": row["note"],
            "created_at": row["created_at"],
            "redeemed_at": row["redeemed_at"],
            "redeemed_mid": row["redeemed_mid"],
            "machine_id": row["machine_id"],
            "revoked_at": row["revoked_at"] or row["batch_revoked"],
        }

    def revoke_card(self, raw_card: str, actor: str, ip: str = "") -> None:
        code = cards.normalize(raw_card)
        if code is None:
            raise LicenseError("card_invalid", "卡密格式不正确", 422)
        dig = cards.digest(code)
        with self.db.write() as conn:
            row = conn.execute("SELECT * FROM cards WHERE digest = ?", (dig,)).fetchone()
            if row is None:
                raise LicenseError("not_found", "没有这张卡密", 404)
            if row["redeemed_mid"] is not None:
                raise LicenseError("card_used", "这张卡已被兑换，不能作废；如需停用请作废对应电脑", 409)
            if row["revoked_at"] is not None:
                raise LicenseError("already_revoked", "这张卡已作废", 409)
            conn.execute("UPDATE cards SET revoked_at = ? WHERE digest = ?", (now(), dig))
            audit(conn, actor, "card.revoke", ip, card_digest=dig)

    def _machine(self, conn: sqlite3.Connection, machine_id: int) -> sqlite3.Row:
        row = conn.execute("SELECT * FROM machines WHERE id = ?", (machine_id,)).fetchone()
        if row is None:
            raise LicenseError("not_found", "没有这台电脑", 404)
        return row

    def revoke_machine(self, machine_id: int, reason: str, actor: str, ip: str = "") -> None:
        reason = (reason or "").strip()[:200]
        if not reason:
            raise LicenseError("reason_required", "请填写作废原因", 422)
        with self.db.write() as conn:
            m = self._machine(conn, machine_id)
            if m["revoked_at"] is not None:
                raise LicenseError("already_revoked", "这台电脑已被作废", 409)
            conn.execute(
                "UPDATE machines SET revoked_at = ?, revoke_reason = ? WHERE id = ?", (now(), reason, machine_id)
            )
            audit(conn, actor, "machine.revoke", ip, m["mid"], reason=reason)

    def unrevoke_machine(self, machine_id: int, actor: str, ip: str = "") -> None:
        with self.db.write() as conn:
            m = self._machine(conn, machine_id)
            if m["revoked_at"] is None:
                raise LicenseError("not_revoked", "这台电脑未被作废", 409)
            conn.execute("UPDATE machines SET revoked_at = NULL, revoke_reason = NULL WHERE id = ?", (machine_id,))
            audit(conn, actor, "machine.unrevoke", ip, m["mid"])

    # ------------------------------------------------------------------ 查询

    def stats(self) -> dict:
        conn = self.db.read()
        at = now()
        per_plan = {
            code: {"label": p.label, "total": 0, "unused": 0, "redeemed": 0, "revoked": 0} for code, p in PLANS.items()
        }
        for r in conn.execute(
            "SELECT c.plan, "
            "SUM(1) AS total, "
            "SUM(c.redeemed_mid IS NOT NULL) AS redeemed, "
            "SUM(c.redeemed_mid IS NULL AND (c.revoked_at IS NOT NULL OR b.revoked_at IS NOT NULL)) AS revoked "
            "FROM cards c JOIN batches b ON b.id = c.batch_id GROUP BY c.plan"
        ):
            d = per_plan.setdefault(r["plan"], {"label": r["plan"], "total": 0, "unused": 0, "redeemed": 0, "revoked": 0})
            d["total"], d["redeemed"], d["revoked"] = r["total"], r["redeemed"], r["revoked"]
            d["unused"] = r["total"] - r["redeemed"] - r["revoked"]
        m = conn.execute(
            "SELECT COUNT(*) AS activated, "
            "SUM(revoked_at IS NULL AND plan IS NOT NULL AND (until IS NULL OR until > ?)) AS active, "
            "SUM(revoked_at IS NULL AND until IS NOT NULL AND until > ? AND until <= ?) AS expiring "
            "FROM machines WHERE first_activated_at IS NOT NULL",
            (at, at, at + 7 * DAY),
        ).fetchone()
        day_ago = at - DAY
        alerts = conn.execute(
            "SELECT "
            "SUM(action = 'admin.login.fail') AS login_fail, "
            "SUM(action = 'activate.fail') AS activate_fail "
            "FROM audit WHERE at >= ?",
            (day_ago,),
        ).fetchone()
        login_ips = [
            r["ip"]
            for r in conn.execute(
                "SELECT DISTINCT ip FROM audit WHERE action = 'admin.login' AND at >= ? ORDER BY ip", (day_ago,)
            )
        ]
        recent = [
            dict(r)
            for r in conn.execute(
                "SELECT at, mid_prefix, detail FROM audit WHERE action = 'activate' ORDER BY id DESC LIMIT 20"
            )
        ]
        return {
            "plans": per_plan,
            "machines": {
                "activated": m["activated"] or 0,
                "active": m["active"] or 0,
                "expiring_7d": m["expiring"] or 0,
            },
            "alerts_24h": {
                "admin_login_fail": alerts["login_fail"] or 0,
                "activate_fail": alerts["activate_fail"] or 0,
                "admin_login_ips": login_ips,
            },
            "recent_activations": recent,
        }

    def list_batches(self, page: int = 1, size: int = 50) -> dict:
        conn = self.db.read()
        offset = (max(page, 1) - 1) * size
        total = conn.execute("SELECT COUNT(*) FROM batches").fetchone()[0]
        rows = conn.execute(
            "SELECT b.*, (SELECT COUNT(*) FROM cards c WHERE c.batch_id = b.id AND c.redeemed_mid IS NOT NULL) AS redeemed "
            "FROM batches b ORDER BY b.id DESC LIMIT ? OFFSET ?",
            (size, offset),
        ).fetchall()
        return {"total": total, "page": page, "size": size, "items": [dict(r) for r in rows]}

    def list_machines(self, q: str = "", status: str = "", page: int = 1, size: int = 50) -> dict:
        conn = self.db.read()
        at = now()
        where, params = ["1=1"], []
        q = (q or "").strip().lower()
        if q:
            if not re.fullmatch(r"[0-9a-f]{1,64}", q):
                raise LicenseError("bad_query", "机器码只能包含 0-9 和 a-f", 422)
            where.append("mid LIKE ?")
            params.append(q + "%")
        if status == "active":
            where.append("revoked_at IS NULL AND plan IS NOT NULL AND (until IS NULL OR until > ?)")
            params.append(at)
        elif status == "expired":
            where.append("revoked_at IS NULL AND until IS NOT NULL AND until <= ?")
            params.append(at)
        elif status == "revoked":
            where.append("revoked_at IS NOT NULL")
        elif status:
            raise LicenseError("bad_query", "未知的状态筛选", 422)
        clause = " AND ".join(where)
        total = conn.execute(f"SELECT COUNT(*) FROM machines WHERE {clause}", params).fetchone()[0]
        rows = conn.execute(
            f"SELECT * FROM machines WHERE {clause} ORDER BY last_seen_at DESC LIMIT ? OFFSET ?",
            [*params, size, (max(page, 1) - 1) * size],
        ).fetchall()
        return {"total": total, "page": page, "size": size, "items": [dict(r) for r in rows]}

    def machine_detail(self, machine_id: int) -> dict:
        conn = self.db.read()
        m = self._machine(conn, machine_id)
        redeemed = [
            {"digest_prefix": r["digest"][:8], "plan": r["plan"], "days": r["days"], "redeemed_at": r["redeemed_at"], "batch_id": r["batch_id"]}
            for r in conn.execute(
                "SELECT digest, plan, days, redeemed_at, batch_id FROM cards WHERE redeemed_mid = ? ORDER BY redeemed_at",
                (m["mid"],),
            )
        ]
        return {**dict(m), "cards": redeemed}

    def list_audit(self, action: str = "", actor: str = "", page: int = 1, size: int = 100) -> dict:
        conn = self.db.read()
        where, params = ["1=1"], []
        if action:
            where.append("action = ?")
            params.append(action[:64])
        if actor:
            where.append("actor = ?")
            params.append(actor[:64])
        clause = " AND ".join(where)
        total = conn.execute(f"SELECT COUNT(*) FROM audit WHERE {clause}", params).fetchone()[0]
        rows = conn.execute(
            f"SELECT * FROM audit WHERE {clause} ORDER BY id DESC LIMIT ? OFFSET ?",
            [*params, size, (max(page, 1) - 1) * size],
        ).fetchall()
        return {"total": total, "page": page, "size": size, "items": [dict(r) for r in rows]}
