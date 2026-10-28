"""V5 式去重配方生成器（技术验证，对照 Grove Max 逆向文档 §6）。

Grove Max 的做法是「服务端签发配方、客户端纯执行」：配方里每个数值都不是固定值，
而是以正弦波形（base + amp·sin(2π·clock/period + phase)）随时间/帧号微扰，音频另做
asetrate 音调微移 + atempo 补偿 + 4 段 EQ + 压缩器随机化，元数据写入随机 iPhone
机型伪装实拍。本模块在本地复现这条「配方签发」链路（不联网）：

- 所有随机量都从同一个 ``seed`` 派生（``random.Random(seed)``），同 seed 必产出
  字节级相同的配方——对应 Grove 的 ``seed`` 可复现语义；
- 每个参数的取值范围严格对照 Grove ``reference_profile.py`` 的 ``_validate``
  （逆向采集的服务端签发参数空间），配方生成后用同一套范围自检；
- 配方 SHA-256 作为 ``referenceSha256`` 落进配方本体——对应 Grove 的全链哈希溯源。

「波形」四元组 ``[base, amp, period, phase]``：period 必须 > 0；clock 用 ``n``
（帧号）或 ``t``（秒）由执行端决定（与 Grove 一致：几何类用 n、色彩类用 t）。
"""
from __future__ import annotations

import hashlib
import json
import math
import random
from datetime import datetime, timedelta, timezone
from typing import Dict, List

# ---------------------------------------------------------------------------
# 参数空间（对照 Grove reference_profile.py _validate 的范围，逆向证据）
# ---------------------------------------------------------------------------

SPEED_RANGE = (1.0, 1.25)
CROP_RATIO_RANGE = (0.8, 1.0)
NOISE_RANGE = (0, 20)
SHARPEN_RANGE = (0.0, 3.0)
ASETRATE_RATIO_RANGE = (0.9, 1.1)
TEMPO_COMPENSATION_RANGE = (0.5, 2.0)
EQ_BANDS = 4
EQ_FREQ_RANGE = (20.0, 18000.0)  # 上限还受采样率/2 约束（见 _validate）
EQ_WIDTH_Q_RANGE = (0.1, 3000.0)
EQ_GAIN_RANGE = (-12.0, 12.0)
COMPRESSOR_RANGE = {
    "thresholdDb": (-60.0, 0.0),
    "ratio": (1.0, 20.0),
    "attackMs": (0.01, 2000.0),
    "releaseMs": (0.01, 9000.0),
    "makeup": (1.0, 64.0),
}
ENCODE_BITRATE_RANGE = (4_000_000, 30_000_000)
KEYFRAME_INTERVAL_RANGE = (1, 1000)
SCALE_FACTOR_RANGE = (0.25, 4.0)
BOX_SIZE_RANGE = (2, 512)
BOX_OPACITY_RANGE = (0.0, 1.0)
BOX_COLORS = ("white", "black")
BOX_ANCHORS_X = ("left", "right")
BOX_ANCHORS_Y = ("top", "bottom")

#: Grove 逆向实测的 8 机型池（Apple iPhone 15 Pro / iOS 17.6.1 为实测种子机型）。
IPHONE_DEVICE_POOL = (
    ("Apple iPhone 15 Pro Max", "iOS 17.6.1"),
    ("Apple iPhone 15 Pro", "iOS 17.6.1"),
    ("Apple iPhone 15", "iOS 17.5.1"),
    ("Apple iPhone 14 Pro Max", "iOS 17.5.1"),
    ("Apple iPhone 14 Pro", "iOS 17.4"),
    ("Apple iPhone 14", "iOS 17.4"),
    ("Apple iPhone 13 Pro", "iOS 17.3.1"),
    ("Apple iPhone 13", "iOS 17.3.1"),
)

ENCODE_TAG_COMMENT_MAX = 73
ENCODE_TAG_COPYRIGHT_MAX = 128
ENCODE_TAG_MAKE_MAX = 64

#: 伪装成「手机实拍」的编码器串（Grove 实测指向 Apple 硬件编码器）。
DISGUISE_ENCODER = "Apple AVFoundation H.264 Encoder"

#: 伪装拍摄时间的确定性派生纪元（2023-11-14T22:13:20Z）；同 seed 配方字节级可复现。
_CREATION_EPOCH = 1_700_000_000


class DedupRecipeError(ValueError):
    """配方参数越界（生成后自检失败）。"""


def _wave(rng: random.Random, base_low: float, base_high: float, amp: float,
          period_low: float, period_high: float) -> List[float]:
    """生成一个 [base, amp, period, phase] 波形。period 必须 > 0（越界即无效配方）。"""
    return [
        round(rng.uniform(base_low, base_high), 12),
        round(amp * rng.random(), 12),
        round(rng.uniform(period_low, period_high), 12),
        round(rng.uniform(-math.pi, math.pi), 12),
    ]


def _geometry(rng: random.Random) -> List[Dict]:
    """每集一个 box 对（对应 Grove variants[].boxes，2 个：AI 角标/水印候选位）。"""
    boxes = []
    for _ in range(2):
        boxes.append(
            {
                "xWave": _wave(rng, 4.0, 40.0, 6.0, 240.0, 3600.0),
                "yWave": _wave(rng, 4.0, 40.0, 6.0, 240.0, 3600.0),
                "width": rng.randint(*BOX_SIZE_RANGE),
                "height": rng.randint(24, 96),
                "opacity": round(rng.uniform(*BOX_OPACITY_RANGE), 12),
                "color": rng.choice(BOX_COLORS),
                "xAnchor": rng.choice(BOX_ANCHORS_X),
                "yAnchor": rng.choice(BOX_ANCHORS_Y),
            }
        )
    return boxes


def _variant(rng: random.Random, rate: int) -> Dict:
    """一集的全部随机参数（对应 Grove reference 的 variants[] 元素）。"""
    eq = []
    for _ in range(EQ_BANDS):
        freq = round(rng.uniform(*EQ_FREQ_RANGE), 12)
        if freq >= rate / 2:
            freq = round(rate / 2 - 100, 12)
        eq.append(
            {
                "frequencyHz": freq,
                "widthQ": round(rng.uniform(*EQ_WIDTH_Q_RANGE), 12),
                "gainDb": round(rng.uniform(*EQ_GAIN_RANGE), 12),
            }
        )
    compressor = {key: round(rng.uniform(low, high), 12) for key, (low, high) in COMPRESSOR_RANGE.items()}
    return {
        "speed": round(rng.uniform(*SPEED_RANGE), 12),
        "cropRatio": round(rng.uniform(*CROP_RATIO_RANGE), 12),
        # 单位：度（±1.2°、摆动 ≤0.8° 的微旋转）；编译 ffmpeg rotate 时才换算成弧度。
        "rotationWave": _wave(rng, -1.2, 1.2, 0.8, 360.0, 3600.0),
        "contrastWave": _wave(rng, 1.0, 1.04, 0.02, 60.0, 600.0),
        "brightnessWave": _wave(rng, -0.01, 0.01, 0.008, 60.0, 600.0),
        "saturationWave": _wave(rng, 0.97, 1.01, 0.02, 60.0, 600.0),
        "hueWave": _wave(rng, -2.0, 2.0, 2.0, 90.0, 900.0),
        "hueSaturationWave": _wave(rng, 0.98, 1.02, 0.02, 90.0, 900.0),
        "noiseStrength": rng.randint(*NOISE_RANGE),
        "sharpenAmount": round(rng.uniform(*SHARPEN_RANGE), 12),
        "asetrateRatio": round(rng.uniform(*ASETRATE_RATIO_RANGE), 12),
        "audioTempoCompensation": round(rng.uniform(*TEMPO_COMPENSATION_RANGE), 12),
        "equalizer": eq,
        "compressor": compressor,
        "bFrames": 0,
        "referenceFrames": 4,
        "sceneCutThreshold": rng.randint(0, 100),
        "boxes": _geometry(rng),
    }


def validate_recipe(recipe: Dict) -> None:
    """按 Grove ``_validate`` 的范围自检；越界抛 :class:`DedupRecipeError`。"""
    def check(cond: bool, detail: str) -> None:
        if not cond:
            raise DedupRecipeError(detail)

    check(isinstance(recipe, dict), "RECIPE_NOT_OBJECT")
    check(recipe["version"] in (1, 2), "VERSION")
    check(recipe["width"] % 2 == 0 and 2 <= recipe["width"] <= 7680, "WIDTH")
    check(recipe["height"] % 2 == 0 and 2 <= recipe["height"] <= 7680, "HEIGHT")
    check(1 <= recipe["frameRate"] <= 240, "FRAME_RATE")
    check(0.25 <= recipe["scaleFactor"] <= 4.0, "SCALE_FACTOR")
    weights = recipe["temporalWeights"]
    check(len(weights) == 3, "TEMPORAL_WEIGHTS")
    check(all(0 <= w <= 100 for w in weights), "TEMPORAL_WEIGHTS_RANGE")
    check(any(round(float(w), 12) > 0 for w in weights), "TEMPORAL_WEIGHTS_EMPTY")

    encoding = recipe["encoding"]
    expected = {"encoder": "libx264", "preset": "medium", "profile": "high",
                "pixelFormat": "yuv420p", "nalHrd": "cbr"}
    for key, value in expected.items():
        check(encoding[key] == value, f"ENCODING_{key.upper()}")
    target = encoding["bitrateTargetBps"]
    check(ENCODE_BITRATE_RANGE[0] <= target <= ENCODE_BITRATE_RANGE[1], "BITRATE")
    check(encoding["bitrateBufferBps"] >= target, "BITRATE_BUFFER")
    interval = encoding["keyframeInterval"]
    check(KEYFRAME_INTERVAL_RANGE[0] <= interval <= KEYFRAME_INTERVAL_RANGE[1], "KEYFRAME_INTERVAL")
    check(1 <= encoding["keyframeMinInterval"] <= interval // 2 + 1, "KEYFRAME_MIN_INTERVAL")
    check(encoding["filler"] is True, "FILLER_REQUIRED")

    audio = recipe["audio"]
    rate = audio["sampleRateHz"]
    valid_rates = {7350, 8000, 11025, 12000, 16000, 22050, 24000, 32000,
                   44100, 48000, 64000, 88200, 96000}
    check(rate in valid_rates, "SAMPLE_RATE")
    check(audio["channels"] == 2, "CHANNELS")
    check(32_000 <= audio["bitrateBps"] <= min(512_000, rate * audio["channels"] * 6), "AUDIO_BITRATE")

    check(1 <= len(recipe["variants"]) <= 500, "VARIANTS")
    metadata = recipe["metadata"]
    check(bool(metadata["encoder"].strip()) and len(metadata["encoder"]) <= 128, "ENCODER_TAG")
    check(isinstance(metadata["creationTime"], str) and metadata["creationTime"].endswith("Z"), "CREATION_TIME")
    check(len(metadata["episodes"]) == len(recipe["variants"]), "EPISODES")
    for item in metadata["episodes"]:
        check(0 < len(item["comment"]) <= ENCODE_TAG_COMMENT_MAX, "COMMENT")
        check(0 < len(item["copyright"]) <= ENCODE_TAG_COPYRIGHT_MAX, "COPYRIGHT")
        check(0 < len(item["make"]) <= ENCODE_TAG_MAKE_MAX, "MAKE")
    for variant in recipe["variants"]:
        check(SPEED_RANGE[0] <= variant["speed"] <= SPEED_RANGE[1], "SPEED")
        check(CROP_RATIO_RANGE[0] <= variant["cropRatio"] <= CROP_RATIO_RANGE[1], "CROP_RATIO")
        for key in ("rotationWave", "contrastWave", "brightnessWave", "saturationWave",
                    "hueWave", "hueSaturationWave"):
            wave = variant[key]
            check(len(wave) == 4, f"{key}_SHAPE")
            check(wave[2] > 1e-12, f"{key}_PERIOD")
        check(0 <= variant["noiseStrength"] <= 20, "NOISE")
        check(0 <= variant["sharpenAmount"] <= 3, "SHARPEN")
        check(0.9 <= variant["asetrateRatio"] <= 1.1, "ASETRATE")
        check(0.5 <= variant["audioTempoCompensation"] <= 2.0, "TEMPO")
        check(len(variant["equalizer"]) == EQ_BANDS, "EQ_BANDS")
        for band in variant["equalizer"]:
            check(20 <= band["frequencyHz"] < rate / 2, "EQ_FREQ")
            check(0.1 <= band["widthQ"] <= 3000, "EQ_WIDTH")
            check(-12 <= band["gainDb"] <= 12, "EQ_GAIN")
        comp = variant["compressor"]
        for key, (low, high) in COMPRESSOR_RANGE.items():
            check(low <= comp[key] <= high, f"COMPRESSOR_{key.upper()}")
        check(0 <= variant["bFrames"] <= min(16, interval - 1), "B_FRAMES")
        check(1 <= variant["referenceFrames"] <= 16, "REFERENCE_FRAMES")
        check(0 <= variant["sceneCutThreshold"] <= 100, "SCENECUT")
        check(len(variant["boxes"]) == 2, "BOXES")
        for box in variant["boxes"]:
            check(box["color"] in BOX_COLORS, "BOX_COLOR")
            check(box["xAnchor"] in BOX_ANCHORS_X and box["yAnchor"] in BOX_ANCHORS_Y, "BOX_ANCHOR")
            check(0 <= box["opacity"] <= 1, "BOX_OPACITY")
            check(box["xWave"][2] > 1e-12 and box["yWave"][2] > 1e-12, "BOX_WAVE_PERIOD")


def recipe_sha256(recipe: Dict) -> str:
    """规范化 JSON 的 SHA-256（对应 Grove 的 referenceSha256 溯源）。"""
    canonical = json.dumps(recipe, ensure_ascii=False, sort_keys=True,
                           separators=(",", ":"), allow_nan=False)
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()


def generate_dedup_recipe(
    seed: int,
    episode_count: int,
    width: int,
    height: int,
    frame_rate: int = 30,
    series_name: str = "示例剧",
) -> Dict:
    """从 seed 确定性地签发一份完整去重配方。

    同一 seed + 同一组入参，输出字节级一致（单测覆盖）。参数空间与 Grove
    reference_profile.py 的 ``_validate`` 对齐，生成后立即自检。
    """
    if episode_count < 1 or episode_count > 500:
        raise DedupRecipeError("分集数量须在 1~500")
    if width % 2 or height % 2:
        raise DedupRecipeError("宽高必须为偶数（yuv420p）")

    rng = random.Random(seed)
    bitrate = rng.randint(*ENCODE_BITRATE_RANGE)
    keyframe_interval = 250
    # 伪装拍摄时间从 seed 确定性派生（Grove 的 creationTime 本质是伪装值，不应把
    # 真实时钟带进配方——否则同 seed 配方无法字节级复现，checkpoint 断点续跑时
    # 重算配方的 referenceSha256 会与已产物信封的哈希失配）。固定纪元 + seed 偏移。
    creation = (
        datetime.fromtimestamp(_CREATION_EPOCH + seed % 86_400, tz=timezone.utc)
        .replace(microsecond=0)
        - timedelta(minutes=episode_count)
    )

    variants = [_variant(rng, 48_000) for _ in range(episode_count)]
    episodes_meta = []
    for _ in range(episode_count):
        make, ios = rng.choice(IPHONE_DEVICE_POOL)
        stamp = (creation + timedelta(minutes=len(episodes_meta))).strftime("%Y%m%d%H%M%S")
        episodes_meta.append(
            {
                "comment": f"{series_name} 第{len(episodes_meta) + 1}集",
                "copyright": f"{series_name} {stamp}",
                "make": make,
                "software": ios,
            }
        )

    recipe = {
        "version": 1,
        "seed": seed,
        "seriesName": series_name,
        "width": width,
        "height": height,
        "frameRate": frame_rate,
        "scaleFactor": 1.0,
        # 时域混帧权重：当前帧为主、前后帧低权重（Grove 实测 previousWeight 0.25）。
        "temporalWeights": [25, 50, 25],
        "encoding": {
            "encoder": "libx264",
            "preset": "medium",
            "profile": "high",
            "pixelFormat": "yuv420p",
            "nalHrd": "cbr",
            "bitrateTargetBps": bitrate,
            "bitrateBufferBps": bitrate * 2,
            "keyframeInterval": keyframe_interval,
            "keyframeMinInterval": 25,
            "aqMode": 2,
            "threads": 0,
            "filterThreads": 2,
            "forceCfr": True,
            "filler": True,
            "faststart": True,
        },
        "audio": {"sampleRateHz": 48_000, "channels": 2, "bitrateBps": 192_000},
        "metadata": {
            "encoder": DISGUISE_ENCODER,
            "creationTime": creation.strftime("%Y-%m-%dT%H:%M:%S.000000Z"),
            "episodes": episodes_meta,
        },
        "variants": variants,
    }
    recipe["referenceSha256"] = recipe_sha256(recipe)
    validate_recipe(recipe)
    return recipe
