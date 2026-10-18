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
    // 积分（M5a）。余额不足是 402，和「没使用权」（403）分开：给的下一步不同。
    insufficient_credits: "积分不足，请先充值",
    price_missing: "服务端缺少计价规则，请联系管理员",
    tier_not_found: "这个充值档位已下架，请刷新后重选",
    order_not_found: "找不到这笔订单",
    amount_mismatch: "回调金额与订单不一致，已拒绝入账",
    payment_not_configured: "这个支付渠道还没配置好，暂时不能用",
    payment_verify_failed: "支付回调没有通过验签，已拒绝",
    invite_not_configured: "邀请奖励还没配置，请联系管理员",
    hold_not_found: "找不到对应的预扣记录",
    hold_closed: "这笔预扣已经结束，不能再改",
    scan_invalid: "扫描件不符合要求：只支持 PNG、JPEG、PDF，每份不超过 5 MB",
    payload_too_large: "提交的内容太大，服务端拒收了：请减少扫描件或集数后重试",
    package_manifest_invalid: "报审包清单没有通过校验（签名不对或不是发给这台电脑的），本次没有写入任何文件，请重新生成",
    package_server_file_mismatch: "服务端发回的文件和清单对不上，本次没有写入任何文件，请重新生成",
    // 处理配方与处理版本（M4b-1）。配方刚领到就在本机验签，验不过的这几种都说明「这次没扣、没生成」。
    recipe_consumed: "这份处理配方已经用过了，请重新申请",
    recipe_expired: "这份处理配方已过期，请重新申请",
    recipe_not_found: "服务端找不到这份处理配方，请重新开始生成",
    recipe_malformed: "服务端发回的处理配方格式不对，本次没有生成任何文件，请稍后重试",
    recipe_bad_signature: "处理配方的签名没有通过校验，本次没有生成任何文件，请稍后重试",
    recipe_bad_payload: "处理配方的内容不完整，本次没有生成任何文件，请稍后重试",
    recipe_wrong_device: "这份处理配方不是发给这台电脑的，本次没有生成任何文件",
    recipe_wrong_task: "处理配方和这部剧的文件对不上，本次没有生成任何文件，请重新开始生成",
    wrong_device: "这份处理配方不是发给这台电脑的",
    // 后端在本机核对时发现的问题已经写明了具体原因（哪个文件、哪一项），直接用后端的说明。
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
  // 「任务」不锁：授权到期了也得能看到并停下正在跑的编码（后端闸门同样放行 job_list / job_pause / job_terminate）。
  //: 需要「使用权」的模块。服务端处理要花积分、占服务端算力，
  //: 和本机处理的付费功能一样，没有使用权就不该进去。
  const LICENSED_MODULES = ["library", "generate", "version", "server", "mount", "transcode", "toolbox"];

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
      return { title: "还没有使用权", message: "兑换卡密后就能使用剧目库、生成报审包、处理版本、挂载、转码和工具箱。", actionLabel: "兑换卡密" };
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
      { id: "recharge", title: "充值积分", detail: "处理版本和报审包按集扣积分，先充一点再开始。", done: (f.creditBalance ?? 0) > 0, action: "credits", actionLabel: "去充值" },
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

  // ---------- 处理版本与任务队列（M4b-1） ----------
  // 输入是 `job_list` / `job-progress` 的 JobView（src-tauri/src/jobs.rs）。

  const STRENGTHS = [
    { value: "light", label: "轻", detail: "变化最小，画质损失最少" },
    { value: "medium", label: "中", detail: "折中" },
    { value: "heavy", label: "重", detail: "变化最大，画质损失也最多" },
  ];

  const JOB_STATES = {
    queued: { label: "排队中", tone: "" },
    running: { label: "运行中", tone: "ok" },
    paused: { label: "已暂停", tone: "warn" },
    terminated: { label: "已终止", tone: "warn" },
    done: { label: "完成", tone: "ok" },
    failed: { label: "失败", tone: "err" },
  };

  const STEP_STATES = {
    pending: "待处理",
    running: "处理中",
    done: "完成",
    failed: "失败",
    skipped: "已跳过",
  };

  /** 任务状态的说法与色调；认不出的状态原样显示，不假装成别的。 */
  function jobStateLabel(state) {
    return JOB_STATES[state] ?? { label: String(state ?? "未知"), tone: "" };
  }

  function stepStateLabel(status) {
    return STEP_STATES[status] ?? String(status ?? "未知");
  }

  /**
   * 一行任务能点哪些按钮（和 jobs.rs 的状态机一致）：
   * - 暂停：排队中或运行中（运行中点了暂停，后端会做完这一集再停）；
   * - 继续：已暂停、已终止、失败；
   * - 终止：排队中、运行中、已暂停。
   */
  function jobActions(job) {
    const state = job?.state;
    return {
      pause: state === "queued" || state === "running",
      resume: state === "paused" || state === "terminated" || state === "failed",
      terminate: state === "queued" || state === "running" || state === "paused",
    };
  }

  /** 进度百分比（0–100 的整数）；没有步骤时是 0，不画假进度。 */
  function jobPercent(job) {
    const total = Number(job?.total) || 0;
    if (total <= 0) return 0;
    const done = Math.min(Math.max(Number(job?.done) || 0, 0), total);
    return Math.round((done / total) * 100);
  }

  /** 进度一句话：「3/10 · 正在处理 第04集.mp4」。 */
  function jobProgressText(job) {
    const base = `${Number(job?.done) || 0}/${Number(job?.total) || 0}`;
    if (job?.state === "running" && job.current) return `${base} · 正在处理 ${job.current}`;
    if ((job?.state === "paused" || job?.state === "terminated") && job.current) return `${base} · 停在 ${job.current}`;
    return base;
  }

  /** 新事件合进任务列表：同一个 id 替换，新任务放最前面（和 `job_list` 的新到旧顺序一致）。 */
  function mergeJob(jobs, view) {
    if (!view?.id) return jobs.slice();
    const index = jobs.findIndex((job) => job.id === view.id);
    if (index < 0) return [view, ...jobs];
    const next = jobs.slice();
    next[index] = view;
    return next;
  }

  /** 侧栏「任务」入口上的数字：还没结束的任务（排队 + 运行 + 暂停）。 */
  function activeJobCount(jobs) {
    return jobs.filter((job) => job.state === "queued" || job.state === "running" || job.state === "paused").length;
  }

  /**
   * 模板表单的本地校验（和 dossier-core VersionTemplate::validate 的规则一致，后端还会再校验一次）。
   * 返回 `[字段, 说明]` 列表；空列表表示通过。
   */
  // ---------- 擦除画布的区域换算（M4b-2） ----------

  /**
   * 画布坐标 → 原始画面坐标。
   *
   * `view` 是画布上显示的画面尺寸，`source` 是视频的原始宽高（取帧时缩过，所以要换算回来）。
   * 擦除区域必须按原始分辨率提交，否则在缩放过的预览上拖出来的框会偏。
   */
  function toSourceRect(rect, view, source) {
    const sx = source.width / Math.max(1, view.width);
    const sy = source.height / Math.max(1, view.height);
    return {
      x: Math.round(rect.x * sx),
      y: Math.round(rect.y * sy),
      width: Math.round(rect.width * sx),
      height: Math.round(rect.height * sy),
    };
  }

  /** 原始画面坐标 → 画布坐标（显示用）。 */
  function toViewRect(rect, view, source) {
    const sx = Math.max(1, view.width) / source.width;
    const sy = Math.max(1, view.height) / source.height;
    return {
      x: rect.x * sx,
      y: rect.y * sy,
      width: rect.width * sx,
      height: rect.height * sy,
    };
  }

  /** 把两个对角点整理成非负宽高的矩形（拖动方向任意）。 */
  function normalizeRect(from, to) {
    const x = Math.min(from.x, to.x);
    const y = Math.min(from.y, to.y);
    return {
      x,
      y,
      width: Math.abs(to.x - from.x),
      height: Math.abs(to.y - from.y),
    };
  }

  /** 区域是否足够大（太小多半是误点）。低于 4×4 画布像素的框直接丢掉。 */
  function rectIsUsable(rect, minSide) {
    const threshold = minSide ?? 4;
    return rect.width >= threshold && rect.height >= threshold;
  }

  /** 把矩形夹到画面内，并取整到像素。 */
  function clampRect(rect, bounds) {
    const x = Math.max(0, Math.min(Math.round(rect.x), bounds.width));
    const y = Math.max(0, Math.min(Math.round(rect.y), bounds.height));
    return {
      x,
      y,
      width: Math.max(0, Math.min(Math.round(rect.width), bounds.width - x)),
      height: Math.max(0, Math.min(Math.round(rect.height), bounds.height - y)),
    };
  }

  /**
   * 命中测试：画布上的点落在哪个区域上，以及落在框内哪个部位。
   *
   * 返回 `{ index, part }`，`part` 是 `"move"`（框内）或 `"resize"`（右下角把手区），
   * 没命中时返回 `null`。从**后往前**找，和绘制顺序一致（后画的压在上面）。
   */
  function hitTestRegions(point, regions, handleSize) {
    const handle = handleSize ?? 12;
    for (let index = regions.length - 1; index >= 0; index -= 1) {
      const rect = regions[index];
      const inRight = Math.abs(point.x - (rect.x + rect.width)) <= handle;
      const inBottom = Math.abs(point.y - (rect.y + rect.height)) <= handle;
      if (inRight && inBottom) return { index, part: "resize" };
      const inside =
        point.x >= rect.x &&
        point.x <= rect.x + rect.width &&
        point.y >= rect.y &&
        point.y <= rect.y + rect.height;
      if (inside) return { index, part: "move" };
    }
    return null;
  }

  /** 区域数量的说法（给界面用）。 */
  function regionSummary(regions) {
    if (!regions || regions.length === 0) return "区域：0 个";
    return `区域：${regions.length} 个`;
  }

  /**
   * 自动识别回来的候选要不要采纳：太小的框（噪点）直接丢掉。
   * `minSide` 按原始分辨率算，默认 8 像素，和 `dossier-inpaint` 的判据一致。
   */
  function acceptableCandidates(candidates, minSide) {
    const threshold = minSide ?? 8;
    return (candidates || []).filter(
      (candidate) => candidate.width >= threshold && candidate.height >= threshold,
    );
  }

  /** 报审包签章状态的说法（和后端 `SigningStatus` 的取值一一对应）。 */
  function signingLabel(status) {
    switch (status) {
      case "none":
        return "无盖章文档";
      case "placeholder":
        return "占位章（未签署）";
      case "scan":
        return "扫描件签章";
      case "mixed":
        return "混合（部分占位章）";
      default:
        // 认不出的状态原样显示，不假装成别的。
        return String(status ?? "未知");
    }
  }

  // ---------- 客户端配置：公告 / 开关 / 版本策略（M7-3） ----------

  /**
   * 拉不到配置时的安全默认值。
   *
   * 三条都朝「不把人挡在门外」的方向降级，理由各不相同：
   * - 公告空：不显示过期或来路不明的通知；
   * - 开关全开：功能默认可用，服务端挂了不该让所有人变砖；
   * - **不强制更新**：拉不到就把人锁在门外是最糟的降级——用户什么都没做错。
   */
  const DEFAULT_CLIENT_CONFIG = {
    announcements: [],
    flags: {
      signup: true,
      recharge: true,
      server_processing: true,
      version_task: true,
    },
    version: {
      minSupported: "0.0.0",
      latest: "0.0.0",
      downloadUrl: "",
      downloadSha256: "",
      downloadSig: "",
      notes: "",
    },
  };

  /** 把服务端返回的配置补齐成本地可用的形状；缺失的字段一律用安全默认值。 */
  function normalizeClientConfig(raw) {
    const config = raw && typeof raw === "object" ? raw : {};
    const flags = config.flags && typeof config.flags === "object" ? config.flags : {};
    const version = config.version && typeof config.version === "object" ? config.version : {};
    const announcements = Array.isArray(config.announcements) ? config.announcements : [];
    return {
      announcements: announcements
        .filter((item) => item && typeof item.title === "string" && typeof item.body === "string")
        .map((item) => ({
          id: String(item.id ?? ""),
          title: item.title,
          body: item.body,
          level: item.level === "warn" ? "warn" : "info",
        })),
      flags: {
        // 逐个取值而不是整体替换：服务端将来少发一个键时，那个键仍然默认「开」。
        signup: flags.signup !== false,
        recharge: flags.recharge !== false,
        server_processing: flags.server_processing !== false,
        version_task: flags.version_task !== false,
      },
      version: {
        minSupported: String(version.minSupported ?? "0.0.0"),
        latest: String(version.latest ?? "0.0.0"),
        downloadUrl: String(version.downloadUrl ?? ""),
        downloadSha256: String(version.downloadSha256 ?? ""),
        downloadSig: String(version.downloadSig ?? ""),
        notes: String(version.notes ?? ""),
      },
    };
  }

  /** 比较两个「1.2.3」形式的版本号：a<b 返回负数，相等 0，a>b 正数。 */
  function compareVersions(a, b) {
    const parse = (value) =>
      String(value ?? "0")
        .split(".")
        .map((part) => {
          const number = Number.parseInt(part, 10);
          return Number.isFinite(number) && number >= 0 ? number : 0;
        });
    const left = parse(a);
    const right = parse(b);
    const length = Math.max(left.length, right.length);
    for (let index = 0; index < length; index += 1) {
      const diff = (left[index] || 0) - (right[index] || 0);
      if (diff !== 0) return diff;
    }
    return 0;
  }

  /**
   * 当前版本要不要更新。
   *
   * `force` 是「必须更新才能继续用」：低于 minSupported 就是这种情况。
   * 注意 minSupported 为 "0.0.0" 时任何版本都不低于它——这是刻意的，
   * 默认不拦人。
   */
  function updateState(currentVersion, version) {
    const current = String(currentVersion ?? "0.0.0");
    const min = String(version?.minSupported ?? "0.0.0");
    const latest = String(version?.latest ?? "0.0.0");
    if (compareVersions(current, min) < 0) {
      return { force: true, available: true, latest };
    }
    return { force: false, available: compareVersions(current, latest) < 0, latest };
  }

  /**
   * 「这个版本已停止支持」时，某个模块还让不让进。
   *
   * 为什么要有这个函数而不是只靠横幅：横幅只是提示，用户点掉就继续干活了——
   * 那「强制更新」就等于没做。总方案要求「强制更新能拦住旧版」，
   * 所以这里给出**明确的拦截判据**，由界面据此挡在模块外。
   *
   * 放行的只有：首页、设置、账号/授权相关（要有地方看提示、兑换卡密、
   * 下载并安装更新）。其余功能模块一律挡住。
   */
  const FORCE_UPDATE_ALLOWED_MODULES = ["home", "settings"];

  function blockedByVersion(currentVersion, version, moduleName) {
    const verdict = updateState(currentVersion, version);
    if (!verdict.force) return null;
    if (FORCE_UPDATE_ALLOWED_MODULES.includes(moduleName)) return null;
    return {
      title: "这个版本已停止支持",
      message: `当前 ${currentVersion}，最低要求 ${version?.minSupported ?? "0.0.0"}。` +
        "请在这一页下载并安装新版后再继续使用其它功能。",
      latest: verdict.latest,
    };
  }

  // ---------- 服务端处理（M5b-8） ----------

  /** 服务端任务的九个状态 → 人话。状态值来自服务端 tasks.state（大写）。 */
  const SERVER_STATES = {
    PENDING: { label: "已提交", tone: "" },
    QUEUED: { label: "排队中", tone: "" },
    RUNNING: { label: "正在处理", tone: "" },
    VERIFYING: { label: "校验中", tone: "" },
    SUCCEEDED: { label: "已完成", tone: "success" },
    RETRY_WAITING: { label: "等待重试", tone: "warn" },
    CANCEL_REQUESTED: { label: "正在取消", tone: "warn" },
    CANCELLED: { label: "已取消", tone: "" },
    FAILED: { label: "失败", tone: "error" },
  };

  /** 结束状态：不再变化，界面就不用再轮询了。 */
  const SERVER_TERMINAL = new Set(["SUCCEEDED", "CANCELLED", "FAILED"]);

  function serverStateLabel(state) {
    return SERVER_STATES[state] ?? { label: String(state ?? "未知"), tone: "" };
  }

  function serverTaskFinished(task) {
    return SERVER_TERMINAL.has(task?.state);
  }

  /** 服务端任务能做的动作。取消只在还没结束时给——结束后取消没意义。 */
  function serverTaskActions(task) {
    const state = task?.state;
    const finished = SERVER_TERMINAL.has(state);
    return {
      cancel: Boolean(state) && !finished,
      // 产物下载只在成功之后有意义。
      download: state === "SUCCEEDED",
    };
  }

  /**
   * 上传进度百分比（0–100 的整数）。
   * 分母用服务端给的总块数，不用本地估的：本地算错会显示「传完了其实没传完」。
   */
  function uploadPercent(progress) {
    const total = Number(progress?.totalChunks) || 0;
    if (total <= 0) return 0;
    const sent = Math.min(Math.max(Number(progress?.sentChunks) || 0, 0), total);
    return Math.round((sent / total) * 100);
  }

  /** 上传进度一句话：「3/8 块（跳过 3 块已传的）」。 */
  function uploadText(progress) {
    const total = Number(progress?.totalChunks) || 0;
    const sent = Number(progress?.sentChunks) || 0;
    const skipped = Number(progress?.skippedChunks) || 0;
    const base = `${sent}/${total} 块`;
    return skipped > 0 ? `${base}（跳过 ${skipped} 块已传的）` : base;
  }

  /**
   * 这次点「开始上传」该续传还是新开一个会话？
   *
   * 规则很短，但错了代价很大：误判成「新开会话」会让用户从头再传一遍几个 GB，
   * 而「断点续传」就成了空话。所以判据只有一条：**同一个文件**且有未完成的会话。
   * 换了文件（另一集）必须新开——接着往旧会话里传别的文件会拼出一个坏文件。
   */
  function uploadResumePlan(state, sourcePath) {
    if (!state?.uploadId) return { resume: false };
    if (state.assetId) return { resume: false }; // 上一次已经传完了
    if (!sourcePath || state.sourcePath !== sourcePath) return { resume: false };
    return {
      resume: true,
      uploadId: state.uploadId,
      chunkSize: state.chunkSize,
      totalChunks: state.totalChunks,
    };
  }

  function templateProblems(template) {
    const problems = [];
    const unit = (value) => Number.isFinite(value) && value >= 0 && value <= 1;
    if (!(Number.isFinite(template.coverAtSec) && template.coverAtSec >= 0)) {
      problems.push(["coverAtSec", "封面取帧秒数必须是 0 或正数"]);
    }
    if (!(Number.isInteger(template.subtitleSize) && template.subtitleSize > 0)) {
      problems.push(["subtitleSize", "字幕字号必须是大于 0 的整数"]);
    }
    if (!unit(template.watermarkOpacity)) problems.push(["watermarkOpacity", "水印透明度必须在 0 到 1 之间"]);
    if (!unit(template.musicVolume)) problems.push(["musicVolume", "背景音乐音量必须在 0 到 1 之间"]);
    if (template.aiMarked && !/^[\x21\x23-\x5b\x5d-\x7e]+$/.test(template.aiContentProducer ?? "")) {
      problems.push([
        "aiContentProducer",
        "AI 生成合成服务提供者只能用字母、数字和常见英文符号（不能有空格、中文、引号和反斜杠）",
      ]);
    }
    if (template.aiMarked && template.watermarkText && template.watermarkCorner === "bottom_right") {
      problems.push(["watermarkCorner", "勾选了 AI 标识时，剧名水印不能放在右下角（那里是 AI 标识的位置）"]);
    }
    if (template.musicPath && !template.musicLicensed) {
      problems.push(["musicLicensed", "使用背景音乐前要先勾选「我有这段音乐的使用授权」"]);
    }
    if (template.recut && template.recutCount != null && !(Number.isInteger(template.recutCount) && template.recutCount > 0)) {
      problems.push(["recutCount", "重新分集的集数必须是大于 0 的整数（留空表示和原来一样多）"]);
    }
    if (template.recut && template.subtitleDir) {
      problems.push(["subtitleDir", "重新分集后原字幕的时间对不上，不能同时烧字幕；请二选一"]);
    }
    return problems;
  }

  // ---------- 公告 ----------
  //
  // 公告不再硬编码在本地：改由服务端的 `/api/v1/client/config` 提供（M7-3）。
  // 拉不到时显示空公告（见 DEFAULT_CLIENT_CONFIG）——不显示过期或来路不明的通知。

  return {
    CARD_ALPHABET,
    CARD_LENGTH,
    LICENSED_MODULES,
    THEME_KEY,
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
    STRENGTHS,
    jobStateLabel,
    stepStateLabel,
    jobActions,
    jobPercent,
    jobProgressText,
    mergeJob,
    activeJobCount,
    templateProblems,
    toSourceRect,
    toViewRect,
    normalizeRect,
    rectIsUsable,
    clampRect,
    hitTestRegions,
    regionSummary,
    acceptableCandidates,
    signingLabel,
    serverStateLabel,
    serverTaskFinished,
    serverTaskActions,
    uploadPercent,
    uploadText,
    uploadResumePlan,
    DEFAULT_CLIENT_CONFIG,
    normalizeClientConfig,
    compareVersions,
    updateState,
    blockedByVersion,
    FORCE_UPDATE_ALLOWED_MODULES,
  };
});
