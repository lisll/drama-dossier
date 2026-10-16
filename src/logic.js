// 界面用到的纯函数：不碰 DOM、不调 Tauri，浏览器里挂在 `window.DossierLogic`，
// `node --test` 里用 `require("../../src/logic.js")` 直接测。
(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  else root.DossierLogic = api;
})(typeof self !== "undefined" ? self : globalThis, () => {
  // ---------- 卡密 ----------
  // 和服务端 cards.py 保持一致：16 位，数字 2-9 和除 I、O 以外的大写字母；可带 `DD` 前缀，空格和短横线忽略。

  const CARD_ALPHABET = "23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
  const CARD_LENGTH = 16;
  const CARD_PREFIX = "DD";
  const CARD_SEPARATORS = /[\s-]+/g;

  /** 去掉分隔符、转大写，再去掉 `DD` 前缀（只有总长正好是 18 位时才算前缀，和服务端一样）。 */
  function cardCore(raw) {
    const text = String(raw ?? "").replace(CARD_SEPARATORS, "").toUpperCase();
    if (text.length === CARD_LENGTH + CARD_PREFIX.length && text.startsWith(CARD_PREFIX)) {
      return text.slice(CARD_PREFIX.length);
    }
    return text;
  }

  /**
   * 提交前检查卡密。通过时返回 `{ ok: true, code }`（16 位，不带分隔符），
   * 否则返回 `{ ok: false, message }`，当场提示，不发请求。
   */
  function validateCard(raw) {
    const stripped = String(raw ?? "").replace(CARD_SEPARATORS, "");
    if (!stripped) return { ok: false, message: "请输入卡密" };
    // 全角字符、中文之类：大多是输入法没切回英文。
    if (!/^[\x21-\x7e]*$/.test(stripped)) {
      return { ok: false, message: "卡密只含英文字母和数字，请把输入法切到英文后重新输入" };
    }
    const core = cardCore(stripped);
    const confusable = [...new Set(core.match(/[01IO]/g) ?? [])];
    if (confusable.length) {
      return {
        ok: false,
        message: `卡密里没有 ${confusable.join("、")}：0 和 1 是数字，I 和 O 是字母，它们都不会出现在卡密里，请对照卡片检查`,
      };
    }
    const unknown = [...new Set(core.split("").filter((c) => !CARD_ALPHABET.includes(c)))];
    if (unknown.length) {
      return { ok: false, message: `卡密里不该有「${unknown.join(" ")}」，请检查是否输错` };
    }
    if (core.length !== CARD_LENGTH) {
      // 写了 DD 前缀但位数不对：数位数时不算前缀，免得用户对着卡片数出来对不上。
      const counted =
        core.length > CARD_LENGTH && core.startsWith(CARD_PREFIX) ? core.length - CARD_PREFIX.length : core.length;
      return { ok: false, message: `卡密是 ${CARD_LENGTH} 位，现在是 ${counted} 位，请检查是否输错或漏输` };
    }
    return { ok: true, code: core };
  }
  /**
   * 输入时自动转大写、按 4 位分组。`caret` 是光标在原文里的位置，返回格式化后光标该在的位置，
   * 这样在中间改字时光标不会跳到末尾。写了 `DD` 前缀的保留为 `DD-` 开头。
   */
  function formatCardInput(raw, caret) {
    const text = String(raw ?? "");
    const at = caret == null ? text.length : Math.max(0, Math.min(caret, text.length));
    const significant = (s) => s.replace(CARD_SEPARATORS, "").toUpperCase();
    let body = significant(text);
    let before = significant(text.slice(0, at)).length;
    let prefix = "";
    // 只有写满 DD 再往后写，才把它当前缀；单独一个 D 或 DD 先原样放着。
    if (body.length > CARD_PREFIX.length && body.startsWith(CARD_PREFIX) && body.length > CARD_LENGTH) {
      prefix = `${CARD_PREFIX}-`;
      body = body.slice(CARD_PREFIX.length);
      before = Math.max(0, before - CARD_PREFIX.length);
    }
    const groups = body.match(/.{1,4}/g) ?? [];
    const value = prefix + groups.join("-");
    // 光标前有 n 个有效字符：每满 4 个多一个短横线（正好在组尾时不算后面的那个短横线）。
    const dashes = before > 0 ? Math.floor((before - 1) / 4) : 0;
    const position = before === 0 && prefix ? prefix.length : prefix.length + before + dashes;
    return { value, caret: Math.min(position, value.length) };
  }

  // ---------- 时间与时长 ----------

  const pad2 = (n) => String(n).padStart(2, "0");

  /** Unix 秒 → 本地日期 `2026-11-14`。 */
  function formatDate(unixSecs) {
    if (unixSecs == null) return "";
    const d = new Date(unixSecs * 1000);
    return `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`;
  }

  /** Unix 秒 → 本地时间 `2026-11-14 08:05`。 */
  function formatDateTime(unixSecs) {
    if (unixSecs == null) return "";
    const d = new Date(unixSecs * 1000);
    return `${formatDate(unixSecs)} ${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
  }

  /** ISO 8601 字符串 → 本地时间；解析不了就原样返回，不显示「Invalid Date」。 */
  function formatIso(text) {
    const ms = Date.parse(text ?? "");
    return Number.isNaN(ms) ? String(text ?? "") : formatDateTime(Math.floor(ms / 1000));
  }

  /** 要等多久：向上取整，「还要等 1 分钟」比「还要等 0 分钟」诚实。 */
  function formatWait(secs) {
    const s = Math.max(0, Math.ceil(Number(secs) || 0));
    if (s < 60) return `${Math.max(s, 1)} 秒`;
    if (s < 3600) return `${Math.ceil(s / 60)} 分钟`;
    if (s < 86400) return `${Math.ceil(s / 3600)} 小时`;
    return `${Math.ceil(s / 86400)} 天`;
  }

  /** 还能用多久：向下取整，不多报。 */
  function formatRemaining(secs) {
    const s = Math.max(0, Math.floor(Number(secs) || 0));
    if (s < 60) return "不到 1 分钟";
    if (s < 2 * 3600) return `${Math.floor(s / 60)} 分钟`;
    // 离线宽限期是 72 小时：4 天以内按小时说，「71 小时」比「2 天」准。
    if (s < 4 * 86400) return `${Math.floor(s / 3600)} 小时`;
    return `${Math.floor(s / 86400)} 天`;
  }

  /** 验证码按钮：冷却中显示倒计时并禁用。 */
  function codeButtonLabel(remainingSecs) {
    const s = Math.ceil(Number(remainingSecs) || 0);
    return s > 0 ? { text: `${s} 秒后可重发`, disabled: true } : { text: "获取验证码", disabled: false };
  }
  // ---------- 错误码 → 文案 ----------
  // 后端错误是 `{error, message, retryAfter?, ...}`（见 src-tauri/src/account/commands.rs）。
  // 认识的错误码给更具体的说法；不认识的用服务端/后端给的 message，绝不吞掉。

  const ERROR_TEXT = {
    invalid_credentials: "邮箱或密码不正确",
    email_taken: "这个邮箱已经注册过，可以直接登录；忘了密码可以重设",
    email_not_registered: "这个邮箱还没注册",
    code_invalid: "验证码不对，请检查后重新输入",
    code_expired: "验证码已过期，请重新获取",
    code_exhausted: "验证码输错次数太多，已经作废，请重新获取",
    wrong_password: "密码不正确",
    card_not_found: "没有这张卡密，请对照卡片检查是否输错",
    card_used: "这张卡密已经被别的账号兑换过了",
    card_revoked: "这张卡密所在的批次已作废，请联系发卡方",
    already_lifetime: "你的账号已经是永久授权，不需要再兑换；这张卡没有被用掉，可以留给别人",
    device_not_found: "这台电脑已经解绑过了",
    mail_unavailable: "验证码邮件没发出去，请稍后重试",
    service_unavailable: "服务暂时不可用，请稍后重试",
    unauthorized: "登录已失效：这台电脑可能已在别处被解绑，请重新登录",
    // 报审包（M4a）。分集被改动、被移走、目录已存在这几种，后端的说明里带了具体文件名，直接用后端的。
    entitlement_required: "没有有效的使用权，不能生成报审包：请先兑换卡密或续期",
    scan_invalid: "扫描件不符合要求：只支持 PNG、JPEG、PDF，每份不超过 5 MB",
    payload_too_large: "提交的内容太大，服务端拒收了：请减少扫描件或集数后重试",
    package_manifest_invalid: "报审包清单没有通过校验（签名不对或不是发给这台电脑的），本次没有写入任何文件，请重新生成",
    package_server_file_mismatch: "服务端发回的文件和清单对不上，本次没有写入任何文件，请重新生成",
    // 处理配方（M4b 接入界面，文案先备好）。
    recipe_consumed: "这份处理配方已经用过了，请重新申请",
    recipe_expired: "这份处理配方已过期，请重新申请",
  };

  /** 把后端错误变成给人看的一句话。 */
  function errorText(error) {
    const code = error?.code ?? error?.error ?? "";
    // 只取 message 字段；对象本身转字符串会变成「[object Object]」，不能给人看。
    const message = typeof error === "string" ? error : error?.message ? String(error.message) : "";
    if (code === "rate_limited") {
      return error?.retryAfter ? `操作太频繁，请 ${formatWait(error.retryAfter)}后再试` : "操作太频繁，请稍后再试";
    }
    if (code === "unbind_limit") {
      const wait = error?.retryAfter ? `，${formatWait(error.retryAfter)}后可以再解绑` : "";
      return `本周期的解绑次数已经用完${wait}`;
    }
    // 参数校验失败时后端已经把各字段的说明拼好了，比固定文案具体。
    if (code === "invalid_request") return message || "填写的内容不正确";
    return ERROR_TEXT[code] ?? (message || "操作失败");
  }

  // ---------- 授权状态 ----------
  // 输入都是 `account_status` 的返回值（src-tauri/src/account/session.rs 的 StatusPayload）。

  /** 使用权的说法：「永久授权」「到期 2026-11-14」「已到期」「未授权」。 */
  function entitlementText(status, nowSecs) {
    const kind = status?.entitlementKind;
    if (kind === "lifetime") return "永久授权";
    if (kind === "timed" && status.entitlementUntil != null) {
      return status.entitlementUntil > nowSecs ? `到期 ${formatDate(status.entitlementUntil)}` : "已到期";
    }
    return "未授权";
  }

  /** 侧栏账号行的徽章：文字 + 色调（ok / warn / err / 空）。离线优先，因为它影响「生成报审包」。 */
  function accountBadge(status, nowSecs) {
    if (!status?.loggedIn) return { text: "未登录", tone: "" };
    if (status.tier === "offline") return { text: "离线", tone: "warn" };
    if (status.tier === "locked") {
      if (status.lockReason === "entitlement_expired") return { text: "已到期", tone: "err" };
      if (status.lockReason === "no_entitlement") return { text: "未授权", tone: "warn" };
      if (status.lockReason === "needs_activation") return { text: "待绑定", tone: "warn" };
      return { text: "已锁定", tone: "err" };
    }
    if (status.entitlementKind === "lifetime") return { text: "永久", tone: "ok" };
    if (status.entitlementKind === "timed" && status.entitlementUntil != null) {
      return { text: `到 ${formatDate(status.entitlementUntil)}`, tone: status.renewSoon ? "warn" : "ok" };
    }
    return { text: entitlementText(status, nowSecs), tone: "" };
  }
  // 全屏锁定页：这几种情况整个程序都用不了，只能按提示处理。
  const LOCK_PAGES = {
    grace_expired: { title: "离线使用期已过", action: "retry", actionLabel: "联网后重试" },
    clock_rollback: { title: "系统时间不对", action: "retry", actionLabel: "已校准，重试" },
    license_invalid: { title: "授权凭证无效", action: "retry", actionLabel: "联网后重试" },
    storage_unavailable: { title: "系统钥匙串不可用", action: "retry", actionLabel: "重试" },
    config_error: { title: "安装包配置有误", action: null, actionLabel: "" },
  };

  // 没有使用权时锁住的模块；首页和设置一直能进。
  const LICENSED_MODULES = ["library", "generate", "mount", "transcode", "toolbox"];

  /**
   * 该显示哪一屏：
   * - `restoring`：启动时还在恢复登录；
   * - `auth`：没登录、被解绑、注销后；
   * - `lock`：全屏锁定页（见 LOCK_PAGES）；
   * - `app`：主界面（没有使用权时，受限模块显示遮罩；名额满了时弹出设备管理）。
   */
  function accountView(status) {
    if (!status || status.restoring) return "restoring";
    const reason = status.tier === "locked" ? status.lockReason : null;
    // 配置错误和钥匙串不可用与登录无关，没登录时也要先把它们说清楚，否则登录了也存不下。
    if (reason === "config_error" || reason === "storage_unavailable") return "lock";
    if (!status.loggedIn || reason === "not_logged_in" || reason === "device_unbound") return "auth";
    if (reason && LOCK_PAGES[reason]) return "lock";
    return "app";
  }

  /** 全屏锁定页的内容；`message` 用后端给的具体原因。 */
  function lockPage(status) {
    const page = LOCK_PAGES[status?.lockReason] ?? { title: "暂时不能使用", action: "retry", actionLabel: "重试" };
    return { ...page, message: status?.message || "请联网后重试", logout: Boolean(status?.loggedIn) };
  }

  /** 受限模块的遮罩：没有使用权或已到期时返回提示，否则返回 null。 */
  function moduleLock(status, moduleName) {
    if (!LICENSED_MODULES.includes(moduleName)) return null;
    if (status?.tier !== "locked") return null;
    if (status.lockReason === "no_entitlement") {
      return { title: "还没有使用权", message: "兑换卡密后就能使用剧目库、生成报审包、挂载、转码和工具箱。", actionLabel: "兑换卡密" };
    }
    if (status.lockReason === "entitlement_expired") {
      return { title: "使用权已到期", message: "兑换新的卡密续期后，就能继续使用这些功能。", actionLabel: "兑换卡密续期" };
    }
    if (status.lockReason === "needs_activation") {
      return { title: "这台电脑还没绑定", message: "账号绑定的电脑已满，解绑一台后就能绑定这台电脑。", actionLabel: "管理设备" };
    }
    return null;
  }

  /**
   * 顶部横幅（可以同时有几条）：`{ id, tone, text, action?, actionLabel?, dismissible? }`。
   * `dismissed` 是用户关掉过的横幅 id 集合。
   */
  function accountBanners(status, nowSecs, dismissed = new Set()) {
    if (!status?.loggedIn) return [];
    const banners = [];
    if (status.tier === "offline") {
      const left = status.offlineUntil != null ? Math.max(0, status.offlineUntil - nowSecs) : status.offlineRemainingSecs;
      banners.push({
        id: "offline",
        tone: "warn",
        text: `连不上服务端，还能离线使用 ${formatRemaining(left)}；生成报审包需要联网。`,
        action: "retry",
        actionLabel: "重新连接",
      });
    }
    if (status.renewSoon && status.entitlementUntil != null) {
      const id = `renew-${status.entitlementUntil}`;
      if (!dismissed.has(id)) {
        const days = Math.max(0, Math.ceil((status.entitlementUntil - nowSecs) / 86400));
        banners.push({
          id,
          tone: "warn",
          text: days > 0 ? `使用权还剩 ${days} 天（${formatDate(status.entitlementUntil)} 到期）。` : "使用权今天到期。",
          action: "redeem",
          actionLabel: "去续期",
          dismissible: true,
        });
      }
    }
    if (status.storageWarning && !dismissed.has("storage")) {
      banners.push({ id: "storage", tone: "warn", text: status.storageWarning, dismissible: true });
    }
    return banners;
  }
  // ---------- 新手引导 ----------

  /**
   * 6 步引导，按真实状态打勾。`facts`：
   * - `entitled`：有使用权（联网或离线档）；
   * - `libraryRoot` / `dramaCount`：记住的素材目录（扫描成功后才会记住）/ 本次扫描到的剧目数；
   * - `ledgerCount`：生成流水条数；
   * - `jianyingReady`：探测到剪映草稿目录，或手动设置过。
   */
  function guideSteps(facts) {
    const f = facts ?? {};
    const generated = (f.ledgerCount ?? 0) > 0;
    return [
      { id: "license", title: "授权卡密", detail: "兑换卡密，开通剧目库、报审包和工具箱。", done: Boolean(f.entitled), action: "redeem", actionLabel: "兑换卡密" },
      { id: "recharge", title: "充值积分", detail: "积分与充值即将推出，现在可以跳过。", done: false, disabled: true, actionLabel: "即将推出" },
      { id: "material", title: "准备素材", detail: "选好素材根目录并扫描，一部剧一个子目录。", done: Boolean(f.libraryRoot) || (f.dramaCount ?? 0) > 0, action: "library", actionLabel: "去剧目库" },
      { id: "cost", title: "配置成本报表", detail: "在「生成报审包」里填好成本信息，生成一次报审包。", done: generated, action: "generate", actionLabel: "去生成" },
      { id: "jianying", title: "找到剪映草稿目录", detail: "在设置里探测剪映专业版，或手动填写草稿目录。", done: Boolean(f.jianyingReady), action: "jianying", actionLabel: "去设置" },
      { id: "start", title: "开始任务", detail: "生成流水里有了记录，就说明整条流程走通了。", done: generated, action: "mount", actionLabel: "去挂载" },
    ];
  }

  /** 充值那一步还没上线，不算进总数，免得进度永远到不了 100%。 */
  function guideProgress(steps) {
    const counted = steps.filter((step) => !step.disabled);
    return { done: counted.filter((step) => step.done).length, total: counted.length };
  }

  /** 第一次成功生成报审包之前一直显示引导，除非用户手动收起。 */
  function guideVisible(ledgerCount, collapsed) {
    return !collapsed && !(ledgerCount > 0);
  }

  // ---------- 近 7 天统计 ----------

  /** 本地日期 `2026-11-14`。流水的 `ts` 是带时区偏移的本地时间，前 10 位就是同一种写法。 */
  function localDayKey(date) {
    return `${date.getFullYear()}-${pad2(date.getMonth() + 1)}-${pad2(date.getDate())}`;
  }

  /** 最近 7 天（含今天）每天生成了几次报审包，从早到晚。 */
  function last7Days(records, now = new Date()) {
    const days = [];
    for (let back = 6; back >= 0; back -= 1) {
      const day = new Date(now.getFullYear(), now.getMonth(), now.getDate() - back);
      days.push({ key: localDayKey(day), label: `${day.getMonth() + 1}/${day.getDate()}`, count: 0 });
    }
    const index = new Map(days.map((day, i) => [day.key, i]));
    for (const record of records ?? []) {
      const i = index.get(String(record?.ts ?? "").slice(0, 10));
      if (i !== undefined) days[i].count += 1;
    }
    return days;
  }

  /** 柱状图几何：纵轴上限取整到「好看」的数，全是 0 时也画出底线。 */
  function barChart(days, { width = 420, height = 140, gap = 14, top = 16, bottom = 22 } = {}) {
    const max = Math.max(...days.map((day) => day.count), 0);
    const ceiling = max <= 4 ? 4 : Math.ceil(max / 5) * 5;
    const plot = height - top - bottom;
    const barWidth = (width - gap * (days.length + 1)) / days.length;
    const bars = days.map((day, i) => {
      const h = Math.round((day.count / ceiling) * plot);
      return {
        ...day,
        x: Math.round(gap + i * (barWidth + gap)),
        y: top + plot - h,
        width: Math.round(barWidth),
        height: h,
      };
    });
    return { width, height, ceiling, baseline: top + plot, bars, total: days.reduce((sum, day) => sum + day.count, 0) };
  }
  // ---------- 设备 ----------

  const PLATFORM_LABELS = { windows: "Windows", macos: "macOS", linux: "Linux" };

  function platformLabel(platform) {
    return PLATFORM_LABELS[platform] ?? String(platform ?? "未知");
  }

  /** 解绑次数：`{limit, window_days, remaining}` → 「本周期（30 天）还能解绑 2 次，共 3 次」。 */
  function unbindQuotaText(quota) {
    if (!quota) return "";
    return `本周期（${quota.window_days} 天）还能解绑 ${quota.remaining} 次，共 ${quota.limit} 次`;
  }

  /** 设备列表排序：本机在最前，其余按最近在线从新到旧。 */
  function sortDevices(devices) {
    return [...(devices ?? [])].sort((a, b) => {
      if (a.current !== b.current) return a.current ? -1 : 1;
      return String(b.last_seen_at).localeCompare(String(a.last_seen_at));
    });
  }

  // ---------- 注销确认 ----------

  /** 注销的最后一道确认：密码非空，且再输一遍的邮箱和账号邮箱一致（不分大小写、忽略首尾空格）。 */
  function canConfirmDelete(accountEmail, typedEmail, password) {
    const normalize = (value) => String(value ?? "").trim().toLowerCase();
    return Boolean(password) && Boolean(normalize(accountEmail)) && normalize(accountEmail) === normalize(typedEmail);
  }

  // ---------- 主题 ----------

  const THEME_KEY = "dossier.theme";

  /** 存的值只认 `light`，其余（没存过、被改坏）一律暗色。 */
  function resolveTheme(stored) {
    return stored === "light" ? "light" : "dark";
  }

  // ---------- 公告（内置在本地，M7 换成远程来源） ----------

  const ANNOUNCEMENTS = [
    { title: "账号与授权上线", body: "兑换卡密开通功能；一个账号同时绑定 1 台电脑，在「设备管理」里可以查看和解绑。" },
    { title: "断网也能用", body: "联网登录过一次之后，断网 72 小时内照常使用本地功能；生成报审包需要联网。" },
    { title: "换电脑怎么办", body: "先在旧电脑或新电脑的「设备管理」里解绑旧电脑，再在新电脑上登录。每 30 天最多解绑 3 次。" },
  ];

  return {
    CARD_ALPHABET,
    CARD_LENGTH,
    LICENSED_MODULES,
    THEME_KEY,
    ANNOUNCEMENTS,
    cardCore,
    validateCard,
    formatCardInput,
    formatDate,
    formatDateTime,
    formatIso,
    formatWait,
    formatRemaining,
    codeButtonLabel,
    errorText,
    entitlementText,
    accountBadge,
    accountView,
    lockPage,
    moduleLock,
    accountBanners,
    guideSteps,
    guideProgress,
    guideVisible,
    localDayKey,
    last7Days,
    barChart,
    platformLabel,
    unbindQuotaText,
    sortDevices,
    canConfirmDelete,
    resolveTheme,
  };
});
