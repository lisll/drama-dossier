"""Python 侧自检：向量文件必须与生成器一致；服务端验签对每条向量的结论与期望一致。"""

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

from dd_license.token import TokenError, verify

from .vectors import TEST_SEED, VECTOR_PATH, build_vectors, render


def test_vector_file_matches_generator():
    assert VECTOR_PATH.exists(), "请先执行 python -m tests.vectors 生成向量文件"
    assert VECTOR_PATH.read_text(encoding="utf-8") == render(), "向量文件与生成器不一致：请重新生成并同步提交"


def test_server_verifier_agrees_on_signature_and_payload():
    pub = Ed25519PrivateKey.from_private_bytes(TEST_SEED).public_key()
    for c in build_vectors()["cases"]:
        try:
            claims = verify(c["token"], [pub])
        except TokenError:
            assert c["expect"] in {"malformed", "bad_signature", "bad_payload"}, c["name"]
            continue
        # 服务端不判断机器/时间，签名与载荷有效即可；其余结论由客户端 decide 给出
        assert c["expect"] in {"ok", "expired", "wrong_machine", "clock_rollback"}, c["name"]
        if c["expect"] == "ok":
            assert (claims.plan, claims.until, claims.exp) == (c["plan"], c["until"], c["exp"]), c["name"]
