"""处理配方第一层：各平台的上传规格（竖屏短剧）。

现有的平台档案（dossier-core `platforms.rs`）是报审材料的档案，没有抖音、快手、视频号的上传规格，
所以这里单独建表。取值是**保守值**：按公开资料里各平台都能接受的范围取交集，不追求每个平台的上限。

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
PLATFORM_SPECS: Dict[str, Dict[str, object]] = {
    "douyin": {
        "width": 1080, "height": 1920, "fps": 30, "video_bitrate_kbps": 6000, "audio_bitrate_kbps": 128,
        "max_duration_sec": 900, "container": "mp4", "video_codec": "h264", "audio_codec": "aac",
    },
    "kuaishou": {
        "width": 1080, "height": 1920, "fps": 30, "video_bitrate_kbps": 6000, "audio_bitrate_kbps": 128,
        "max_duration_sec": 900, "container": "mp4", "video_codec": "h264", "audio_codec": "aac",
    },
    "channels": {
        "width": 1080, "height": 1920, "fps": 30, "video_bitrate_kbps": 5000, "audio_bitrate_kbps": 128,
        "max_duration_sec": 900, "container": "mp4", "video_codec": "h264", "audio_codec": "aac",
    },
}

#: 整理日期与来源。复核时更新这里。
#:
#: 取值依据（2026-10-15 检索，只有快手有官方帮助页正文，其余是二手转述）：
#: - 时长：快手官方「不能超出 15 分钟」；视频号手机端 30 分钟；抖音二手说法 15 分钟起 → 统一取 15 分钟。
#: - 文件大小：快手手机端官方 2G；视频号开放社区「最大 2G」→ 6000kbps × 15 分钟约 680MB，留足余量。
#: - 帧率/码率：视频号二手转述「60fps、10Mbps 以内」；快手官方支持 60fps；抖音二手「6000kbps 上限」
#:   → 取 30fps、≤6000kbps。
#: - 封装编码：三家都没有官方明确要求，二手资料一致推荐 MP4 + H.264 + AAC。
SOURCES = {
    "reviewed_at": "2026-10-15",
    "status": "待复核：抖音、视频号无官方原文，M4b 前需按官方帮助中心再核一次",
    "urls": [
        "https://www.kuaishou.com/help/feedback/2730",
        "https://www.kuaishou.com/help/feedback/4000",
        "https://developers.weixin.qq.com",
        "https://developer.open-douyin.com/docs/resource/zh-CN/dop/develop/openapi/video-management/video-upload/upload-video",
    ],
}
