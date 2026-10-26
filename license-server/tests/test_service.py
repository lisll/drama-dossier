import threading
from unittest import mock

import pytest

from dd_license import db as dbmod
from dd_license import service as svcmod
from dd_license.errors import LicenseError
from dd_license.token import verify

from .conftest import mid

DAY = 86400


def one_card(service, plan="month"):
    _, codes = service.create_batch(plan, 1, "t", "boss")
    return codes[0]


def at(ts):
    """把 service 与 db 两处的 now() 同时固定到 ts。"""
    return mock.patch.multiple(svcmod, now=lambda: ts), mock.patch.object(dbmod, "now", lambda: ts)


def test_activate_issues_valid_token(service):
    code = one_card(service)
    issued = service.activate(code, mid(1), "0.2.18")
    c = verify(issued.token, [service.pub])
    assert c.mid == mid(1) and c.plan == "month"
    assert issued.until - c.iat == 30 * DAY
    assert c.exp - c.iat == 7 * DAY


def test_db_has_no_plaintext(service, db):
    code = one_card(service)
    service.activate(code, mid(1))
    dump = "\n".join(db.read().iterdump())
    raw = code.replace("DDK-", "").replace("-", "")
    assert raw not in dump and code not in dump


def test_card_used_by_other_machine(service):
    code = one_card(service)
    service.activate(code, mid(1))
    with pytest.raises(LicenseError) as e:
        service.activate(code, mid(2))
    assert e.value.code == "card_used"


def test_same_machine_reactivation_is_idempotent(service):
    code = one_card(service)
    first = service.activate(code, mid(1))
    again = service.activate(code, mid(1))
    assert again.already_redeemed and again.until == first.until


def test_concurrent_redeem_only_one_wins(service):
    code = one_card(service)
    results, errors = [], []
    barrier = threading.Barrier(8)

    def worker(i):
        barrier.wait()
        try:
            results.append(service.activate(code, mid(100 + i)))
        except LicenseError as exc:
            errors.append(exc.code)

    threads = [threading.Thread(target=worker, args=(i,)) for i in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert len(results) == 1
    assert errors == ["card_used"] * 7


def test_time_stacks(service):
    t0 = 1_800_000_000
    p1, p2 = at(t0)
    with p1, p2:
        a = service.activate(one_card(service), mid(1))
        b = service.activate(one_card(service, "quarter"), mid(1))
    assert a.until == t0 + 30 * DAY
    assert b.until == t0 + 120 * DAY


def test_expired_license_restarts_from_now(service):
    t0 = 1_800_000_000
    p1, p2 = at(t0)
    with p1, p2:
        service.activate(one_card(service), mid(1))
    later = t0 + 100 * DAY
    p1, p2 = at(later)
    with p1, p2:
        r = service.activate(one_card(service), mid(1))
    assert r.until == later + 30 * DAY


def test_trial_only_on_fresh_machine(service):
    service.activate(one_card(service, "trial"), mid(1))
    with pytest.raises(LicenseError) as e:
        service.activate(one_card(service, "trial"), mid(1))
    assert e.value.code == "trial_not_allowed"
    service.activate(one_card(service, "month"), mid(2))
    with pytest.raises(LicenseError):
        service.activate(one_card(service, "trial"), mid(2))


def test_lifetime(service):
    r = service.activate(one_card(service, "lifetime"), mid(1))
    c = verify(r.token, [service.pub])
    assert r.until is None and c.until is None and c.plan == "lifetime"
    with pytest.raises(LicenseError) as e:
        service.activate(one_card(service), mid(1))
    assert e.value.code == "already_lifetime"


def test_token_exp_capped_by_until(service):
    t0 = 1_800_000_000
    p1, p2 = at(t0)
    with p1, p2:
        r = service.activate(one_card(service, "trial"), mid(1))
    assert r.exp == t0 + 3 * DAY  # 试用 3 天 < 凭证 7 天


def test_revoked_card_and_batch(service):
    batch, codes = service.create_batch("month", 3, "", "boss")
    service.activate(codes[0], mid(1))
    service.revoke_card(codes[1], "boss")
    with pytest.raises(LicenseError) as e:
        service.activate(codes[1], mid(2))
    assert e.value.code == "card_revoked"
    assert service.revoke_batch(batch, "boss") == 1  # 只剩 codes[2] 未兑换
    with pytest.raises(LicenseError) as e:
        service.activate(codes[2], mid(3))
    assert e.value.code == "card_revoked"
    # 已兑换的不受批次作废影响
    assert service.activate(codes[0], mid(1)).already_redeemed


def test_cannot_revoke_redeemed_card(service):
    code = one_card(service)
    service.activate(code, mid(1))
    with pytest.raises(LicenseError) as e:
        service.revoke_card(code, "boss")
    assert e.value.code == "card_used"


def test_refresh_and_revoke_machine(service, db):
    r = service.activate(one_card(service), mid(1))
    r2 = service.refresh(r.token, mid(1))
    assert verify(r2.token, [service.pub]).mid == mid(1)
    m_id = db.read().execute("SELECT id FROM machines WHERE mid=?", (mid(1),)).fetchone()[0]
    service.revoke_machine(m_id, "退款", "boss")
    with pytest.raises(LicenseError) as e:
        service.refresh(r.token, mid(1))
    assert e.value.code == "machine_revoked"
    service.unrevoke_machine(m_id, "boss")
    service.refresh(r.token, mid(1))


def test_refresh_rejects_other_machine_and_forged(service):
    r = service.activate(one_card(service), mid(1))
    with pytest.raises(LicenseError) as e:
        service.refresh(r.token, mid(2))
    assert e.value.code == "token_invalid"
    with pytest.raises(LicenseError):
        service.refresh(r.token[:-2] + "AA", mid(1))


def test_refresh_after_until_expired(service):
    t0 = 1_800_000_000
    p1, p2 = at(t0)
    with p1, p2:
        r = service.activate(one_card(service, "trial"), mid(1))
    p1, p2 = at(t0 + 4 * DAY)
    with p1, p2:
        with pytest.raises(LicenseError) as e:
            service.refresh(r.token, mid(1))
    assert e.value.code == "license_expired"


def test_binding_is_fixed_no_transfer(service):
    """固定绑定：没有迁移接口；旧机的凭证在新机上续签会被拒。"""
    assert not hasattr(service, "transfer_machine")
    r = service.activate(one_card(service, "year"), mid(1))
    with pytest.raises(LicenseError) as e:
        service.refresh(r.token, mid(2))
    assert e.value.code == "token_invalid"

def test_stats_and_lists(service):
    service.create_batch("month", 5, "a", "boss")
    service.activate(one_card(service, "year"), mid(1))
    s = service.stats()
    assert s["plans"]["month"]["total"] == 5 and s["plans"]["month"]["unused"] == 5
    assert s["plans"]["year"]["redeemed"] == 1
    assert s["machines"]["active"] == 1
    assert service.list_batches()["total"] == 2
    assert service.list_machines(q=mid(1)[:6])["total"] == 1
    with pytest.raises(LicenseError):
        service.list_machines(q="'; DROP TABLE cards; --")
    assert service.list_audit(action="activate")["total"] == 1


def test_bad_inputs(service):
    with pytest.raises(LicenseError):
        service.activate("nope", mid(1))
    with pytest.raises(LicenseError):
        service.activate(one_card(service), "XYZ")
    with pytest.raises(LicenseError) as e:
        service.activate("DDK-2222-2222-2222-2222", mid(1))
    assert e.value.code == "card_invalid"
