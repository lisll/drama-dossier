"""处理配方：服务端签发、带签名、只能用一次、绑定机器码和任务（总方案 2.2 节、M4a 节）。

- 签发：随机种子 + 平台 + 强度 → 两层参数（第一层平台规格，第二层细微处理）。同一种子算出同样的参数，
  种子记进配方和 recipes 表，处理结果可以复现。平台只有 `universal` 一个取值（一套通用版本，
  取抖音 / 快手 / 视频号三家公开要求里最严的值），字段保留是为了不改签名格式。
- 签名用途前缀是 `recipe`，和授权凭证、报审包清单互不冒充。
- 消费：开工前桌面端联网消费一次；第二次消费返回 409，过期 410。只靠本地记录挡不住重放，所以在服务端记账。
- `task` 是桌面端算的任务描述哈希（规则见 COMMERCE.md），服务端只绑定、不解析：服务端看不到剧目内容。

第二层每档参数都有固定上下限（[`STRENGTHS`]），真正按参数处理视频、做画质损失检测在 M4b。
"""

from __future__ import annotations

import random
import secrets
import uuid
from typing import Dict, Tuple

from .db import audit
from .errors import CommerceError
from .license_token import PAYLOAD_VERSION, Signer
from .package_service import require_access
from .platform_specs import PLATFORM_SPECS
from .schemas import RecipeRequest
from .sessions import AuthContext

#: 配方有效期：签发后 24 小时内要开工。
RECIPE_TTL_SECONDS = 24 * 3600

#: 第二层细微处理的上下限。数值的含义：
#: - grain：ffmpeg `noise` 滤镜的 alls 强度（0–100）；
#: - zoom_percent：先放大再裁回原尺寸的比例（%），总方案定为 1%–3%；
#: - brightness：`eq` 滤镜亮度偏移（-1–1）；saturation：饱和度倍数；hue_degrees：色相偏移（度）；
#: - tempo：整体变速倍数（画面和声音一起）；pitch_semitones：变调（半音）；
#: - trim_head_frames / trim_tail_frames：片头片尾各裁几帧（扩展现有的「智能微调」）；
#: - min_ssim / min_psnr：画质损失检测的下限，处理后低于它就判定处理过头（M4b）。
STRENGTHS: Dict[str, Dict[str, Tuple[float, float]]] = {
    "light": {
        "grain": (1, 3), "zoom_percent": (1.0, 1.5), "brightness": (0.0, 0.01), "saturation": (0.98, 1.02),
        "hue_degrees": (0.0, 1.0), "tempo": (0.99, 1.01), "pitch_semitones": (0.0, 0.1),
        "trim_head_frames": (1, 2), "trim_tail_frames": (1, 2),
    },
    "medium": {
        "grain": (3, 6), "zoom_percent": (1.5, 2.2), "brightness": (0.0, 0.02), "saturation": (0.96, 1.04),
        "hue_degrees": (0.0, 2.0), "tempo": (0.98, 1.02), "pitch_semitones": (0.0, 0.2),
        "trim_head_frames": (2, 3), "trim_tail_frames": (2, 3),
    },
    "heavy": {
        "grain": (6, 10), "zoom_percent": (2.2, 3.0), "brightness": (0.0, 0.03), "saturation": (0.94, 1.06),
        "hue_degrees": (0.0, 3.0), "tempo": (0.97, 1.03), "pitch_semitones": (0.0, 0.3),
        "trim_head_frames": (3, 5), "trim_tail_frames": (3, 5),
    },
}
#: 画质下限（M4b-1 实测定）：每档取上面区间的上限参数，在合成测试画面、AI 动画成片、暗场低对比、
#: 近纯色暗画面四种素材上跑，逐帧最低 SSIM 分别为 light 0.988 / medium 0.947 / heavy 0.869，
#: PSNR 最低 43.1 / 37.2 / 33.1 dB。SSIM 对平坦区域的颗粒很敏感，下限比实测最低值留约 0.02–0.03 余量；
#: 把颗粒加到 40 的「处理过头」样本是 SSIM 0.44 / PSNR 26.3，三档都会判失败。
QUALITY_FLOORS = {"light": (0.97, 38.0), "medium": (0.92, 35.0), "heavy": (0.84, 32.0)}
#: 这几项是「偏移量」：上下限描述幅度，正负号随机。
_SIGNED = ("brightness", "hue_degrees", "pitch_semitones")
_INTEGER = ("grain", "trim_head_frames", "trim_tail_frames")


def derive_params(platform: str, strength: str, seed: int) -> dict:
    """同一组输入永远算出同样的参数（random.Random 的算法在 CPython 各版本间保持稳定）。"""
    rng = random.Random(seed)
    layer2 = {}
    for name, (low, high) in STRENGTHS[strength].items():
        if name in _INTEGER:
            value = rng.randint(int(low), int(high))
        else:
            value = round(rng.uniform(low, high), 4)
            if name in _SIGNED and rng.random() < 0.5:
                value = -value
        layer2[name] = value
    min_ssim, min_psnr = QUALITY_FLOORS[strength]
    return {
        "platform": dict(PLATFORM_SPECS[platform]),
        "subtle": layer2,
        "quality": {"min_ssim": min_ssim, "min_psnr": min_psnr},
    }


class RecipeService:
    def __init__(self, signer: Signer):
        self.signer = signer

    def issue(self, conn, ctx: AuthContext, payload: RecipeRequest) -> dict:
        require_access(conn, ctx.user_id, payload.device_id)
        recipe_id = str(uuid.uuid4())
        seed = secrets.randbits(63)
        params = derive_params(payload.platform, payload.strength, seed)
        with conn.transaction():
            row = conn.execute(
                """
                INSERT INTO recipes (id, user_id, device_id, task, platform, strength, seed, expires_at)
                VALUES (%s::uuid, %s::uuid, %s, %s, %s, %s, %s, now() + %s * interval '1 second')
                RETURNING floor(extract(epoch FROM issued_at))::bigint AS iat,
                          floor(extract(epoch FROM expires_at))::bigint AS exp
                """,
                (recipe_id, ctx.user_id, payload.device_id, payload.task, payload.platform, payload.strength,
                 seed, RECIPE_TTL_SECONDS),
            ).fetchone()
            audit(conn, ctx.user_id, "recipe.issue", recipe_id,
                  {"platform": payload.platform, "strength": payload.strength})
        token = self.signer.sign_envelope("recipe", {
            "v": PAYLOAD_VERSION,
            "kid": self.signer.kid,
            "rid": recipe_id,
            "sub": ctx.user_id,
            "dev": payload.device_id,
            "task": payload.task,
            "platform": payload.platform,
            "strength": payload.strength,
            "seed": seed,
            "params": params,
            "iat": row["iat"],
            "exp": row["exp"],
        })
        return {"recipe_id": recipe_id, "token": token, "expires_at": row["exp"], "params": params}

    def consume(self, conn, ctx: AuthContext, recipe_id: str, device_id: str) -> dict:
        with conn.transaction():
            row = conn.execute(
                """
                SELECT device_id, consumed_at, expires_at <= now() AS expired
                FROM recipes WHERE id = %s::uuid AND user_id = %s::uuid FOR UPDATE
                """,
                (recipe_id, ctx.user_id),
            ).fetchone()
            # 别人的配方和不存在的配方回同一个错误：不透露配方 ID 是否存在。
            if row is None:
                raise CommerceError(404, "recipe_not_found", "找不到这个处理配方")
            if row["device_id"] != device_id:
                raise CommerceError(403, "wrong_device", "这个处理配方不是发给这台电脑的")
            if row["consumed_at"] is not None:
                raise CommerceError(409, "recipe_consumed", "这个处理配方已经用过了，请重新申请")
            if row["expired"]:
                raise CommerceError(410, "recipe_expired", "这个处理配方已过期，请重新申请")
            require_access(conn, ctx.user_id, device_id)
            conn.execute("UPDATE recipes SET consumed_at = now() WHERE id = %s::uuid", (recipe_id,))
            audit(conn, ctx.user_id, "recipe.consume", recipe_id)
        return {"recipe_id": recipe_id, "consumed": True}
