# 配方规范（Recipe Schema `1.0`）

> 交付物之二，对应设计文档 §6「配方系统」。
>
> **边界**：配方是**数据**，不是代码。它只能引用代码里白名单注册的处理模块，参数由模块自己的
> Pydantic 模型校验（`StrictModel` 的 `extra="forbid"`），因此配方里**不可能**夹带 shell 命令、
> 可执行路径或越界取值——这是 `§10 安全边界` 的实现方式。

## 1. 配方是什么

一份配方描述「把一组输入素材加工成什么产物」：

```text
配方（Recipe）  = 目标规格（target）
                + 兼容性声明（compatibility）
                + 管线（pipeline：有序的模块步骤）
                + 质量策略（quality_policy）

任务（Task）    = 一份冻结的配方快照 + 一组输入素材（asset_ids）+ 本次的参数覆盖（user_params）
可执行计划（Plan）= 配方快照 + 输入素材 → 展开每一步的模块参数、做能力检查与输入角色分配
```

配方**发布后不可修改**：同一 `(recipe_id, version)` 一旦写入 `recipe_versions` 就固定，
改动只能发布新版本（见 `DATABASE_SCHEMA.md` §`recipe_versions`）。

## 2. 顶层结构

| 字段 | 类型 | 必填 | 默认 | 说明 |
| --- | --- | --- | --- | --- |
| `schema_version` | string | 是 | `"1.0"` | 只接受当前版本；不匹配直接拒绝 |
| `recipe_id` | string | 是 | — | 配方标识，如 `drama.full_edit` |
| `version` | int ≥1 | 是 | — | 版本号，递增；已存在的版本不可覆盖 |
| `status` | enum | 否 | `"draft"` | `draft` / `reviewing` / `published` / `deprecated` / `revoked` |
| `target` | object | 是 | — | 目标规格，见 §3 |
| `compatibility` | object | 否 | `{}` | 兼容性声明，见 §4 |
| `preset` | object | 否 | `{"mode":"no_edit"}` | 工作流模式，见 §5 |
| `pipeline` | array | 是 | — | 处理步骤，见 §6；不能为空 |
| `quality_policy` | object | 否 | 见 §7 | 质量门禁策略 |

`content_sha256` 由**除 `status` 之外**的全部字段计算，所以发布/撤回/弃用不会改变内容指纹，
任务里的 `recipe_sha256` 也因此保持稳定。

## 3. `target`

| 字段 | 类型 | 必填 | 说明 |
| --- | --- | --- | --- |
| `platform_profile` | string | 是 | 平台档位名，如 `vertical-1080x1920`、`generic-h264-mp4`；只是标签 |
| `container` | string | 是 | `mp4` / `mkv` / `webm` |
| `video_codec` | string | 是 | `h264` / `h265` / `vp9` / `av1` |
| `audio_codec` | string | 是 | `aac` / `mp3` / `opus` / `none` |
| `width` | int 16..7680 | 否 | 目标宽度；声明了就必须有对应的 `video.quality.scale` |
| `height` | int 16..4320 | 否 | 目标高度；同上 |

`target` 与 `media.encode` 的参数是同一件事的两种表达，校验期强制一致（§9.8）。
分辨率同理：`target.width/height` 必须与 `video.quality.params.scale` 对上，否则拒绝——
避免「配方声明了 1080×1920，实际编码出来还是原尺寸」这种跑到门禁才发现的问题。

## 4. `compatibility`

| 字段 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `min_worker_version` | string | `"1.0.0"` | Worker 版本低于它就拒绝执行（`capability_missing`） |
| `required_capabilities` | string[] | `[]` | 配方级能力声明，创建任务时一次性快速失败 |

能力标签来自模块声明（见 §8 末表），并列 `drama-cloud status` 查看当前集群能力集；
`DRAMA_CLOUD_HOME` 之外还可以用配置里的 `capabilities` 追加部署侧自定义标签。

## 5. `preset.mode` —— 三档模式（C3）

| 模式 | 约束 |
| --- | --- |
| `no_edit` | 「不剪辑」：只做规格归一化与编码，不引入字幕/时间线改动 |
| `subtitle` | **必须**包含至少一个 `subtitle.mux` 步骤，否则配方不合法 |
| `full_edit` | 完整剪辑：时间线、画面、音频、章节等自由组合 |

模式是给客户端和审核人看的**语义档位**，真正的执行语义全部由 `pipeline` 决定。

## 6. `pipeline`

数组，按顺序执行。每一步形如：

```json
{ "id": "encode", "module": "media.encode", "params": { "crf": 23 } }
```

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `id` | string | 步骤标识：只能字母、数字、`_`、`-`；同一配方内不允许重复 |
| `module` | string | 必须是注册表里的模块名（白名单） |
| `params` | object | 该模块的参数；未知字段直接拒绝 |

固定的形状约束：

1. **第一步必须是 `media.probe`**——后面所有模块都依赖它探测出的时长/轨道信息。
2. **要求门禁时最后一步必须是 `media.quality_gate`**（`quality_policy.require_decodable_output=true` 时）。
3. **依赖必须闭合**：每一步 `requires` 的角色，必须已经由输入素材（`video`/`audio`/`metadata`）
   或前序步骤的 `produces` 满足，否则发布即拒绝。
4. 只有 `media.encode` 会**物化文件**（写出视频）。其余模块只往计划（`ctx.plan`）里追加内容，
   由编码步骤统一编译成一条 ffmpeg 命令——这样一次编码就完成全部滤镜与时间线，
   不会因为多步处理反复重编码。

## 7. `quality_policy`

| 字段 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `require_decodable_output` | bool | `true` | 产物必须能探测出视频轨 |
| `check_audio_video_sync` | bool | `true` | 检查音视频时长差是否在容差内 |
| `validate_target_profile` | bool | `true` | 校验编码器、容器、分辨率是否符合 `target` |
| `max_av_sync_ms` | float | `200.0` | 音画同步容差（毫秒） |
| `min_duration_seconds` | float | `0.5` | 产物最短时长 |

模块 `media.quality_gate` 的同名参数若显式给出，则**覆盖**这里的策略值（便于单任务微调）。

## 8. 模块参数表（13 个）

注册表在 `src/drama_cloud/recipes/registry.py`，字面量写死，配方改不动它。

### `media.probe` · 媒体探测

无参数。产出角色 `video` / `audio` / `metadata`，是每条管线的起点。

### `video.quality` · 画面质量处理（C4）

| 参数 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `color` | object\|null | `null` | `eq` 滤镜参数 |
| `color.brightness` | float −1..1 | `0.0` | 亮度 |
| `color.contrast` | float −1..1 | `0.0` | 对比度 |
| `color.saturation` | float −1..1 | `0.0` | 饱和度 |
| `color.gamma` | float −1..1 | `0.0` | 伽马 |
| `sharpen` | float 0..3 | `0.0` | `unsharp` 强度，`0` 表示不锐化 |
| `denoise` | `off`/`light`/`medium`/`strong` | `off` | `hqdn3d` 强度档 |
| `scale` | object\|null | `null` | 分辨率归一化 |
| `scale.width` | int 16..7680\|null | `null` | 只给一边时另一边按 `-2` 自动推导（保证偶数） |
| `scale.height` | int 16..4320\|null | `null` | 同上；至少给一边 |
| `scale.mode` | `fit`/`fill`/`pad` | `fit` | `fit`=缩放留边；`fill`=铺满后中心裁剪；`pad`=缩放后补边 |
| `scale.pad_color` | string | `black` | `pad` 模式的补边颜色 |

### `frame.fps_convert` · 帧率转换（C9）

| 参数 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `fps` | float >0 且 ≤240 | 必填 | 目标帧率 |
| `mode` | `cfr`/`drop`/`dup` | `cfr` | 恒定帧率重采样 / 丢帧 / 补帧 |

### `frame.deflicker` · 去闪烁、去抖（C9）

| 参数 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `size` | int 2..129 | `5` | 时间窗口大小（帧） |
| `mode` | `am`/`gm`/`hm`/`ma`/`md`/`pm` | `am` | ffmpeg `deflicker` 的度量方式 |

### `frame.inspect` · 逐帧质量抽检（C9）

| 参数 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `sample_count` | int 1..200 | `8` | 均匀抽检帧数，用 `signalstats` 统计 |

产出角色 `frame_report`（只读报告，不改画面）。

### `timeline.user_edits` · 时间线编辑（C5）

| 参数 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `segments` | array | `[]` | 保留片段，按给定顺序拼接；空 = 整片不裁剪 |
| `segments[].input_index` | int ≥0 | `0` | 片段来自哪份输入素材 |
| `segments[].start` | float ≥0 | `0.0` | 起点（秒） |
| `segments[].end` | float >`start`\|null | `null` | 终点；`null` 表示到素材结尾 |
| `scene_detect` | bool | `false` | 场景检测；**只产出候选切点**，不会自动套用 |
| `scene_threshold` | float 0..1 | `0.35` | 场景切换阈值 |
| `scene_max_analyze_seconds` | float >0 | `1800.0` | 场景检测最多分析多少秒 |
| `max_output_seconds` | float >0\|null | `null` | 输出时长上限，超出直接拒绝 |

### `chapters.reorg` · 分集重组（C10）

| 参数 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `mode` | `chapters`/`episodes` | `chapters` | `chapters` 只写章节标记；`episodes` 按目标时长切集 |
| `episode_seconds` | float >1\|null | `null` | `mode=episodes` 时**必填** |
| `min_segment_seconds` | float >0 | `20.0` | 分集最短时长；只对 `episodes` 生效 |
| `titles` | string[] | `[]` | 分集标题；`chapters` 模式下标题数决定章节数 |
| `split` | bool | `false` | `true` 才真的输出多个分集文件（stream-copy） |

`episodes` 模式的收尾碎片会并入上一集，不会被切成一个过短的尾巴。

### `commentary.rebuild` · 解说重剪（C11）

| 参数 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `commentary_input_index` | int ≥0 | `0` | 解说音轨所在的输入素材下标 |
| `transcript` | array | `[]` | 语音转写：`{start, end, text}`；**本机无 ASR，需外部提供** |
| `script` | string[] | `[]` | 解说脚本，一句一行，按顺序匹配转写片段 |
| `match_threshold` | float 0..1 | `0.45` | 相似度阈值（`difflib` 比率） |
| `approve` | bool | `false` | 人工审核开关；`false` 只产出候选时间线，不改变输出 |

### `audio.quality` · 音频质量处理（C7）

| 参数 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `loudness` | object\|null | `null` | `loudnorm` 两遍响度归一 |
| `loudness.target_lufs` | float −40..−5 | `-16.0` | 目标整体响度 |
| `loudness.true_peak` | float −9..0 | `-1.5` | 真峰值上限（dBTP） |
| `loudness.range_lu` | float 1..30 | `11.0` | 响度范围 |
| `compressor` | object\|null | `null` | 动态压缩 |
| `compressor.threshold` | float −60..0 | `-20.0` | 阈值（dB） |
| `compressor.ratio` | float 1..20 | `2.0` | 压缩比 |
| `compressor.attack` | float 0.01..2000 | `20.0` | 起音（毫秒） |
| `compressor.release` | float 0.01..9000 | `250.0` | 释放（毫秒） |
| `eq` | array | `[]` | EQ 频段：`{frequency, gain, width_octaves}` |
| `dynaudnorm` | bool | `false` | 动态范围归一化，适合音量起伏大的解说素材 |
| `measure` | bool | `true` | 只读测量输入响度，写进报告 |
| `channels` | 1\|2\|null | `null` | 重混声道数 |

输入没有音轨时该步骤**显式跳过并记一条说明**，不会假装生效。

### `subtitle.mux` · 字幕处理（C3）

| 参数 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `source` | object | 必填 | 字幕来源 |
| `source.kind` | `external`/`embedded` | 必填 | 外挂字幕文件 / 素材内字幕轨 |
| `source.input_index` | int ≥0 | — | `external` 必填；指向角色为 `subtitle` 的输入 |
| `source.stream_index` | int ≥0 | — | `embedded` 必填；字幕轨下标 |
| `mode` | `burn`/`soft` | `soft` | 烧进画面 / 作为可关闭字幕轨封装 |
| `language` | string | `chi` | 语言标签 |
| `title` | string | `中文` | 字幕轨标题 |

软字幕会由门禁检查字幕轨是否真的封装进产物。

### `metadata.policy` · 元数据策略（C8）

| 参数 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `mode` | `keep`/`strip`/`edit` | `keep` | 原样保留 / 清除全部 / 按 `set`+`remove` 调整 |
| `set` | object | `{}` | 要写入的元数据键值 |
| `remove` | string[] | `[]` | 要移除的键 |
| `strip_location` | bool | `true` | 抹掉定位与设备相关字段（`location`、`com.apple.quicktime.location.*` 等） |

### `media.encode` · 编码与封装（C6）

| 参数 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `video_codec` | `h264`/`h265`/`vp9`/`av1` | `h264` | 逻辑编码器名，映射见下 |
| `audio_codec` | `aac`/`mp3`/`opus`/`none` | `aac` | `none` 表示丢弃音轨 |
| `container` | `mp4`/`mkv`/`webm` | `mp4` | 与编解码组合受容器矩阵约束 |
| `crf` | int 0..51 | `20` | 质量模式质量因子 |
| `rate_control` | `crf`/`bitrate` | `crf` | 码率控制方式 |
| `video_bitrate_kbps` | int 100..200000\|null | `null` | `rate_control=bitrate` 时必填 |
| `audio_bitrate_kbps` | int 32..512 | `128` | 音频码率 |
| `preset` | `ultrafast`/`fast`/`medium`/`slow` | `medium` | 编码 preset（h264/h265 生效） |
| `gop_seconds` | float 0..30 | `2.0` | GOP 长度；`0` 交给编码器默认 |
| `scene_cut` | bool | `true` | 强制在场景切换处插关键帧 |
| `frame_rate_policy` | `preserve`/`cfr` | `preserve` | 保持原帧率 / 恒定帧率输出 |
| `pix_fmt` | `yuv420p`/`yuv444p`\|null | `null` | 像素格式 |

容器矩阵（`CONTAINER_MATRIX`，不匹配抛 `target_unsupported`）：

| 容器 | 允许的视频编码 | 允许的音频编码 |
| --- | --- | --- |
| `mp4` | h264, h265, av1 | aac, mp3, none |
| `mkv` | h264, h265, vp9, av1 | aac, mp3, opus, none |
| `webm` | vp9, av1 | opus, none |

逻辑名 → 本机 ffmpeg 编码器：`h264`→`libx264`、`h265`→`libx265`、`vp9`→`libvpx-vp9`、
`av1`→`libaom-av1`。

### `media.quality_gate` · 质量门禁（§10）

| 参数 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `require_decodable_output` | bool\|null | `null` | 未给出时回落到 `quality_policy` |
| `check_audio_video_sync` | bool\|null | `null` | 同上 |
| `validate_target_profile` | bool\|null | `null` | 同上 |
| `max_av_sync_ms` | float >0\|null | `null` | 同上 |
| `min_duration_seconds` | float ≥0\|null | `null` | 同上 |
| `duration_tolerance_seconds` | float 0..30 | `1.0` | 时长偏差容差（剪辑/重采样会带来零头） |

### 模块能力与角色一览

| 模块 | 能力标签 | `requires` | `produces` | 类型 |
| --- | --- | --- | --- | --- |
| `media.probe` | `ffprobe` | — | video, audio, metadata | plan |
| `video.quality` | `video_filters` | video | video_filters | transform |
| `frame.fps_convert` | `video_filters` | video | video_filters | transform |
| `frame.deflicker` | `video_filters` | video | video_filters | transform |
| `frame.inspect` | `signalstats` | video | frame_report | transform |
| `timeline.user_edits` | `ffmpeg` | video, audio | timeline | transform |
| `chapters.reorg` | `ffmpeg` | timeline | chapters | transform |
| `commentary.rebuild` | `ffmpeg` | audio | timeline | transform |
| `audio.quality` | `audio_filters` | audio | audio_filters | transform |
| `subtitle.mux` | `subtitles` | video | subtitle_plan | transform |
| `metadata.policy` | `ffmpeg` | metadata | metadata_policy | transform |
| `media.encode` | `h264_encode` | video | encoded | transform |
| `media.quality_gate` | `ffprobe` | encoded | quality_report | transform |

`kind` 只用于说明与检查点策略：只有 `media.encode` 物化文件，因此只有它的阶段产物可以被复用。

## 9. 校验规则（`validate_recipe()` 的执行顺序）

1. **结构**：Pydantic 模型校验，未知字段、类型、范围不合法 → `recipe_schema_invalid`。
2. **`schema_version`**：必须等于 `1.0`。
3. **步骤 id 唯一**：重复即拒绝。
4. **模块白名单**：`module` 必须在 `REGISTRY` 里。
5. **参数校验**：按模块的参数模型校验，越界与未知字段同样被拒。
6. **依赖闭包**（`_validate_dependencies`）：逐步累加可用角色，`requires` 未被满足即拒绝。
7. **策略**（`_validate_policy`）：
   - 管线非空；
   - 第一步是 `media.probe`；
   - `require_decodable_output=true` 时最后一步是 `media.quality_gate`；
   - `subtitle` 模式必须含 `subtitle.mux`；
8. **`target` 与编码参数一致**（`_validate_target_matches_encode`）：容器/视频编码/音频编码必须相同；
   声明了 `target.width/height` 时必须存在 `video.quality.scale` 且数值一致。

> 校验在**发布时**（`POST /api/v1/recipes`、`drama-cloud publish`）与**创建任务时**（快照重建计划）
> 各跑一次，所以「发布成功但跑不起来」的情况在原型里被压到最低。

## 10. `user_params` 覆盖

创建任务时可以用 `user_params` 按**步骤 id** 覆盖已有参数：

```json
{ "encode": { "crf": 21 }, "video": { "sharpen": 0.6 } }
```

规则：

- 只能覆盖**已存在**的参数，不能新增字段（防止绕过白名单）；
- 覆盖后的配方会**重新过一遍** `validate_recipe()`，越界取值一样被拒；
- 覆盖结果写进任务的 `recipe_snapshot_json`，执行期间不再跟随发布指针变化。

## 11. 完整示例

见仓库内置配方：`recipes/drama-no-edit-generic.json`、`drama-subtitle-soft.json`、
`drama-subtitle-burn.json`、`drama-full-edit.json`、`drama-episodes-split.json`、
`drama-commentary-draft.json`（`status=draft`，用于演示「草稿不可被任务引用」）。

`drama.full_edit` 的简化形态：

```json
{
  "schema_version": "1.0",
  "recipe_id": "drama.full_edit",
  "version": 1,
  "status": "published",
  "target": {
    "platform_profile": "vertical-1080x1920",
    "container": "mp4",
    "video_codec": "h264",
    "audio_codec": "aac",
    "width": 1080,
    "height": 1920
  },
  "compatibility": {
    "min_worker_version": "1.0.0",
    "required_capabilities": ["h264_encode", "video_filters", "audio_filters"]
  },
  "preset": { "mode": "full_edit" },
  "pipeline": [
    { "id": "probe", "module": "media.probe", "params": {} },
    { "id": "timeline", "module": "timeline.user_edits",
      "params": { "segments": [], "scene_detect": true } },
    { "id": "video", "module": "video.quality",
      "params": { "scale": { "width": 1080, "height": 1920, "mode": "pad" }, "sharpen": 0.4 } },
    { "id": "audio", "module": "audio.quality",
      "params": { "loudness": { "target_lufs": -16.0 } } },
    { "id": "metadata", "module": "metadata.policy", "params": { "mode": "edit" } },
    { "id": "chapters", "module": "chapters.reorg", "params": { "mode": "chapters" } },
    { "id": "encode", "module": "media.encode",
      "params": { "video_codec": "h264", "audio_codec": "aac", "container": "mp4", "crf": 23 } },
    { "id": "gate", "module": "media.quality_gate", "params": {} }
  ],
  "quality_policy": { "max_av_sync_ms": 200.0, "min_duration_seconds": 0.5 }
}
```

## 12. 机器可读 Schema

- 仓库内快照：`docs/recipe.schema.json`（与端点输出**同源**，改过 schema 后重新生成）
- 运行时导出：`GET /api/v1/recipes/schema.json`（`Recipe.model_json_schema()`）

```bash
# 重新生成快照（等价于 GET /api/v1/recipes/schema.json）
cd cloud && PYTHONPATH=src ../.venv/bin/python -c \
  "import json,pathlib;from drama_cloud.recipes.schema import recipe_json_schema as s;pathlib.Path('docs/recipe.schema.json').write_text(json.dumps(s(),indent=2,ensure_ascii=False,sort_keys=True)+chr(10),encoding='utf-8')"
```

注意 JSON Schema 只覆盖**结构**（字段、类型、范围）。白名单、依赖闭包、策略与
`target`↔`encode` 一致性这四类规则是跨字段的，必须调用 `validate_recipe()`——
`drama-cloud publish` 与 `POST /api/v1/recipes` 都会做。
