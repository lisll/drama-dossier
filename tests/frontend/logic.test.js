// src/logic.js 的单元测试：node --test tests/frontend/
// 日期按本地时区算，固定成东八区，结果才不随机器变。
process.env.TZ = "Asia/Shanghai";

const test = require("node:test");
const assert = require("node:assert/strict");
const L = require("../../src/logic.js");

// 2026-01-01 00:00:00 +08:00
const T0 = Date.UTC(2025, 11, 31, 16, 0, 0) / 1000;
const DAY = 86400;

test("卡密：合法写法都能通过，结果是 16 位无分隔符", () => {
  for (const raw of ["DD-2345-6789-ABCD-EFGH", "2345 6789 abcd efgh", "dd2345-6789ABCDefgh", " 23456789ABCDEFGH "]) {
    assert.deepEqual(L.validateCard(raw), { ok: true, code: "23456789ABCDEFGH" }, raw);
  }
  // 16 位、恰好以 DD 开头：不是前缀，原样保留（和服务端一样只在总长 18 位时去前缀）。
  assert.deepEqual(L.validateCard("DD23456789ABCDEF"), { ok: true, code: "DD23456789ABCDEF" });
});

test("卡密：0、1、I、O 当场提示，不发请求", () => {
  const result = L.validateCard("2345-6789-ABCD-EF0I");
  assert.equal(result.ok, false);
  assert.match(result.message, /0、I/);
});

test("卡密：位数不对时报的位数不算 DD 前缀", () => {
  assert.match(L.validateCard("2345-6789-ABCD-EFG").message, /现在是 15 位/);
  assert.match(L.validateCard("DD-2345-6789-ABCD-EFG").message, /现在是 15 位/);
  assert.match(L.validateCard("2345-6789-ABCD-EFGH-J").message, /现在是 17 位/);
});

test("卡密：空、全角、其它字符", () => {
  assert.equal(L.validateCard("").message, "请输入卡密");
  assert.equal(L.validateCard("  - ").message, "请输入卡密");
  assert.match(L.validateCard("２３４５").message, /输入法/);
  assert.match(L.validateCard("2345-6789-ABCD-EF#H").message, /「#」/);
});

test("卡密输入：自动大写、4 位分组，光标跟着字走", () => {
  assert.deepEqual(L.formatCardInput("abcd", 4), { value: "ABCD", caret: 4 });
  assert.deepEqual(L.formatCardInput("abcde", 5), { value: "ABCD-E", caret: 6 });
  // 在中间改字：光标留在原来那个字后面。
  assert.deepEqual(L.formatCardInput("2345678", 2), { value: "2345-678", caret: 2 });
  assert.deepEqual(L.formatCardInput("ABCD-EFGH", 5), { value: "ABCD-EFGH", caret: 4 });
  // 写满 DD 再往后写，保留为 DD- 前缀。
  assert.equal(L.formatCardInput("dd23456789abcdefgh").value, "DD-2345-6789-ABCD-EFGH");
  // 只写了 DD：还看不出是不是前缀，先原样放着。
  assert.equal(L.formatCardInput("dd").value, "DD");
});

test("时长：要等多久向上取整，还能用多久向下取整", () => {
  assert.equal(L.formatWait(0), "1 秒");
  assert.equal(L.formatWait(59), "59 秒");
  assert.equal(L.formatWait(61), "2 分钟");
  assert.equal(L.formatWait(3601), "2 小时");
  assert.equal(L.formatWait(86401), "2 天");
  assert.equal(L.formatRemaining(59), "不到 1 分钟");
  assert.equal(L.formatRemaining(119 * 60), "119 分钟");
  assert.equal(L.formatRemaining(72 * 3600 - 1), "71 小时");
  assert.equal(L.formatRemaining(4 * DAY + 10), "4 天");
  assert.equal(L.formatRemaining(-5), "不到 1 分钟");
});

test("验证码按钮：冷却中禁用并倒计时", () => {
  assert.deepEqual(L.codeButtonLabel(42.2), { text: "43 秒后可重发", disabled: true });
  assert.deepEqual(L.codeButtonLabel(0), { text: "获取验证码", disabled: false });
  assert.deepEqual(L.codeButtonLabel(-3), { text: "获取验证码", disabled: false });
});

test("日期：按本地时区显示，ISO 解析不了就原样返回", () => {
  assert.equal(L.formatDate(T0), "2026-01-01");
  assert.equal(L.formatDateTime(T0 + 3600 + 5 * 60), "2026-01-01 01:05");
  assert.equal(L.formatIso("2025-12-31T16:00:00+00:00"), "2026-01-01 00:00");
  assert.equal(L.formatIso("不是时间"), "不是时间");
  assert.equal(L.formatDate(null), "");
});

test("错误文案：认识的错误码给具体说法，不认识的用后端 message", () => {
  assert.match(L.errorText({ code: "card_used", message: "x" }), /已经被别的账号兑换/);
  assert.match(L.errorText({ error: "already_lifetime" }), /永久授权/);
  assert.equal(L.errorText({ code: "rate_limited", retryAfter: 30 }), "操作太频繁，请 30 秒后再试");
  assert.equal(L.errorText({ code: "rate_limited" }), "操作太频繁，请稍后再试");
  assert.equal(L.errorText({ code: "unbind_limit", retryAfter: 3 * DAY }), "本周期的解绑次数已经用完，3 天后可以再解绑");
  assert.equal(L.errorText({ code: "invalid_request", message: "密码长度要在 8 到 128 个字符之间" }), "密码长度要在 8 到 128 个字符之间");
  assert.equal(L.errorText({ code: "network", message: "连不上服务端（超时）" }), "连不上服务端（超时）");
  assert.equal(L.errorText(new Error("普通错误")), "普通错误");
  assert.equal(L.errorText({}), "操作失败");
});

const online = (extra = {}) => ({
  restoring: false, loggedIn: true, email: "a@example.com", tier: "online", lockReason: null,
  entitlementKind: "lifetime", entitlementUntil: null, renewSoon: false, ...extra,
});
const locked = (lockReason, extra = {}) => online({ tier: "locked", lockReason, ...extra });

test("使用权文案和侧栏徽章", () => {
  assert.equal(L.entitlementText(online(), T0), "永久授权");
  const timed = online({ entitlementKind: "timed", entitlementUntil: T0 + DAY });
  assert.equal(L.entitlementText(timed, T0), "到期 2026-01-02");
  assert.equal(L.entitlementText(timed, T0 + DAY), "已到期");
  assert.equal(L.entitlementText(online({ entitlementKind: "none" }), T0), "未授权");

  assert.deepEqual(L.accountBadge(null, T0), { text: "未登录", tone: "" });
  assert.deepEqual(L.accountBadge(online({ tier: "offline" }), T0), { text: "离线", tone: "warn" });
  assert.deepEqual(L.accountBadge(locked("no_entitlement"), T0), { text: "未授权", tone: "warn" });
  assert.deepEqual(L.accountBadge(locked("entitlement_expired"), T0), { text: "已到期", tone: "err" });
  assert.deepEqual(L.accountBadge(online(), T0), { text: "永久", tone: "ok" });
  assert.deepEqual(L.accountBadge({ ...timed, renewSoon: true }, T0), { text: "到 2026-01-02", tone: "warn" });
});

test("显示哪一屏", () => {
  assert.equal(L.accountView(null), "restoring");
  assert.equal(L.accountView(online({ restoring: true })), "restoring");
  assert.equal(L.accountView({ restoring: false, loggedIn: false, tier: "locked", lockReason: "not_logged_in" }), "auth");
  assert.equal(L.accountView(locked("device_unbound")), "auth");
  // 配置错误、钥匙串不可用：没登录也先说清楚。
  assert.equal(L.accountView({ restoring: false, loggedIn: false, tier: "locked", lockReason: "config_error" }), "lock");
  assert.equal(L.accountView({ restoring: false, loggedIn: false, tier: "locked", lockReason: "storage_unavailable" }), "lock");
  for (const reason of ["grace_expired", "clock_rollback", "license_invalid"]) {
    assert.equal(L.accountView(locked(reason)), "lock", reason);
  }
  // 没有使用权、名额满：主界面 + 遮罩 / 设备管理。
  for (const reason of ["no_entitlement", "entitlement_expired", "needs_activation"]) {
    assert.equal(L.accountView(locked(reason)), "app", reason);
  }
  assert.equal(L.accountView(online()), "app");
  assert.equal(L.accountView(online({ tier: "offline" })), "app");
});

test("锁定页：配置错误没有重试按钮，已登录的给退出按钮", () => {
  const config = L.lockPage({ lockReason: "config_error", message: "公钥不对", loggedIn: false });
  assert.equal(config.action, null);
  assert.equal(config.message, "公钥不对");
  assert.equal(config.logout, false);
  const grace = L.lockPage(locked("grace_expired", { message: "断网太久" }));
  assert.equal(grace.action, "retry");
  assert.equal(grace.logout, true);
  // 不认识的原因：也能重试，不会是空白页。
  assert.equal(L.lockPage({ lockReason: null, message: "读取失败" }).action, "retry");
});

test("受限模块的遮罩：首页和设置不锁", () => {
  assert.equal(L.moduleLock(locked("no_entitlement"), "home"), null);
  assert.equal(L.moduleLock(locked("no_entitlement"), "settings"), null);
  assert.equal(L.moduleLock(online(), "library"), null);
  assert.equal(L.moduleLock(online({ tier: "offline" }), "generate"), null);
  for (const name of L.LICENSED_MODULES) {
    assert.equal(L.moduleLock(locked("no_entitlement"), name).actionLabel, "兑换卡密", name);
  }
  assert.equal(L.moduleLock(locked("entitlement_expired"), "toolbox").actionLabel, "兑换卡密续期");
  assert.equal(L.moduleLock(locked("needs_activation"), "mount").actionLabel, "管理设备");
});

test("顶部横幅：离线剩余时间、快到期（可关闭）、钥匙串写入失败", () => {
  assert.deepEqual(L.accountBanners(null, T0), []);
  const offline = L.accountBanners(online({ tier: "offline", offlineUntil: T0 + 71 * 3600 }), T0);
  assert.equal(offline.length, 1);
  assert.match(offline[0].text, /还能离线使用 71 小时；生成报审包需要联网/);
  assert.equal(offline[0].action, "retry");

  const status = online({ entitlementKind: "timed", entitlementUntil: T0 + 2 * DAY + 10, renewSoon: true });
  const [renew] = L.accountBanners(status, T0);
  assert.match(renew.text, /还剩 3 天/);
  assert.equal(renew.dismissible, true);
  // 关掉以后不再出现；续期后到期时间变了，横幅 id 也变了，会重新出现。
  assert.deepEqual(L.accountBanners(status, T0, new Set([renew.id])), []);
  const renewed = { ...status, entitlementUntil: T0 + 2 * DAY + 20 };
  assert.equal(L.accountBanners(renewed, T0, new Set([renew.id])).length, 1);

  const storage = L.accountBanners(online({ storageWarning: "钥匙串写入失败" }), T0);
  assert.equal(storage[0].id, "storage");
});

test("新手引导：按真实状态打勾，充值不计入总数", () => {
  const empty = L.guideSteps({});
  assert.equal(empty.length, 6);
  assert.deepEqual(L.guideProgress(empty), { done: 0, total: 5 });
  assert.equal(empty[1].disabled, true);

  const steps = L.guideSteps({ entitled: true, libraryRoot: "/素材", ledgerCount: 2, jianyingReady: true });
  assert.deepEqual(L.guideProgress(steps), { done: 5, total: 5 });
  // 只扫描到了剧目、还没记住目录：也算准备好素材。
  assert.equal(L.guideSteps({ dramaCount: 3 })[2].done, true);

  assert.equal(L.guideVisible(0, false), true);
  assert.equal(L.guideVisible(0, true), false);
  // 第一次成功生成报审包之后不再显示。
  assert.equal(L.guideVisible(1, false), false);
});

test("近 7 天：按本地日期分桶，从早到晚，窗口外的不算", () => {
  const now = new Date((T0 + 6 * DAY + 3600) * 1000); // 2026-01-07 01:00
  const records = [
    { ts: "2026-01-07T00:30:00+08:00" },
    { ts: "2026-01-07T23:59:59+08:00" },
    { ts: "2026-01-01T08:00:00+08:00" },
    { ts: "2025-12-31T23:59:59+08:00" }, // 窗口外
    { ts: "坏数据" },
    {},
  ];
  const days = L.last7Days(records, now);
  assert.deepEqual(days.map((day) => day.key), [
    "2026-01-01", "2026-01-02", "2026-01-03", "2026-01-04", "2026-01-05", "2026-01-06", "2026-01-07",
  ]);
  assert.deepEqual(days.map((day) => day.count), [1, 0, 0, 0, 0, 0, 2]);
  assert.equal(days[6].label, "1/7");
});

test("柱状图：纵轴上限取整，全是 0 时也有底线", () => {
  const zero = L.barChart(L.last7Days([], new Date(T0 * 1000)));
  assert.equal(zero.total, 0);
  assert.equal(zero.ceiling, 4);
  assert.ok(zero.bars.every((bar) => bar.height === 0 && bar.y === zero.baseline));

  const days = [3, 0, 7, 0, 0, 0, 12].map((count, i) => ({ key: String(i), label: String(i), count }));
  const chart = L.barChart(days);
  assert.equal(chart.ceiling, 15);
  assert.equal(chart.total, 22);
  const tallest = chart.bars[6];
  assert.ok(tallest.height > chart.bars[2].height && chart.bars[2].height > chart.bars[0].height);
  assert.ok(chart.bars.every((bar) => bar.y + bar.height === chart.baseline));
  // 柱子从左到右排，不重叠、不出画布。
  for (let i = 1; i < chart.bars.length; i += 1) {
    assert.ok(chart.bars[i].x >= chart.bars[i - 1].x + chart.bars[i - 1].width);
  }
  assert.ok(chart.bars[6].x + chart.bars[6].width <= chart.width);
});

test("设备：本机排最前，其余按最近在线；解绑次数文案", () => {
  const sorted = L.sortDevices([
    { id: "a", current: false, last_seen_at: "2026-01-02T00:00:00+00:00" },
    { id: "b", current: true, last_seen_at: "2026-01-01T00:00:00+00:00" },
    { id: "c", current: false, last_seen_at: "2026-01-03T00:00:00+00:00" },
  ]);
  assert.deepEqual(sorted.map((device) => device.id), ["b", "c", "a"]);
  assert.deepEqual(L.sortDevices(undefined), []);
  assert.equal(L.unbindQuotaText({ limit: 3, window_days: 30, remaining: 2 }), "本周期（30 天）还能解绑 2 次，共 3 次");
  assert.equal(L.unbindQuotaText(null), "");
  assert.equal(L.platformLabel("macos"), "macOS");
  assert.equal(L.platformLabel("freebsd"), "freebsd");
});

test("注销确认：邮箱一致（不分大小写）且填了密码", () => {
  assert.equal(L.canConfirmDelete("User@Example.com", " user@example.com ", "pw"), true);
  assert.equal(L.canConfirmDelete("user@example.com", "user@example.com", ""), false);
  assert.equal(L.canConfirmDelete("user@example.com", "other@example.com", "pw"), false);
  assert.equal(L.canConfirmDelete("", "", "pw"), false);
});

test("主题：只认 light，其余一律暗色", () => {
  assert.equal(L.resolveTheme("light"), "light");
  for (const stored of ["dark", null, undefined, "", "LIGHT", "<script>"]) {
    assert.equal(L.resolveTheme(stored), "dark", String(stored));
  }
});

test("报审包错误文案：有固定说法的给说法，带文件名的原样用后端 message", () => {
  // M4a 新加的错误码：每一条都命中对应文案里的关键词。
  const known = {
    entitlement_required: /没有有效的使用权/,
    scan_invalid: /PNG、JPEG、PDF/,
    payload_too_large: /太大/,
    package_manifest_invalid: /清单没有通过校验/,
    package_server_file_mismatch: /和清单对不上/,
    recipe_consumed: /已经用过/,
    recipe_expired: /已过期/,
  };
  for (const [code, pattern] of Object.entries(known)) {
    assert.match(L.errorText({ code }), pattern, code);
    // 后端就算也给了 message，固定文案优先：这是给人看的说法，比后端原文清楚。
    assert.match(L.errorText({ code, message: "后端原文" }), pattern, code);
    assert.match(L.errorText({ error: code }), pattern, code);
  }
  // 这几种后端的说明里带具体文件名，没有固定文案，原样显示 message。
  assert.equal(
    L.errorText({ code: "package_local_changed", message: "第02集.mp4 在提交后被改动或调换过" }),
    "第02集.mp4 在提交后被改动或调换过",
  );
  assert.equal(
    L.errorText({ code: "package_local_missing", message: "找不到本机文件 第02集.mp4，它可能被移走或改名了" }),
    "找不到本机文件 第02集.mp4，它可能被移走或改名了",
  );
  assert.equal(
    L.errorText({ code: "package_already_exists", message: "报审包目录已存在：/输出/报审包" }),
    "报审包目录已存在：/输出/报审包",
  );
  // 前端自己发的包错也走这条：后端只给码、不给 message 时兜底一句，不是空白。
  assert.equal(L.errorText({ code: "package_local_missing" }), "操作失败");
});
