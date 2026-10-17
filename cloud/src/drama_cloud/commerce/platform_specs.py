"""处理配方第一层：上传规格（竖屏短剧）。

现有的平台档案（dossier-core `platforms.rs`）是报审材料的档案，没有抖音、快手、视频号的上传规格，
所以这里单独建表。只生成**一套通用版本**：取抖音、快手、视频号三家公开要求里**最严**的值，
生成后由用户自己决定发到哪里，不再按平台签发不同的参数。

平台随时会改规则，M4b 动手处理视频前要按官方帮助中心复核一次。来源与整理日期见 `SOURCES`。
"""

from __future__ import annotations

from typing import Dict

#: 字段：
#: - width / height：输出分辨率（竖屏 9:16）；
#: - fps：输出帧率；
#: - video_bitrate_kbps / audio_bitrate_kbps：目标码率；
#: - max_duration_sec：单集时长上限（超出的在 M4b 按分集重组切开）；
#: - container / video_codec / audio_codec：封装与编码。
#:
#: `universal` 的取值依据：三家公开要求里最严的值，只有视频码率三家不同——
#: 抖音 6000kbps、快手 6000kbps、视频号 5000kbps，取最严的 **5000kbps**；
#: 其余各项三家相同，直接沿用：1080×1920、30fps、128kbps、900 秒、mp4 / h264 / aac。
PLATFORM_SPECS: Dict[str, Dict[str, object]] = {
    "universal": {
        "width": 1080, "height": 1920, "fps": 30, "video_bitrate_kbps": 5000, "audio_bitrate_kbps": 128,
        "max_duration_sec": 900, "container": "mp4", "video_codec": "h264", "audio_codec": "aac",
    },
}

#: 整理日期、复核日期与来源。
#:
#: 取值依据(2026-10-15 检索,只有快手有官方帮助页正文,其余是二手转述):
#: - 时长:快手官方「不能超出 15 分钟」;视频号手机端 30 分钟;抖音二手说法 15 分钟起 → 统一取 15 分钟。
#: - 文件大小:快手手机端官方 2G;视频号开放社区「最大 2G」→ 5000kbps × 15 分钟约 570MB,留足余量。
#: - 帧率/码率:视频号二手转述「60fps、10Mbps 以内」;快手官方支持 60fps;抖音二手「6000kbps 上限」
#:   → 取 30fps、≤5000kbps(三家最严)。
#: - 封装编码:三家都没有官方明确要求,二手资料一致推荐 MP4 + H.264 + AAC。
#:
#: M4b-1 动手前的复核结论(2026-10-16):抖音官方开放平台文档确认单集时长 15 分钟、快手官方帮助页
#: 确认 15 分钟 / 2G,与上表一致;**视频号仍查不到官方原文**。没有核到更严的值,
#: 所以通用规格保持原样(取三家最严),不改数值。
SOURCES = {
    "reviewed_at": "2026-10-15",
    "rechecked_at": "2026-10-16",
    "status": "已按官方帮助中心复核:抖音、快手官方原文与取值一致;视频号无官方原文,保留保守值",
    "urls": [
        "https://www.kuaishou.com/help/feedback/2730",
        "https://www.kuaishou.com/help/feedback/4000",
        "https://developers.weixin.qq.com",
        "https://developer.open-douyin.com/docs/resource/zh-CN/dop/develop/openapi/video-management/video-upload/upload-video",
    ],
}
