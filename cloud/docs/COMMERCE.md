# 账号与授权服务（drama_cloud.commerce）

这个服务管三件事：账号（注册、登录、邮件验证码、令牌），授权（卡密兑换、使用权、电脑绑定、
可以离线验证的授权凭证），和积分（账本、按集计价、充值支付、邀请奖励）。它和媒体控制面（`drama-cloud`）
分开部署：处理任务再重，也不拖慢登录。数据库用 PostgreSQL，因为卡密兑换、抢设备名额和支付回调都需要可靠的并发事务。

- M1（已完成）：邮件验证码、注册、登录、刷新与吊销令牌、重设密码、修改密码、集中限流、审计日志。
- M2：卡密批量生成与兑换、使用权、电脑绑定与解绑、授权凭证的签发与刷新、管理命令。
- 桌面端接入（钥匙串、本机机器码、本地验签、离线提示）在 M3。
- M4a：报审包（`POST /packages`）与处理配方（`POST /recipes`、`/recipes/{id}/consume`），
  三种签名用途前缀隔离；处理配方接桌面界面在 M4b。
- M5a：积分账本与按集计价（3.7）、充值订单与支付渠道抽象（3.8）、邀请码与防刷（3.7）。
  报审包与处理配方在这一期开始扣积分；真实收款渠道仍只有占位实现。
- M7（本期）：**管理后台**（3.10）、**客户端配置与错误上报**（3.11）、升级包签名用途
  （`drama-dossier/release/v1\n`）、双私钥轮换。运维手册见 [`ADMIN_GUIDE.md`](ADMIN_GUIDE.md)，
  部署见 [`DEPLOYMENT.md`](DEPLOYMENT.md)，密钥轮换见 [`../../docs/KEY_ROTATION.md`](../../docs/KEY_ROTATION.md)。

## 1. 本地开发

```bash
cd cloud
docker compose -f compose.dev.yaml up -d --wait     # PostgreSQL 18.6 + Mailpit v1.31.6，只绑定 127.0.0.1

export DRAMA_COMMERCE_DSN='postgresql://drama:drama-dev-only@127.0.0.1:5432/drama_commerce'
export DRAMA_COMMERCE_CODE_SECRET='<至少 32 个字符的随机串>'   # 例如 python3 -c 'import secrets;print(secrets.token_urlsafe(48))'
export DRAMA_COMMERCE_SMTP_HOST=127.0.0.1 DRAMA_COMMERCE_SMTP_PORT=1025 DRAMA_COMMERCE_SMTP_SECURITY=plain
export DRAMA_COMMERCE_LICENSE_KEY_FILE="$HOME/.config/drama-commerce/license-dev.pem"
export DRAMA_COMMERCE_MOCK_PAY_SECRET='<随便一个固定串，模拟支付用它签回调>'   # 不配时 mock 渠道报「未配置」

.venv/bin/drama-commerce keygen "$DRAMA_COMMERCE_LICENSE_KEY_FILE"   # 只做一次；打印的 public_key 给桌面端
.venv/bin/drama-commerce migrate          # 建表；重复执行没有副作用
.venv/bin/drama-commerce serve --port 8100

# 另开一个终端，生成 5 张测试用月卡（同样要先 export DRAMA_COMMERCE_DSN）
.venv/bin/drama-commerce cards generate --days 30 --count 5 --out "$HOME/cards-dev.csv"
```

- 验证码邮件发到 Mailpit，不会发出真实邮件。打开 <http://127.0.0.1:8025> 查看。
- 验证码在数据库里存的是用 `CODE_SECRET` 算的 HMAC。换了这个密钥，之前发出的验证码全部失效。
- 想跑通充值：配了 `DRAMA_COMMERCE_MOCK_PAY_SECRET` 之后，在客户端的「积分与充值」里选档位下单，
  会拿到一个指向本服务 `/pay/mock/checkout?out_trade_no=…` 的地址；在那个页面上点按钮即可发出真实格式的回调。
  本地开发不需要配 `DRAMA_COMMERCE_PUBLIC_BASE_URL`（Mock 会用本次请求的来源补成完整地址，客户端可以直接点开）。
- 停止：`docker compose -f compose.dev.yaml down`；连数据一起删掉加 `-v`。
- compose 里的账号密码是开发专用的固定值，不能用在任何对外环境。
- 私钥和导出的卡密 CSV 都放在仓库目录外面：`.gitignore` 没有排除它们，放进仓库很容易被误提交。

**本地也要建一个管理员**，否则管理后台打不开（登录页会一直停在「邮箱或密码不正确」）：

```bash
.venv/bin/drama-commerce admin create --email ops@example.com
# 交互时用 getpass 提示两次；脚本里从 stdin 读一行。密码至少 12 个字符。
# 然后打开 http://127.0.0.1:8100/admin
```

管理后台的页面在 `/admin`，接口在 `/api/v1/admin/*`；用 curl 拿令牌调试的写法见
[`ADMIN_GUIDE.md`](ADMIN_GUIDE.md) 第 7 节。公开的客户端配置在 `/api/v1/client/config`，
不需要登录就能直接 `curl`。整套服务的部署（四个容器 + Caddy + 备份恢复）见 [`DEPLOYMENT.md`](DEPLOYMENT.md)。

## 2. 配置（环境变量）

| 变量 | 必填 | 说明 |
| --- | --- | --- |
| `DRAMA_COMMERCE_DSN` | 是 | PostgreSQL 连接串。`migrate` 只需要这一项 |
| `DRAMA_COMMERCE_CODE_SECRET` | 是 | 至少 32 个字符；用来算验证码和限流键的 HMAC |
| `DRAMA_COMMERCE_SMTP_HOST` | 是 | 发信服务器 |
| `DRAMA_COMMERCE_SMTP_SECURITY` | 否 | `starttls`（默认）/ `ssl` / `plain`。`plain` 只允许连本机 |
| `DRAMA_COMMERCE_SMTP_PORT` | 否 | 默认按上一项取 587 / 465 / 25 |
| `DRAMA_COMMERCE_SMTP_FROM` | 否 | 发件人，默认 `no-reply@<SMTP_HOST>` |
| `DRAMA_COMMERCE_SMTP_USERNAME` / `_PASSWORD` | 否 | 发信账号；有用户名才登录 |
| `DRAMA_COMMERCE_TRUSTED_PROXIES` | 否 | 前面有几层可信反向代理（0..5，默认 0）。0 表示不读 `X-Forwarded-For` |
| `DRAMA_COMMERCE_PRODUCT_NAME` | 否 | 邮件标题里的产品名，默认「剧备 DramaDossier」 |
| `DRAMA_COMMERCE_TERMS_VERSION` | 否 | 注册时记录用户同意的条款版本 |
| `DRAMA_COMMERCE_POOL_MIN_SIZE` / `_MAX_SIZE` | 否 | 连接池大小，默认 1 / 10 |
| `DRAMA_COMMERCE_HASH_CONCURRENCY` | 否 | 同时进行的密码哈希上限（1..64，默认 4）。每次哈希占 64 MiB，默认最多约 256 MiB |
| `DRAMA_COMMERCE_LICENSE_KEY_FILE` | 是 | 授权签名私钥（Ed25519 PEM，不加密）的文件路径，用 `keygen` 生成。私钥本身不放进环境变量 |
| `DRAMA_COMMERCE_LICENSE_KEY_FILE_SECONDARY` | 否 | 次签名私钥的文件路径（M7-5 密钥轮换的过渡期用）。**留空表示不做轮换**；配了之后启动日志会打一条 warning，把两把私钥的 `kid` 都写出来。详见 [`../../docs/KEY_ROTATION.md`](../../docs/KEY_ROTATION.md) |
| `DRAMA_COMMERCE_LICENSE_TTL_HOURS` | 否 | 授权凭证有效期，也就是断网后还能用多久（1..720，默认 72） |
| `DRAMA_COMMERCE_LICENSE_REFRESH_HOURS` | 否 | 联网时桌面端每隔多久刷新一次凭证（1..720，默认 1，必须小于上一项） |
| `DRAMA_COMMERCE_DEVICE_LIMIT` | 否 | 每个账号同时绑定几台电脑（1..20，默认 1） |
| `DRAMA_COMMERCE_UNBIND_LIMIT` | 否 | 每个周期最多解绑几次（1..100，默认 3） |
| `DRAMA_COMMERCE_UNBIND_WINDOW_DAYS` | 否 | 解绑次数的统计周期，单位天（1..365，默认 30） |
| `DRAMA_COMMERCE_PDF_FONT` | 否 | 报审包 PDF 用的中文字体（`.otf` / `.ttf` / `.ttc` 文件路径）。不设时按常见系统路径找 Noto Sans CJK SC；**找不到字体服务拒绝启动** |
| `DRAMA_COMMERCE_ARTIFACTS_BIN` | 否 | `dossier-artifacts` 可执行文件路径。不设时按 PATH → `target/release` → `target/debug` 查找（见 `toolchain.py`） |
| `DRAMA_COMMERCE_PUBLIC_BASE_URL` | 否 | 本服务的对外地址（例如 `https://drama.example.com`），用来拼支付回调地址。Mock 支付不需要它；真实渠道缺它时下单报「未配置对外地址」 |
| `DRAMA_COMMERCE_MOCK_PAY_SECRET` | 否 | 模拟支付回调的 HMAC-SHA256 签名密钥。留空时 mock 渠道报「未配置」——空密钥等于谁都能伪造回调，所以开发环境也要显式配一个固定串 |
| `DRAMA_COMMERCE_INTERNAL_SECRET` | 否 | `/internal/*` 内部接口的共享密钥（M5b）。与媒体控制面的 `DRAMA_CLOUD_INTERNAL_SECRET` 必须是**同一个值**。**留空等于关闭这些路由**：没配时它们一律 404，而不是「不鉴权就放行」。只在同机房内网里传，不该出现在公网 |
| `DRAMA_COMMERCE_ALIPAY_APP_ID` / `_PRIVATE_KEY` / `_PUBLIC_KEY` | 否 | 支付宝沙箱凭证。缺任何一项时报 `payment_not_configured` 并列出缺的是哪几个变量 |
| `DRAMA_COMMERCE_ALIPAY_GATEWAY` | 否 | 支付宝网关地址，不设时用代码里钉住的沙箱网关 |
| `DRAMA_COMMERCE_WECHAT_MCH_ID` / `_API_V3_KEY` / `_PRIVATE_KEY` / `_SERIAL_NO` | 否 | 微信支付凭证。个人主体申请不了商户号，本期只留接口 |

配置缺失或非法时，服务拒绝启动，报错里只出现变量名，不出现变量值。
`migrate`、`cleanup`、`cards`、`credits` 只需要 `DRAMA_COMMERCE_DSN`；`keygen`、`keyinfo` 不读任何配置。
`admin create` / `admin disable` / `admin list` 也只要 `DRAMA_COMMERCE_DSN`（与 `cards`、`credits` 同一类）。
支付凭证只在用到对应渠道时才检查：没配好不影响服务启动，只让那个渠道报「未配置」。
`DRAMA_COMMERCE_INTERNAL_SECRET` 也不影响启动：留空时服务照常起来，只是 `/internal/*` 一律 404
（媒体控制面那边会看到「账号服务的计费接口不可用」并返回 503）。
报审包与处理配方相关的接口还会用到上面两项：字体缺失时**启动**就失败；`dossier-artifacts`
缺失时启动不失败，但 `POST /packages` 会返回 503 `service_unavailable`。

**关于 `DRAMA_COMMERCE_REQUIRE_TLS`：这个变量不存在。** commerce 的 `serve` 只接受
`--host` / `--port` / `--log-level`（`commerce/cli.py:43-49`），既不读任何 TLS 环境变量，
也没有 `--tls-cert` / `--tls-key` 参数——它只会起明文 HTTP。强制 TLS 的那个开关是媒体控制面的
`DRAMA_CLOUD_REQUIRE_TLS`（见 [`../docs/DEVELOPMENT.md`](../../docs/DEVELOPMENT.md) 的环境变量表）。
部署编排里出现的 `DRAMA_COMMERCE_REQUIRE_TLS: "0"`（`cloud/compose.deploy.yaml:66`）是给阅读者看的注释性写法，
**当前代码不会读它**。commerce 的对外 HTTPS 靠反向代理终结（见 [`DEPLOYMENT.md`](DEPLOYMENT.md) 第 5 节）。

## 3. 接口

所有接口前缀 `/api/v1`，请求和响应都是 JSON。时间字段是带时区的 ISO 8601 字符串。

### 3.1 账号

| 方法 | 路径 | 认证 | 请求体 | 成功 |
| --- | --- | --- | --- | --- |
| GET | `/health` | 无 | — | 200 `{"ok": true}`（会查一次数据库） |
| POST | `/auth/codes` | 无 | `email`、`purpose`（`register` / `reset_password`） | 202 `{"cooldown_seconds": 60, "expires_in": 600}` |
| POST | `/auth/register` | 无 | `email`、`code`、`password`、`accept_terms: true` | 201 `{"user": …, "tokens": …}` |
| POST | `/auth/login` | 无 | `email`、`password` | 200 `{"user": …, "tokens": …}` |
| POST | `/auth/refresh` | 无 | `refresh_token` | 200 `{"tokens": …}` |
| POST | `/auth/logout` | Bearer | — | 204，吊销当前会话 |
| POST | `/auth/password/reset` | 无 | `email`、`code`、`new_password` | 204，吊销该账号全部会话 |
| POST | `/auth/password/change` | Bearer | `current_password`、`new_password` | 204，只保留当前会话 |
| GET | `/me` | Bearer | — | 200 `{"user": {"id", "email", "created_at"}}` |
| GET | `/me/export` | Bearer | — | 200，个人数据 JSON（见下） |
| POST | `/me/delete` | Bearer | `password` | 204，立即注销并匿名化，不可撤销 |

`tokens` 的结构：

```json
{"access_token": "at_…", "refresh_token": "rt_…", "token_type": "Bearer",
 "expires_in": 900, "refresh_expires_in": 2592000}
```

- 邮箱会去掉首尾空格并转成小写，只接受 ASCII 邮箱。密码 8..128 个字符；登录时不检查长度下限。
- `accept_terms` 必须是 JSON 的 `true`，`"true"`、`1` 都不算。
- 请求体里多出来的字段一律拒绝。

导出（`/me/export`）的结构，`format` 是 `drama-dossier/account-export/v1`：

| 字段 | 内容 |
| --- | --- |
| `account` | `id`、`email`、`created_at`、`terms_version`、`terms_accepted_at` |
| `entitlement` | 同 `/me` 里的使用权 |
| `devices` | 全部绑定记录，含已解绑的：`code`（4 位一组的展示形式）、`name`、`platform`、`app_version`、`bound_at`、`last_seen_at`、`unbound_at`、`unbind_reason` |
| `redemptions` | 兑换记录：卡的 `kind`、`days`、`redeemed_at`，不含卡密 |
| `sessions` | 登录会话：`created_at`、`expires_at`、`revoked_at`、`revoke_reason` |
| `packages` | 报审包记录（**只有元数据**，不含文档内容）：`id`、`created_at`、`signer`、`episode_count` |
| `audit_log` | 本账号发起的操作：`at`、`action`、`subject`、`detail` |
| `exported_at` | 导出时间（数据库时间） |

导出里没有任何摘要、令牌、密码哈希；IP 本来就不存。

注销（`/me/delete`）必须重新输入密码，在一个事务里完成：

- 清空邮箱和密码哈希，记下 `deleted_at`；
- 删除会话（访问令牌、刷新令牌随之删除）、使用权、全部设备绑定、这个邮箱的验证码、报审包记录
  （`packages`）和处理配方记录（`recipes`）——前者含机器码和生成时间，后者绑定机器码，都属于个人数据；
- 审计日志记一条 `user.delete`，只记各项删除的数量。

用户行本身保留：兑换过的卡仍然指向它，不会被别人再兑换一次；审计日志只记账号 ID，邮箱删掉以后对不上具体的人。
注销后同一个邮箱可以重新注册，得到的是一个全新的账号，拿不到旧账号的使用权。
注销过的账号登录返回 `invalid_credentials`，重设密码返回 `email_not_registered`，和邮箱从没注册过一样。

### 3.2 卡密与授权

都要登录（`Authorization: Bearer <访问令牌>`）。

| 方法 | 路径 | 请求体 / 参数 | 成功 |
| --- | --- | --- | --- |
| POST | `/cards/redeem` | `code` | 200 `{"entitlement", "card": {"kind", "days"}, "already_redeemed"}` |
| POST | `/license/activate` | `device_id`、`name`、`platform`、`app_version` | 200 `{"license", "device", "entitlement"}` |
| POST | `/license/refresh` | `device_id`，可选 `app_version` | 200，结构同上 |
| GET | `/devices` | 可选查询参数 `current_device`（本机机器码） | 200 `{"devices": […], "device_limit", "unbind_quota"}` |
| DELETE | `/devices/{id}` | `id` 取自设备列表 | 204 |

`/license/activate` 和 `/license/refresh` 的响应：

```json
{"license": {"token": "<base64url 载荷>.<base64url 签名>", "kid": "9f86d081884c7d65",
             "issued_at": "…", "expires_at": "…", "refresh_after": "…"},
 "device": {"id": "<绑定 ID>", "code": "3F2A-91C0-7B44-E1D8", "name": "办公室电脑", "platform": "windows",
            "app_version": "0.3.0", "bound_at": "…", "last_seen_at": "…", "current": true},
 "entitlement": {"kind": "timed", "active": true, "expires_at": "2026-11-14T08:00:00+00:00"}}
```

- `license.expires_at` 是凭证到期（离线宽限期的终点），`entitlement.expires_at` 是使用权到期，两者不是一回事。
- `entitlement.kind`：`none` 没有使用权；`timed` 按天计，到 `expires_at` 为止；`lifetime` 永久，`expires_at` 是 `null`。
  `active` 是服务端按数据库时间算好的「现在能不能用」。
- `unbind_quota` 形如 `{"limit": 3, "window_days": 30, "remaining": 2}`：本周期还能解绑几次。
- `code`（卡密）不分大小写，空格、短横线和 `DD` 前缀都会被忽略，`DD-ABCD-…` 和 `abcd efgh …` 都能兑换。
  长度不对或含 0、1、I、O 直接返回 422，不查库。
- `device_id`：桌面端算出的机器码摘要，64 位小写十六进制（SHA-256），只认这一种写法。
  `device.code` 是它的前 16 位，转成大写、4 位一组，给人看、报给发卡方用。
- `name` 去掉首尾空白后 1..64 个字符，不能含控制字符、换行、零宽字符这类看不见的字符（全角空格可以）；
  `platform` 只能是 `windows` / `macos` / `linux`；`app_version` 以数字开头，只含字母、数字和 `.+-`，最长 32 个字符。
- `current_device` 只用来在列表里标出本机（`current: true`）。机器码不是凭证，单凭它调不了任何接口。

兑换规则：

- 月卡从「当前到期时间」和「现在」中较晚的那个往后顺延（还没到期就接着续，过期了从现在算）；永久卡把账号变成永久，不再计时。
- 同一个人重复兑换同一张卡：原样返回当前授权，`already_redeemed: true`，不会重复加时间。
- 两个人同时兑换同一张卡：只有一个成功，另一个拿到 `card_used`。
- 已经是永久的账号再兑换任何卡：返回 409 `already_lifetime`，这张卡不算用掉，可以留给别人。

绑定与解绑（名额和次数是默认值，见第 2 节配置）：

- 每个账号同时绑定 1 台电脑。同一台电脑重复调用 `activate` 不占新名额，只更新名称、平台、版本并换发凭证。
- 名额满了返回 409 `device_limit`，响应体里带上已绑定的设备、`device_limit` 和 `unbind_quota`，
  客户端可以直接让用户选一台解绑。
- 没有使用权也能绑定（凭证里 `ent` 是 `none`）；兑换卡密后调一次 `refresh`，新凭证里就是新的使用权。
- 退出登录、重设密码、会话过期都不归还名额：名额跟着电脑走，换电脑必须先解绑。
- `refresh` 只给仍然绑定在这个账号上的电脑换发凭证，否则返回 403 `device_not_bound`。
- 解绑（踢设备）：那台电脑的绑定作废，它最近一次绑定或刷新时用的登录会话一并吊销，联网请求立即失败。
  踢的如果正是发起请求的这台电脑，当前登录也随之失效。
  但它手里已签发的凭证收不回来：那台电脑一直断网的话，本地还能用到凭证的 `exp`（最多 72 小时），这是离线宽限期的代价。
- 每 30 天最多解绑 3 次（滚动计算），用完返回 429 `unbind_limit`，`Retry-After` 给出还要等多少秒。
  只数用户自己发起的解绑（表里预留了后台解绑的标记，目前没有对应的命令）。

### 3.3 授权凭证与本地验签

授权凭证是服务端签名的一段文本，桌面端用内置的公钥在本地验签，断网也能验。

- 格式：`base64url(载荷 JSON) + "." + base64url(签名)`，base64url 不带填充，只认这一种写法。
- 签名：Ed25519，签名对象是 `"drama-dossier/license/v1\n" + 载荷字节`。固定前缀用来隔离用途：
  同一把私钥以后签别的东西，那些签名也不能被挪来冒充授权凭证。
- 载荷是一个 JSON 对象（服务端按键名排序、不带空格输出；验签针对原始字节，不要求这种写法）：

  | 字段 | 含义 |
  | --- | --- |
  | `v` | 版本号，目前是 1 |
  | `kid` | 签名公钥的指纹：公钥 SHA-256 的前 16 位十六进制 |
  | `sub` / `dev` | 账号 ID / 机器码 |
  | `ent` / `ent_until` | 使用权类型；`timed` 时 `ent_until` 是使用权到期的 Unix 秒数，其余情况为 `null` |
  | `iat` / `exp` | 签发 / 凭证到期的 Unix 秒数 |

- 桌面端验签按这个顺序，哪一步不过就停在哪一步，并给出对应的错误码：
  格式 `malformed` → 签名 `bad_signature` → 载荷 `bad_payload` → 机器码 `wrong_device` → 有效期 `expired`。
  先验签名再解析载荷：没通过签名的内容，一个字节都不信。
- 凭证有效只说明「这台电脑属于这个账号」。能不能用付费功能还要看 `ent`：
  `lifetime` 能用；`timed` 要当前时间早于 `ent_until`；`none` 不能用。
- 细则以参考实现 `license_token.verify` 和测试向量 `tests/commerce/license_vectors_v1.json`（覆盖全部错误码）为准，
  桌面端（Rust）必须逐条通过。其中几条容易漏：base64url 解码后再编码要和原文一字不差；签名必须正好 64 字节；
  整数字段必须是 JSON 整数且在 0..2^63-1（`true`、`1.0`、`"1"` 都不算）；`exp` 要晚于 `iat`；
  有重复的键、`NaN` 之类的非法常量一律拒绝，不认识的字段忽略；`kid` 要和实际验过签的公钥一致；
  当前时间等于 `exp` 就算过期。
- 测试向量里的签名种子是公开的，任何人都能用它签名：正式环境绝不能信任那里的公钥。
- 离线宽限期：凭证默认 72 小时有效；联网时桌面端到 `refresh_after`（默认签发后 1 小时）就刷新，
  所以断网那一刻剩下的宽限期接近完整的 72 小时。过了 `exp` 必须联网刷新。
- 离线验证只能相信本机时钟：把系统时间往回调，可以拖长宽限期。这是离线授权本身的限制。
  服务端签发只用数据库时间，改本机时钟影响不到签发出来的 `iat`、`exp`。

### 3.4 错误

响应体固定为 `{"error": <机器码>, "message": <中文说明>}`。客户端按 `error` 分支，`message` 可以直接展示。
参数校验失败（422）还会带 `fields: [{"field", "message"}]`，不回显用户提交的值。
个别错误会带附加字段（见下表），客户端遇到不认识的字段忽略即可。

| HTTP | `error` | 含义 |
| --- | --- | --- |
| 400 | `code_invalid` / `code_expired` / `code_exhausted` | 验证码不对 / 过期 / 输错 5 次已锁定（要重新获取） |
| 400 | `wrong_password` | 修改密码时当前密码不对；注销时密码不对 |
| 401 | `invalid_credentials` | 邮箱或密码不正确（两种情况不区分） |
| 401 | `unauthorized` | 没带令牌、令牌无效或已过期、会话已吊销（包括这台电脑被解绑） |
| 402 | `insufficient_credits` | 积分不足：消费配方或生成报审包预扣时都可能返回；附带 `required`（需要多少）和 `balance`（当前多少）。事务回滚，配方 / 报审包都没有开始处理 |
| 403 | `device_not_bound` | 刷新凭证：这台电脑没绑定到当前账号，或已被解绑；生成报审包、签发 / 消费配方时也是这个含义 |
| 403 | `entitlement_required` | 账号还没有使用权，或者使用权已到期（生成报审包、签发 / 消费配方的共同前提） |
| 403 | `wrong_device` | 消费配方：这个配方不是发给这台电脑的 |
| 404 | `email_not_registered` | 重设密码：邮箱没注册 |
| 404 | `card_not_found` / `device_not_found` | 卡密不存在 / 要解绑的电脑不存在或已解绑 |
| 404 | `recipe_not_found` | 消费配方：找不到这个配方（别人的配方和不存在的配方回同一个错误，不透露配方 ID 是否存在） |
| 404 | `tier_not_found` / `order_not_found` | 充值档位不存在或已下架 / 找不到这笔订单（别人的订单同样回这个错误） |
| 409 | `email_taken` | 注册：邮箱已被占用 |
| 409 | `card_used` / `card_revoked` | 卡密已被别人用过 / 所在批次已作废 |
| 409 | `already_lifetime` | 账号已经是永久授权，不需要再兑换（卡不会被用掉） |
| 409 | `device_limit` | 名额满了；附带 `devices`、`device_limit`、`unbind_quota` |
| 409 | `recipe_consumed` | 消费配方：这个配方已经用过了，要重新申请 |
| 409 | `amount_mismatch` | 支付回调的金额与订单不一致，已拒绝入账 |
| 410 | `recipe_expired` | 消费配方：这个配方已过期，要重新申请 |
| 413 | `payload_too_large` | 请求体太大：一般接口上限 1 MiB；`POST /packages` 上限 1 MiB + 4 份扫描件（见 3.5）。在解析 JSON 之前就拒绝 |
| 422 | `invalid_request` | 参数校验失败；报审包还包括「文档内容太长压到签章位」这类排版错误，`message` 是 Rust 侧给的中文说明 |
| 422 | `scan_invalid` | 生成报审包：扫描件不是合法的 base64、格式不支持、超过 5 MB、页数与原文档不一致，或者给出了不需要盖章的文档路径 |
| 429 | `rate_limited` | 太频繁；`Retry-After` 响应头给出要等的秒数 |
| 429 | `unbind_limit` | 本周期解绑次数用完；附带 `unbind_quota`，`Retry-After` 给出要等的秒数（可能是好几天） |
| 503 | `mail_unavailable` / `service_unavailable` | 发信失败（可立即重试） / 数据库不可用；报审包还包括服务端缺少 `dossier-artifacts`、工具运行失败或超时、签章服务未实现 |
| 503 | `price_missing` / `invite_not_configured` / `payment_not_configured` | 缺少计价规则 / 邀请奖励配置丢了 / 支付渠道没配好（Mock 缺签名密钥、真实渠道缺凭证或对外地址、Mock 渠道查单） |

### 3.5 报审包

| 方法 | 路径 | 请求体 / 参数 | 成功 |
| --- | --- | --- | --- |
| POST | `/packages` | 见下 | 200 `{"package_id", "manifest", "signature", "credits", "files"}` |

三个前提缺一不可：**登录**（没带令牌或令牌失效是 401 `unauthorized`，见 3.4）、`device_id` 是当前账号
绑定且未解绑的电脑（否则 403 `device_not_bound`）、账号有**有效使用权**（否则 403 `entitlement_required`）。
另外积分要够：集数越多扣得越多，不够时在预扣阶段就返回 402 `insufficient_credits`，不开始渲染；
渲染或签章失败会把预扣的积分退还（见 3.7）。

请求体（字段 snake_case，多一个不认识的字段一律 422）：

| 字段 | 必填 | 说明与上限 |
| --- | --- | --- |
| `device_id` | 是 | 机器码，64 位小写十六进制 |
| `drama_name` | 是 | 剧名，1..200 个字符（Rust 侧还要求能当文件名用、不含 `/`） |
| `ai_generated` | 是 | 只接受 JSON 的 `true` / `false` |
| `copy_episodes` | 是 | 是否把分集视频复制进包；`false` 时只登记哈希 |
| `cost_sheet` | 否 | `company_name` / `contact` 各 ≤ 200 个字符；`total_investment_wan` 0..1e9；`actor_ratio_percent` 0..100 |
| `synopsis` / `cover` | 否 | `file_name` 1..255 个字符、`size_bytes` 0..2^50、`sha256` 64 位小写十六进制 |
| `episodes` | 是 | 最多 **500** 集；每集含 `file_name`、`size_bytes`、`sha256`、可选 `episode_number`（0..100000）和 `spec` |
| `episodes[].spec` | 是 | `duration_sec` 0..86400；`width` / `height` 0..16384；`video_codec` / `audio_codec` 各 ≤ 32 字符；`fps` 0..1000；`bitrate_bps` 0..1e10 |
| `signer` | 否 | `mock`（默认，占位章）/ `scan`（上传盖章扫描件） |
| `scans` | 否 | `signer: scan` 时：文档路径 → 扫描件（PNG / JPEG / PDF）的 base64，每份 ≤ 7,000,000 个字符（约 5 MB 原始字节），最多 **4** 份。可以只覆盖一部分要盖章的文档（混合签章） |

整个请求体的上限是 1 MiB + 4 × 7,000,000 字节（`commerce/body_limit.py`）：先看 `Content-Length`，
没有或谎报时边收边数，超了立即返回 413 `payload_too_large`，不把请求读进内存。其余接口一律 1 MiB。

`scans` 的键必须是这次要盖章的文档路径（`04_成本配置表/成本配置表.pdf`、
`05_权属声明/内容权属声明.pdf`），给了别的路径返回 422 `scan_invalid`；
用 `signer: mock` 时不允许带 `scans`（同样 422）——想用占位章就不要顺手把扫描件一起发上来。
`scans` **可以只覆盖其中一部分**文档：给了的走扫描件，没给的退回占位章（见下面的「混合签章」）。
扫描件按**文件头**判断类型（不信扩展名）：PDF 原样检查后返回（不能加密、不能是空文件），
PNG / JPEG 铺满一页 A4；页数必须与原文档一致，否则 422 `scan_invalid`。

服务端做的事：检查前提 → 算价并预扣积分（见 3.7）→ 调 `dossier-artifacts render-package` 生成文本文件与排版模型 →
把两份文档画成 A4 PDF 并盖章 → 拼清单 `drama-dossier/package@2`、用 Ed25519 签名
（用途前缀 `package-manifest`）→ 在一个事务里记一行 `packages` 和一条审计日志 → 结算积分 → 返回。

返回里多一个 `credits`（这次扣了多少积分）：

```json
{"package_id": "<UUID>",
 "manifest": "<manifest.json 的原样字符串>",
 "signature": "{\"kid\": \"…\", \"sig\": \"…\"}",
 "credits": "<本次扣掉的积分>",
 "files": [{"path": "01_剧集信息/剧名.txt", "content": "<base64>"}]}
```

`manifest` 和 `signature` 是**原样字符串**：签名针对的就是这些字节，桌面端必须把原文写回包里、
不能重新序列化。`files` 里是包内路径 → base64 内容，桌面端逐个重算 SHA-256 与清单比对。

- **视频不上传**：分集、封面、简介只传桌面端算好的文件名、大小和 SHA-256，服务端把它们登记进清单，
  组装时由桌面端重新算一遍对比。服务端**不接收也不保存**任何文档内容。
- 占位章（`signer: mock`）：文件名在 `.pdf` 前加 `_未签署`，清单里 `signature.placeholder = true`。
- **混合签章**（`signer: scan`，但 `scans` 只覆盖一部分要盖章的文档）：给了扫描件的那份用扫描件
  （文件名不加后缀），没给的那份仍然走占位章（加 `_未签署` 后缀）。此时 `signature.placeholder`
  仍为 `true`，因为包里确实还有未签署的文档。
- 清单里 `signature.perDocument` 逐份给出实际用的签章方式：

  ```json
  "signature": {
    "signer": "scan",
    "placeholder": true,
    "documents": ["04_成本配置表/成本配置表.pdf", "05_权属声明/内容权属声明_未签署.pdf"],
    "perDocument": [
      {"document": "04_成本配置表/成本配置表.pdf", "signer": "scan"},
      {"document": "05_权属声明/内容权属声明_未签署.pdf", "signer": "mock"}
    ]
  }
  ```

  桌面端据此逐份标明「扫描件签章」或「占位章 · 未签署」，**不能只显示整包一个结论**——
  否则使用者会以为整包都盖好了。旧版服务端不发这个字段，桌面端按「整包占位」处理（向后兼容）。
  注意 `perDocument[].document` 是**实际写进包里的路径**（占位章那份带 `_未签署` 后缀），
  而 `signature.signer` 是一次生成里用到的签章器名字（`scan` 或 `mock`）：
  `signer: scan` 且 `scans` 为空时，逐份都会是 `mock`，此时等价于整包占位章。
- 失败时不会留下半成品：`dossier-artifacts` 找不到 / 运行失败 / 超时（30 秒）/ 输出不是 JSON
  一律 503 `service_unavailable`，请求本身不合法（退出码 2）返回 422 `invalid_request`，
  `message` 就是 Rust 侧的中文说明。`dossier-artifacts` 对传给它的请求 JSON 有 1 MiB
  （`1 << 20`）上限，超了同样按请求不合法处理。

限流（每账号，固定时间窗）：

| 接口 | 规则 |
| --- | --- |
| `POST /packages` | 每账号每小时 **30** 次 |
| `POST /recipes` | 每账号每小时 **60** 次 |
| `POST /recipes/{id}/consume` | 每账号每小时 **120** 次 |

### 3.6 处理配方

处理配方（M4a 服务端接口，M4b 才接桌面界面）由服务端签发参数，绑定机器码和任务，**只能消费一次**。

| 方法 | 路径 | 请求体 | 成功 |
| --- | --- | --- | --- |
| POST | `/recipes` | `device_id`、`task`、`platform`、`strength`、`episode_count` | 200 `{"recipe_id", "token", "expires_at", "params", "credits", "episode_count"}` |
| POST | `/recipes/{recipe_id}/consume` | `device_id` | 200 `{"recipe_id", "consumed": true, "credits"}` |

- 两个接口都要求登录、这台电脑绑定在当前账号上、使用权有效（与 3.5 同样的三个前提）。
- 请求字段：`device_id` 是 64 位小写十六进制机器码；`task` 是**任务描述的哈希**，64 位小写十六进制；
  `platform` 只能是 `universal`（只生成一套通用版本，见 3.6 的平台规格）；
  `strength` 只能是 `light` / `medium` / `heavy`；
  `episode_count` 是这次要处理的集数（1..500），**由桌面端报上来**——服务端只收到任务哈希、看不到集数，
  而计价按集算（见 3.7）。

**任务哈希（`task`）**：由桌面端计算后传给服务端；服务端只把它绑进配方并原样回填，**不解析也不校验内容**。
规则（`dossier-license::version_task`）：

- `task = SHA-256( 规范化 JSON )`，JSON 只含这几项，键按字母序、无空白（非 ASCII 字符按 UTF-8 原样输出）：

```json
{"episodes":[{"name":"第01集.mp4","sha256":"…"}],"rights":"<权属材料 sha256>","schema":"version-task@1","strength":"medium","template":"<模板哈希>"}
```

- `episodes`：按文件名升序排列；`name` 是文件本身的名字（不带目录）；`sha256` 是文件内容的 SHA-256（小写十六进制）。
- `rights`：权属材料文件内容的 SHA-256（同上）。没声明权属材料就没有任务哈希，也就申请不了配方。
- `template`：第一层模板 JSON 的哈希。算法：把模板解析成 JSON 对象，按同样的规则规范化（键排序、无空白、UTF-8 原样）后取 SHA-256。
- `strength`：`light` / `medium` / `heavy`。
- 换一集、改模板、换档位、换权属材料，哈希就变，本机验签（`wrong_task`）就不过。
- Python 参考实现（对拍用）在 `cloud/tests/commerce/version_task.py`。

`token` 是 `base64url(载荷 JSON) + "." + base64url(签名)`，签名用途前缀 `recipe`（和授权凭证、报审包清单
互不冒充）。载荷字段与 `dossier-license::recipe` 一致：`v`（版本号，目前 1）、`kid`、`rid`（配方 ID）、
`sub`（账号 ID）、`dev`（机器码）、`task`、`platform`、`strength`、`seed`、`params`、`iat`、`exp`。
桌面端本地验签顺序是 格式 → 签名 → 载荷 → 机器码 → 任务 → 有效期，错误码分别是
`malformed` / `bad_signature` / `bad_payload` / `wrong_device` / `wrong_task` / `expired`。

**有效期**：`RECIPE_TTL_SECONDS = 24 × 3600`，即签发后 **24 小时**内要开工。桌面端判断「当前时间等于
`exp`」即算过期。

**只能消费一次**：由服务端记账。消费时先锁住这一行，再按顺序检查——找不到（别人的配方也是这个结果）
→ `recipe_not_found`（404）→ 机器码不对 `wrong_device`（403）→ 已消费 `recipe_consumed`（409）
→ 已过期 `recipe_expired`（410）→ 再查一次使用权（403 `entitlement_required`）→ 扣积分 → 记 `consumed_at`。
第二次消费一定失败；只靠本地记录挡不住重放（把本地记录删掉就能再用一次），所以必须在服务端记账。
`recipes` 表里记着 `seed` 和平台 / 强度，配合 `derive_params` 就能算出同样的参数，处理结果可以复现。

**扣费**：签发时按 `episode_count` 算好 `credits` 写库，消费时按这个值扣（不重算，理由见 3.7）。
扣费与「标记已消费」在同一个事务里，先 `hold` 再立刻 `settle`——消费即定案。余额不足返回
402 `insufficient_credits`（附带 `required` 与 `balance`），整个事务回滚，配方仍然可以充值后再消费。
处理在本机执行、服务端看不到结果，所以**处理失败不退积分**，界面要在确认框里写明。

#### 各强度的参数上下限

`params.subtle` 的每一项都在该档的上下限内取值（同一 `seed` 永远算出同样的参数）。
`brightness` / `hue_degrees` / `pitch_semitones` 是**偏移量**：上下限描述幅度，正负号随机。

| 参数 | 含义 | light | medium | heavy |
| --- | --- | --- | --- | --- |
| `grain` | ffmpeg `noise` 滤镜 alls 强度（0–100） | 1–3 | 3–6 | 6–10 |
| `zoom_percent` | 放大再裁回原尺寸的比例（%） | 1.0–1.5 | 1.5–2.2 | 2.2–3.0 |
| `brightness` | `eq` 亮度偏移（-1–1） | 0–0.01 | 0–0.02 | 0–0.03 |
| `saturation` | 饱和度倍数 | 0.98–1.02 | 0.96–1.04 | 0.94–1.06 |
| `hue_degrees` | 色相偏移（度） | 0–1.0 | 0–2.0 | 0–3.0 |
| `tempo` | 整体变速倍数（画面和声音一起） | 0.99–1.01 | 0.98–1.02 | 0.97–1.03 |
| `pitch_semitones` | 变调（半音） | 0–0.1 | 0–0.2 | 0–0.3 |
| `trim_head_frames` | 片头裁几帧 | 1–2 | 2–3 | 3–5 |
| `trim_tail_frames` | 片尾裁几帧 | 1–2 | 2–3 | 3–5 |
| `quality.min_ssim` | 画质损失下限（M4b 检测用） | 0.97 | 0.92 | 0.84 |
| `quality.min_psnr` | 画质损失下限（M4b 检测用） | 38.0 | 35.0 | 32.0 |

#### 平台规格（第一层 `params.platform`）

只生成**一套通用版本**（`platform = "universal"`），生成后由用户自己决定发到哪里：取值是抖音 / 快手 /
视频号三家公开要求里**最严**的值，三家各自的数值见 `platform_specs.py` 的注释（只有视频码率三家不同：
抖音 6000kbps、快手 6000kbps、视频号 5000kbps，取最严的 5000kbps）。

**保守值，待复核**：平台随时会改规则，M4b 动手处理视频前要按官方帮助中心复核一次。

| 平台 | 分辨率 | 帧率 | 视频码率 | 音频码率 | 单集时长上限 | 封装 / 编码 |
| --- | --- | --- | --- | --- | --- | --- |
| `universal` | 1080×1920 | 30 | 5000 kbps | 128 kbps | 900 秒 | mp4 / h264 / aac |

整理日期 **2026-10-15**，来源（`platform_specs.py` 的 `SOURCES`）：

- <https://www.kuaishou.com/help/feedback/2730>
- <https://www.kuaishou.com/help/feedback/4000>
- <https://developers.weixin.qq.com>
- <https://developer.open-douyin.com/docs/resource/zh-CN/dop/develop/openapi/video-management/video-upload/upload-video>

状态：**待复核**——抖音、视频号没有官方原文，其余是二手转述，M4b 前需按官方帮助中心再核一次。

### 3.7 积分

积分是付费功能的计量单位：处理版本和报审包都按**集**收费。设计目标是「任何一笔积分变动都能从流水推回来」。

#### 账本设计

`credit_ledger` 只有插入，没有修改和删除（应用层没有任何 UPDATE / DELETE 它的代码路径）。**余额一律由
`SUM(delta)` 算出来，不存可变的 `balance` 字段**：

- 余额是钱。存一个可变字段，就意味着「余额」和「流水」是两份独立数据，算错一次之后再也没法从流水推回正确余额，
  对账时也说不清哪一边对；
- 代价是查一次余额要扫一次索引（`credit_ledger_user_time`），用户量级小，可接受。

`delta` 是整数（正数加、负数扣），单位是积分，**不用浮点**。五种 `kind`：

| `kind` | 含义 | 方向 |
| --- | --- | --- |
| `grant` | 充值到账（支付回调入账）、管理员手动加分 | 正 |
| `hold` | 预扣 | 负 |
| `release` | 预扣退还（报审包生成失败） | 正 |
| `reward` | 邀请奖励（邀请人得 `invite:` 键、被邀请人得 `invitee:` 键，各一条） | 正 |
| `expire` | 悬挂过久的预扣被自动回收 | 正 |

`settled_state` 只有 `hold` 才有，三种状态：

- `reserved` 预扣中：钱已经从余额里减掉了，但还没定案；
- `settled` 已消费：预扣的那笔钱就是花掉了，**不再写新流水**；
- `released` 已退还：原 hold 标成退还，并**另外写一条正的 `release` / `expire` 流水**。

为什么失败要写新流水，而不是把 hold 删掉或改成 0：账本只增不改，「退过钱」这件事本身必须留下痕迹，
否则事后看不出一笔钱进来过又出去过。数据库层还有一条约束：`(kind = 'hold') = (settled_state IS NOT NULL)`，
非 `hold` 的流水不许带状态。

`hold` 里**先锁用户行、再动流水**（`lock_user`）：余额检查与写入之间不能有别人插进来。`settle` / `release`
只动流水、不需要用户行锁，但加锁顺序照样固定，才不会互相等死。`release_stale_holds` 用
`FOR UPDATE SKIP LOCKED` 扫描，整个扫描与改写在一个事务里完成——不在事务里的话锁立刻释放，
两个并发调用会挑中同一批行。

#### 计价

价目表 `price_rules` 一个计价对象一行，目前三个对象：`version`（处理版本）、`package`（报审包）、
`server_version`（服务端处理，M5b-4）。后者的种子数据在 `0010_server_version_price.sql`：
`credits_per_unit = 5`、`min_units = 1`，**与本地处理版本同价**——服务端只是替用户跑同一件事，
差别在谁的机器出力；等真有机房/GPU 成本要摊，再按实际情况调那一行。

计价单位只有 `episode`（集），初始值是**每个对象每集 5 积分**，`min_units` 是 1（不足一集也按一集算）。
价目表的初始值是迁移里写死的种子数据，后台改价留到 M7；表里查不到规则时返回 503 `price_missing`，
**不会默认成 0**（那等于免费），也不猜一个数。

代码里的清单是 `credits.SUBJECTS = ("version", "package", "server_version")`：
`cost_for()` 先用它挡掉不认识的计价对象（422 `invalid_request`），再去查 `price_rules`。
数据库那层只挡空串（`0010` 把 `0006` 的 `CHECK (subject IN (...))` 换成了 `CHECK (subject <> '')`），
理由是「在数据库里维护一份取值清单，每次加价目都要改迁移，且容易与代码里的清单不同步」。

- 处理版本：`cost_for("version", episode_count)`。集数由桌面端报上来（服务端只收到任务哈希，看不到集数），
  上限 500，和报审包一致。
- 报审包：`cost_for("package", len(episodes))`。集数就是请求里 `episodes` 的条数（最多 500）。
- 服务端处理：`cost_for("server_version", 集数)`，由媒体控制面通过 `/internal/credits/cost` 问价，
  **客户端不能传价格**。媒体控制面按 `max(1, 素材数)` 算 `units`——服务端处理就是逐集跑。

**价格在签发时定死**：`POST /recipes` 算出 `required` 后，把它和集数一起写进 `recipes.credits` / `episode_count`；
消费时直接读这个值扣款，**不重算**。否则中途改过价目表，就会出现「签发时说 25、扣钱时扣 30」。
0007 之前签发的旧配方这两列是 NULL（那时还没有计费），按 0 处理，只参与审计。

`credits.quote()` 是「只算价、不扣钱」的辅助函数（返回 `required` / `balance` / `enough`），
`price_per_unit()` 只查单个计价对象的单价；目前没有把它们暴露成接口。界面上的预估价是桌面端按「每集 5 积分」
自己乘出来的，确认框里同时写明「准确数额以服务端签发时为准」——以 `POST /recipes` 返回的 `credits` 为准。

#### 扣费时机

三个付费功能的定案时点不同，这是刻意的：

**处理版本：消费即定案。** 扣费发生在 `POST /recipes/{id}/consume` 里，和「标记配方已消费」在**同一个事务**：
先 `hold` 再立刻 `settle`，要么配方被消费且钱被扣，要么两件都没发生。余额不足时 `hold` 抛 402，
整个事务回滚，配方**仍然可以充值后再消费**。

失败不退，原因写在代码注释里：**处理在本机执行，视频不上传，服务端无从判断成败**，留着「预扣中」等一个永远不会
到来的结算只会让钱悬挂。界面必须在确认框里写明这一点（见 3.6 与用户手册）。

**报审包：三段式（hold → 渲染 → settle / release）。** 渲染要跑子进程、可能几秒，所以：

1. 先算价、在**独立事务**里 `hold`（预扣）。放在渲染之前，避免余额不足的请求白占 CPU 与字体资源；
2. 再渲染、签章、写库（`_create_charged`）。渲染耗时，不能和扣费挤在同一个长事务里占着连接与锁；
3. 全部成功才 `settle`；渲染、签章、写库任何一处出错（`except BaseException`，进程被取消也算）走 `release` 退还。

预扣的 `ref` 用一次性的 `package:<uuid>` 而不是剧名——`ref` 会进流水和审计日志，而审计日志一直刻意只存剧名的哈希。

**服务端处理（M5b-4）：预扣 → 跑任务 → 结算 / 退还，由媒体控制面驱动。** 与服务端自己执行的报审包不同，
这条链路的每一步都是**另一个服务**来调的，走 3.9 的内部接口：

1. 建任务**之前**先 `hold`。没扣到钱就不该占队列资源；`ref` 是 `server_task:<幂等键或一次性 id>`；
2. 预扣成功才建任务；建任务失败（参数错、配额、数据库）就把预扣退回去，
   媒体控制面用 `except BaseException` 兜住，进程被取消时也不把钱留下；
3. 任务终态时按 `credit_ref` 结算或退还：成功 `settle`，失败/取消 `release`。

「视频在服务端跑」让退还成为可能——服务端知道任务到底成没成。但**结算/退还这一步本身也可能失败**
（commerce 连不上）：此时媒体控制面**不把已经成功的任务判成失败**，而是把失败显式返回并写进审计
（`credit.settle` / `credit.release`），留给人工对账。把「钱没结算成功」变成任务失败，
用户会以为视频没做出来，而真实情况是视频好了、账没对上。

#### 预扣超时回收

`HOLD_TTL_SECONDS = 2 × 3600`，即 **2 小时**：超过这么久还没结算的 `reserved` 视为放弃，自动退还
（写一条 `expire` 正数流水并把原 hold 标 `released`）。

- `release_stale_holds` 扫的是「所有 `kind='hold' AND settled_state='reserved'` 且超时的行」，不按 `ref` 区分，
  所以报审包与**服务端处理**的悬挂预扣都靠它兜底。处理版本的预扣与结算在同一个事务里，不存在悬挂状态。
- 回收是**查余额时顺带做的**：`balance()` 与 `hold()` 都先按 `user_id` 回收这个人的悬挂预扣再算余额，
  否则用户看到的余额里有一笔其实早该退回来的钱，会以为积分丢了。
- 进程崩溃、服务重启留下的 `reserved` 由管理员兜底：`drama-commerce credits expire`（见 5.3）扫全库回收。

#### 接口

| 方法 | 路径 | 认证 | 成功 | 典型错误 |
| --- | --- | --- | --- | --- |
| GET | `/credits` | Bearer | 200 `{"balance", "history"}`，`history` 是最近 **20** 条 | 429 `rate_limited` |
| GET | `/credits/history` | Bearer | 200 `{"items"}`，`limit` 默认 50、夹在 1..200 | 429 `rate_limited` |
| GET | `/invites` | Bearer | 200 `{"code", "invited", "rewards", "earnedCredits", "inviterCredits", "inviteeCredits", "maxRewards"}` | 503 `invite_not_configured` |

`GET /credits` 与 `GET /invites` 共用一个限流桶（`credits:balance`，每账号每小时 240 次），
`GET /credits/history` 单独一个（每账号每小时 120 次）。

流水条目：`{"delta", "kind", "ref", "settledState", "memo", "createdAt"}`，新到旧。它只用于展示，
不参与任何判断——判断一律看 `SUM(delta)`。

`GET /credits` 一次就把余额和最近流水都返回，界面上少一次往返。

#### 邀请

四道防刷闸，缺一不可（`invites.py` 的模块注释与 0009 迁移）：

1. **一个账号只能被邀请一次**：`users.invited_by` 只写一次，重复绑定不覆盖；
2. **同一台电脑最多给一个账号当邀请来源**：用已有的 `devices.device_id`（机器码）判断，不引入新指纹。
   只看「被邀请过」的账号，正常用户自己一台电脑绑自己一个账号不受影响；
3. **被邀请人真的付过钱才发奖励**：注册就发等于白送，刷号成本几乎为零；
4. **每个邀请人有奖励次数上限**：防止有人用一批小号互相邀请。

邀请码由 `user_id` 派生（`SHA-256("invite:" + user_id)` 的前 10 位十六进制，大写），可复现、不需要额外存储。
注册时可以带 `invite_code`（最长 16 字符）和 `device_id`；**码填错或触发防刷都不会让注册失败**，
只是不绑，触发防刷的会记审计日志（`invite.same_device`、`invite.self`）。

奖励配置在 `invite_settings`（单行表）：**邀请人 50 积分、被邀请人 20 积分、每人最多 20 次**。
配置行缺失时报 503 `invite_not_configured`——宁可报错，也不按 0 发。

发放时机：支付回调入账成功后调 `invites.grant_on_first_payment()`。它**自己再查一次**这个账号有没有
`status = 'paid'` 的订单，不信任调用方——这个方法一旦被误调用就是白送积分，多查一次 SQL 比信任调用方便宜。
幂等键是 `invite:<被邀请人>`（邀请人侧）和 `invitee:<被邀请人>`（被邀请人侧），
所以重复触发（多次付费、重复调用）只发一次。发放时把邀请人这一份的数量写进 `invite_rewards.credits`
（`/invites` 的 `earnedCredits` 是它的合计），之后改配置不影响已经发出去的奖励。

#### 媒体库自己的 `billing_*` 表为什么保留

媒体控制面（`drama-cloud`）库里还有一套 `billing_accounts` / `billing_quotes` / `billing_ledger`。
它和这里的 `credit_ledger` 是**两本账，不是两套真相**：

- **服务端处理的扣费走 commerce 积分**（就是本节讲的这本账）。报审包、本机处理版本扣的也是它，
  所以用户只有一个余额、一份流水，不会看到「服务端处理扣了 A、报审包扣了 B」。
  媒体控制面通过 3.9 的内部接口调用，`ref` 形如 `server_task:<task_id>`，审计里的操作人是 `media-server`。
- **媒体库的 `billing_*` 表保留为「可独立运行的账本」**：CLI 与既有测试在用，也让媒体控制面能在
  没有 commerce 的环境里单独跑通（学习原型要能单独演示一条任务链路）。
- **两条路互斥**：任务上有 `credit_ref` 就走 commerce，有 `billing_quote_id` 就走本地账本
  （`settle_task_billing` 按这个顺序判断），两者都有时以 `credit_ref` 为准，两者都空则不扣费。
- **为什么不干脆删掉本地那套**：删掉要改 CLI、测试与任务创建路径，收益只是少两张表；
  而且「同一份代码两种部署形态都能跑」本身是学习原型想要的性质。

### 3.8 支付

支付只做一件事：把「渠道说这笔钱到账了」变成一条 `grant` 流水。订单表 `payment_orders` 记原因
（谁、什么时候、买了多少积分、渠道侧单号），账本记结果。

#### 渠道抽象

`PaymentProvider`（`payments/base.py`）四个方法：`create()` 下单返回「怎么去付」、`parse_notify()` 把回调变成
`PaymentEvent`（**先验签再取数据**）、`ack()` 按渠道要求应答、`query()` 主动查单。

一条硬规则：**任何渠道的实现都不许自己改账本。** 入账只有一条路 `payments.service.handle_event()`，
渠道实现只负责「把渠道的报文变成 `PaymentEvent`」。一个渠道要能用，必须**同时**满足「代码里有实现」
和「配置里有凭证」；缺任何一样都显式报 `NotConfigured`（`payment_not_configured`），绝不降级成「假装成功」。

| 渠道 | 状态 | 说明 |
| --- | --- | --- |
| `mock` | 可用 | 本地模拟收银台，没有商户号也能把「下单 → 支付 → 回调 → 入账」整条链路跑通 |
| `alipay` | 占位 | 凭证齐了也不下单：签名算法还没实现，`create` / `parse_notify` / `query` 一律抛「还没实现」 |
| `wechat` | 占位 | 个人主体申请不了商户号，且 APIv3 没有官方沙箱，只留接口 |

**Mock 的签名方式**：本地 HMAC-SHA256，签名头是 `X-Mock-Signature`，签名对象是回调报文的原始字节，
比较用 `hmac.compare_digest`（防时序侧信道）。**关键设计是回调走和真实渠道完全相同的代码路径**，
唯一不同的只是签名算法（本地 HMAC，而不是 RSA2 / AES-GCM 解密）——这样才真的测到验签、金额校验和重复回调去重。

下单返回的跳转地址指向本服务自己的模拟收银台 `/pay/mock/checkout?out_trade_no=…`。地址由 `public_base_url` 拼成；
没配时用**本次请求的来源**（`request.base_url`）补成完整地址——客户端要把它渲染成可点的链接，
只给相对路径等于让用户点不开、本地闭环走不下去。这个路由不进 `/openapi.json`。收银台上四个按钮——
「支付成功」「重复回调」（发一份**逐字节相同**的报文，才测得到幂等）「支付超时」「伪造金额」（改大一分再签名）——
都只是把页面上展示的那份报文带着签名 POST 到真实的 `/api/v1/payments/mock/notify`，
入账仍然走 `handle_event()` 那一条路。页面不存会话状态，报文与签名是服务端渲染时算好放进 `data-` 属性的。
没配 `mock_pay_secret` 时收银台不显示（构造 provider 就抛「未配置」），而不是显示一个点了没用的假页面。

Mock **没有渠道侧可查**：`query()` 直接报「模拟支付没有渠道侧可查单」，
不假装「查到未支付」——那会让对账以为真的查过了。

**`public_base_url` 的作用**：支付渠道是**主动把通知推到我们的服务**的，所以回调地址必须能从公网访问。
`notify_url_for()` 用 `public_base_url` + `/api/v1/payments/{channel}/notify` 拼地址；
没配时 Mock 用相对路径，真实渠道**明确报错**（「没有配置对外地址，支付渠道无法把结果推回来」），
而不是拼一个假地址让人以为配好了。接支付宝沙箱时本机开发得用内网穿透。

#### 订单

`POST /payments/orders` 按档位下单，订单里的**金额与积分在下单时定死**，之后改档位不影响已有订单。
订单号是「时间前缀 + 12 位随机十六进制」，全局唯一，回调按它去重。订单有效期 `ORDER_TTL_SECONDS = 2 × 3600`，
即 2 小时，和支付宝 / 微信的默认有效期一致。

下单时渠道报错（没配好 / 网络）会把订单标成 `closed` 而不是删掉：留一行「曾经尝试过但渠道没成功」对排查有用，
删掉会让对账少一条记录；也不能留着 `created`——那会让「待支付」的统计永远包含一笔根本付不了的订单。

订单状态：`created` 待支付 / `paid` 已支付 / `closed` 已关闭 / `refunded` 已退款。
已支付的订单必须有 `paid_at` 和渠道单号（数据库 CHECK 约束）——否则这一行说不清钱到底到没到。

充值档位 `recharge_tiers`（下架的不出现在列表里，但历史订单仍然对得上），初始三档：

| 档位 | 售价 | 到账积分 |
| --- | --- | --- |
| `tier-100` | 1000 分（10 元） | 100 |
| `tier-500` | 5000 分（50 元） | 550 |
| `tier-2000` | 20000 分（200 元） | 2400 |

金额是**示例价**（1 积分 = 1 分钱，量大再给一点优惠），正式上线前按实际定价改，改价留到 M7。

#### 回调的三条硬规则

1. **只在「已验签的 `PaymentEvent`」上入账。** `handle_event()` 不接受回调原文，调用方必须先把渠道回调变成
   `PaymentEvent`；验签失败一律拒绝，返回渠道要求的失败应答让它重投。
2. **订单锁 + 账本幂等键，两层去重。** 渠道会重复投递回调：先 `SELECT … FOR UPDATE` 锁订单行看状态，
   已经是 `paid` 就直接返回（`already: true`、`credited: 0`）；即使这一层漏了，账本的
   `idempotency_key = "order:<订单号>"` 唯一约束也会挡住第二次入账。
3. **金额与订单号必须对得上。** 回调金额和订单金额不一致，通常意味着有人在伪造，一律拒绝并记审计日志。

两种拒绝都要**留痕后再拒绝**，而且审计写在**独立事务**里（`_audit_alone`）：拒绝时我们会 `raise`，
写在同一个事务里会被异常一起回滚掉，而「有人拿错金额来撞接口」恰恰最需要留痕。

| 情况 | 审计动作 | 结果 |
| --- | --- | --- |
| 订单号不存在 | `payment.unknown_order` | 抛 404 `order_not_found`；回调路由接住后按渠道要求回**失败应答**（Mock 是 400），不入账 |
| 金额不符 | `payment.amount_mismatch` | 抛 409 `amount_mismatch`；同样回失败应答，不入账 |
| `closed` / `refunded` 事件 | `payment.closed` / `payment.refunded`，**只更新订单状态、不动账本** | 回成功应答，`credited: 0` |
| 首次入账成功 | `payment.paid`，随后触发邀请奖励 | 回成功应答，`X-Credited` 响应头给出这次到账的积分 |

#### 接口

| 方法 | 路径 | 认证 | 成功 | 典型错误 |
| --- | --- | --- | --- | --- |
| GET | `/payments/tiers` | Bearer | 200 `{"items": [{"id", "amountFen", "credits"}]}` | — |
| POST | `/payments/orders` | Bearer | 200 `{"outTradeNo", "channel", "checkout": {"kind", "payload"}, "amountFen", "credits", "expiresAt"}` | 404 `tier_not_found`；503 `payment_not_configured` |
| GET | `/payments/orders/{out_trade_no}` | Bearer | 200 订单详情（含 `status`、`paidAt`、`createdAt`、`expiresAt`） | 404 `order_not_found` |
| POST | `/payments/orders/{out_trade_no}/reconcile` | Bearer | 200 `{"status", "credited", "already"}`：订单已是 `paid` 时是 `already: true`、`credited: 0`，否则是 `handle_event` 的结果 | 404 `order_not_found`；503 `payment_not_configured` |
| POST | `/payments/{channel}/notify` | **无**（靠验签） | 按渠道要求应答：Mock 200 `{"ok":true}`、支付宝 200 `success`、微信 204 无正文 | 验签失败 / 订单不存在 / 金额不符都回失败应答，不入账 |

- 请求体只有两个字段：`tier_id`（1..32 字符）和 `channel`（`mock` / `alipay` / `wechat`）。
  **金额与积分由服务端按档位决定，客户端不能传。**
- `checkout.kind` 是 `redirect`（跳转收银台）或 `qrcode`（展示二维码内容），`payload` 是跳转地址或二维码里要放的字符串。
- 查单**只能查自己的订单**：别人的订单和不存在的订单回同一个 404，不透露单号是否存在。
- `reconcile` 是用户点「我已完成支付」时的兜底（回调可能因网络或服务重启丢失）。它先看订单状态，
  已支付就直接返回 `already: true`；否则调 `provider.query()`，查不到就**如实说「还没付」，不猜、不手动置为已付**。
  Mock 渠道没有渠道侧可查，这一步会返回 503 `payment_not_configured` 并说明原因。
- `/payments/{channel}/notify` **不要登录**：渠道不会带我们的令牌，只能靠验签确认来源。顺序是
  先限流（按来源 IP）、再取 provider（没配好报 503）、再验签、最后才入账。验签失败只记日志
  （日志里不含报文内容，可能含敏感信息）并回失败应答。

### 3.9 内部接口（给同机房的其他服务用）

前缀 `/internal`，**不在 OpenAPI 文档里**（`include_in_schema=False`）。这些接口只给同机房的服务调用
（目前只有媒体控制面 `drama-cloud`），不该走公网。

**为什么需要它**：桌面端的登录状态在 commerce 里，媒体控制面自己**没有账号体系**。
与其让用户登录两次、再人工对齐两套用户，不如让它拿桌面端已有的访问令牌来问一句「这是谁」；
扣费同理——两边共用一个余额，用户才不会看到两个数对不上。

#### 调用方认证

| 请求头 | 用途 |
| --- | --- |
| `X-Internal-Secret` | 共享密钥，与 `DRAMA_COMMERCE_INTERNAL_SECRET` 同一个值 |
| `X-Access-Token` | 待验证的访问令牌（**只在 `/internal/introspect` 用**） |

- 用自定义头而不是 `Authorization`，免得和「用户令牌」的 Bearer 语义混在一起。
- 比较走 `hmac.compare_digest`（恒定时间），和验签一样认真——否则可以从响应时间反推密钥。
- **密钥留空时这些路由一律 404**，不是「放行」。理由写在 `internal_routes.py` 的模块注释里：
  留空若等于不鉴权，一次配置遗漏就会让任何人都能拿令牌换用户身份。
- 待验证的访问令牌走**请求头**，不进请求体：请求体会被框架记进错误日志，令牌不该出现在日志里。
- 只返回 `user_id` 与 `email`，不返回令牌、密码摘要、设备等任何其他东西。

#### 限流

两类接口各有一个独立的桶，按调用方来源计数，**每分钟 6000 次**：

| 规则 | 用于 |
| --- | --- |
| `internal:introspect` | `/internal/introspect` |
| `internal:credits` | `/internal/credits/*` |

调用方是服务而不是人，所以给得比面向用户的接口宽；但仍然要限，否则一个死循环的客户端就能打爆这里。

#### 两类接口

**一、令牌内省**

| 方法 | 路径 | 请求 | 成功 | 典型错误 |
| --- | --- | --- | --- | --- |
| POST | `/internal/introspect` | 头部 `X-Internal-Secret`、`X-Access-Token` | 200 `{"user_id", "email"}` | 401 `unauthorized`（密钥不对 / 令牌无效、过期、已吊销 / 用户已注销）；404（没配密钥）；422 `invalid_request`（缺访问令牌） |

令牌无效、过期、已吊销，以及「令牌有效但用户已被注销」，这几件事在这个接口里**回同一个 401**：
调用方该做的事都是「让用户重新登录」，区分开只会多透露一点信息。

**二、积分**（媒体控制面做「服务端处理」时用）

| 方法 | 路径 | 请求体 | 成功 | 典型错误 |
| --- | --- | --- | --- | --- |
| POST | `/internal/credits/balance` | `{"user_id"}` | 200 `{"balance"}` | 401 / 404 |
| POST | `/internal/credits/cost` | `{"subject", "units"}` | 200 `{"subject", "units", "required"}` | 422 `invalid_request`（不认识的 subject / units ≤ 0）；503 `price_missing` |
| POST | `/internal/credits/hold` | `{"user_id", "amount", "ref", "memo"?}` | 200 `{"hold_key", "balance"}` | 402 `insufficient_credits`（与对外接口同一个错误码）；401 / 404 |
| POST | `/internal/credits/settle` | `{"ref", "memo"?}` | 200 `{"settled": true}` | 404 `hold_not_found`；409 `hold_closed` |
| POST | `/internal/credits/release` | `{"ref", "memo"?}` | 200 `{"released": true}` | 404 `hold_not_found`；409 `hold_closed` |

几条要点：

- **算价只由服务端决定**：`/credits/cost` 收的是 `subject` 与 `units`，调用方（哪怕是媒体控制面）
  传不了价格，只能问「这些单位要多少积分」。
- **`hold` 的幂等键由 `ref` 派生**（`hold_key_for(conn, ref)` 查的是 `hold:<ref>`）。
  同一个 `ref` 重复调用不会重复扣：媒体控制面在建任务这一步会重试，必须能安全重放。
- **`settle` / `release` 用 `ref` 找回预扣**，找不到就**如实回 404 `hold_not_found`**，
  而不是假装结算了（例如任务在扣费之前就失败了）。重复结算 / 重复退还都是幂等的（直接返回）；
  方向反了（已退还又要结算）则 409 `hold_closed`。
- **`hold` 在调用方已开的事务里执行**（`with conn.transaction():`）：余额检查与写入之间不能有人插进来。
- 审计记录的操作人是 `media-server`，所以事后能从 `audit_events.actor` 分清「这笔是媒体控制面扣的」。

#### 与媒体控制面的约定

媒体控制面那边对应的两个模块是 `identity.py`（内省）与 `credit_client.py`（积分）。它们在调用侧要分清三件事：

| 结果 | 媒体控制面怎么做 |
| --- | --- |
| 有效（200） | 继续；`user_id` 用作任务归属与扣费对象 |
| 明确无效（401 / 404） | 内省返回 `None` → 给客户端 401；404 另外记一条 error 日志（那是部署问题：没配 `INTERNAL_SECRET`） |
| **问不到**（网络错、超时、5xx、报文不符） | 抛 `IntrospectUnavailable` → 给客户端 503 |

第三种必须单独区分：把「问不到」当成「令牌无效」会让人以为要重新登录；把「问不到」当成「有效」等于没鉴权。
**两者都不能做。** 内省超时是 5 秒，扣费超时是 8 秒——都是同步动作，宁可让用户重试也不要挂很久。

扣费侧同理：预扣阶段问不到 commerce 就**直接让请求失败**（503），绝不放行「先跑着、回头再扣」，
那会变成免费算力。结算 / 退还阶段问不到则返回 `CreditOutcome(ok=False)` 而不抛异常——任务已经跑完了，
这时候把失败升级成「任务失败」会误导用户（见 3.7 的「扣费时机」）。

### 3.10 管理后台

实现在 `admins.py`（账号与会话）、`admin_routes.py`（认证）、`admin_views.py`（业务接口）、
`admin_page.py` + `web/admin.html`（页面）。**操作手册见 [`ADMIN_GUIDE.md`](ADMIN_GUIDE.md)**，
这一节只讲接口与鉴权。

#### 页面与接口分开

| 东西 | 路径 | 返回 |
| --- | --- | --- |
| 页面 | `GET /admin` | 静态 HTML（`web/admin.html`），不进 OpenAPI |
| 接口 | `/api/v1/admin/*` | JSON |

页面缺失时返回 500 并给一句明确提示（「安装包不完整：没有找到 `commerce/web/admin.html`」），
而不是一个堆栈——「装完包没带页面」是容易排查的部署问题（`admin_page.py:27-38`）。

#### 认证

| 方法 | 路径 | 认证 | 说明 |
| --- | --- | --- | --- |
| POST | `/api/v1/admin/login` | **无**（靠邮箱密码） | 成功返回 `{token, expires_in, admin: {id, email}}` |
| POST | `/api/v1/admin/logout` | 管理员令牌 | 204，吊销当前会话 |
| GET | `/api/v1/admin/me` | 管理员令牌 | `{"admin": {"id", "email"}}`，界面用它判断令牌还有没有效 |

- 请求体是 `{email, password}`（邮箱 3–254 字符，密码 1–128 字符），`extra="forbid"`。
- 失败信息统一为「邮箱或密码不正确」（401）。邮箱不存在、密码错、账号被停用**回同一个错误**，
  且邮箱不存在时也做一次同等开销的哈希校验（`admins.py:111-121`）。
- 登录限流两个桶：按来源 IP **20 次 / 10 分钟**，按邮箱 **8 次 / 10 分钟**
  （`admin_routes.py:33-34`）。比用户登录更紧——管理员账号能动钱与价目。
- 令牌前缀 **`adm_`**（`admins.py:26`），只认 `Authorization: Bearer adm_...`；
  前缀不对时不必查库直接拒（`admins.py:42-48`）。
- 会话有效期 **8 小时**（`admins.py:28`）。令牌只存 SHA-256 摘要（`security.token_digest`）。

#### 与用户令牌**互不通用**

管理员与用户是两条完全独立的授权路径：

- 管理员是**独立的表** `admins`，不是给 `users` 加 `is_admin` 字段
  （`migrations/0011_admins.sql` 的注释：管理员是运营人员，不一定是付费用户，两种身份生命周期也不同）；
- 会话与令牌也各自成表（`admin_sessions` / `admin_tokens`）；
- 两种令牌前缀不同（`adm_` vs `at_`），**交叉使用一律拒绝**：
  - 用户令牌调 `/api/v1/admin/*` → 401（`admin_routes.py:45-61`，`require_admin` 只认管理员令牌）；
  - 管理员令牌调用户接口 → 同样过不去（`admins.py` 里没有管理员令牌换用户身份的路）。

理由写在 `admin_routes.py` 的注释里：混用等于让「任何付费用户」变成「管理员」。

#### 业务接口清单

**只读**（都是 `GET`，都要管理员令牌，都分页，`limit` 上限 `MAX_PAGE = 200`）：

| 路径 | 返回的主要字段 |
| --- | --- |
| `/api/v1/admin/users` | `total`、`items[]`：`id`、`email`、`createdAt`、`balance`、`entitlement`、`expiresAt`、`deviceCount`；支持 `keyword` 模糊筛邮箱 |
| `/api/v1/admin/card-batches` | `items[]`：`id`、`name`、`kind`、`days`、`createdBy`、`createdAt`、`revoked`、`total`、`redeemed`、`remaining`。**不返回卡密明文** |
| `/api/v1/admin/orders` | `items[]`：`outTradeNo`、`channel`、`amountFen`、`credits`、`status`、`tierId`、`email`、`createdAt`、`paidAt`、`expiresAt`；`status` 只接受 `created` / `paid` / `closed` / `refunded`，别的值 422 |
| `/api/v1/admin/ledger` | `items[]`：`key`、`delta`、`kind`、`ref`、`settledState`、`memo`、`email`、`createdAt`；支持 `email` 精确筛 |
| `/api/v1/admin/devices` | `items[]`：`deviceIdPrefix`（**机器码前 12 位**）、`name`、`platform`、`appVersion`、`email`、`boundAt`、`lastSeenAt`、`unboundAt`、`unbindReason`；支持 `email` 精确筛 |
| `/api/v1/admin/client-errors` | `items[]`：`code`、`appVersion`、`platform`、`location`、`occurrences`、`firstSeen`、`lastSeen`；按 `last_seen` 倒序，`limit` 夹在 1–200 |

**可写**：

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET / POST | `/api/v1/admin/announcements` | 列表 / 新建（201 返回 `{id}`） |
| PATCH / DELETE | `/api/v1/admin/announcements/{id}` | **整行**更新 / 删除（404 表示找不到） |
| GET | `/api/v1/admin/flags` | 四个开关的 `key` / `enabled` / `note` / `updatedAt` |
| PUT | `/api/v1/admin/flags/{key}` | 请求体 `{enabled, note}`；不认识的 key → 404 `flag_not_found` |
| GET / PUT | `/api/v1/admin/version-policy` | 单行版本策略的读 / 整行写 |

请求体的字段与校验：

- 公告：`title`（1–120）、`body`（1–2000）、`level`（`info` / `warn`，默认 `info`）、
  `sort_order`（0–10000，默认 100）、`active`（默认 `true`）。**写接口不收 `starts_at` / `ends_at`**，
  表上这两列只有直接改库才能填。
- 开关：`enabled`（布尔）、`note`（≤ 200 字符）。key 是表上的 `CHECK` 白名单字面量
  （`signup` / `recharge` / `server_processing` / `version_task`），后台造不出第五个。
- 版本策略：`min_supported` / `latest`（1–32 字符，形如 `1.2.3`，只校验形状）、`download_url`（≤ 500）、
  `download_sha256`（空串或 64 位小写十六进制）、`download_sig`（≤ 200）、`notes`（≤ 2000）。

**所有写操作都记审计**：`admin.announcement.create` / `.update` / `.delete`、`admin.flag.set`、
`admin.version_policy.set`，以及 `admin.create` / `admin.disable` / `admin.login` / `admin.logout`。
`admin.create` 的明细里只记邮箱**域名**（`admins.py:178-181`）——管理员邮箱是个人信息，不该进日志明细。

#### 管理员的创建与停用只走 CLI

```bash
drama-commerce admin create --email ops@example.com     # 密码从标准输入读，至少 12 个字符
drama-commerce admin list
drama-commerce admin disable --email ops@example.com
```

**没有注册入口**：网页上开放注册的管理后台，注册入口本身就是攻击面（`admins.py:7-8`）。
密码从标准输入读而不走命令行参数——命令行参数会进 shell 历史与进程列表（`cli.py` 的 `_read_password`）。

停用是**不删除**：审计日志会引用管理员 id，删掉就断了线索。停用的同时立刻把该管理员的全部会话标成
`revoked_at`、`revoke_reason = 'admin_disabled'`（`admins.py:100-105`），不等 8 小时自然过期。

#### 已知限制

- **没有 MFA**，没有细粒度角色（所有管理员权限相同），没有改密码命令。
- **没有 CSRF token**：令牌走 `Authorization` 头而不是 Cookie。
- 令牌存在浏览器 `sessionStorage`（不是 `localStorage`）：关掉标签页就失效。
- 审计日志只在 `audit_log` 表里，后台没有查看页面。

### 3.11 客户端配置与错误上报

桌面端启动时要读两份东西：一份是「要不要强制更新、有没有公告、哪些功能开着」，
一份是「用户遇到的错误长什么样」。前者是配置，后者是遥测。两者都是**公开接口**，
都限流，都不含任何用户数据。

#### `GET /api/v1/client/config`（公开）

**为什么公开**：桌面端启动时就要知道「要不要强制更新」，那时可能还没登录
（`client_config.py` 的模块注释）。所以这个接口里**绝不能有用户数据**——只有公告正文、
开关布尔值、版本号与下载地址。`test_client_config.py` 的 `test_the_config_never_contains_user_data` 钉着这条。

返回：

```json
{
  "announcements": [{"id": "...", "title": "...", "body": "...", "level": "info"}],
  "flags": {"signup": true, "recharge": true, "server_processing": true, "version_task": true},
  "version": {
    "minSupported": "0.0.0", "latest": "0.0.0",
    "downloadUrl": "", "downloadSha256": "", "downloadSig": "", "notes": ""
  }
}
```

- 公告只返回 `active = true` 且在生效窗口内（`starts_at` 为空或已到、`ends_at` 为空或未到）的，
  按 `sort_order, created_at DESC`，**最多 20 条**（`cloud/src/drama_cloud/commerce/client_config.py:33-43`）。
- 版本策略是单行表；查不到时回 `0.0.0` / 空串的默认值（`client_config.py:59-64`）。
- 限流：**每来源 IP 每小时 600 次**（`client_config.py:25`）。桌面端每次启动拉一次，正常不会多。

**拉不到时桌面端怎么降级**：`src/logic.js` 的 `DEFAULT_CLIENT_CONFIG`（`logic.js:640-656`）——
空公告、开关全开、不强制更新。三条都朝「不把人挡在门外」的方向降级：
服务端挂了不该让所有客户端变砖。`normalizeClientConfig` 逐键取值，服务端将来少发一个键时，
那个键仍然默认「开」（`logic.js:659-689`）。

> **开关只管界面入口。** 桌面端按开关收起入口（`src/main.js` 的 `applyFeatureFlags`），
> 但界面藏起来不等于服务端放行——两处都要挡。

#### `POST /api/v1/client/errors`（公开）

**为什么公开**：出错时用户可能还没登录，而「启动就崩」这类问题恰恰最需要上报。

| 方法 | 路径 | 认证 | 请求体 | 成功 |
| --- | --- | --- | --- | --- |
| POST | `/api/v1/client/errors` | **无** | `{code, app_version, platform?, location?}` | 202 `{"accepted": true}` |

白名单**四个字段，多一个都不收**（`client_errors.py:41`）：

| 字段 | 约束 | 说明 |
| --- | --- | --- |
| `code` | 1–64 字符 | 错误码，例如 `insufficient_credits` / `io` / `network` |
| `app_version` | 1–32 字符 | 应用版本 |
| `platform` | ≤ 16 字符，默认 `unknown` | 只接受 `windows` / `macos` / `linux` / `unknown`，别的值一律记为 `unknown`（不拒绝整条上报） |
| `location` | ≤ 64 字符，默认空 | 出错位置（**模块名**，例如 `upload`）。**带斜杠或反斜杠的一律清空** |

- 请求模型是 `StrictModel`（`extra="forbid"`），所以多传 `email` / `path` / `token` / `message`
  会直接 422。**故意不收任何自由文本**：错误消息本身可能带文件路径或剧名，收它就等于把
  「不带路径」这条要求作废。
- **计数而不是逐条存**：`UNIQUE (code, app_version, platform, location)`，冲突时
  `occurrences + 1` 并更新 `last_seen`（`client_errors.py:75-83`）。同一个错误一天可能上报成百上千次，
  逐条存会把表撑爆。
- 限流：**每来源 IP 每小时 300 次**（`client_errors.py:38`）。
- 日志里也不记 payload：`log.info("客户端错误上报：code=%s platform=%s", ...)`（`client_errors.py:85`）。
- 表上根本没有能放路径或邮箱的列（`test_client_errors.py` 的 `test_the_database_has_no_column_for_sensitive_data`）。

**默认关闭**：桌面端不上报，除非用户在设置里主动打开「出错时上报诊断信息」
（`crates/dossier-core/src/settings.rs` 的 `report_errors`，`#[serde(default)]` 默认 `false`）。
前端在上报前先看这个开关（`src/main.js` 的 `reportDiagnostic`：`if (!state.settings?.reportErrors) return;`）。

**客户端也做一层白名单**：`src-tauri/src/account/commands.rs` 的 `sanitize_short` 只保留字母数字、
下划线与短横线并截断到 64 字符，所以即使前端传了带路径或空格的字符串也发不出去。

#### 后台查看

`GET /api/v1/admin/client-errors`（要管理员令牌，见 3.10）。它挂在 `/api/v1/admin` 下而不是
`/api/v1/client` 下——查看是管理动作，不是客户端动作。

## 4. 安全规则

- **密码**：argon2id，参数用 argon2-cffi 的默认值（RFC 9106 低内存档：t=3、m=64 MiB、p=4）。
  以后调高参数，老用户下次登录时自动升级。数据库里的哈希损坏时按登录失败处理。
  同时进行的哈希数有上限（默认 4 个）：一大波并发登录只会排队，不会把内存撑爆；
  排队太久、拿不到数据库连接的请求返回 503。
- **验证码**：6 位数字。60 秒内不能重发，10 分钟过期，最多试 5 次，用一次就作废，重发后旧码失效。
  数据库只存 HMAC，算的时候带上用途和邮箱。邮件没发出去就删掉这条验证码，不占冷却时间。
- **令牌**：访问令牌 15 分钟；刷新令牌 30 天，只能用一次，每次刷新换一对新令牌，同时顺延会话有效期。
  - 旧刷新令牌被再次使用，就吊销整个会话（不论是被偷了还是客户端并发刷新）。
    **桌面端必须保证同一时刻只发一个刷新请求**，否则会把自己踢下线。
  - 数据库只存令牌的 SHA-256 摘要。
  - 令牌只认 `Authorization: Bearer` 请求头，不接受网址参数：网址会被写进访问日志和浏览器历史。
    媒体控制面的用户令牌同样已经去掉了 `?token=`。
- **吊销**：退出吊销当前会话；重设密码吊销全部会话；修改密码保留当前会话、吊销其他会话。
- **限流**：计数存在 PostgreSQL 里，多个服务实例共用。被拒绝的请求也算一次。

  | 接口 | 规则 |
  | --- | --- |
  | 发验证码 | 每 IP 每小时 20 次；每邮箱每小时 10 次（另有 60 秒冷却） |
  | 注册 | 每 IP 每小时 20 次 |
  | 登录 | 每 IP 每 10 分钟 30 次；每邮箱每 10 分钟 10 次 |
  | 重设密码 | 每 IP 每小时 20 次 |
  | 修改密码 | 每账号每小时 10 次 |
  | 注销账号 | 每账号每小时 5 次（要校验密码，限住次数，偷来的访问令牌也没法靠它猜密码） |
  | 导出个人数据 | 每账号每小时 10 次 |
  | 兑换卡密 | 每 IP 每小时 60 次；每账号每小时 20 次 |
  | 绑定电脑 | 每账号每小时 20 次 |
  | 刷新凭证 | 每账号每小时 120 次 |
  | 设备列表、解绑 | 合计每账号每小时 60 次（解绑另有 30 天 3 次的上限） |
  | 生成报审包 | 每账号每小时 30 次 |
  | 签发处理配方 | 每账号每小时 60 次 |
  | 消费处理配方 | 每账号每小时 120 次 |
  | 查余额、查邀请码与奖励 | 合计每账号每小时 240 次（两个接口共用一个计数桶） |
  | 查积分流水 | 每账号每小时 120 次 |
  | 下单、主动查单 | 合计每账号每小时 30 次；`GET /payments/tiers` 不限流 |
  | 支付渠道回调 | 每来源 IP 每小时 300 次（渠道会重投，但不会一秒几十次） |
  | **管理员登录** | **每来源 IP 每 10 分钟 20 次；每邮箱每 10 分钟 8 次**（M7，比用户登录更紧） |
  | **客户端配置**（公开） | **每来源 IP 每小时 600 次** |
  | **客户端错误上报**（公开） | **每来源 IP 每小时 300 次** |

  限流键是 IP、邮箱的 HMAC，数据库里不出现明文。卡密有 80 位随机数，靠猜是猜不中的，
  兑换的限流不是为了防猜，而是挡住脚本刷接口。
- **账号是否存在**：注册时邮箱被占用会明确提示（409），重设密码时邮箱没注册也会明确提示（404）。
  注册接口本来就会暴露这一点，在别处隐藏没有意义，提示清楚对用户更友好；靠上面的限流挡批量探测。
  登录失败不区分邮箱不存在还是密码错误，邮箱不存在时也做一次同等开销的哈希校验。
- **卡密**：16 位，字符集去掉了容易看错的 0、1、I、O，每张 80 位随机数。数据库只存
  `SHA-256("drama-dossier/card/v1\n" + 卡密)`，明文只出现在生成时导出的 CSV 里：数据库泄露了也拿不到能兑换的卡密。
  这个摘要算法一改，库里所有没兑换的卡全部失效，所以用测试钉死了。
- **授权签名私钥**：只放在服务器上（权限 600，权限过宽时启动日志会警告），不进仓库，也不进环境变量。
  泄露了，任何人都能给自己签永久授权；丢了，要换新密钥并发布内置新公钥的桌面端（见 5.1）。
  M7 起支持**主私钥 + 次私钥**两把（`DRAMA_COMMERCE_LICENSE_KEY_FILE_SECONDARY`），
  用于密钥轮换的过渡期；操作流程见 [`../../docs/KEY_ROTATION.md`](../../docs/KEY_ROTATION.md)。
- **签名用途隔离**：同一把私钥签四种东西，签名对象前面各加一个固定前缀
  （`drama-dossier/license/v1\n`、`drama-dossier/package-manifest/v1\n`、`drama-dossier/recipe/v1\n`、
  `drama-dossier/release/v1\n`），
  所以授权凭证的签名挪去冒充报审包清单、处理配方或升级包都验不过，反过来也一样。
  `release` 那一项是 M7 新增的：升级包的 detached 签名必须由桌面端已信任的私钥签出，
  版本策略里 `download_sig` 留空时客户端会拒绝安装（见 3.10 与 [`DEPLOYMENT.md`](DEPLOYMENT.md) 第 8 节）。
- **报审包只存元数据**：`packages` 表只有 ID、账号、机器码、生成时间、签章方式、集数与
  `manifest.json` 原始字节的 SHA-256；**服务端不保存任何文档内容**（PDF、文本、封面、简介、视频都不存），
  生成完就直接发回桌面端。清单和签名也由桌面端在验签后写进本地包里。
- **审计日志不记原文**：生成报审包只记一条 `package.create`，内容是**剧名的 SHA-256 前 16 位**、
  集数和签章方式；不记剧名、公司名、联系方式原文。签发 / 消费配方记 `recipe.issue`（平台、强度）和
  `recipe.consume`；任务哈希和机器码都不进审计日志的明细。
  管理员相关动作记 `admin.create` / `admin.disable` / `admin.login` / `admin.logout` /
  `admin.announcement.*` / `admin.flag.set` / `admin.version_policy.set`；
  `admin.create` 的明细里**只记邮箱域名**，不记管理员邮箱（`admins.py:178-181`）。
- **扫描件**：只在请求体里出现，`scan_invalid` 时立即返回；上限 5 MB、像素上限 4000 万（按文件头判断
  类型，不信扩展名），防止「解压炸弹」。不是通用文件上传接口，路径必须落在这次要盖章的文档上。
- **机器码**：桌面端只上传摘要，接口也只收 64 位十六进制，服务端拿不到硬件原始信息。
  机器码是桌面端自己算的，改过的客户端可以冒用别的机器码，服务端分辨不出来：
  名额限制挡的是随手共享账号，挡不住有意破解；解绑次数上限和审计日志用来限制、发现「轮流换电脑」。
- **审计日志**：注册、登录、退出、重设密码、修改密码、注销、刷新令牌重放，以及生成和作废卡密批次、兑换卡密、
  绑定和解绑电脑、生成报审包、签发和消费配方、积分与支付、邀请都会记录。只记 ID、计数、卡的类型和天数、电脑平台、
  使用权的前后变化，不记邮箱、令牌、验证码、卡密、授权凭证和 IP。
  卡密批次的操作人记 `--by`，没给就记当前系统用户名。
- **积分与支付**：账本只增不改，余额由 `SUM(delta)` 算，没有可被算错的第二份数据。
  每一次加减都留审计动作（`credit.grant` / `credit.hold` / `credit.settle` / `credit.release` / `credit.expire`，
  明细只有金额、`ref` 和笔数）。入账只有 `payments.service.handle_event()` 一条路，渠道实现不许自己改账本。
  回调先验签再取数据，验签失败一律不入账；订单锁 + 账本幂等键两层去重；回调金额与订单不符一律拒绝并记审计
  （`payment.unknown_order` / `payment.amount_mismatch`，写在独立事务里，不会被随后的异常回滚掉）。
  签名密钥与渠道凭证只从环境变量读，不进仓库也不进日志；Mock 缺签名密钥时报「未配置」而不是用空密钥签名。
- **积分流水不记内容**：`ref` 只放订单号、配方编号或一次性的开销号，不放剧名——
  报审包的预扣用 `package:<uuid>` 而不是剧名，否则等于绕过了「审计日志只存剧名哈希」那条规矩。
- **邀请防刷**：一个账号只能被邀请一次、同一台电脑最多给一个账号当邀请来源、被邀请人真的付过钱才发、
  每个邀请人的奖励次数有上限。`grant_on_first_payment()` 自己再查一次「有没有已支付订单」，
  不信任调用方；发奖走账本幂等键，重复触发不会重发。
- 所有时间判断都用数据库的 `now()`，不依赖应用服务器的时钟；授权凭证的签发和到期时间也取数据库时间。

## 5. 运维

- `drama-commerce migrate`：按编号执行 `src/drama_cloud/commerce/migrations/` 下的 SQL，每个迁移一个事务，
  用咨询锁防止两个进程同时迁移。已执行的迁移被改动、或数据库里有程序不认识的版本，都会报错停止。
  新改动写成新的编号文件。
- 服务启动时检查迁移、授权私钥和**中文字体**：数据库没迁移到最新、私钥读不到或不是 Ed25519 私钥、
  字体找不到，都拒绝启动。
- 部署机要装**中文字体**（报审包的两份 PDF 要嵌入简体中文）：Debian / Ubuntu 上装 `fonts-noto-cjk`
  （`apt install fonts-noto-cjk`），或把 `DRAMA_COMMERCE_PDF_FONT` 指向一个 `.otf` / `.ttf` / `.ttc`
  文件。找不到时日志报「找不到报审包 PDF 要用的中文字体」并**拒绝启动**——不生成一份方块字的 PDF。
  `.ttc` 合集里会取简体中文那一款（Noto Sans CJK SC / Noto Sans SC / Source Han Sans SC / CN），
  拆成单独的字体文件交给 fpdf2，进程退出时删掉。
- 报审包还依赖 Rust 侧的 `dossier-artifacts`：用 `DRAMA_COMMERCE_ARTIFACTS_BIN` 指定，
  或让它在 `PATH` / 仓库的 `target/release`、`target/debug` 里能被找到。
  它缺失时服务能启动，但 `POST /packages` 会返回 503 `service_unavailable`。
- `drama-commerce cleanup`：清理过期的验证码、令牌、会话和限流计数，建议每天跑一次（cron 或 systemd timer）。
  审计日志、卡密、电脑绑定、报审包记录和处理配方记录都不在这里清理：解绑次数要从绑定历史里数。
  积分账本、支付订单和邀请记录同样不清理。会话被清掉后，绑定上记的会话变成空，绑定本身不受影响。
- 悬挂的预扣（`reserved` 超过 2 小时）靠两种途径回收：查余额时顺带（每账号）、`credits expire` 扫全库（见 5.3）。
  想让它无人值守地兜底，把 `credits expire` 也排进 cron。
- 对外部署：必须在反向代理上终止 HTTPS，并把 `DRAMA_COMMERCE_TRUSTED_PROXIES` 设成实际的代理层数，
  否则限流拿到的都是代理的地址。SMTP 用 `starttls` 或 `ssl`。
  commerce 自己**不支持** HTTPS：`serve` 只接受 `--host` / `--port` / `--log-level`，没有任何 TLS 参数。
  整套服务的部署编排（Caddy 终结 TLS + 反向代理路由表）见 [`DEPLOYMENT.md`](DEPLOYMENT.md)。
- `/docs`、`/openapi.json` 是 FastAPI 自带的接口文档，对外部署时可以在反向代理上屏蔽。
- **管理员账号只能用 CLI 创建**（`admin create` / `admin disable` / `admin list`），没有注册入口，
  也没有改密码命令。停用即刻吊销该管理员的全部会话。日常操作见 [`ADMIN_GUIDE.md`](ADMIN_GUIDE.md)。
- **管理后台的页面与接口分开挂**：页面 `GET /admin`（静态 HTML），接口 `/api/v1/admin/*`（JSON）。
  页面缺失时返回 500 并给一句可排查的提示，而不是一个堆栈。
- **备份不要包含私钥与 `.env`**：`cloud/scripts/backup.sh` 只备两个库与媒体数据目录，
  刻意不备签名私钥、`.env` 与证书——一份泄露就全泄露。恢复流程与恢复后要核对的四件事见
  [`DEPLOYMENT.md`](DEPLOYMENT.md) 第 7 节。
- **`cleanup` 不清理管理后台的数据**：`audit_log`、`announcements`、`feature_flags`、
  `version_policy`、`client_errors`、`admins` 都不在清理范围内。`client_errors` 会一直累积
  （同一组字段只留一行并累加计数，所以增长是有限的），需要时直接删旧行。

### 5.1 授权签名密钥

```bash
drama-commerce keygen /etc/drama-commerce/license.pem    # 生成私钥（权限 600）；文件已存在就拒绝，绝不覆盖
drama-commerce keyinfo /etc/drama-commerce/license.pem   # 查看已有私钥对应的 kid 和公钥
```

两条命令都打印 `{"kid", "public_key", "path"}`。`public_key` 是 32 字节公钥的 base64url，要内置到桌面端。
私钥文件权限是 600，只有文件属主能读：用运行服务的系统账号执行 `keygen`，或者生成后 `chown` 给它。

验签支持同时信任多把公钥（桌面端 `crates/dossier-license/src/keys.rs` 的 `trusted_keys()` 返回列表，
验签时逐把尝试、返回实际验过签的那把的 `kid`），换密钥才不用让所有人掉线。

M7 起服务端也能同时持有两把私钥（`Signer` 的 `secondary_key`，配置项
`DRAMA_COMMERCE_LICENSE_KEY_FILE_SECONDARY`），`sign_detached(..., with_secondary=True)` 用次私钥签；
**没配次私钥却要求用它签会直接报错**，不会静默退回主私钥。

完整流程（含每一步为什么必须在这个顺序、过渡期多长、怎么回退）写在
[`../../docs/KEY_ROTATION.md`](../../docs/KEY_ROTATION.md)。要点：

1. 新密钥用 `keygen` 生成到新路径，**先不启用**。
2. 发布**同时信任新旧两把公钥**的桌面端，等用户升级（公钥是编译期注入，必须重新发版）。
3. 把 `DRAMA_COMMERCE_LICENSE_KEY_FILE` 指向新私钥、旧私钥挪到 `..._SECONDARY`，重启并核对启动日志里两个 `kid`。
   旧凭证最多再有效 `LICENSE_TTL_HOURS`（默认 72 小时），所以过渡期**最短**是 72 小时，实际建议 30 天。
4. 清空 `..._SECONDARY`、移走旧私钥文件；下一个桌面端版本去掉旧公钥。

私钥泄露时不能走这个平滑流程：要尽快发布只信任新公钥的桌面端，同时换掉服务端私钥。

升级包也由这把钥匙签（用途前缀 `drama-dossier/release/v1\n`）。所以发版时版本策略里的
`download_sig` 必须由**该客户端已信任的**私钥签出，否则客户端会在验签这一步拒绝安装。

#### 本地联调与迁移

`drama-commerce migrate` 必须在 `serve` **之前**跑：`serve` 启动时检查迁移，缺了就直接拒绝启动
（「数据库还没有迁移到最新，缺少 …」）。M7 新增的两个迁移是 `0011_admins.sql`（管理员与会话）
与 `0012_ops_content.sql`（公告、功能开关、版本策略、客户端错误）。

### 5.2 卡密

```bash
drama-commerce cards generate --days 30 --count 500 --out 月卡-第一批.csv --name 月卡-第一批 --by 张三
drama-commerce cards generate --lifetime --count 20 --out 永久卡.csv
drama-commerce cards list [--limit 50]                    # 最近的批次：总数、已兑换数、是否作废
drama-commerce cards status DD-XXXX-XXXX-XXXX-XXXX        # unused / redeemed / revoked / not_found
drama-commerce cards revoke --batch <批次 ID> --yes        # 作废这一批里还没兑换的卡密，不能撤销
```

- 只需要 `DRAMA_COMMERCE_DSN`。结果是 JSON，打印到标准输出（`list` 每行一个批次）；提示打印到标准错误。
- 退出码：0 成功；1 `status` 查不到这张卡，或者卡密已入库但 CSV 没能改成正式文件名（报错里给出临时文件路径）；
  2 参数、配置或数据库错误。
- `generate`：一批 1..10000 张，`--days` 1..3650；`--name` 默认按类型和时间生成，`--by` 默认当前系统用户。
  CSV 是 UTF-8（带 BOM，Excel 能直接打开），权限 600，列为 `code, kind, days, batch_name`。
- **CSV 是卡密明文的唯一副本**：丢了找不回来；泄露了用 `revoke` 作废整批。
  目标文件已存在就拒绝生成；写库失败不会留下明文文件。
- `revoke` 不影响已经兑换的卡，已经加上的使用权照常有效。

### 5.3 积分

```bash
drama-commerce credits balance  --email a@example.com           # 查一个账号的可用积分
drama-commerce credits grant    --email a@example.com --amount 500 --memo 测试积分 --by 张三
drama-commerce credits history  --user-id <UUID> --limit 50     # 最近的流水（新到旧，每行一个 JSON）
drama-commerce credits expire                                   # 回收悬挂超过 2 小时的预扣，打印回收笔数
```

- 四个子命令都只需要 `DRAMA_COMMERCE_DSN`。账号用 `--email` 或 `--user-id` 二选一指定；
  邮箱找不到时报用法错误（退出码 2）。
- `balance` 打印 `{"user_id", "balance"}`。它和接口里的查余额走同一个函数，会先回收这个人的悬挂预扣再求和。
- `grant` 打印 `{"user_id", "balance"}`（加完之后的余额）。没充值渠道时的发测试积分就用它。
  **幂等**：幂等键是 `admin:<操作人>:<用户>:<金额>:<备注>`，所以同一条命令重复执行不会重复加分，
  而不同的人给同一用户加同样金额也不会被误判成重复。`--amount` 必须大于 0。
- `history` 每行打印一条流水，字段与 `GET /credits/history` 的条目一致。
- `expire` 不带账号参数，扫全库回收 `kind = 'hold'` 且 `settled_state = 'reserved'` 且已超过
  `HOLD_TTL_SECONDS` 的记录，打印 `{"released": N}`。进程崩溃、服务重启后留下的悬挂预扣靠它兜底。
- 退出码：0 成功；2 参数、配置或数据库错误。

## 6. 测试

```bash
cd cloud
export COMMERCE_TEST_DSN='postgresql://drama:drama-dev-only@127.0.0.1:5432/postgres'   # 需要建库权限
export COMMERCE_TEST_MAILPIT='http://127.0.0.1:8025'                                   # 可选：真实 SMTP 端到端
.venv/bin/pytest tests/commerce -q
```

- 每次测试会话新建一个临时库并执行迁移，结束后删掉，不碰开发库。
- 没设 `COMMERCE_TEST_DSN` 时，数据库测试自动跳过，只跑单元测试。
- 测试用低开销的 argon2 参数，只为跑得快；生产用默认参数。授权签名私钥每次测试会话现生成，不进仓库。
- 授权凭证的测试向量 `tests/commerce/license_vectors_v1.json` 由 `test_license_token.py` 里的 `build_vectors()`
  确定性生成并提交进仓库，桌面端用同一份文件对拍。平时测试只核对文件和生成器一致。
  只有真的改了格式或用例，才带 `UPDATE_LICENSE_VECTORS=1` 重新生成，并和桌面端的改动一起提交。
  日常回归和最终验证都不要带这个变量：它会先覆盖文件再比较，等于跳过了这项检查。

M7 新增的测试文件：

| 文件 | 验什么 |
| --- | --- |
| `test_admins.py` | 登录返回 `adm_` 前缀、两种令牌互不通用、失败信息不区分原因、密码 < 12 字符被拒、停用后会话立刻失效且不能再登录、退出吊销令牌、审计不记管理员邮箱、登录限流、`/admin` 页面能发出来 |
| `test_admin_views.py` | 七个视图都要管理员令牌（用户令牌 401）、分页上限、设备只给前 12 位、卡密批次不含明文、订单状态白名单、公告增改删、四个开关默认存在、不认识的开关被拒、版本号与 sha256 形状校验、写操作都进审计 |
| `test_client_config.py` | 公开可读、默认不拦人、**不含用户数据**、只返回生效窗口内的公告、按 `sort_order` 排、开关改动反映到配置、版本策略反映到配置、限流 |
| `test_client_errors.py` | 免登录可上报、多传字段 422、表里没有能放敏感数据的列、带斜杠的 `location` 被清空、重复上报只累加计数、平台不认识记 `unknown`、后台查看要管理员令牌 |
| `test_key_rotation.py` | 两把私钥的语义：默认用主钥、`with_secondary=True` 用次钥、两把都信任时都能验、退役后旧钥签名被拒、没配次钥却要求用它签会报错、用途隔离仍成立、坏文件报清楚 |
| `test_signing_purposes.py` | Rust `Purpose::context()` 与 Python `PURPOSE_CONTEXTS` **逐字节**对拍，且两边都没有孤儿用途 |

密钥轮换那组测试各验什么、怎么解读，见 [`../../docs/KEY_ROTATION.md`](../../docs/KEY_ROTATION.md) 第 7 节。

另有一个**端到端脚本**（不进 pytest，真进程 + 真 PostgreSQL + 真 HTTP）：

```bash
.venv/bin/python tests/e2e_m7.py
```

它走这条链路并逐步断言：CLI 建管理员 → 登录 → 改版本策略（强制更新）→ 发公告 → 关一个开关
→ 用户端公开接口拉到的配置随之变化（且不含用户数据）→ 错误上报（含敏感字段被拒）→ 后台能看到
→ 普通用户令牌进不了任何管理接口 → 停用管理员后会话立刻失效。
库名随机、跑完自删，不动既有数据库。
