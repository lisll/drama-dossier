# 授权签名密钥的轮换

> 面向运维与发布负责人。讲清楚**为什么**要换密钥、**按什么顺序**换、每步为什么必须在前一步之后，
> 以及出错时怎么退回去。
>
> 相关背景：密钥的作用与安全规则见 [`cloud/docs/COMMERCE.md`](../cloud/docs/COMMERCE.md) 的 4 与 5.1 节；
> 部署步骤见 [`cloud/docs/DEPLOYMENT.md`](../cloud/docs/DEPLOYMENT.md)。

## 1. 这把密钥在做什么

服务端有一把 Ed25519 私钥，桌面端内置对应的公钥。凡是「服务端说、桌面端信」的东西都由它签：

| 签名对象 | 用途前缀 | 桌面端在哪验 |
| --- | --- | --- |
| 授权凭证（能用到什么时候、绑定哪台电脑） | `drama-dossier/license/v1\n` | 登录后与每小时刷新 |
| 报审包完整性清单 `manifest.json` | `drama-dossier/package-manifest/v1\n` | 组装报审包前 |
| 处理配方 | `drama-dossier/recipe/v1\n` | 申请配方后 |
| **升级包本体** | `drama-dossier/release/v1\n` | 下载完升级包、交给安装器之前 |

前缀在服务端是 `license_token.PURPOSE_CONTEXTS`（`cloud/src/drama_cloud/commerce/license_token.py:36-44`），
在桌面端是 `dossier_license::envelope::Purpose::context`（`crates/dossier-license/src/envelope.rs:31-43`），
两边必须逐字节相同（第 7 节有对拍测试）。

前缀的作用是**用途隔离**：用授权凭证的签名去冒充升级包签名验不过，反过来也一样。

**一个容易漏的点：升级包也用这把钥匙。** 换私钥时，版本策略里 `download_sig` 必须用
**客户端已信任的**那把私钥签，否则客户端会在验签这一步拒绝安装（`src-tauri/src/updater.rs:134-139`）。
所以密钥轮换和「发一个新版本」这两件事是**绑在一起**的——见第 4 节第 2 步。

## 2. 为什么要轮换、什么时候轮换

轮换的理由只有两类，处理方式完全不同。

### 2.1 计划内轮换（密钥没泄露，只是该换了）

例如：换了负责发布的人、换了运维团队、公司主体变更、内部规范要求定期轮换。

这种情况可以走**平滑流程**：新旧公钥在客户端里共存一段时间，谁都不会掉线。
第 4 节讲的就是这条路。

### 2.2 私钥泄露（应急）

私钥泄露意味着任何拿到它的人都能给自己签一份永久授权、签一个伪造的升级包。
这时候**不能**走平滑流程：

- 平滑流程的前提是「旧的、只认旧公钥的客户端在此期间仍然可用」，而那正是攻击者能利用的那批客户端；
- 必须尽快发布**只信任新公钥**的桌面端，同时把服务端私钥换掉。

代价是所有旧版客户端在换完之后都会验签失败、被挡在门外，用户必须手动下载安装新版。
这是有意的取舍：宁可让所有人手动更新一次，也不能让伪造的授权继续有效。

应急流程与平滑流程的差别只有一处：**跳过去掉旧公钥的漫长等待**，直接发只认新公钥的版本。
第 4 节的步骤顺序依然有效，只是第 4 步（等过渡期）压缩到「新客户端覆盖率够用」为止。

## 3. 双方各支持什么

### 3.1 桌面端：内置多把公钥，逐把尝试

`crates/dossier-license/src/keys.rs` 的 `trusted_keys()`（`keys.rs:121-128`）返回一个**列表**：

- 来源一：`BUILTIN_KEYS`，源码里内置的正式公钥。现在这个列表是**空的**（`keys.rs:21`）；
- 来源二：编译时环境变量 `DOSSIER_LICENSE_PUBKEYS`（`keys.rs:24` 的 `option_env!`），
  逗号分隔的 base64url，追加在内置列表后面。开发与真机测试用它注入本机 `drama-commerce keyinfo` 打印的 `public_key`。

验签时**逐把尝试**，返回实际验过签的那把公钥的 `kid`（`envelope.rs` 的 `verify_detached`）。
所以「客户端同时信任新旧两把公钥」是天然支持的，不需要改验签逻辑。

两条约束要记住：

- **公钥只能在编译期注入**（`option_env!`），换了公钥必须重新编译、重新发版；
- 列表里出现测试向量那两把公开公钥时，一律按「安装包配置有误」处理（`keys.rs:27-31`、`104-108`），
  不会悄悄放行。

`kid` 的定义两边一致：公钥 SHA-256 的前 8 字节转 16 位小写十六进制
（Python 侧 `license_token.py:88-89`，Rust 侧 `keys.rs:70-75`）。
排查时可以拿 `kid` 对齐「这个凭证是哪把钥匙签的」。

### 3.2 服务端：一把主私钥 + 一把可选的次私钥

`Signer` 支持同时加载两把私钥（`license_token.py:96-151`）：

| 配置项 | 含义 |
| --- | --- |
| `DRAMA_COMMERCE_LICENSE_KEY_FILE` | 主私钥（必填）。默认用它签 |
| `DRAMA_COMMERCE_LICENSE_KEY_FILE_SECONDARY` | 次私钥（可选）。**留空表示不做轮换** |

- 两把私钥各自一个 PEM 文件，都是 Ed25519、不加密，权限建议 600（权限过宽时启动日志会警告，
  `license_token.py:164`）。
- 配了次私钥时，服务启动会打一条 warning，把两把私钥的 `kid` 都写进日志
  （`license_token.py:136-151`）。**这条日志是轮换期间最重要的自检**：两个 `kid` 不同才说明真的加载了两把。
- 签名时用哪把由调用方显式指定：`sign_detached(purpose, payload, with_secondary=True)`
  用次私钥签，不传就用主私钥（`license_token.py:180-190`）。
- **没配次私钥却要求用次私钥签，会直接报错**，不会静默退回主私钥
  （`license_token.py:188`：`ConfigError("没有配置次私钥（DRAMA_COMMERCE_LICENSE_KEY_FILE_SECONDARY）")`）。
  这条是刻意的：静默退回会让人以为「已经用新钥签了」，实际签的还是旧钥。

`drama-commerce keygen` 生成新私钥（`license_token.py:203-219`）：权限 600，**文件已存在就拒绝，绝不覆盖**；
`drama-commerce keyinfo` 打印已有私钥的 `kid` 与 `public_key`。

## 4. 平滑轮换的完整步骤

**顺序不能换。**每一步后面的括号里写了「为什么必须是这个顺序」。

### 第 1 步：生成新私钥，先不启用

```bash
drama-commerce keygen /etc/drama-commerce/license-new.pem     # 文件已存在会被拒绝
drama-commerce keyinfo /etc/drama-commerce/license-new.pem    # 记下打印的 kid 与 public_key
```

同时把**当前在用的**那把也打印一遍，两个 `kid` 写进变更记录：

```bash
drama-commerce keyinfo /etc/drama-commerce/license.pem
```

> **为什么先不启用**：这一步只是往磁盘上多放一个文件，服务端还在用旧私钥签。
> 此时如果把新私钥配成主私钥，所有旧客户端立刻验签失败——它们的公钥列表里还没有新公钥。

### 第 2 步：把新公钥编进客户端，发布

在构建环境里把**新旧两把公钥**都放进受信列表，然后发包：

```bash
export DOSSIER_LICENSE_PUBKEYS='<旧公钥>,<新公钥>'      # 逗号分隔，base64url，不带填充
cargo tauri build
```

如果正式公钥是走 `BUILTIN_KEYS` 内置的（`crates/dossier-license/src/keys.rs:21`），
就是把两把都写进那个常量。

> **为什么必须是这个顺序**：客户端的公钥是**编译期**固定的。只有「同时信任新旧两把」的客户端存在之后，
> 服务端换成新私钥才不会有任何人掉线。先换私钥再发客户端，等于把中间那批用户全部锁在门外。

### 第 3 步：服务端把旧私钥挪到次私钥位置

把新私钥配成主私钥，**旧私钥配成次私钥**：

```bash
# 生产环境（.env → compose 环境变量）
DRAMA_COMMERCE_LICENSE_KEY_FILE=/keys/license-new.pem
DRAMA_COMMERCE_LICENSE_KEY_FILE_SECONDARY=/keys/license-old.pem
```

**用仓库自带的 `cloud/compose.deploy.yaml` 部署时要注意**：主私钥的路径在编排文件里是**写死的**
`/keys/license.pem`（`compose.deploy.yaml:62`），只有次私钥从 `.env` 的 `LICENSE_KEY_SECONDARY` 取
（`compose.deploy.yaml:63`）。所以在这套编排下，轮换要按文件名换而不是改路径：

```bash
# 在 .env 指向的 KEYS_DIR 目录里（宿主机侧，目录以只读方式挂进容器）
mv license.pem license-old.pem      # 旧的改名，留作次私钥
cp license-new.pem license.pem      # 新的顶替主私钥的位置
# .env 里填容器内路径：
LICENSE_KEY_SECONDARY=/keys/license-old.pem
```

改路径就得同时改编排文件，而编排文件是共享的；改文件名只动部署侧的密钥目录，风险更小。

然后重启服务，**确认启动日志里两个 `kid` 都在、且不相同**（`license_token.py:136-151` 那条 warning）。

> **为什么要反过来（新主 / 旧次），而不是直接只用新私钥**：
> 换成新私钥之后，最新版客户端两把都认，没问题；但**还没升级的旧客户端只认旧公钥**。
> 这些用户下一次刷新授权凭证时，如果凭证是新私钥签的，他们就会验签失败、被要求重新登录、然后依然失败。
> 保留旧私钥的意义是让服务端在需要时**还能用旧私钥签**（`with_secondary=True`），
> 给旧客户端留下一段时间的可验证凭证。
>
> 注意这是**服务端的能力**，不是自动行为：具体某条流程用主钥还是次钥由调用方决定
> （`sign_detached` / `sign_envelope` 的 `with_secondary` 参数）。
> 轮换期间要确认所有会发凭证的路径都按预期选了钥匙——目前 `package_service.py:49`（报审包清单）
> 与 `recipes.py:116`（处理配方）用的都是主钥。

### 第 4 步：等过渡期

**最短：一个授权凭证有效期，也就是 `DRAMA_COMMERCE_LICENSE_TTL_HOURS`，默认 72 小时**
（`cloud/src/drama_cloud/commerce/config.py:59`）。

理由很直接：凭证的有效期就是「断网后还能用多久」。72 小时之内，一个断网用户手里的旧凭证
在过期前都不需要重新签发，也就不会撞上「签发方换了钥匙」这件事。

**实际建议：30 天。**

- 72 小时只覆盖「凭证还没过期」，不覆盖「用户多久开一次应用」。短剧制作的使用频率通常是
  一部剧一个周期，中间隔一两周不开是常态——那批人会在 72 小时后才第一次发现要重新验证；
- 30 天也给「发现有问题、回退」留出时间：回退需要旧私钥还在（第 8 节）；
- 这个数字是运维建议值，不是代码里的常量：过渡期的长短写在你的变更记录里，代码不强制。

过渡期内要做的事：

- 盯住客诉里「授权凭证无效 / 签名失败」的数量。持续出现说明有客户端只认旧公钥且旧私钥已经不再参与签发；
- 看客户端的版本分布，判断新版覆盖率。覆盖率不达标就不要进第 5 步。

### 第 5 步：移除旧私钥

覆盖率够了之后：

1. 清空 `DRAMA_COMMERCE_LICENSE_KEY_FILE_SECONDARY`（`.env` 里的 `LICENSE_KEY_SECONDARY`）；
2. 重启服务，确认启动日志里**只剩一个 `kid`**，且等于新私钥的 `kid`；
3. 把旧私钥文件从服务器上移走（归档到离线介质或按公司规范销毁）——留在磁盘上只是多一个泄露面；
4. 下一个桌面端版本把旧公钥从受信列表里去掉。

> **为什么旧公钥要等到「下一个版本」才去掉**：第 5 步之后旧私钥已经不再使用，旧公钥留着也不会放行任何东西，
> 但立刻去掉它需要再发一次版；把这一步并进下一次常规发版最省事。
> 去掉之后系统就真正进入「只信任新钥匙」的状态——这也是 2.2 节应急流程想要的终点。

## 5. 一次轮换要改的东西（检查清单）

| 位置 | 改什么 |
| --- | --- |
| 服务器私钥文件 | 新增 `license-new.pem`（权限 600） |
| `DRAMA_COMMERCE_LICENSE_KEY_FILE` | 从旧路径改为新路径 |
| `DRAMA_COMMERCE_LICENSE_KEY_FILE_SECONDARY` | 过渡期填旧路径，结束后清空 |
| `cloud/.env.example` 里的 `KEYS_DIR` / `LICENSE_KEY_SECONDARY` | 部署侧确认挂载目录里两个文件都在（私钥目录是**只读**挂进容器的，`cloud/compose.deploy.yaml:69`） | 
| 客户端 `DOSSIER_LICENSE_PUBKEYS` 或 `BUILTIN_KEYS` | 过渡期两把，结束后一把 |
| 版本策略里的 `download_sig` | 下一次发版时必须用**客户端已信任的**私钥签（见第 1 节的提醒） |
| 变更记录 | 两个 `kid`、切换时间、计划移除旧私钥的日期 |

## 6. 升级包也要重新签

升级包的验签走的是同一个受信公钥列表（`src-tauri/src/updater.rs:134-139` 调
`verify_detached(Purpose::Release, ...)`）。所以：

- 你把新版本的 `download_url` / `download_sha256` 填进管理后台的版本策略时，
  `download_sig` 必须由**该客户端版本已经信任的**私钥签出；
- 过渡期内，客户端两把都认，用主钥签最省事；
- 一旦某个客户端版本只认新公钥（去掉旧公钥之后），用旧私钥签的包它一律拒绝，报
  「升级包签名无效……这可能不是官方发布的包，请不要安装」。

签名值是 **base64url、不带填充**的 detached 签名，签名对象是**升级包文件的原始字节**加上用途前缀
`drama-dossier/release/v1\n`（`license_token.py:180-190`、`envelope.rs:31-43`）。
版本策略接口只校验它是「≤ 200 字符的字符串」，签得对不对只有客户端验得出来。

**用 `drama-commerce release sign` 生成**，不要手工算：

```bash
drama-commerce release sign ./DramaDossier_1.2.0_x64-setup.exe
```

输出是 JSON，四个值直接对应后台的版本策略字段：

```json
{
  "file": "./DramaDossier_1.2.0_x64-setup.exe",
  "size_bytes": 48271360,
  "sha256": "…64 位小写十六进制…",
  "signature": "…base64url 签名…",
  "key_id": "1d0fb094607cb616"
}
```

`key_id` 是签名用的那把公钥指纹——出问题时能一眼看出是哪把钥匙签的
（轮换期间新旧私钥并存，这个值很有用）。这条命令用当前配置的主私钥签；
如果过渡期需要用次私钥签，见第 5 步的说明。

## 7. 验证：两条测试各验什么

### 7.1 `pytest tests/commerce/test_key_rotation.py`

验的是「两把私钥 / 两把公钥」的语义（只用 Python 侧验签；跨语言的一致性交给下一条）：

| 测试 | 验什么 |
| --- | --- |
| `test_a_signer_reports_both_public_keys` | 两把私钥的公钥不同、`kid` 都非空且不同 |
| `test_the_primary_key_signs_by_default` | 不指定时用主私钥签；主公钥能验、次公钥验不过 |
| `test_the_secondary_key_can_sign_when_asked` | `with_secondary=True` 时用次私钥签，次公钥能验、主公钥验不过 |
| `test_both_keys_verify_when_both_public_keys_are_trusted` | 过渡期的真实形态：客户端同时信任两把，两把签的都能验 |
| `test_a_retired_key_no_longer_verifies` | **轮换的完成标准**：过渡期结束后，只信任新公钥时旧私钥的签名被拒 |
| `test_asking_for_the_secondary_without_one_is_an_error` | 没配次私钥却要求用它签 → 报错，不静默退回主钥 |
| `test_purpose_isolation_still_holds_across_keys` | 两把都信任时，升级包签名仍然不能冒充授权凭证 |
| `test_the_secondary_path_is_loaded_from_a_file` | 真实用法：两个 PEM 文件；不传次私钥时只有主私钥 |
| `test_the_two_keys_are_really_different` | 两把文件真的不同——否则「轮换」什么也没换 |
| `test_a_bad_secondary_file_is_reported_clearly` | 次私钥文件不存在时，报错里带上那个文件名 |

跑法：

```bash
cd cloud
export COMMERCE_TEST_DSN='postgresql://drama:drama-dev-only@127.0.0.1:5432/postgres'
.venv/bin/pytest tests/commerce/test_key_rotation.py -q
```

（这一组大多不碰数据库，但同目录的 `conftest.py` 会按需建库；没设 `COMMERCE_TEST_DSN` 时数据库夹具自动跳过。）

### 7.2 `pytest tests/commerce/test_signing_purposes.py`

验的是「Rust 与 Python 的用途前缀逐字节相同」。它直接读 `crates/dossier-license/src/envelope.rs`
的源码，把 `Purpose::X => b"..."` 抠出来与 Python 的 `PURPOSE_CONTEXTS` 比对：

| 测试 | 验什么 |
| --- | --- |
| `test_every_python_purpose_has_a_rust_counterpart` | Python 的每个用途在对拍表与 Rust 里都有对应项 |
| `test_the_context_bytes_are_identical_on_both_sides` | **逐字节**相同——多一个字符就验不过 |
| `test_every_rust_purpose_is_covered` | Rust 那边也不能有孤儿用途（加了却没人签它，等于永远验不过） |
| `test_contexts_are_prefixed_and_end_with_a_newline` | 每个前缀都以 `drama-dossier/` 开头、以换行结尾，且串里含用途名 |

这条测试的价值在于**症状定位**：前缀不一致的表现只是「客户端说签名无效」，
光看日志根本猜不到是「两边字符串差一个换行」。

## 8. 出错了怎么回退

回退的可行性取决于**旧私钥还在不在**，所以第 5 步里「移走旧私钥」之前一定确认新版覆盖率达标。

### 情况 A：第 3 步之后发现新版客户端有问题（还没清空次私钥）

最好办，旧私钥还在：

1. 把 `DRAMA_COMMERCE_LICENSE_KEY_FILE` 改回旧路径；
2. 清空 `DRAMA_COMMERCE_LICENSE_KEY_FILE_SECONDARY`（或指向已被弃用的那把，保持两项都在也无害）；
3. 重启，确认启动日志里的 `kid` 等于旧私钥的 `kid`；
4. **不要撤下已经发出去的新版客户端**：它同时信任两把公钥，旧私钥签的凭证它照样能验。这正是当初要发双公钥版本的收益。

### 情况 B：第 5 步之后（旧私钥已不再加载）

旧私钥文件如果还留着：

1. 把它放回服务器（权限 600）；
2. `DRAMA_COMMERCE_LICENSE_KEY_FILE` 改回旧路径，`..._SECONDARY` 指向那把新私钥；
3. 重启并核对两个 `kid`；
4. 但这只对**仍然信任旧公钥的客户端**有效。已经装上「只认新公钥」版本的客户端会验不过旧私钥签的凭证——
   也就是说回退是**部分的**，那批用户需要再升级一次。

旧私钥文件已经销毁：

- 回退不可能。唯一出路是继续向前：把新版客户端的覆盖率推到 100%，并用旧版客户端会遇到的
  「授权凭证无效」提示引导用户手动下载新版；
- 这就是为什么第 5 步要求「覆盖率达标之后」才移除旧私钥，也是为什么过渡期建议 30 天而不是 72 小时。

### 情况 C：私钥泄露（应急回退的变体）

见 2.2 节：不走平滑流程，直接发只认新公钥的客户端。此时**没有「回退到旧私钥」这个选项**——
旧私钥已经不可信。要做的是：

1. 立刻生成新私钥并启用；
2. 发布只信任新公钥的桌面端；
3. 用公告（管理后台 → 公告）通知所有用户必须更新；
4. 排查泄露渠道（是不是进了仓库、是不是权限过宽、是不是被备份带走）。

## 9. 不要做的事

- **不要用同一个 PEM 文件同时填主私钥和次私钥**：`keygen` 会拒绝覆盖已有文件，所以不可能拿到两个相同的文件；
  用 `keyinfo` 核对两个 `kid` 不同才是最省事的自检。
- **不要只换私钥不发客户端**：那是一步把所有人挡住的操作。
- **不要在没有次私钥的情况下调用 `with_secondary=True`**：它会直接报错，不会偷偷用主钥签。
- **不要把私钥放进备份包**：`cloud/scripts/backup.sh` 刻意只备数据库与媒体数据，
  不备私钥与 `.env`（脚本头部注释），一份泄露就全泄露。私钥要单独离线保管。
- **不要指望客户端能通过配置改公钥**：`DOSSIER_LICENSE_PUBKEYS` 是编译期注入（`option_env!`），
  运行时改环境变量无效。调试版可以用同名环境变量覆盖 `DOSSIER_API_BASE`，但**公钥不行**。
