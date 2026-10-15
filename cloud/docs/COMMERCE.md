# 账号服务（drama_cloud.commerce）

账号服务负责注册、登录、邮件验证码和令牌。它和媒体控制面（`drama-cloud`）分开部署：处理任务再重，
也不拖慢登录。数据库用 PostgreSQL，因为后续的卡密兑换和支付回调需要可靠的并发事务。

本期（M1）范围：邮件验证码、注册、登录、刷新与吊销令牌、重设密码、修改密码、集中限流、审计日志。

## 1. 本地开发

```bash
cd cloud
docker compose -f compose.dev.yaml up -d --wait     # PostgreSQL 18.6 + Mailpit v1.31.6，只绑定 127.0.0.1

export DRAMA_COMMERCE_DSN='postgresql://drama:drama-dev-only@127.0.0.1:5432/drama_commerce'
export DRAMA_COMMERCE_CODE_SECRET='<至少 32 个字符的随机串>'   # 例如 python3 -c 'import secrets;print(secrets.token_urlsafe(48))'
export DRAMA_COMMERCE_SMTP_HOST=127.0.0.1 DRAMA_COMMERCE_SMTP_PORT=1025 DRAMA_COMMERCE_SMTP_SECURITY=plain

.venv/bin/drama-commerce migrate          # 建表；重复执行没有副作用
.venv/bin/drama-commerce serve --port 8100
```

- 验证码邮件发到 Mailpit，不会发出真实邮件。打开 <http://127.0.0.1:8025> 查看。
- 验证码在数据库里存的是用 `CODE_SECRET` 算的 HMAC。换了这个密钥，之前发出的验证码全部失效。
- 停止：`docker compose -f compose.dev.yaml down`；连数据一起删掉加 `-v`。
- compose 里的账号密码是开发专用的固定值，不能用在任何对外环境。

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

配置缺失或非法时，服务拒绝启动，报错里只出现变量名，不出现变量值。

## 3. 接口

所有接口前缀 `/api/v1`，请求和响应都是 JSON。

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

`tokens` 的结构：

```json
{"access_token": "at_…", "refresh_token": "rt_…", "token_type": "Bearer",
 "expires_in": 900, "refresh_expires_in": 2592000}
```

- 邮箱会去掉首尾空格并转成小写，只接受 ASCII 邮箱。密码 8..128 个字符；登录时不检查长度下限。
- `accept_terms` 必须是 JSON 的 `true`，`"true"`、`1` 都不算。
- 请求体里多出来的字段一律拒绝。

### 3.1 错误

响应体固定为 `{"error": <机器码>, "message": <中文说明>}`。客户端按 `error` 分支，`message` 可以直接展示。
参数校验失败（422）还会带 `fields: [{"field", "message"}]`，不回显用户提交的值。

| HTTP | `error` | 含义 |
| --- | --- | --- |
| 400 | `code_invalid` / `code_expired` / `code_exhausted` | 验证码不对 / 过期 / 输错 5 次已锁定（要重新获取） |
| 400 | `wrong_password` | 修改密码时当前密码不对 |
| 401 | `invalid_credentials` | 邮箱或密码不正确（两种情况不区分） |
| 401 | `unauthorized` | 没带令牌、令牌无效或已过期、会话已吊销 |
| 404 | `email_not_registered` | 重设密码：邮箱没注册 |
| 409 | `email_taken` | 注册：邮箱已被占用 |
| 422 | `invalid_request` | 参数校验失败 |
| 429 | `rate_limited` | 太频繁；`Retry-After` 响应头给出要等的秒数 |
| 503 | `mail_unavailable` / `service_unavailable` | 发信失败（可立即重试） / 数据库不可用 |

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

  限流键是 IP、邮箱的 HMAC，数据库里不出现明文。
- **账号是否存在**：注册时邮箱被占用会明确提示（409），重设密码时邮箱没注册也会明确提示（404）。
  注册接口本来就会暴露这一点，在别处隐藏没有意义，提示清楚对用户更友好；靠上面的限流挡批量探测。
  登录失败不区分邮箱不存在还是密码错误，邮箱不存在时也做一次同等开销的哈希校验。
- **审计日志**：注册、登录、退出、重设密码、修改密码、刷新令牌重放都会记录。只记 ID 和计数，
  不记邮箱、令牌、验证码和 IP。
- 所有时间判断都用数据库的 `now()`，不依赖应用服务器的时钟。

## 5. 运维

- `drama-commerce migrate`：按编号执行 `src/drama_cloud/commerce/migrations/` 下的 SQL，每个迁移一个事务，
  用咨询锁防止两个进程同时迁移。已执行的迁移被改动、或数据库里有程序不认识的版本，都会报错停止。
  新改动写成新的编号文件。
- 服务启动时检查迁移：数据库没迁移到最新就拒绝启动。
- `drama-commerce cleanup`：清理过期的验证码、令牌、会话和限流计数，建议每天跑一次（cron 或 systemd timer）。
  审计日志不在这里清理。
- 对外部署：必须在反向代理上终止 HTTPS，并把 `DRAMA_COMMERCE_TRUSTED_PROXIES` 设成实际的代理层数，
  否则限流拿到的都是代理的地址。SMTP 用 `starttls` 或 `ssl`。
- `/docs`、`/openapi.json` 是 FastAPI 自带的接口文档，对外部署时可以在反向代理上屏蔽。

## 6. 测试

```bash
cd cloud
export COMMERCE_TEST_DSN='postgresql://drama:drama-dev-only@127.0.0.1:5432/postgres'   # 需要建库权限
export COMMERCE_TEST_MAILPIT='http://127.0.0.1:8025'                                   # 可选：真实 SMTP 端到端
.venv/bin/pytest tests/commerce -q
```

- 每次测试会话新建一个临时库并执行迁移，结束后删掉，不碰开发库。
- 没设 `COMMERCE_TEST_DSN` 时，数据库测试自动跳过，只跑单元测试。
- 测试用低开销的 argon2 参数，只为跑得快；生产用默认参数。
