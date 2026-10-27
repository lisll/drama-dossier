# 待办：卡密授权正式上线前的收尾工作

## Context

卡密授权（离线测试版）已合并进 `master`（`7dc718e`），用来给内部测试人员试用。测试版的特征：

- 只接受一张通用测试卡密 `DDK-TEST-CARD-2345-6789`，激活不联网，本机签发 30 天凭证；
- 测试私钥写死在 `src-tauri/src/license_test_mode.rs`，测试卡密写在该文件与 `docs/LICENSING.md`、`README.md`；
- 仓库是公开的，因此**任何人都能看到测试私钥，自己签发测试凭证**。

**正式发版前必须完成本文件的全部事项。** 在此之前打 `app-v*` 标签，CI 会把测试版当成预发布版公开发布，任何人下载后都能用公开的测试卡密一直免费使用，发出去收不回来。

已验证的事实（2026-10-27）：
- master 与远端一致，三系统 CI 全绿，手动触发的打包运行 `40454975665` 三个平台全部成功；
- 测试版安装包内的版本号仍是 `0.2.19`；
- 现有 `PRODUCTION_KEYS` 为空，`LICENSE_SERVER` 为空，此时发布的正式版会锁定，不会放行。

## 一、删除测试版（必做）

按 `docs/LICENSING.md`「删除测试版」一节执行。所有接入点都标了 `TEST-MODE`，用 `git grep -n "TEST-MODE\|license-test-mode\|license_test_mode"` 可以找全，共涉及：

| 位置 | 要删的内容 |
|---|---|
| `src-tauri/src/license_test_mode.rs` | 整个文件 |
| `src-tauri/src/lib.rs` | `#[cfg(feature = "license-test-mode")] mod license_test_mode;` |
| `src-tauri/Cargo.toml` | `[features]` 段（`default` 与 `license-test-mode`）；`[dev-dependencies]` 里的 `test-issuer` **保留**（单元测试要用） |
| `src-tauri/src/license.rs` | 编译期检查、`trusted_keys()` 里的测试公钥分支、`license_activate` 里的测试卡密分支、`test_mode()`、`LicenseStatus.test_mode` 字段 |
| `src/license.js`、`src/license.css` | 测试版标识与相关说明文字 |
| `docs/LICENSING.md` | 「离线测试版」一章与上线检查清单里的测试版条目 |
| `README.md` | 「内部测试版：授权激活」一节与目录里对应条目 |

删除后必须验证：
1. `cargo fmt --all -- --check`；
2. `cargo clippy --workspace --all-targets --no-deps -- -D warnings`（Linux 与 Windows 目标各一次）；
3. `cargo test --workspace --all-features`；
4. `license-server/.venv/bin/pytest -q`；
5. `node --check src/main.js && node --check src/license.js`；
6. 确认 release 构建里没有测试卡密：`cargo build --release` 后用 `strings` 搜 `DDK-TEST-CARD` 与 `TEST_SEED` 不应命中。

## 二、部署授权服务器（必做）

1. 按 `license-server/README.md` 在一台 Ubuntu/Debian VPS 上部署，执行 `bash deploy/setup-firewall.sh`（会先检查 authorized_keys 里确有公钥、用 `sshd -T` 读取实际 SSH 端口）。
2. `license-admin keygen` 生成**正式私钥**，私钥只留在服务器上（权限 0640，root:ddlicense），并离线备份。
3. `license-admin doctor` 每一项都要通过。
4. 配好 `DD_LICENSE_ADMIN_ALLOW_IPS`（管理后台只允许固定 IP）。
5. 配置 Caddy 的 HTTPS 域名；确认 `/v1/activate`、`/v1/refresh`、`/v1/health` 可从公网访问，`/admin` 只有白名单 IP 能进。
6. 用 `license-admin admin create` 建管理员，登录后台生成几张真实卡密做一次冒烟：激活、续签、作废。

## 三、客户端填入正式配置（必做）

| 文件 | 要改的内容 |
|---|---|
| `crates/dossier-cardkey/src/keys.rs` | `PRODUCTION_KEYS` 填入服务器公钥（base64url，无填充）。填错或填成测试向量公钥会编译/运行失败 |
| `src-tauri/src/license.rs` | `LICENSE_SERVER` 填服务器 HTTPS 地址（末尾不要带 `/`） |

填完后确认：`cargo build --release -p drama-dossier` 能过（测试版开关没删时会直接编译失败，这是预期保护）。

## 四、真机验收（必做）

在 Windows 和 macOS 各做一遍：

1. 全新安装首次打开，出现激活页（此时**不应**再有"测试版授权"标识）；
2. 输入后台生成的真实卡密，激活成功进入主界面；
3. 断网后重启，仍可正常使用；
4. 后台作废这台电脑 → 客户端联网（或点激活页「联网重试」）后被锁定；
5. 锁定后重启软件，仍然锁定；
6. 把授权文件拷到另一台电脑，判定无效；
7. 用记事本修改授权文件的 `last_seen`，判定无效；
8. Windows 上确认同一台电脑重启前后机器码一致、两台电脑机器码不同；
9. 转码进行中让授权失效，"停止"按钮仍可用。

## 五、发布流程（必做）

1. 版本号：与 liyanghe 确认由谁改（`Cargo.toml`、`src-tauri/Cargo.toml`、`src-tauri/tauri.conf.json` 三处要一致）；
2. 推送 master，等三系统 CI 全绿；
3. 打 `app-v*` 标签发布，**这是第一次把带授权的版本公开发布**；
4. 发布后从发布页下载安装包，随机抽一台干净电脑，用真实卡密走一遍激活；
5. 在服务器上确认后台能看到这次激活记录，审计表正常。

## 六、上线后要做的运维

| 事项 | 频率 | 做法 |
|---|---|---|
| 清理审计日志 | 每月 | `license-admin prune-audit --days 180`（见 `license-server/README.md`） |
| 备份数据库与私钥 | 每天 | `sqlite3 license.sqlite3 ".backup ..."` 后加密存异地；私钥单独离线备份 |
| 检查限流是否误伤 | 上线首周 | 企业/校园网共用出口 IP 时，若有人反馈"操作过于频繁"，看 `ref:ip` 是否打满 |
| 检查磁盘 | 每月 | 审计表清理后确认数据库体积回落 |

## 七、已知限制（要不要处理，由产品决定）

| 问题 | 现状 | 影响 |
|---|---|---|
| 仓库公开 + MIT 许可证 | 任何人都能自行编译一个去掉检查的版本 | 只对下载官方安装包的用户有效；要收紧只能改私有仓库并换许可证 |
| 授权文件校验码的密钥在程序里 | 熟悉逆向的人可以自己重算 | 挡的是"用记事本改数字"，不是逆向 |
| 长期把系统时间设在过去 + 每次还原旧文件 | 离线验证的固有局限 | 文档已如实写明 |
| 管理后台只按 IP 白名单 | 运维人员换网络就要改白名单 | 可按需改成加一层"管理后台专用域名 + 客户端证书" |
| 沙箱选项 `RestrictAddressFamilies` 不含 `AF_NETLINK` | 当前代码不解析域名，无影响 | 以后若要在服务里解析域名，会静默失败 |

## 八、相关文档

- [docs/LICENSING.md](LICENSING.md)：协议、机器码、锁定规则、能防住/防不住、删除测试版步骤、上线检查清单
- [license-server/README.md](../license-server/README.md)：服务器部署、日常运维
- [README.md](../README.md)：内部测试版的使用方法（给测试人员看的）

## 九、完成标准

只有以下全部成立，才算收尾完成：

- [ ] `git grep -n "TEST-MODE\|license-test-mode\|license_test_mode\|DDK-TEST-CARD"` 在 `src-tauri/`、`src/`、`README.md` 里无命中（`docs/LICENSING.md` 里关于历史的说明可保留，但不得再出现测试卡密明文）；
- [ ] `PRODUCTION_KEYS` 非空且不是测试向量公钥，`LICENSE_SERVER` 非空；
- [ ] release 构建产物里搜不到 `DDK-TEST-CARD`；
- [ ] 真机验收 9 项全部通过；
- [ ] 带授权的正式版已发布，且发布后抽测激活成功；
- [ ] 本文件存档，或改写成"已完成"的记录。
