# 卡密授权

剧备使用"卡密激活 + 离线凭证"的授权方式：

- **激活时必须联网**：服务器核销卡密，并把授权和这台电脑绑定。
- **日常使用不联网**：每次都在本机校验服务器的签名。
- **每 24 小时自动联网续期一次**。连续 7 天连不上服务器，软件就会锁定；重新联网后自动恢复。

授权与电脑**固定绑定**，不支持迁移。用户重装系统或更换硬件后，机器码会改变，需要重新兑换一张卡。

| 部分 | 位置 |
|---|---|
| 授权服务器 + 网页管理后台 | [license-server/](../license-server/README.md) |
| 验签、机器码、时钟检查、判定（纯逻辑） | `crates/dossier-cardkey` |
| 桌面端命令、闸门、功能内验权、守护线程 | `src-tauri/src/license.rs` |
| 激活页 | `src/license.js`、`src/license.css` |

## 协议

### 卡密

- 由 16 个字符组成，字符集为 `23456789ABCDEFGHJKLMNPQRSTUVWXYZ`，去掉了容易认错的 0、O、1、I。
- 显示格式为 `DDK-XXXX-XXXX-XXXX-XXXX`。
- 用 CSPRNG（密码学安全随机数生成器）生成，约 80 bit 随机量。
- 数据库里只保存 `sha256("drama-dossier/card/v1\n" + 卡密)`，不保存明文。

### 机器码

先把原始标识规范化：去掉花括号和短横线，转成小写，结果必须是 32 位十六进制。然后计算：

`sha256("drama-dossier/machine/v1\n" + 平台 + "\n" + 规范化后的原始标识)`

原始标识的来源：

- Windows：读注册表里的 `MachineGuid`，强制使用 64 位视图。
- macOS：读 `IOPlatformUUID`。
- Linux：读 `/etc/machine-id`。

读不到原始标识时直接锁定，不会退回到主机名这类容易变化或伪造的弱指纹。

### 授权凭证

格式为 `base64url(payload) "." base64url(Ed25519 签名)`，两段都不带填充。签名覆盖的内容是：

`"drama-dossier/license/v1\n" + payload 原始字节`

payload 是 JSON 对象，序列化时按键名排序、使用紧凑分隔符、只包含 ASCII 字符：

| 字段 | 含义 |
|---|---|
| `v` | 版本号，固定为 1 |
| `kid` | 签名公钥 `sha256` 的前 16 位十六进制 |
| `lid` | 服务器上的电脑记录编号 |
| `mid` | 机器码 |
| `plan` | 卡种：`trial` / `month` / `quarter` / `year` / `lifetime` |
| `until` | 使用权到期时间（Unix 秒）；永久卡为 `null` |
| `iat` | 签发时间 |
| `exp` | 凭证到期时间：`min(iat + 7 天, until)`。永久卡也会到期，需要定期续签 |

客户端按以下顺序判定，任何一步失败都会锁定：

1. 格式
2. 签名（逐一尝试内置公钥，`kid` 必须与签名公钥一致）
3. 严格解析载荷（字段必须一个不多、一个不少，不能有重复键，不接受布尔值、浮点数或负数）
4. 机器码
5. 时钟回拨（`now + 600 < max(last_seen, iat)` 视为回拨）
6. 有效期

两端的实现由 `crates/dossier-cardkey/tests/vectors_v1.json` 中的 36 条测试向量逐条对照。这些向量由 `license-server/tests/vectors.py` 生成，修改协议后必须重新生成，并且一起提交。

### 卡种规则

| 卡种 | 时长 | 规则 |
|---|---|---|
| 试用卡 | 3 天 | 只能在从未激活过的电脑上使用 |
| 月卡 / 季卡 / 年卡 | 30 / 90 / 365 天 | 在已有到期时间的基础上叠加；已过期则从兑换当天开始算 |
| 永久卡 | 不限 | 兑换后，这台电脑不能再兑换其他卡 |

时长统一在 `license-server/src/dd_license/plans.py` 里配置。生成卡密时会把当时的天数复制进卡密记录，之后修改配置，不影响已经卖出去的卡。

## 客户端多层验权

| 层 | 做什么 |
|---|---|
| ① 激活页 | 未授权时全屏遮挡界面。这一层只影响体验，删掉它也绕不过后面几层 |
| ② 总闸门 `license::gate` | 包裹 `invoke_handler`。未授权时，白名单之外的**所有**命令都会被拒绝，以后新增的命令也默认被锁 |
| ③ 功能内验权 `license::guard()` | 每个主要功能入口的第一行都调用它。每次从磁盘重新读取凭证并完整验签，不依赖缓存；函数标记为 `#[inline(always)]`，会被展开到各个调用处 |
| ④ 守护线程 | 每隔 3–9 分钟（随机间隔）重新验签一次，每 24 小时联网续签一次。发现授权失效就锁定，并通知前端 |
| ⑤ 金丝雀自检 | 启动时以及每次验权时，用内置的"已知坏签名"和"已知好签名"各验一次。如果验签逻辑被改成永远返回通过，坏签名也会通过，自检随即失败并锁定 |

调用 `guard()` 的位置：
- `generate_package`
- `plan_erase` 和 `run_erase`
- `apply_rename`、`fix_bitrate`、`transcode_execute`、`dedup_demo_execute`
- `export_draft` 和 `generate_mount_copy_csv`
- 所有 AI 调用的统一入口 `ai::http_client`

正式版（release）构建中，一次完整验权平均约 0.2 毫秒，见 `license::tests::full_check_path_is_fast`。正常使用时不会联网，也不会弹窗。

**能防住**：拷贝授权文件、修改授权文件、调整系统时间、自建假服务器、用开发者工具修改界面。正式版不带开发者工具。

**防不住**：熟悉逆向工程的人直接修改二进制文件。多层验权的作用，是把破解从"改一处"变成"找到并改掉十几处分散的检查，同时还要绕开自检和守护线程"，而且每发一个新版本都得重新来一遍。要进一步提高门槛，只能靠商业加壳，或者把核心功能放到服务端去执行。

**本地开发**：debug 构建设置 `DD_LICENSE_DEV_BYPASS=1` 后，可以跳过所有授权检查，每次跳过都会记录日志；release 构建中不存在这个开关。debug 构建还可以通过 `DD_LICENSE_SERVER=http://127.0.0.1:8200` 连接本地授权服务器。

## 上线检查清单

合并到 master 之前必须全部完成。否则打 `app-v*` tag 时，CI 会自动发布 Release，而发出去的安装包谁都激活不了，程序会锁定，不会放行。

1. [ ] 按 [license-server/README.md](../license-server/README.md) 部署服务器，并确认 `license-admin doctor` 的每一项都通过。
2. [ ] 把 `license-admin keygen` 输出的公钥填进 `crates/dossier-cardkey/src/keys.rs` 里的 `PRODUCTION_KEYS`。
3. [ ] 把服务器的 HTTPS 地址填进 `src-tauri/src/license.rs` 里的 `LICENSE_SERVER`。
4. [ ] 运行 `scripts/check.sh` 并全部通过。
5. [ ] 在 Windows 和 macOS 上各手动测试一次：首次打开会出现激活页 → 输入卡密后激活成功 → 断网后可以继续使用 → 在后台作废这台电脑 → 联网后被锁定。
6. [ ] 通知其他开发者：本地运行 `cargo tauri dev` 时需要设置 `DD_LICENSE_DEV_BYPASS=1`。

## 密钥轮换应急

私钥泄露时，攻击者可以伪造任意授权。按以下步骤处理：

1. 在服务器上执行 `license-admin keygen --out 新路径`，然后把 `DD_LICENSE_KEY_FILE` 指向新私钥，重启服务。从这一刻起，新签发的凭证都使用新私钥。
2. 把客户端的 `PRODUCTION_KEYS` 改成**只包含新公钥**，然后发布新版本。旧版本客户端仍然信任旧公钥，攻击者伪造的凭证在旧版本上依然有效。因此只能通过强制升级止损。
3. 老用户升级到新版本后，本地凭证是旧私钥签发的，新版本验不过，会显示"授权信息无效"，用户需要重新激活。服务器认得这台电脑，用户输入原来那张卡密时，服务器按"本机重复兑换"处理，不会消耗新卡，会直接签发新凭证。

正常的计划内轮换（私钥没有泄露）步骤如下：
1. 先发布一个客户端版本，`PRODUCTION_KEYS` 里同时包含新旧两把公钥。
2. 等大多数用户升级完成后，服务器切换到新私钥，同时把旧公钥填进 `DD_LICENSE_EXTRA_PUBKEYS`。这样老用户手里旧私钥签发的凭证仍可用来续签，续签拿到的新凭证则由新私钥签发。
3. 再发布一个只保留新公钥的版本。7 天以后，清空 `DD_LICENSE_EXTRA_PUBKEYS`。

私钥泄露时**不要**填 `DD_LICENSE_EXTRA_PUBKEYS`，否则攻击者伪造的凭证也能拿来续签。
