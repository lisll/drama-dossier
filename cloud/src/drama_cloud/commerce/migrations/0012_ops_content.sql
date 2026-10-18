-- 0012 运营内容：公告、功能开关、版本策略、客户端错误上报。
--
-- 四张表放一个迁移：它们都属于「管理后台要改、桌面端要读」的运营数据，
-- 生命周期一致，分开提交只会多三次迁移而没有实质好处。
--
-- 客户端配置接口（/api/v1/client/config）**不需要登录**：桌面端启动时
-- 就要知道「要不要强制更新」，那时可能还没登录。所以这几张表里
-- 绝不能放任何用户数据——只有公告正文、开关布尔值、版本号与下载地址。

CREATE TABLE announcements (
    id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    title      text NOT NULL CHECK (length(title) BETWEEN 1 AND 120),
    body       text NOT NULL CHECK (length(body) BETWEEN 1 AND 2000),
    level      text NOT NULL DEFAULT 'info' CHECK (level IN ('info', 'warn')),
    -- 生效窗口。留空表示不限制；起止都填时要求起点早于终点。
    starts_at  timestamptz,
    ends_at    timestamptz,
    sort_order integer NOT NULL DEFAULT 100,
    active     boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK (starts_at IS NULL OR ends_at IS NULL OR starts_at < ends_at)
);
CREATE INDEX idx_announcements_live ON announcements (active, sort_order);

-- 功能开关。key 是白名单字面量，不是任意字符串：客户端只认它认识的几个，
-- 后台也不该能造出一个客户端看不懂的开关。
CREATE TABLE feature_flags (
    key        text PRIMARY KEY
               CHECK (key IN ('signup', 'recharge', 'server_processing', 'version_task')),
    enabled    boolean NOT NULL DEFAULT true,
    note       text NOT NULL DEFAULT '',
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- 版本策略：单行表（id 固定为 1 并用 CHECK 钉住，防止出现第二行）。
CREATE TABLE version_policy (
    id              smallint PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    -- 低于这个版本就强制更新（拦住后续操作）。
    min_supported   text NOT NULL DEFAULT '0.0.0',
    -- 最新版本；用于「可更新」提示。
    latest          text NOT NULL DEFAULT '0.0.0',
    download_url    text NOT NULL DEFAULT '',
    -- 升级包整文件的 sha256（64 位小写十六进制；空串表示还没上传包）。
    download_sha256 text NOT NULL DEFAULT ''
                   CHECK (download_sha256 = '' OR download_sha256 ~ '^[0-9a-f]{64}$'),
    -- 升级包的 Ed25519 detached signature（base64url）。空串表示未签名，客户端应当拒绝安装。
    download_sig    text NOT NULL DEFAULT '',
    notes           text NOT NULL DEFAULT '',
    updated_at      timestamptz NOT NULL DEFAULT now()
);

-- 客户端错误上报（M7-6）。默认关；开了之后只收白名单字段。
-- 明确**不存**：邮箱、文件路径、剧名、视频内容、令牌。
CREATE TABLE client_errors (
    id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- 错误码（例如 insufficient_credits / io / network）。
    code        text NOT NULL CHECK (length(code) BETWEEN 1 AND 64),
    -- 应用版本、平台、出错位置（模块名）。这三项是白名单里的。
    app_version text NOT NULL CHECK (length(app_version) BETWEEN 1 AND 32),
    platform    text NOT NULL CHECK (platform IN ('windows', 'macos', 'linux', 'unknown')),
    location    text NOT NULL DEFAULT '' CHECK (length(location) <= 64),
    -- 同一天同一份错误可能重复上报很多次；计数而不是每来一条存一行。
    occurrences integer NOT NULL DEFAULT 1,
    first_seen  timestamptz NOT NULL DEFAULT now(),
    last_seen   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (code, app_version, platform, location)
);
CREATE INDEX idx_client_errors_last_seen ON client_errors (last_seen DESC);

-- 种子数据：四个开关默认全开（功能默认可用，不会因为没配就变砖）。
INSERT INTO feature_flags (key, enabled, note) VALUES
    ('signup', true, '注册开关'),
    ('recharge', true, '充值入口'),
    ('server_processing', true, '服务端处理'),
    ('version_task', true, '本机处理版本');

-- 版本策略初始为「不强制、不提示」，避免刚上线就把所有人拦住。
INSERT INTO version_policy (id) VALUES (1) ON CONFLICT DO NOTHING;
