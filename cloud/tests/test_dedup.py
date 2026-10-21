"""V5 去重配方生成器与执行模块的技术验证测试（对照 Grove 逆向文档 §6）。"""
from __future__ import annotations

import json

import pytest

from drama_cloud.recipes.dedup import (
    IPHONE_DEVICE_POOL,
    DedupRecipeError,
    generate_dedup_recipe,
    recipe_sha256,
    validate_recipe,
)
from drama_cloud.modules.dedup import (
    VideoDedupParams,
    compile_dedup_audio,
    compile_dedup_finish,
    compile_dedup_video,
    format_wave,
)


def _make_recipe():
    return generate_dedup_recipe(seed=42, episode_count=3, width=720, height=1280)


class TestRecipeGenerator:
    def test_same_seed_same_recipe(self):
        """同 seed 必须产出字节级相同的配方（Grove 的 seed 可复现语义）。"""
        a = generate_dedup_recipe(seed=7, episode_count=2, width=1080, height=1920)
        b = generate_dedup_recipe(seed=7, episode_count=2, width=1080, height=1920)
        assert json.dumps(a, sort_keys=True) == json.dumps(b, sort_keys=True)
        assert recipe_sha256(a) == recipe_sha256(b)

    def test_different_seed_differs(self):
        a = generate_dedup_recipe(seed=1, episode_count=1, width=720, height=1280)
        b = generate_dedup_recipe(seed=2, episode_count=1, width=720, height=1280)
        assert a["variants"] != b["variants"]

    def test_recipe_passes_validation(self):
        recipe = _make_recipe()
        validate_recipe(recipe)  # 不抛即通过

    def test_reference_sha256_is_canonical_hash(self):
        recipe = _make_recipe()
        body = {k: v for k, v in recipe.items() if k != "referenceSha256"}
        assert recipe["referenceSha256"] == recipe_sha256(body)

    def test_device_pool_values(self):
        """伪装 make 全部来自 8 机型池（Grove 逆向实测池）。"""
        recipe = generate_dedup_recipe(seed=9, episode_count=16, width=720, height=1280)
        pool_makes = {make for make, _ in IPHONE_DEVICE_POOL}
        for episode in recipe["metadata"]["episodes"]:
            assert episode["make"] in pool_makes

    def test_out_of_range_rejected(self):
        recipe = _make_recipe()
        recipe["variants"][0]["speed"] = 1.5  # 越出 Grove 上限 1.25
        with pytest.raises(DedupRecipeError):
            validate_recipe(recipe)

    def test_odd_dimensions_rejected(self):
        with pytest.raises(DedupRecipeError):
            generate_dedup_recipe(seed=1, episode_count=1, width=721, height=1280)


class TestFilterCompilation:
    def test_wave_expression_format(self):
        expr = format_wave([1.0, 0.02, 60.0, 0.5], "t")
        assert expr.startswith("1.000000000000+")
        assert "sin(2*PI*t/60.000000000000+" in expr

    def test_video_chain_contains_core_filters(self):
        """母版链必须含 Grove 的核心去重滤镜：tmix 混帧 + 波形旋转 + 比例裁切。"""
        recipe = _make_recipe()
        params = VideoDedupParams(
            width=recipe["width"], height=recipe["height"],
            temporalWeights=recipe["temporalWeights"],
            variant=recipe["variants"][0],
        )
        chain = compile_dedup_video(params)
        joined = ",".join(chain)
        assert "tmix=frames=3:weights=" in joined
        assert "rotate=a=" in joined and "fillcolor=black" in joined
        assert "crop=trunc(iw*" in joined
        assert chain[0] == "format=yuv420p"
        assert chain[-1] == "setsar=1"

    def test_finish_chain_contains_grove_filters(self):
        recipe = _make_recipe()
        chain = compile_dedup_finish(recipe["variants"][0])
        joined = ",".join(chain)
        assert "eq=brightness=" in joined and "eval=frame" in joined
        assert "hue=h=" in joined
        assert "noise=alls=" in joined
        assert "unsharp=5:5:" in joined

    def test_audio_chain_speed_compensation(self):
        """asetrate 微移后 atempo 精确补偿，总变速恰为 speed（v5-video-clock/1）。"""
        recipe = _make_recipe()
        variant = recipe["variants"][0]
        chain = compile_dedup_audio(recipe["audio"], variant)
        assert chain[0] == "aresample=48000"
        assert chain[2].startswith("asetrate=")
        # 补偿 tempo = rate/shifted（correction 抵消手工 compensation）
        rate = 48000
        shifted = int(rate * variant["asetrateRatio"] + 0.5)
        assert chain[4] == f"atempo={rate / shifted:.12f}"
        assert chain[-1] == f"atempo={variant['speed']:.12f}"
        assert sum("equalizer=" in part for part in chain) == 4
        assert any(part.startswith("acompressor=") for part in chain)

    def test_safe_crop_ratio_limits_rotation_black_edges(self):
        """大旋转波形会把裁切比例压到安全值以内（照抄 Grove _geometry 投影）。"""
        from drama_cloud.modules.dedup import _safe_crop_ratio

        rotated = _safe_crop_ratio(720, 1280, 1.0, 1.0, [0.5, 0.5, 600.0, 0.0])
        assert rotated < 1.0
        assert _safe_crop_ratio(720, 1280, 1.0, 1.0, [0.0, 0.0, 600.0, 0.0]) == 1.0
