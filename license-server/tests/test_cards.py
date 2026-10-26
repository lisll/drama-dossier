from dd_license import cards


def test_generate_uses_alphabet_and_length():
    for _ in range(200):
        c = cards.generate_code()
        assert len(c) == 16
        assert set(c) <= set(cards.ALPHABET)


def test_alphabet_excludes_confusable():
    assert not set("0O1I") & set(cards.ALPHABET)
    assert len(cards.ALPHABET) == 32


def test_display_and_normalize_round_trip():
    c = cards.generate_code()
    shown = cards.display(c)
    assert shown.startswith("DDK-") and shown.count("-") == 4
    assert cards.normalize(shown) == c
    assert cards.normalize(shown.lower()) == c
    assert cards.normalize("  " + shown.replace("-", " ") + " ") == c
    assert cards.normalize(c) == c


def test_normalize_rejects_garbage():
    for bad in ["", "DDK", "ABC", "0" * 16, "DDK-" + "I" * 16, "x" * 100, None, 123]:
        assert cards.normalize(bad) is None


def test_digest_is_stable_and_not_plain():
    c = "ABCDEFGHJKLMNPQR"
    d = cards.digest(c)
    assert d == cards.digest(c)
    assert c not in d and len(d) == 64


def test_batch_unique_and_bounds():
    codes = cards.generate_batch(500)
    assert len(codes) == len(set(codes)) == 500
    for n in (0, 10001):
        try:
            cards.generate_batch(n)
        except ValueError:
            pass
        else:
            raise AssertionError("应拒绝越界数量")
