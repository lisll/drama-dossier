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
from drama_cloud.demo_dedup import (
    _load_checkpoint,
    _make_logger,
    _record_stage,
    _stage_valid,
    sha256_file,
)
from drama_cloud.errors import WorkError


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
        # 旋转波形单位是度：最坏 |base|+amp = 2° 也只裁很窄一圈（以前把度数当弧度只剩 0.49）。
        for width, height in [(1080, 1920), (1920, 1080), (360, 640)]:
            worst = _safe_crop_ratio(width, height, 1.0, 1.0, [1.2, 0.8, 600.0, 0.0])
            assert 0.93 <= worst < 1.0, (width, height, worst)

    def test_rotation_converted_to_radians_for_ffmpeg(self):
        """配方旋转波形是度，交给 ffmpeg rotate 的必须是弧度（ffmpeg -h filter=rotate：in radians）。"""
        import math

        recipe = _make_recipe()
        variant = recipe["variants"][0]
        params = VideoDedupParams(
            width=recipe["width"], height=recipe["height"],
            temporalWeights=recipe["temporalWeights"],
            variant=variant,
        )
        rotate = next(part for part in compile_dedup_video(params) if part.startswith("rotate=a='"))
        expr = rotate[len("rotate=a='"):rotate.index("'", len("rotate=a='"))]
        base_text, rest = expr.split("+", 1)
        amp_text = rest.split("*", 1)[0]
        base_deg, amp_deg = variant["rotationWave"][0], variant["rotationWave"][1]
        assert math.isclose(float(base_text), math.radians(base_deg), abs_tol=1e-11)
        assert math.isclose(float(amp_text), math.radians(amp_deg), abs_tol=1e-11)
        assert abs(float(base_text)) + abs(float(amp_text)) < 0.05


class TestAudioBackend:
    """变调后端（Grove 逆向文档 §12.3：rubberband 优先、atempo 链兜底）。"""

    def test_rubberband_backend_replaces_atempo_head(self):
        recipe = _make_recipe()
        variant = recipe["variants"][0]
        chain = compile_dedup_audio(recipe["audio"], variant, backend="rubberband")
        joined = ",".join(chain)
        assert f"rubberband=tempo={variant['speed']:.12f}:pitch={variant['asetrateRatio']:.12f}" in joined
        assert "asetrate=" not in joined
        assert "atempo=" not in joined  # tempo 已折入 rubberband，链尾不再追加
        assert sum("equalizer=" in part for part in chain) == 4
        assert any(part.startswith("acompressor=") for part in chain)

    def test_rubberband_net_speed_matches_atempo_tail(self):
        """两后端净语义一致：rubberband 的 tempo 折入总变速，等于 atempo 链尾的 speed。"""
        recipe = _make_recipe()
        variant = recipe["variants"][0]
        atempo_chain = compile_dedup_audio(recipe["audio"], variant, backend="atempo")
        rubber_chain = compile_dedup_audio(recipe["audio"], variant, backend="rubberband")
        assert atempo_chain[-1] == f"atempo={variant['speed']:.12f}"
        tempo_part = next(p for p in rubber_chain if p.startswith("rubberband=tempo="))
        assert tempo_part.split("tempo=")[1].split(":")[0] == f"{variant['speed']:.12f}"

    def test_default_backend_keeps_legacy_chain(self):
        """默认 atempo 后端必须与历史行为逐字节一致（不破坏已有配方/产物）。"""
        recipe = _make_recipe()
        chain = compile_dedup_audio(recipe["audio"], recipe["variants"][0])
        assert chain[0] == "aresample=48000"
        assert chain[2].startswith("asetrate=")
        assert chain[-1].startswith("atempo=")

    def test_unknown_backend_rejected(self):
        recipe = _make_recipe()
        with pytest.raises(WorkError):
            compile_dedup_audio(recipe["audio"], recipe["variants"][0], backend="nope")


class TestCheckpointResume:
    """断点续跑（Grove 逆向文档 §12.2 母版续跑 + checkpoint.json 语义）。"""

    @staticmethod
    def _config(**overrides):
        config = {"seed": 42, "episode": 1, "series": "剧", "badge": "",
                  "watermark": "", "timeStretch": "atempo", "inputSha256": "a" * 64}
        config.update(overrides)
        return config

    def test_load_missing_checkpoint_returns_fresh(self, tmp_path):
        cp = _load_checkpoint(tmp_path, self._config(), lambda line: None)
        assert cp["stages"] == {}
        assert cp["config"] == self._config()

    def test_load_valid_checkpoint_keeps_stages(self, tmp_path):
        (tmp_path / "checkpoint.json").write_text(json.dumps(
            {"schemaVersion": 1, "config": self._config(),
             "stages": {"firstEncode": {"status": "done", "outputs": {}}}}),
            encoding="utf-8")
        cp = _load_checkpoint(tmp_path, self._config(), lambda line: None)
        assert "firstEncode" in cp["stages"]

    def test_load_config_mismatch_resets(self, tmp_path):
        """seed/输入/文案等配置变化 → 全量重跑（Grove：seed 不一致全部重来）。"""
        (tmp_path / "checkpoint.json").write_text(json.dumps(
            {"schemaVersion": 1, "config": self._config(seed=7),
             "stages": {"firstEncode": {"status": "done", "outputs": {}}}}),
            encoding="utf-8")
        lines = []
        cp = _load_checkpoint(tmp_path, self._config(), lines.append)
        assert cp["stages"] == {}
        assert any("全量重跑" in line for line in lines)

    def test_load_corrupted_resets(self, tmp_path):
        (tmp_path / "checkpoint.json").write_text("{not json", encoding="utf-8")
        lines = []
        cp = _load_checkpoint(tmp_path, self._config(), lines.append)
        assert cp["stages"] == {}
        assert any("全量重跑" in line for line in lines)

    def test_stage_valid_requires_matching_sha256(self, tmp_path):
        target = tmp_path / "first-encode.mp4"
        target.write_bytes(b"payload")
        good = {"status": "done", "outputs": {"first-encode.mp4": sha256_file(target)}}
        assert _stage_valid(tmp_path, good, ("first-encode.mp4",))
        # 产物 sha 不匹配 / 产物缺失 / 清单不一致 / 状态非 done 都视为未完成
        assert not _stage_valid(tmp_path, {"status": "done",
                                           "outputs": {"first-encode.mp4": "0" * 64}},
                                ("first-encode.mp4",))
        assert not _stage_valid(tmp_path, good, ("first-encode.mp4", "overlay/x.png"))
        assert not _stage_valid(tmp_path, {"status": "pending"}, ("first-encode.mp4",))
        target.unlink()
        assert not _stage_valid(tmp_path, good, ("first-encode.mp4",))

    def test_record_stage_drops_downstream(self, tmp_path):
        """上游重做后下游记录作废（下游产物由上游派生）。"""
        cp = {"schemaVersion": 1, "config": {}, "stages": {
            "firstEncode": {"status": "done", "outputs": {}},
            "overlay": {"status": "done", "outputs": {}},
            "finalEncode": {"status": "done", "outputs": {}},
        }}
        _record_stage(tmp_path, cp, "firstEncode",
                      {"first-encode.mp4": "x" * 64}, lambda line: None)
        assert "firstEncode" in cp["stages"]
        assert "overlay" not in cp["stages"]
        assert "finalEncode" not in cp["stages"]
        assert (tmp_path / "checkpoint.json").is_file()

    def test_logger_writes_per_episode_log(self, tmp_path):
        """per-episode 日志命名（Grove episode-<ep>.log 规范）+ 追加双写。"""
        log, path = _make_logger(tmp_path, 3)
        log("hello")
        log("world again")
        assert path.name == "episode-3.log"
        content = path.read_text(encoding="utf-8")
        assert "hello" in content and "world again" in content
        assert content.count("\n") == 2
