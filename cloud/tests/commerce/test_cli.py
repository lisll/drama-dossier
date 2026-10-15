"""命令行：签名密钥（keygen / keyinfo）与卡密批次（generate / list / revoke / status）。"""

from __future__ import annotations

import csv
import json
import stat

import pytest

from drama_cloud.commerce import cards
from drama_cloud.commerce import license_token as lt
from drama_cloud.commerce.cli import main

from .conftest import auth


def run(capsys, *argv):
    code = main(list(argv))
    captured = capsys.readouterr()
    return code, captured.out, captured.err


def read_csv(path):
    with open(path, encoding="utf-8-sig", newline="") as handle:
        return list(csv.DictReader(handle))


@pytest.fixture()
def cli_env(monkeypatch, database_dsn, pg):
    monkeypatch.setenv("DRAMA_COMMERCE_DSN", database_dsn)
    return pg


def test_keygen_and_keyinfo(tmp_path, capsys):
    path = tmp_path / "keys" / "license.pem"
    code, out, err = run(capsys, "keygen", str(path))
    assert code == 0, err
    info = json.loads(out)
    signer = lt.Signer.from_pem_file(path)
    assert info == {"kid": signer.kid, "public_key": lt.b64url_encode(signer.public_key), "path": str(path)}
    assert stat.S_IMODE(path.stat().st_mode) == 0o600
    body = "".join(path.read_text().splitlines()[1:-1])
    assert body not in out + err  # 私钥不打印出来
    assert json.loads(run(capsys, "keyinfo", str(path))[1]) == info
    code, _, err = run(capsys, "keygen", str(path))
    assert code == 2 and "不会覆盖" in err
    assert lt.Signer.from_pem_file(path).kid == signer.kid
    code, _, err = run(capsys, "keyinfo", str(tmp_path / "missing.pem"))
    assert code == 2 and "keygen" in err


def test_generate_exports_plaintext_and_stores_digests(cli_env, tmp_path, capsys):
    pg = cli_env
    out_path = tmp_path / "cards.csv"
    code, out, err = run(capsys, "cards", "generate", "--days", "30", "--count", "25",
                         "--out", str(out_path), "--name", "十月月卡", "--by", "ops-alice")
    assert code == 0, err
    summary = json.loads(out)
    assert (summary["count"], summary["kind"], summary["days"], summary["name"]) == (25, "timed", 30, "十月月卡")
    assert stat.S_IMODE(out_path.stat().st_mode) == 0o600
    assert sorted(item.name for item in tmp_path.iterdir()) == ["cards.csv"]  # 临时文件已清掉
    rows = read_csv(out_path)
    assert len(rows) == 25 and len({row["code"] for row in rows}) == 25
    assert all(row["kind"] == "timed" and row["days"] == "30" and row["batch_name"] == "十月月卡" for row in rows)
    codes = [cards.normalize(row["code"]) for row in rows]
    assert all(code is not None and row["code"].startswith("DD-") for code, row in zip(codes, rows))
    stored = {bytes(row["code_digest"]) for row in pg.execute("SELECT code_digest FROM cards")}
    assert stored == {cards.digest(code) for code in codes}
    batch = pg.execute("SELECT id, kind, days, created_by FROM card_batches").fetchone()
    assert str(batch["id"]) == summary["batch_id"] and batch["created_by"] == "ops-alice"
    audit = pg.execute("SELECT actor, detail FROM audit_log WHERE action = 'cards.batch_create'").fetchone()
    assert audit["actor"] == "ops-alice" and audit["detail"] == {"count": 25, "kind": "timed", "days": 30}
    dumped = str(pg.execute("SELECT * FROM audit_log").fetchall()) + str(pg.execute("SELECT * FROM card_batches").fetchall())
    assert not any(code in dumped for code in codes)


def test_generate_refuses_bad_arguments(cli_env, tmp_path, capsys):
    pg = cli_env
    existing = tmp_path / "taken.csv"
    existing.write_text("keep me", encoding="utf-8")
    code, _, err = run(capsys, "cards", "generate", "--days", "30", "--count", "1", "--out", str(existing))
    assert code == 2 and "不会覆盖" in err and existing.read_text(encoding="utf-8") == "keep me"
    for extra in (["--days", "30", "--count", "0"], ["--days", "0", "--count", "1"],
                  ["--days", "30", "--count", "10001"], ["--lifetime", "--count", "1", "--name", " "]):
        code, _, err = run(capsys, "cards", "generate", *extra, "--out", str(tmp_path / "x.csv"))
        assert code == 2, extra
    with pytest.raises(SystemExit):  # --days 和 --lifetime 只能选一个，也必须选一个
        main(["cards", "generate", "--days", "30", "--lifetime", "--count", "1", "--out", str(tmp_path / "y.csv")])
    with pytest.raises(SystemExit):
        main(["cards", "generate", "--count", "1", "--out", str(tmp_path / "z.csv")])
    capsys.readouterr()
    assert pg.execute("SELECT count(*) AS n FROM card_batches").fetchone()["n"] == 0
    assert sorted(item.name for item in tmp_path.iterdir()) == ["taken.csv"]


def test_generate_cleans_up_when_database_is_down(monkeypatch, tmp_path, capsys):
    monkeypatch.setenv("DRAMA_COMMERCE_DSN", "postgresql://nobody@127.0.0.1:1/x?connect_timeout=2")
    code, _, err = run(capsys, "cards", "generate", "--lifetime", "--count", "3", "--out", str(tmp_path / "c.csv"))
    assert code == 2 and "无法连接数据库" in err
    assert list(tmp_path.iterdir()) == []  # 没写进数据库，明文也不能留在磁盘上


def test_list_status_and_revoke(cli_env, client, register, tmp_path, capsys):
    out_path = tmp_path / "lifetime.csv"
    assert run(capsys, "cards", "generate", "--lifetime", "--count", "2", "--out", str(out_path))[0] == 0
    used, unused = [row["code"] for row in read_csv(out_path)]
    listed = json.loads(run(capsys, "cards", "list")[1])
    batch_id = listed["batch_id"]
    assert (listed["kind"], listed["days"], listed["total"], listed["redeemed"]) == ("lifetime", None, 2, 0)

    def status(code):
        return json.loads(run(capsys, "cards", "status", code)[1])

    assert status(used)["state"] == "unused"
    account = register()
    response = client.post("/api/v1/cards/redeem", json={"code": used},
                           headers=auth(account["tokens"]["access_token"]))
    assert response.status_code == 200
    redeemed = status(used.lower())
    assert redeemed["state"] == "redeemed" and redeemed["redeemed_by"] == account["user"]["id"]
    code, _, err = run(capsys, "cards", "revoke", "--batch", batch_id)
    assert code == 2 and "--yes" in err and status(unused)["state"] == "unused"
    code, out, _ = run(capsys, "cards", "revoke", "--batch", batch_id, "--yes")
    assert code == 0 and json.loads(out) == {"batch_id": batch_id, "already_revoked": False, "unused": 1}
    assert json.loads(run(capsys, "cards", "revoke", "--batch", batch_id, "--yes")[1])["already_revoked"] is True
    assert status(unused)["state"] == "revoked" and status(used)["state"] == "redeemed"
    assert json.loads(run(capsys, "cards", "list")[1])["redeemed"] == 1
    code, out, _ = run(capsys, "cards", "status", cards.generate_code())
    assert code == 1 and json.loads(out) == {"state": "not_found"}
    assert run(capsys, "cards", "status", "not-a-card")[0] == 2
    assert run(capsys, "cards", "revoke", "--batch", "nope", "--yes")[0] == 2
    assert run(capsys, "cards", "revoke", "--batch", "00000000-0000-4000-8000-000000000000", "--yes")[0] == 2
