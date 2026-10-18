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

test("新手引导：按真实状态打勾，充值步骤 M5a 起可用", () => {
  const empty = L.guideSteps({});
  assert.equal(empty.length, 6);
  // M5a 起「充值积分」不再是「即将推出」：有余额就算完成，所以计入总数。
  assert.deepEqual(L.guideProgress(empty), { done: 0, total: 6 });
  assert.equal(empty[1].disabled, undefined);
  assert.equal(empty[1].action, "credits");

  const steps = L.guideSteps({
    entitled: true,
    libraryRoot: "/素材",
    ledgerCount: 2,
    jianyingReady: true,
    creditBalance: 100,
  });
  assert.deepEqual(L.guideProgress(steps), { done: 6, total: 6 });
  // 余额为 0 或未知时，这一步不算完成。
  assert.equal(L.guideSteps({ creditBalance: 0 })[1].done, false);
  assert.equal(L.guideSteps({})[1].done, false);
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

// ---------- 擦除画布的区域换算（M4b-2） ----------

test("签章状态的说法：四种状态各一句，认不出的原样显示", () => {
  // 和后端 SigningStatus 的取值一一对应，不能漏、不能错位。
  assert.equal(L.signingLabel("none"), "无盖章文档");
  assert.equal(L.signingLabel("placeholder"), "占位章（未签署）");
  assert.equal(L.signingLabel("scan"), "扫描件签章");
  assert.equal(L.signingLabel("mixed"), "混合（部分占位章）");
  // 后端将来加了新状态：原样显示，不假装成已知的四种之一。
  assert.equal(L.signingLabel("sealed_by_lawyer"), "sealed_by_lawyer");
  assert.equal(L.signingLabel(null), "未知");
  assert.equal(L.signingLabel(undefined), "未知");
});

test("画布坐标换算：预览缩放过时要换回原始分辨率", () => {
  // 原始 1080×1920，预览缩到 540×960（一半）。
  const view = { width: 540, height: 960 };
  const source = { width: 1080, height: 1920 };
  const inView = { x: 100, y: 200, width: 50, height: 40 };
  assert.deepEqual(L.toSourceRect(inView, view, source), {
    x: 200,
    y: 400,
    width: 100,
    height: 80,
  });
  // 反方向回到画布。
  assert.deepEqual(
    L.toViewRect({ x: 200, y: 400, width: 100, height: 80 }, view, source),
    inView,
  );
  // 比例是 1 时原样返回。
  assert.deepEqual(
    L.toSourceRect(inView, source, source),
    inView,
  );
});

test("拖出矩形：两个对角点顺序任意，结果一致", () => {
  const a = { x: 30, y: 40 };
  const b = { x: 10, y: 10 };
  assert.deepEqual(L.normalizeRect(a, b), { x: 10, y: 10, width: 20, height: 30 });
  assert.deepEqual(L.normalizeRect(b, a), { x: 10, y: 10, width: 20, height: 30 });
  // 同一个点：宽高为 0。
  assert.deepEqual(L.normalizeRect(a, a), { x: 30, y: 40, width: 0, height: 0 });
});

test("太小的框算误点，不该提交", () => {
  assert.equal(L.rectIsUsable({ x: 0, y: 0, width: 3, height: 30 }), false);
  assert.equal(L.rectIsUsable({ x: 0, y: 0, width: 30, height: 3 }), false);
  assert.equal(L.rectIsUsable({ x: 0, y: 0, width: 4, height: 4 }), true);
  assert.equal(L.rectIsUsable({ x: 0, y: 0, width: 3, height: 3 }, 2), true);
});

test("区域要夹在画面内：拖出边界不会得到负坐标或超长宽", () => {
  const bounds = { width: 200, height: 100 };
  assert.deepEqual(
    L.clampRect({ x: -10, y: -5, width: 50, height: 20 }, bounds),
    { x: 0, y: 0, width: 50, height: 20 },
  );
  // 右边的框超出画面：宽度被裁到边界。
  assert.deepEqual(
    L.clampRect({ x: 180, y: 10, width: 50, height: 20 }, bounds),
    { x: 180, y: 10, width: 20, height: 20 },
  );
  // 完全在画面外：宽度夹成 0（调用方据此丢弃）。
  const outside = L.clampRect({ x: 500, y: 500, width: 10, height: 10 }, bounds);
  assert.equal(outside.x, 200);
  assert.equal(outside.y, 100);
  assert.equal(outside.width, 0);
  assert.equal(outside.height, 0);
});

test("命中测试：后画的框优先，右下角是缩放、框内是移动", () => {
  const regions = [
    { x: 0, y: 0, width: 100, height: 100 },
    { x: 50, y: 50, width: 100, height: 100 },
  ];
  // 两个框重叠处：后一个（下标 1）优先。
  assert.deepEqual(L.hitTestRegions({ x: 60, y: 60 }, regions, 12), {
    index: 1,
    part: "move",
  });
  // 只有第 1 个框覆盖的地方。
  assert.deepEqual(L.hitTestRegions({ x: 10, y: 10 }, regions, 12), {
    index: 0,
    part: "move",
  });
  // 第 2 个框的右下角（150,150）附近：缩放。
  assert.deepEqual(L.hitTestRegions({ x: 148, y: 152 }, regions, 12), {
    index: 1,
    part: "resize",
  });
  // 空白处：没命中。
  assert.equal(L.hitTestRegions({ x: 400, y: 400 }, regions, 12), null);
});

test("区域数量与自动识别候选的过滤", () => {
  assert.equal(L.regionSummary([]), "区域：0 个");
  assert.equal(L.regionSummary([{ x: 0, y: 0, width: 10, height: 10 }]), "区域：1 个");
  assert.equal(
    L.regionSummary([
      { x: 0, y: 0, width: 10, height: 10 },
      { x: 20, y: 20, width: 10, height: 10 },
    ]),
    "区域：2 个",
  );

  // 自动识别回来的候选：短边不足 8 像素的（噪点残影）要丢掉。
  const candidates = [
    { x: 0, y: 0, width: 40, height: 30 },
    { x: 0, y: 0, width: 7, height: 30 },
    { x: 0, y: 0, width: 40, height: 5 },
  ];
  assert.deepEqual(L.acceptableCandidates(candidates), [
    { x: 0, y: 0, width: 40, height: 30 },
  ]);
  assert.deepEqual(L.acceptableCandidates(null), []);
});


/** 一行任务的 JobView 骨架，默认是最没内容的样子。 */
const job = (extra = {}) => ({ id: "j1", state: "queued", done: 0, total: 0, current: null, ...extra });

test("任务状态：六种状态各有中文说法和色调", () => {
  const expected = [
    ["queued", { label: "排队中", tone: "" }],
    ["running", { label: "运行中", tone: "ok" }],
    ["paused", { label: "已暂停", tone: "warn" }],
    ["terminated", { label: "已终止", tone: "warn" }],
    ["done", { label: "完成", tone: "ok" }],
    ["failed", { label: "失败", tone: "err" }],
  ];
  for (const [state, want] of expected) {
    assert.deepEqual(L.jobStateLabel(state), want, state);
  }
  // 认不出的状态原样显示，不假装成别的；空值兜底成「未知」。
  assert.deepEqual(L.jobStateLabel("encoding"), { label: "encoding", tone: "" });
  assert.deepEqual(L.jobStateLabel(null), { label: "未知", tone: "" });
  assert.deepEqual(L.jobStateLabel(undefined), { label: "未知", tone: "" });
});

test("步骤状态：五种状态各自的说法，认不出的原样显示", () => {
  assert.equal(L.stepStateLabel("pending"), "待处理");
  assert.equal(L.stepStateLabel("running"), "处理中");
  assert.equal(L.stepStateLabel("done"), "完成");
  assert.equal(L.stepStateLabel("failed"), "失败");
  assert.equal(L.stepStateLabel("skipped"), "已跳过");
  assert.equal(L.stepStateLabel("weird"), "weird");
  assert.equal(L.stepStateLabel(null), "未知");
  assert.equal(L.stepStateLabel(undefined), "未知");
});

test("任务操作：每种状态给出哪些按钮", () => {
  const actions = (state) => L.jobActions(job({ state }));
  assert.deepEqual(actions("queued"), { pause: true, resume: false, terminate: true });
  assert.deepEqual(actions("running"), { pause: true, resume: false, terminate: true });
  assert.deepEqual(actions("paused"), { pause: false, resume: true, terminate: true });
  assert.deepEqual(actions("terminated"), { pause: false, resume: true, terminate: false });
  assert.deepEqual(actions("failed"), { pause: false, resume: true, terminate: false });
  // 已经完成的任务没有任何操作，别给用户一个点了没反应的按钮。
  assert.deepEqual(actions("done"), { pause: false, resume: false, terminate: false });
  // 认不出的状态、空的 job：一个按钮都不给。
  assert.deepEqual(actions("encoding"), { pause: false, resume: false, terminate: false });
  assert.deepEqual(L.jobActions(null), { pause: false, resume: false, terminate: false });
});

test("任务进度百分比：0 集、部分、全部，total 为 0 不出现 NaN", () => {
  assert.equal(L.jobPercent({ done: 0, total: 10 }), 0);
  assert.equal(L.jobPercent({ done: 3, total: 10 }), 30);
  assert.equal(L.jobPercent({ done: 1, total: 3 }), 33);
  assert.equal(L.jobPercent({ done: 10, total: 10 }), 100);
  // 一集都没排进来、字段缺失、整个 job 为空：都是 0，不能是 NaN 或 Infinity。
  for (const empty of [{ done: 0, total: 0 }, {}, { done: 5, total: 0 }, null]) {
    const percent = L.jobPercent(empty);
    assert.equal(percent, 0, JSON.stringify(empty));
    assert.equal(Number.isFinite(percent), true, JSON.stringify(empty));
  }
  // 进度超过总数（后端回写不及时）截到 100；负数按 0 算。
  assert.equal(L.jobPercent({ done: 9, total: 4 }), 100);
  assert.equal(L.jobPercent({ done: -2, total: 5 }), 0);
});

test("任务进度文案：有当前集时带上集名，没有就只报计数", () => {
  assert.equal(
    L.jobProgressText({ state: "running", done: 3, total: 10, current: "第04集.mp4" }),
    "3/10 · 正在处理 第04集.mp4",
  );
  assert.equal(L.jobProgressText({ state: "running", done: 3, total: 10, current: null }), "3/10");
  assert.equal(
    L.jobProgressText({ state: "paused", done: 3, total: 10, current: "第04集.mp4" }),
    "3/10 · 停在 第04集.mp4",
  );
  assert.equal(
    L.jobProgressText({ state: "terminated", done: 3, total: 10, current: "第04集.mp4" }),
    "3/10 · 停在 第04集.mp4",
  );
  // 已暂停但没记下停在哪一集：只说计数。
  assert.equal(L.jobProgressText({ state: "paused", done: 3, total: 10 }), "3/10");
  // 完成时不再说「正在处理第几集」。
  assert.equal(L.jobProgressText({ state: "done", done: 10, total: 10, current: "第10集.mp4" }), "10/10");
  assert.equal(L.jobProgressText({ state: "failed" }), "0/0");
});

test("合并任务：同 id 原地替换，新任务放最前面", () => {
  const list = [job({ id: "a", state: "running" }), job({ id: "b", state: "queued" }), job({ id: "c", state: "done" })];

  // 列表里已有同 id：替换那一条，位置和其余顺序都不变。
  const update = job({ id: "b", state: "paused", done: 2, total: 5 });
  const replaced = L.mergeJob(list, update);
  assert.notEqual(replaced, list); // 返回的是新数组
  assert.deepEqual(replaced.map((item) => item.id), ["a", "b", "c"]);
  assert.deepEqual(replaced[1], update);
  // 原数组没被就地改掉。
  assert.equal(list[1].state, "queued");
  assert.equal(list[1].done, 0);

  // 没有同 id：新任务插到最前面（和 job_list 的新到旧顺序一致），不是追加到末尾。
  const added = L.mergeJob(list, job({ id: "d", state: "queued" }));
  assert.deepEqual(added.map((item) => item.id), ["d", "a", "b", "c"]);
  assert.equal(list.length, 3);
  assert.deepEqual(list.map((item) => item.id), ["a", "b", "c"]);

  // 没有 id 的事件：原样返回一份副本，不插入也不报错。
  const copied = L.mergeJob(list, { state: "running" });
  assert.notEqual(copied, list);
  assert.deepEqual(copied, list);
  assert.deepEqual(L.mergeJob(list, null).map((item) => item.id), ["a", "b", "c"]);
});

test("侧栏任务数：只数排队、运行、暂停", () => {
  const list = [
    job({ id: "a", state: "queued" }),
    job({ id: "b", state: "running" }),
    job({ id: "c", state: "paused" }),
    job({ id: "d", state: "done" }),
    job({ id: "e", state: "failed" }),
    job({ id: "f", state: "terminated" }),
  ];
  // 已终止的不算：用户已经点了终止，不再催他去处理。
  assert.equal(L.activeJobCount(list), 3);
  assert.equal(L.activeJobCount([]), 0);
  assert.equal(L.activeJobCount([job({ id: "g", state: "terminated" })]), 0);
});

/** 一份通过校验的模板，各条测试只改其中一项。 */
const legalTemplate = () => ({
  coverAtSec: 0,
  subtitleSize: 32,
  watermarkOpacity: 0.5,
  musicVolume: 0.3,
  aiMarked: false,
  aiContentProducer: "",
  watermarkText: "剧名",
  watermarkCorner: "bottom_right",
  musicPath: "",
  musicLicensed: false,
  recut: false,
  recutCount: null,
  subtitleDir: "",
});

test("模板校验：合法模板返回空数组，0 和 1 都在区间内", () => {
  assert.deepEqual(L.templateProblems(legalTemplate()), []);
  // 边界值：透明度/音量 0 和 1 都算合法，封面取 0 秒（第一帧）也合法。
  assert.deepEqual(L.templateProblems({ ...legalTemplate(), coverAtSec: 0, watermarkOpacity: 0, musicVolume: 1 }), []);
});

test("模板校验：封面取帧秒数必须是 0 或正数", () => {
  const problems = (value) => L.templateProblems({ ...legalTemplate(), coverAtSec: value });
  const want = [["coverAtSec", "封面取帧秒数必须是 0 或正数"]];
  assert.deepEqual(problems(-1), want);
  assert.deepEqual(problems(NaN), want);
  assert.deepEqual(problems("3"), want); // 字符串不当数字用
  assert.deepEqual(problems(undefined), want);
  assert.deepEqual(problems(0), []);
  assert.deepEqual(problems(12.5), []); // 小数秒可以
});

test("模板校验：字幕字号必须是大于 0 的整数", () => {
  const problems = (value) => L.templateProblems({ ...legalTemplate(), subtitleSize: value });
  const want = [["subtitleSize", "字幕字号必须是大于 0 的整数"]];
  assert.deepEqual(problems(0), want);
  assert.deepEqual(problems(-4), want);
  assert.deepEqual(problems(10.5), want);
  assert.deepEqual(problems(24), []);
  assert.deepEqual(problems(1), []);
});

test("模板校验：水印透明度与背景音乐音量必须在 0 到 1 之间", () => {
  const opacity = (value) => L.templateProblems({ ...legalTemplate(), watermarkOpacity: value });
  const opacityWant = [["watermarkOpacity", "水印透明度必须在 0 到 1 之间"]];
  assert.deepEqual(opacity(1.2), opacityWant);
  assert.deepEqual(opacity(-0.1), opacityWant);
  assert.deepEqual(opacity(null), opacityWant);
  assert.deepEqual(opacity(0), []);
  assert.deepEqual(opacity(1), []);

  const volume = (value) => L.templateProblems({ ...legalTemplate(), musicVolume: value });
  const volumeWant = [["musicVolume", "背景音乐音量必须在 0 到 1 之间"]];
  assert.deepEqual(volume(2), volumeWant);
  assert.deepEqual(volume(-1), volumeWant);
  assert.deepEqual(volume(undefined), volumeWant);
  assert.deepEqual(volume(0), []);
  assert.deepEqual(volume(0.5), []);
});

test("模板校验：勾了 AI 标识要填服务提供者，且水印不能放右下角", () => {
  // 只看服务提供者这一条：水印先挪到左上角，免得同时命中右下角那条冲突。
  const producer = (value) =>
    L.templateProblems({ ...legalTemplate(), aiMarked: true, watermarkCorner: "top_left", aiContentProducer: value });
  const want = [
    ["aiContentProducer", "AI 生成合成服务提供者只能用字母、数字和常见英文符号（不能有空格、中文、引号和反斜杠）"],
  ];
  assert.deepEqual(producer(""), want); // 勾了就必须填
  assert.deepEqual(producer(undefined), want);
  assert.deepEqual(producer("某某科技"), want); // 中文不行
  assert.deepEqual(producer("Foo Bar"), want); // 空格不行
  assert.deepEqual(producer('Foo"Bar'), want); // 引号不行
  assert.deepEqual(producer("Foo\\Bar"), want); // 反斜杠不行
  assert.deepEqual(producer("ACME-Tech_1.0"), []);
  // 没勾 AI 标识时不校验提供者，填什么都不管。
  assert.deepEqual(L.templateProblems({ ...legalTemplate(), aiMarked: false, aiContentProducer: "某某科技" }), []);

  const corner = (watermarkText, watermarkCorner) =>
    L.templateProblems({ ...legalTemplate(), aiMarked: true, aiContentProducer: "ACME", watermarkText, watermarkCorner });
  assert.deepEqual(corner("剧名", "bottom_right"), [
    ["watermarkCorner", "勾选了 AI 标识时，剧名水印不能放在右下角（那里是 AI 标识的位置）"],
  ]);
  // 没写水印文字、或者水印不在右下角：都不冲突。
  assert.deepEqual(corner("", "bottom_right"), []);
  assert.deepEqual(corner("剧名", "top_left"), []);
});

test("模板校验：用背景音乐要先勾授权", () => {
  assert.deepEqual(L.templateProblems({ ...legalTemplate(), musicPath: "/音乐/bgm.mp3", musicLicensed: false }), [
    ["musicLicensed", "使用背景音乐前要先勾选「我有这段音乐的使用授权」"],
  ]);
  assert.deepEqual(L.templateProblems({ ...legalTemplate(), musicPath: "/音乐/bgm.mp3", musicLicensed: true }), []);
  // 没选音乐时，授权勾不勾都不管。
  assert.deepEqual(L.templateProblems({ ...legalTemplate(), musicLicensed: false }), []);
});

test("模板校验：重新分集的集数，以及不能同时烧字幕", () => {
  const recut = (extra) => L.templateProblems({ ...legalTemplate(), recut: true, ...extra });
  const countWant = [["recutCount", "重新分集的集数必须是大于 0 的整数（留空表示和原来一样多）"]];
  assert.deepEqual(recut({ recutCount: 0 }), countWant);
  assert.deepEqual(recut({ recutCount: 2.5 }), countWant);
  assert.deepEqual(recut({ recutCount: -3 }), countWant);
  assert.deepEqual(recut({ recutCount: null }), []); // 留空表示和原来一样多
  assert.deepEqual(recut({ recutCount: 12 }), []);
  // 重新分集后原字幕的时间对不上：不能同时烧字幕。
  assert.deepEqual(recut({ subtitleDir: "/字幕" }), [
    ["subtitleDir", "重新分集后原字幕的时间对不上，不能同时烧字幕；请二选一"],
  ]);
  // 不重新分集时，字幕目录和原来的字幕对得上，不冲突。
  assert.deepEqual(L.templateProblems({ ...legalTemplate(), subtitleDir: "/字幕" }), []);
});

test("模板校验：多个问题一起报，按封面→字号→透明度→音量→音乐授权的顺序", () => {
  const fields = L.templateProblems({
    coverAtSec: -1,
    subtitleSize: 0,
    watermarkOpacity: 2,
    musicVolume: 2,
    musicPath: "/音乐/bgm.mp3",
    musicLicensed: false,
  }).map(([field]) => field);
  assert.deepEqual(fields, ["coverAtSec", "subtitleSize", "watermarkOpacity", "musicVolume", "musicLicensed"]);
});

test("处理档位：三档的取值、中文标签和说明", () => {
  assert.equal(L.STRENGTHS.length, 3);
  assert.deepEqual(L.STRENGTHS.map((item) => item.value), ["light", "medium", "heavy"]);
  assert.deepEqual(L.STRENGTHS.map((item) => item.label), ["轻", "中", "重"]);
  assert.deepEqual(L.STRENGTHS.map((item) => item.detail), [
    "变化最小，画质损失最少",
    "折中",
    "变化最大，画质损失也最多",
  ]);
});

test("受限模块清单：处理版本在锁定范围内，有使用权时不上遮罩", () => {
  // 不用 deepEqual 钉死整个清单：加一个受限模块（例如 M5b 的 server）
  // 不该让这条测试失败——它要保证的是「在处理版本、生成报审包这些付费
  // 功能上，没使用权就上遮罩」。
  for (const name of ["library", "generate", "version", "mount", "transcode", "toolbox"]) {
    assert.ok(L.LICENSED_MODULES.includes(name), `${name} 应当在受限模块清单里`);
    assert.ok(L.moduleLock(locked("no_entitlement"), name), `${name} 没使用权时应当上遮罩`);
    assert.equal(L.moduleLock(online(), name), null, `${name} 有使用权时不该上遮罩`);
  }
  assert.equal(L.moduleLock(locked("no_entitlement"), "version").actionLabel, "兑换卡密");
  // 首页与任务不是付费功能，不进清单。
  assert.ok(!L.LICENSED_MODULES.includes("home"));
  assert.ok(!L.LICENSED_MODULES.includes("jobs"));
});

test("处理配方与处理版本错误文案：每个码都有非空中文说法", () => {
  const known = {
    recipe_not_found: /找不到这份处理配方/,
    recipe_malformed: /格式不对/,
    recipe_bad_signature: /签名没有通过校验/,
    recipe_bad_payload: /内容不完整/,
    recipe_wrong_device: /不是发给这台电脑的/,
    recipe_wrong_task: /和这部剧的文件对不上/,
    wrong_device: /不是发给这台电脑的/,
  };
  for (const [code, pattern] of Object.entries(known)) {
    const text = L.errorText({ code });
    assert.match(text, pattern, code);
    assert.match(text, /[\u4e00-\u9fa5]/, code); // 是中文句子，不是把错误码原样抛给用户
    assert.notEqual(text, "操作失败", code); // 命中了固定文案，不是兜底
    // 固定文案优先：后端就算也给了 message，也给这句更清楚的说法。
    assert.equal(L.errorText({ code, message: "后端原文" }), text, code);
    assert.equal(L.errorText({ error: code }), text, code);
  }
  // 配方相关的码都要说清是「处理配方」出的问题。
  for (const code of [
    "recipe_consumed",
    "recipe_expired",
    "recipe_not_found",
    "recipe_malformed",
    "recipe_bad_signature",
    "recipe_bad_payload",
    "recipe_wrong_device",
    "recipe_wrong_task",
  ]) {
    assert.match(L.errorText({ code }), /处理配方/, code);
  }
  // 认不出的码：有 message 用 message，没有就兜底一句，绝不吞掉错误。
  assert.equal(L.errorText({ code: "version_invalid", message: "模板里没有可选字幕轨" }), "模板里没有可选字幕轨");
  assert.equal(L.errorText({ code: "version_invalid" }), "操作失败");
  assert.equal(L.errorText({ code: "totally_unknown_code" }), "操作失败");
  assert.equal(L.errorText({ code: "totally_unknown_code", message: "服务端原话" }), "服务端原话");
});

test("服务端处理：状态文案与动作", () => {
  // 每个状态都要有人话，不能把英文状态码直接抛给用户。
  for (const state of [
    "PENDING",
    "QUEUED",
    "RUNNING",
    "VERIFYING",
    "SUCCEEDED",
    "RETRY_WAITING",
    "CANCEL_REQUESTED",
    "CANCELLED",
    "FAILED",
  ]) {
    const info = L.serverStateLabel(state);
    assert.match(info.label, /[\u4e00-\u9fa5]/, state);
  }
  // 认不出的状态原样显示，不假装成别的。
  assert.equal(L.serverStateLabel("WHAT").label, "WHAT");
  assert.equal(L.serverStateLabel(null).label, "未知");

  // 只有成功的才算「有产物可下」。
  assert.equal(L.serverTaskActions({ state: "SUCCEEDED" }).download, true);
  assert.equal(L.serverTaskActions({ state: "RUNNING" }).download, false);
  assert.equal(L.serverTaskActions({ state: "FAILED" }).download, false);
  // 取消只在没结束时给。
  assert.equal(L.serverTaskActions({ state: "RUNNING" }).cancel, true);
  assert.equal(L.serverTaskActions({ state: "SUCCEEDED" }).cancel, false);
  assert.equal(L.serverTaskActions({}).cancel, false);
});

test("服务端处理：结束状态判定", () => {
  assert.equal(L.serverTaskFinished({ state: "SUCCEEDED" }), true);
  assert.equal(L.serverTaskFinished({ state: "CANCELLED" }), true);
  assert.equal(L.serverTaskFinished({ state: "FAILED" }), true);
  assert.equal(L.serverTaskFinished({ state: "RUNNING" }), false);
  assert.equal(L.serverTaskFinished({ state: "QUEUED" }), false);
  assert.equal(L.serverTaskFinished(null), false);
});

test("服务端处理：上传进度", () => {
  assert.equal(L.uploadPercent({ totalChunks: 4, sentChunks: 1 }), 25);
  assert.equal(L.uploadPercent({ totalChunks: 4, sentChunks: 4 }), 100);
  // 总块数为 0 时不能除零，也不能画成 100%。
  assert.equal(L.uploadPercent({ totalChunks: 0, sentChunks: 0 }), 0);
  assert.equal(L.uploadPercent(null), 0);
  // 超过总数时封顶，负值归零——进度条不该出现怪数值。
  assert.equal(L.uploadPercent({ totalChunks: 4, sentChunks: 9 }), 100);
  assert.equal(L.uploadPercent({ totalChunks: 4, sentChunks: -3 }), 0);

  assert.equal(L.uploadText({ totalChunks: 8, sentChunks: 3, skippedChunks: 0 }), "3/8 块");
  assert.equal(
    L.uploadText({ totalChunks: 8, sentChunks: 8, skippedChunks: 3 }),
    "8/8 块（跳过 3 块已传的）",
  );
});

test("服务端处理：是受限模块，需要使用权", () => {
  const locked = { tier: "locked", lockReason: "no_entitlement" };
  assert.ok(L.moduleLock(locked, "server"), "服务端处理要花积分，没使用权应当被挡");
  const active = { tier: "month", lockReason: null };
  assert.equal(L.moduleLock(active, "server"), null);
});

test("服务端处理：该续传还是新开会话", () => {
  const unfinished = {
    uploadId: "up_1",
    chunkSize: 8388608,
    totalChunks: 10,
    assetId: null,
    sourcePath: "/剧/第01集.mp4",
  };
  // 同一个文件、没传完 → 续传，不能从头再来。
  const resume = L.uploadResumePlan(unfinished, "/剧/第01集.mp4");
  assert.equal(resume.resume, true);
  assert.equal(resume.uploadId, "up_1");
  assert.equal(resume.chunkSize, 8388608);

  // 换了另一集 → 必须新开：接着往旧会话里传别的文件会拼出坏文件。
  assert.equal(L.uploadResumePlan(unfinished, "/剧/第02集.mp4").resume, false);
  // 上一次已经传完 → 新开。
  assert.equal(L.uploadResumePlan({ ...unfinished, assetId: "as_1" }, "/剧/第01集.mp4").resume, false);
  // 没有会话 → 新开。
  assert.equal(L.uploadResumePlan(null, "/剧/第01集.mp4").resume, false);
  assert.equal(L.uploadResumePlan({}, "/剧/第01集.mp4").resume, false);
  // 拿不到本机路径 → 不能「续传」一个不知道是什么的文件。
  assert.equal(L.uploadResumePlan(unfinished, null).resume, false);
});
