const { invoke } = window.__TAURI__?.core ?? {};
const tauriEvent = window.__TAURI__?.event ?? null;

// 与 dossier-core 的 ChecklistRules::default() 保持一致。
const DEFAULT_RULES = {
  minEpisodes: 4,
  minDurationSec: 31,
  maxDurationSec: 480,
  maxBitrateBps: 8_000_000,
  preferPortrait: true,
};

// 检查状态的中文说法：界面上不再出现 pass / warn / fail / pending 这类内部 token。
const STATUS_LABELS = {
  pass: "通过",
  warn: "需确认",
  fail: "不通过",
  pending: "尚未探测",
};

const state = {
  root: "",
  dramas: [],
  selected: null,
  analysis: null,
  module: "home",
  runtime: null,
  // 已保存的设置：探测与生成都读这里，设置页里没保存的改动不生效。
  settings: {},
  // 进行中的长任务标签；非空时不再启动第二个长任务。
  task: null,
  taskLabel: null,
  taskTimer: null,
  taskStartedAt: 0,
  busyButtons: [],
  // 结果区按钮引用的路径（键 → 绝对路径），避免把路径拼进 HTML 属性。
  paths: {},
  // 最近一次账号状态（account_status / account-changed 事件的负载，见 src-tauri/src/account/session.rs）。
  account: null,
  // 最近一次读到的积分余额（M5a）。只用于界面上提前提示，扣费永远以服务端为准。
  creditBalance: null,
  // 最近一次读到的生成流水：首页的引导和近 7 天图表用。
  ledger: [],
  // 本次运行里剪映探测是否找到了草稿目录（引导第 5 步）。
  jianyingFound: false,
  mountStep: 1,
  mountPlan: null,
  publishRows: [],
  // 处理版本（M4b-1）：当前载入的是哪部剧、已保存的模板、权属状态、最近一次预览计划。
  version: {
    drama: null,
    template: null,
    rights: null,
    strength: "medium",
    plan: null,
    planKey: null,
  },
  // 任务列表（job_list / job-progress，新到旧）；展开了「每集情况」的任务 id。
  jobs: [],
  jobsOpen: new Set(),
};

let renamePlan = null;
let bitrateItems = null;
let bitratePending = null;
// 最近一次从配置文件读到的设置：保存时用它带回 ffmpegPath（界面上没有对应输入框）。
let loadedSettings = null;

// 纯函数在 logic.js（可以单独测试），这里只管 DOM。
const L = window.DossierLogic;
const nowSecs = () => Math.floor(Date.now() / 1000);

const $ = (selector) => document.querySelector(selector);
const $$ = (selector) => Array.from(document.querySelectorAll(selector));
const esc = (value) =>
  String(value ?? "").replace(
    /[&<>'"]/g,
    (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", '"': "&quot;" })[c],
  );

/**
 * 弹出提示。`action` 形如 `{ label, run }`，把「下一步该去哪」做成按钮，
 * 避免用户看完一句路径报错后还得自己找设置页。
 */
function toast(message, isError = false, action = null) {
  const node = $("#toast");
  // 颜色之外再给一个文字信号：色觉差异、投屏或截图里也能分辨成功与失败。
  node.textContent = isError ? `出错了：${message}` : message;
  if (action) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "toast-action";
    button.textContent = action.label;
    button.addEventListener("click", action.run);
    node.appendChild(button);
  }
  node.classList.toggle("error", isError);
  node.classList.remove("hidden");
  node.onclick = () => node.classList.add("hidden");
  clearTimeout(toast.timer);
  // 带按钮的提示要留出「读完再点」的时间，不能和纯文字一样 9 秒就消失。
  toast.timer = setTimeout(
    () => node.classList.add("hidden"),
    isError ? (action ? 25000 : 9000) : 4500,
  );
}

/** 跳到「设置 → 本机环境」并重新检测；环境类问题的一键去处。 */
function gotoRuntimeSettings() {
  // 设置是独立视图：打开后直接跳到「本机环境」子页看检测详情。
  switchModule("settings");
  switchToSubAnd("env");
  $("#runtime-list").closest(".panel")?.scrollIntoView({ behavior: "smooth", block: "center" });
  checkRuntime(true);
}

/**
 * 后端错误的统一出口：环境类附「重新检测」、路径类附「打开设置」，
 * 其余保持原样，不让所有错误都被塞进同一个入口。
 */
function reportError(error) {
  // 被功能闸门拒绝：刷新账号状态（界面会切到锁定页或遮罩），再给一个能直接处理的按钮。
  if (error instanceof CommandError && error.code === "license_locked") {
    handleLicenseLocked(error);
    return;
  }
  const message = error instanceof CommandError ? L.errorText(error) : String(error);
  if (/ffmpeg|ffprobe|运行环境/i.test(message)) {
    toast(message, true, { label: "重新检测", run: gotoRuntimeSettings });
    return;
  }
  if (/目录|路径|权限|不存在|磁盘|空间/i.test(message)) {
    toast(message, true, { label: "打开设置", run: () => switchModule("settings") });
    return;
  }
  toast(message, true);
}

// ============ 字段级校验 ============

/** 在字段下方就地提示错误，并把输入框标成 aria-invalid。 */
function setFieldError(selector, message) {
  const field = $(selector);
  if (!field) return;
  const holder = field.closest("label") ?? field.parentElement;
  if (!holder) return;
  let node = holder.querySelector(".field-error");
  if (!message) {
    field.removeAttribute("aria-invalid");
    if (node) node.remove();
    return;
  }
  field.setAttribute("aria-invalid", "true");
  if (!node) {
    node = document.createElement("p");
    node.className = "field-error";
    holder.appendChild(node);
  }
  node.textContent = message;
}

/** 清掉某一组字段的错误提示。 */
function clearFieldErrors(selectors) {
  selectors.forEach((selector) => setFieldError(selector, null));
}

// ============ 结果区：语义色与路径操作 ============

function clearResult(selector) {
  const node = $(selector);
  if (!node) return;
  node.className = "result hidden";
  node.innerHTML = "";
}

/** 登记路径并生成「打开目录 / 复制路径」按钮组。 */
function pathActions(entries) {
  const keys = [];
  entries.forEach(([key, value]) => {
    if (!value) return;
    state.paths[key] = value;
    keys.push(key);
  });
  if (!keys.length) return "";
  const buttons = keys
    .flatMap((key) => [
      `<button class="ghost" type="button" data-open-key="${esc(key)}">打开目录</button>`,
      `<button class="ghost" type="button" data-copy-key="${esc(key)}">复制路径</button>`,
    ])
    .join("");
  return `<div class="result-actions">${buttons}</div>`;
}

/** 统一渲染结果块：tone 决定底色（ok / warn / error），避免失败信息穿成功色。 */
function renderResult(selector, { tone = "ok", html = "", paths = [] } = {}) {
  const node = $(selector);
  node.className = tone === "ok" ? "result" : `result ${tone}`;
  node.innerHTML = html + pathActions(paths);
  return node;
}

async function copyText(text) {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // 落到下面的兜底方案。
  }
  try {
    const area = document.createElement("textarea");
    area.value = text;
    area.setAttribute("readonly", "");
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.appendChild(area);
    area.select();
    const ok = document.execCommand("copy");
    area.remove();
    return ok;
  } catch {
    return false;
  }
}

async function openPath(key) {
  const path = state.paths[key];
  if (!path) return;
  try {
    await call("open_path", { path });
  } catch (error) {
    reportError(error);
  }
}

async function copyPath(key) {
  const path = state.paths[key];
  if (!path) return;
  if (await copyText(path)) {
    toast("路径已复制到剪贴板");
  } else {
    toast(`复制失败，请手动复制：${path}`, true);
  }
}

// ============ 长任务：全局进度与按钮禁用 ============

/** 进度文案始终带上「已用 n 秒」：拿不到总量时，这至少能说明它还在跑。 */
function renderProgressLabel() {
  const seconds = state.taskStartedAt
    ? Math.round((Date.now() - state.taskStartedAt) / 1000)
    : 0;
  const suffix = state.task && seconds >= 1 ? `（已用 ${seconds} 秒）` : "";
  $("#progress-label").textContent = `${state.taskLabel ?? ""}${suffix}`;
}

function setProgress({ label, done = null, total = null }) {
  state.taskLabel = label ?? state.taskLabel;
  const node = $("#progress");
  node.classList.remove("hidden");
  if (done != null && total) {
    node.classList.remove("indeterminate");
    $("#progress-fill").style.width = `${Math.round((done / total) * 100)}%`;
  } else {
    node.classList.add("indeterminate");
    $("#progress-fill").style.width = "";
  }
  renderProgressLabel();
}

function hideProgress() {
  const node = $("#progress");
  node.classList.add("hidden");
  node.classList.remove("indeterminate");
  $("#progress-fill").style.width = "0";
  state.taskLabel = null;
}

/**
 * 开始一个长任务：全局只允许一个，期间禁用所有标了 `data-long-task` 的按钮。
 * 有真实进度就画确定进度条（逐集探测），没有就画不确定进度条 + 计时。
 */
function beginTask(label) {
  if (state.task) {
    toast(`已经有任务在进行（${state.task}），请等它结束后再试。`, true);
    return false;
  }
  state.task = label;
  state.taskStartedAt = Date.now();
  state.busyButtons = $$("[data-long-task]").map((button) => {
    const previous = button.disabled;
    button.disabled = true;
    return [button, previous];
  });
  setProgress({ label });
  state.taskTimer = setInterval(renderProgressLabel, 1000);
  return true;
}

function endTask() {
  state.task = null;
  state.taskStartedAt = 0;
  clearInterval(state.taskTimer);
  state.taskTimer = null;
  state.busyButtons.forEach(([button, previous]) => {
    button.disabled = previous;
  });
  state.busyButtons = [];
  // 任务期间用户可能切换了剧目（旧计划已作废、码率结果已清空），
  // 所以这里按「当前状态」重算这两个按钮，而不是恢复到任务开始前的值。
  syncRenameApplyState();
  syncBitrateFixState();
  syncVersionStartState();
  hideProgress();
}

/** 处理后端推来的逐集探测进度（`probe-progress`）。 */
function handleProbeProgress(payload) {
  // 没有进行中的任务就忽略：进度条只服务于当前这次操作。
  if (!state.task || !payload || !payload.total) return;
  const { task, done, total, fileName } = payload;
  if (done >= total) {
    setProgress({
      label:
        task === "generate"
          ? "探测完成，正在计算分集校验值（视频不上传，只发校验值）"
          : `探测完成（${total} 集），正在汇总检查结果`,
    });
    return;
  }
  setProgress({ label: `正在探测分集 ${done}/${total}：${fileName}`, done, total });
}

/**
 * 处理后端推来的工具箱单元进度（`task-progress`）。
 * 转码/码率修复是「逐个文件跑 ffmpeg」，后端按单元上报，这里直接画确定进度条。
 */
function handleTaskProgress(payload) {
  // 没有进行中的任务就忽略：进度条只服务于当前这次操作。
  if (!state.task || !payload || !payload.total) return;
  const { done, total, label } = payload;
  setProgress({
    label: label ?? `已完成 ${done}/${total}`,
    done: Math.min(Number(done) || 0, total),
    total,
  });
}

/**
 * 擦除的两个阶段（与 `dossier-inpaint` 的 `LOCATE_STAGE` / `INPAINT_STAGE` 一致）。
 * 阶段名只用于显示，进度本身完全由后端上报，界面不猜。
 */
const ERASE_STAGES = {
  locate: "定位字幕区域",
  inpaint: "逐片段修复画面",
};

/**
 * 处理后端推来的擦除进度（`erase-progress`）。
 *
 * 擦除按「阶段 + 已完成帧数」上报（`stageIndex/stageTotal` + `unitDone/unitTotal`），
 * 所以总进度由两段合成：`(阶段序号 + 阶段内比例) / 阶段总数`。
 * 阶段内还没拿到总量时退回不确定进度——宁可显示「不知道」也不显示假百分比。
 *
 * 失败也是终态事件；进度条收起前留下的最后一句话最需要说清结论，
 * 所以失败时把「失败」提到最前面，而不是让它读起来和正常收尾一样。
 */
function handleEraseProgress(payload) {
  if (!state.task || !payload) return;
  const stageTotal = Number(payload.stageTotal) || 0;
  const stageIndex = Number(payload.stageIndex) || 0;
  const done = Number(payload.unitDone) || 0;
  const total = Number(payload.unitTotal) || 0;
  const stageLabel = ERASE_STAGES[payload.stage] ?? payload.stage ?? "处理中";
  if (payload.message && payload.terminal) {
    setProgress({
      label: payload.failed
        ? `擦除失败 · ${stageLabel}：${payload.message}`
        : `${stageLabel} · ${payload.message}`,
    });
    return;
  }
  if (!stageTotal || total <= 0) {
    setProgress({ label: `擦除中 · ${stageLabel}（已处理 ${done} 帧）` });
    return;
  }
  // 用 1000 份的粒度合成总进度，避免浮点百分比出现 0.1% 的抖动。
  const within = Math.min(done / total, 1);
  setProgress({
    label: `擦除中 · ${stageLabel}（第 ${stageIndex + 1}/${stageTotal} 段）：已完成 ${done}/${total} 帧`,
    done: Math.round((stageIndex + within) * 1000),
    total: stageTotal * 1000,
  });
}

/**
 * 后端返回的结构化错误（账号命令的 `{error, message, ...}`、闸门的 `{error: "license_locked", reason, message}`）。
 * `String(error)` 仍然得到中文说明，所以原来按字符串处理错误的地方不用改。
 */
class CommandError extends Error {
  constructor(payload) {
    super(String(payload?.message ?? "操作失败"));
    this.name = "CommandError";
    this.code = String(payload?.error ?? "");
    this.reason = payload?.reason ?? null;
    this.retryAfter = payload?.retryAfter ?? null;
    this.extra = payload?.extra ?? null;
  }

  toString() {
    return this.message;
  }
}

async function call(command, args) {
  if (!invoke) {
    throw new Error("请在 DramaDossier 桌面应用中运行界面。");
  }
  try {
    return await invoke(command, args);
  } catch (error) {
    if (error && typeof error === "object" && !(error instanceof Error) && "error" in error) {
      throw new CommandError(error);
    }
    throw error;
  }
}

function formatBytes(bytes) {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const exponent = Math.min(Math.floor(Math.log2(bytes) / 10), units.length - 1);
  return `${(bytes / 1024 ** exponent).toFixed(exponent ? 1 : 0)} ${units[exponent]}`;
}

/** 读设置表单；`strict` 时逐字段校验并就地提示（不只在 toast 里说一句）。 */
function readRulesForm(strict) {
  const fields = {
    minEpisodes: "#rule-min-episodes",
    minDurationSec: "#rule-min-duration",
    maxDurationSec: "#rule-max-duration",
    maxBitrateMbps: "#rule-max-bitrate",
  };
  const raw = (selector) => {
    const value = $(selector).value.trim();
    return value === "" ? null : Number(value);
  };
  const minEpisodes = raw(fields.minEpisodes) ?? DEFAULT_RULES.minEpisodes;
  const minDurationSec = raw(fields.minDurationSec) ?? DEFAULT_RULES.minDurationSec;
  const maxDurationSec = raw(fields.maxDurationSec) ?? DEFAULT_RULES.maxDurationSec;
  const maxBitrateMbps = raw(fields.maxBitrateMbps);
  const maxBitrateBps =
    maxBitrateMbps === null ? DEFAULT_RULES.maxBitrateBps : Math.round(maxBitrateMbps * 1e6);

  if (strict) {
    const problems = [];
    if (!Number.isInteger(minEpisodes) || minEpisodes <= 0) {
      problems.push([fields.minEpisodes, "最少集数必须是大于 0 的整数。"]);
    }
    if (!(minDurationSec > 0)) {
      problems.push([fields.minDurationSec, "单集时长下限必须是大于 0 的秒数。"]);
    }
    if (!(maxDurationSec > minDurationSec)) {
      problems.push([fields.maxDurationSec, "单集时长上限必须大于下限。"]);
    }
    if (!(maxBitrateBps > 0)) {
      problems.push([fields.maxBitrateMbps, "码率上限必须大于 0。"]);
    }
    Object.values(fields).forEach((selector) => setFieldError(selector, null));
    problems.forEach(([selector, message]) => setFieldError(selector, message));
    if (problems.length) {
      throw new Error(problems[0][1]);
    }
  }

  return {
    minEpisodes,
    minDurationSec,
    maxDurationSec,
    maxBitrateBps,
    preferPortrait: $("#rule-prefer-portrait").checked,
  };
}

/** 生效的检查规则：只认已保存的设置（表单里没保存的改动不算数）。 */
function effectiveRules() {
  return { ...DEFAULT_RULES, ...(state.settings?.rules ?? {}) };
}

/** 探测/生成/工具用的 ffprobe 路径：表单里的值优先，其次已保存的设置（都没有就交给后端查找）。 */
function effectiveFfprobe() {
  const typed = $("#ffprobe-path").value.trim();
  if (typed) return typed;
  const saved = state.settings?.ffprobePath;
  return saved && String(saved).trim() ? String(saved) : null;
}

function fillRules(rules) {
  const merged = { ...DEFAULT_RULES, ...(rules ?? {}) };
  $("#rule-min-episodes").value = merged.minEpisodes;
  $("#rule-min-duration").value = merged.minDurationSec;
  $("#rule-max-duration").value = merged.maxDurationSec;
  $("#rule-max-bitrate").value = Number((merged.maxBitrateBps / 1e6).toFixed(3));
  $("#rule-prefer-portrait").checked = merged.preferPortrait !== false;
}

/**
 * 生成页与工具箱的输出目录都只放占位提示：真正的落点由后端算
 * （`dossier_core::resolve_product_dir`），这里只如实复述那条规则，不参与决策。
 *
 * 三层规则：本次覆盖 → 设置里的「默认输出目录」（产物根目录）下的 `<剧名>/<产物名>`
 * → 剧目目录内的 `<产物名>`。
 */
function renderDefaultHints() {
  const root = state.settings?.outputDir?.trim()
    ? stripPath(state.settings.outputDir.trim())
    : "";
  $("#output-dir").placeholder = root
    ? `留空时生成到 ${root}/<剧名>/报审包`
    : "留空时生成到 剧目目录/报审包";
  $("#exec-output").placeholder = root
    ? `留空时写到 ${root}/<剧名>/微调输出 或 /重组输出`
    : "留空时写到剧目目录/微调输出 或 /重组输出";
  $("#library-root").placeholder = state.settings?.libraryRoot
    ? `上次扫描：${state.settings.libraryRoot}`
    : "选择或输入剧集素材根目录";
}

function settingsSnapshot() {
  let rules;
  try {
    rules = readRulesForm(false);
  } catch {
    rules = null;
  }
  return JSON.stringify({
    rules,
    ffprobe: $("#ffprobe-path").value.trim(),
    output: $("#settings-output").value.trim(),
    font: $("#settings-font").value.trim(),
    software: $("#settings-software-encode").checked,
  });
}

/** 表单与已保存的设置不一致时给出「有改动未保存」提示。 */
function refreshDirtyBadge() {
  const saved = JSON.stringify({
    rules: { ...DEFAULT_RULES, ...(state.settings?.rules ?? {}) },
    ffprobe: state.settings?.ffprobePath ?? "",
    output: state.settings?.outputDir ?? "",
    font: state.settings?.fontPath ?? "",
    software: Boolean(state.settings?.softwareEncodeOnly),
  });
  $("#settings-dirty").classList.toggle("hidden", settingsSnapshot() === saved);
}

async function loadSettings() {
  try {
    const payload = await call("load_app_settings");
    const settings = payload.settings ?? {};
    loadedSettings = settings;
    state.settings = settings;
    $("#settings-path").textContent = payload.path;
    fillRules(settings.rules);
    $("#ffprobe-path").value = settings.ffprobePath ?? "";
    $("#settings-output").value = settings.outputDir ?? "";
    if (settings.libraryRoot) $("#library-root").value = settings.libraryRoot;
    // 新增设置字段：剪映 / AI 模板 / 工作目录（设置独立视图子页）。
    if (settings.jianyingDir) $("#jianying-dir").value = settings.jianyingDir;
    if (settings.jianyingDraftDir)
      $("#jianying-draft-dir").value = settings.jianyingDraftDir;
    if (settings.aiTemplate) $("#ai-template").value = settings.aiTemplate;
    $("#settings-font").value = settings.fontPath ?? "";
    $("#settings-software-encode").checked = Boolean(settings.softwareEncodeOnly);
    if (state.root) $("#env-workdir").value = state.root;
    renderDefaultHints();
    renderEmptyGuide();
    refreshDirtyBadge();
    if (payload.warning) toast(payload.warning, true);
  } catch (error) {
    reportError(error);
  }
}

async function saveSettings() {
  let rules;
  try {
    rules = readRulesForm(true);
  } catch (error) {
    reportError(error);
    return;
  }
  try {
    const settings = {
      rules,
      // ffmpegPath 由「一键配置运行环境」写入，界面无输入框，保存时原样带回避免丢失。
      ffmpegPath: loadedSettings?.ffmpegPath ?? state.settings?.ffmpegPath ?? null,
      ffprobePath: $("#ffprobe-path").value.trim() || null,
      outputDir: $("#settings-output").value.trim() || null,
      libraryRoot: state.settings?.libraryRoot ?? null,
      // 设置独立视图新增的三项。
      aiTemplate: $("#ai-template").value.trim() || null,
      jianyingDir: $("#jianying-dir").value.trim() || null,
      jianyingDraftDir: $("#jianying-draft-dir").value.trim() || null,
      // 处理版本（M4b-1）：中文字体、只用软件编码。
      fontPath: $("#settings-font").value.trim() || null,
      softwareEncodeOnly: $("#settings-software-encode").checked,
    };
    const path = await call("save_app_settings", { settings });
    state.settings = settings;
    loadedSettings = settings;
    $("#settings-path").textContent = path;
    renderDefaultHints();
    refreshDirtyBadge();
    toast("设置已保存到本机配置文件");
    // ffprobe 路径可能变了，重新探一次，头部状态才不会说假话。
    checkRuntime(true);
  } catch (error) {
    reportError(error);
  }
}

/** 记住素材根目录：扫描成功后静默持久化，下次启动直接回填。 */
async function rememberRoot(root) {
  if (!root) return;
  if (state.settings?.libraryRoot === root) return;
  const settings = {
    rules: state.settings?.rules ?? null,
    ffmpegPath: state.settings?.ffmpegPath ?? null,
    ffprobePath: state.settings?.ffprobePath ?? null,
    outputDir: state.settings?.outputDir ?? null,
    // 设置独立视图新增的三项原样带回，避免记目录时把它们抹掉。
    aiTemplate: state.settings?.aiTemplate ?? null,
    jianyingDir: state.settings?.jianyingDir ?? null,
    jianyingDraftDir: state.settings?.jianyingDraftDir ?? null,
    fontPath: state.settings?.fontPath ?? null,
    softwareEncodeOnly: Boolean(state.settings?.softwareEncodeOnly),
    libraryRoot: root,
  };
  state.settings = settings;
  try {
    await call("save_app_settings", { settings });
    renderDefaultHints();
  } catch {
    // 记不住路径不该打断扫描，界面已经拿到了本次结果。
  }
}

function currentRoot() {
  const root = $("#library-root").value.trim();
  if (!root) {
    toast("请先选择剧集素材根目录", true);
    return "";
  }
  state.root = root;
  return root;
}

function renderDramas() {
  $("#drama-count").textContent = state.dramas.length;
  const list = $("#drama-list");
  if (!state.dramas.length) {
    list.innerHTML = '<div class="empty">未找到剧目子目录。</div>';
    renderEmptyGuide();
    return;
  }

  list.innerHTML = state.dramas
    .map(
      (drama, index) => `
        <button class="drama-item ${index === state.selected ? "active" : ""}" data-index="${index}">
          <div class="drama-title">${esc(drama.name)}</div>
          <div class="drama-sub">${drama.episodes.length} 集 · ${formatBytes(
            drama.episodes.reduce((sum, item) => sum + Number(item.sizeBytes || 0), 0),
          )}</div>
        </button>`,
    )
    .join("");

  list.querySelectorAll(".drama-item").forEach((node) => {
    node.addEventListener("click", () => selectDrama(Number(node.dataset.index)));
  });
  renderEmptyGuide();
}

/** 首屏空态：按「选目录 → 扫描 → 点选剧目」三步走，已完成的一步打勾。 */
function renderEmptyGuide() {
  const hasRoot = Boolean($("#library-root").value.trim());
  const hasDramas = state.dramas.length > 0;
  const steps = [
    {
      title: "选择素材根目录",
      detail:
        "一部剧一个子目录，分集视频放子目录里（推荐命名「第01集.mp4」）；封面可叫「封面.jpg」，简介可叫「简介.txt」。",
      done: hasRoot,
      action: "browse",
      actionLabel: "选择目录",
    },
    {
      title: "扫描剧目",
      detail: "扫描后左侧会列出每个剧目与集数，确认素材都被识别到。",
      done: hasDramas,
      action: "scan",
      actionLabel: "扫描剧目",
    },
    {
      title: "点选剧目并「探测并检查」",
      detail: "探测每集的时长、分辨率与码率，确认材料符合报审要求。",
      done: false,
      action: null,
      actionLabel: "",
    },
  ];
  $("#empty-guide").innerHTML = steps
    .map((step, index) => {
      const mark = step.done ? "✓" : String(index + 1);
      const button = step.action
        ? `<button class="ghost" data-guide-action="${step.action}">${step.actionLabel}</button>`
        : "";
      return `<li class="guide-step ${step.done ? "done" : ""}">
        <span class="guide-mark" aria-hidden="true">${mark}</span>
        <div class="guide-body">
          <p class="guide-title">${esc(step.title)}</p>
          <p class="guide-detail">${esc(step.detail)}</p>
          ${button}
        </div>
      </li>`;
    })
    .join("");
}

/// 剧目或目录一变，工具箱里的旧计划一律作废（别让人拿着上一部剧的计划点执行）。
function invalidatePlans() {
  renamePlan = null;
  bitrateItems = null;
  bitratePending = null;
  $("#rename-apply").disabled = true;
  closeDialog($("#bitrate-confirm"), { restoreFocus: false });
  syncBitrateFixState();
}

const TOOL_RESULT_SELECTORS = [
  "#transcode-result",
  "#exec-result",
  "#platform-result",
  "#draft-result",
  "#erase-result",
  "#rename-result",
  "#bitrate-result",
];

/** 切换剧目或重新扫描后清空工具箱结论。 */
function clearToolResults() {
  TOOL_RESULT_SELECTORS.forEach(clearResult);
}

function selectDrama(index) {
  state.selected = index;
  state.analysis = null;
  const drama = state.dramas[index];
  $("#detail-placeholder").classList.add("hidden");
  $("#detail-view").classList.remove("hidden");
  $("#drama-name").textContent = drama.name;
  $("#drama-path").textContent = drama.dir;
  $("#episode-count").textContent = drama.episodes.length;
  $("#ready-badge").textContent = "待检查";
  $("#ready-badge").className = "badge";
  $("#checklist").innerHTML = '<div class="empty">尚未探测分集规格。</div>';
  $("#episode-body").innerHTML =
    '<tr><td colspan="5" class="empty">尚未探测。</td></tr>';
  $("#generate-result").classList.add("hidden");
  // 剧目切换后，工具箱里的旧计划和旧结论全部作废：
  // 否则上一部剧的结论会挂在新剧目的上下文条下面，看起来像当前结果。
  invalidatePlans();
  clearToolResults();
  clearResult("#generate-result");
  resetVersionState();
  renderDramas();
  renderContexts();
  if (state.module === "version") enterVersionModule();
}

// ============ 模块导航 ============

function switchModule(name) {
  // 设置是独立视图：替换整个工作区，对照竞品「← 返回应用」布局。
  if (name === "settings") {
    openSettingsView();
    return;
  }
  closeSettingsView();
  state.module = name;
  document.querySelectorAll(".nav-item").forEach((node) => {
    node.classList.toggle("active", node.dataset.module === name);
  });
  // 没有使用权时，受限模块换成遮罩（真正的拦截在 Rust 端的闸门，这里只是让用户一眼看明白）。
  const lock = L.moduleLock(state.account, name);
  const visible = lock ? "module-locked" : `module-${name}`;
  document.querySelectorAll(".module").forEach((node) => {
    node.classList.toggle("hidden", node.id !== visible);
  });
  if (lock) {
    $("#locked-title").textContent = lock.title;
    $("#locked-message").textContent = lock.message;
    $("#locked-action").textContent = lock.actionLabel;
    return;
  }
  // 转码模块需要运行环境；切进去时按当前状态刷新阻断层。
  if (name === "transcode") renderTranscodeGate();
  if (name === "mount") refreshMountContext();
  if (name === "home") loadHome();
  if (name === "version") enterVersionModule();
  if (name === "server") enterServerModule();
  if (name === "jobs") loadJobs();
}

/** 账号状态变了：当前模块要不要换成遮罩（或从遮罩恢复）跟着重算。只换显示，不重新读数据。 */
function refreshModuleLock() {
  if (!$("#settings-view").classList.contains("hidden")) return;
  const lock = L.moduleLock(state.account, state.module);
  const showingLock = !$("#module-locked").classList.contains("hidden");
  if (Boolean(lock) !== showingLock || lock) switchModule(state.module);
  if (state.module === "home") renderHome();
}

const CONTEXT_SELECTORS = [
  "#generate-context",
  "#version-context",
  "#server-context",
  "#toolbox-context",
  "#transcode-context",
  "#mount-context",
];

function renderContexts() {
  const chip = $("#current-drama");
  const selected = state.selected == null ? null : state.dramas[state.selected];
  if (!selected) {
    chip.classList.add("hidden");
    const emptyText =
      "尚未选择剧目——请先到「剧目库」扫描并点选一部剧。";
    for (const id of CONTEXT_SELECTORS) {
      const node = $(id);
      node.textContent = emptyText;
      node.classList.add("empty");
    }
    refreshMountContext();
    return;
  }
  chip.textContent = `当前剧目：${selected.name}`;
  chip.classList.remove("hidden");
  for (const id of CONTEXT_SELECTORS) {
    const node = $(id);
    node.textContent = `当前剧目：${selected.name}（${selected.episodes.length} 集）`;
    node.classList.remove("empty");
  }
  refreshMountContext();
}

function renderChecklist(report) {
  const ready = report.ready;
  const badge = $("#ready-badge");
  badge.textContent = ready ? "材料可用" : "有缺失项";
  badge.className = `badge ${ready ? "status-ready" : "status-fail"}`;
  $("#checklist").innerHTML = report.items
    .map(
      (item) => `
        <div class="check-item">
          <div class="check-label">
            <strong>${esc(item.label)}</strong>
            <span class="status-${esc(item.status)}">${esc(
              STATUS_LABELS[item.status] ?? item.status,
            )}</span>
          </div>
          <div class="check-detail">${esc(item.detail)}</div>
        </div>`,
    )
    .join("");
}

function renderEpisodes(probes) {
  $("#episode-body").innerHTML = probes
    .map((probe) => {
      const spec = probe.spec;
      const status = probe.error ? "fail" : "pass";
      return `
        <tr>
          <td>${esc(probe.episodeNumber ?? "—")}</td>
          <td>${esc(probe.fileName)}</td>
          <td>${spec ? `${Number(spec.durationSec).toFixed(1)} 秒` : "—"}</td>
          <td>${spec ? `${spec.width}×${spec.height}` : "—"}</td>
          <td class="status-${status}">${probe.error ? esc(probe.error) : "正常"}</td>
        </tr>`;
    })
    .join("");
}

async function scan() {
  const root = currentRoot();
  if (!root) return;
  if (!beginTask("正在扫描剧目")) return;
  try {
    state.dramas = await call("scan_library", { root });
    state.selected = null;
    state.analysis = null;
    $("#detail-view").classList.add("hidden");
    $("#detail-placeholder").classList.remove("hidden");
    invalidatePlans();
    clearToolResults();
    resetVersionState();
    renderDramas();
    renderContexts();
    // 工作目录与剧目库根目录同步（设置-本机环境回显）。
    $("#env-workdir").value = root;
    toast(`已发现 ${state.dramas.length} 个剧目`);
    // 记住素材根目录：下次启动直接回填，不用每次重新找。
    await rememberRoot(root);
  } catch (error) {
    reportError(error);
  } finally {
    endTask();
  }
}

/** 探测失败时把「缺 ffprobe」这类环境问题说清楚，而不是只抛原始错误。 */
function reportProbeFailure(error) {
  const message = String(error);
  const missing = state.runtime
    ? [state.runtime.ffprobe, state.runtime.ffmpeg]
        .filter((tool) => tool && !tool.found)
        .map((tool) => tool.name)
    : [];
  if (missing.length && /ffprobe|ffmpeg|启动|PATH/i.test(message)) {
    toast(
      `${message}（本机缺少 ${missing.join("、")}：请安装 ffmpeg，或在「设置 → 检查规则与路径」里指定 ffprobe 路径）`,
      true,
      { label: "去设置环境", run: gotoRuntimeSettings },
    );
    return;
  }
  reportError(error);
}

async function analyze() {
  if (state.selected == null) return;
  const drama = state.dramas[state.selected];
  if (!beginTask(`正在探测《${drama.name}》的分集规格`)) return;
  try {
    state.analysis = await call("analyze_drama", {
      root: state.root,
      name: drama.name,
      ffprobePath: effectiveFfprobe(),
      rules: effectiveRules(),
    });
    renderChecklist(state.analysis.checklist);
    renderEpisodes(state.analysis.probes);
    toast(state.analysis.checklist.ready ? "检查完成，材料可用" : "检查完成，请处理缺失或警告项");
  } catch (error) {
    reportProbeFailure(error);
  } finally {
    endTask();
  }
}

/** 成本表单校验：数字范围错了就在字段旁标红，不再只弹一句 toast。 */
function validateCostForm() {
  clearFieldErrors(["#total-investment", "#actor-ratio"]);
  const investmentText = $("#total-investment").value.trim();
  const ratioText = $("#actor-ratio").value.trim();
  const investment = investmentText === "" ? null : Number(investmentText);
  const ratio = ratioText === "" ? null : Number(ratioText);
  const problems = [];
  // 上限和 dossier-core 的 validate_request 一致（1e9 万元），免得本地放过、生成时才报错。
  if (investment !== null && !(investment >= 0 && investment <= 1e9)) {
    problems.push(["#total-investment", "投资总额（万元）必须在 0 到 1,000,000,000 之间。"]);
  }
  if (ratio !== null && !(ratio >= 0 && ratio <= 100)) {
    problems.push(["#actor-ratio", "演员成本占比必须在 0–100 之间。"]);
  }
  problems.forEach(([selector, message]) => setFieldError(selector, message));
  return problems.length ? null : { investment, ratio };
}

async function generate() {
  if (state.selected == null) return;
  const drama = state.dramas[state.selected];
  const cost = validateCostForm();
  if (!cost) {
    toast("请先修正表单里标红的字段", true);
    return;
  }
  // 生成报审包要扣积分（按集算）。先告诉用户要花多少、失败会退多少。
  const episodes = drama.episodes ? drama.episodes.length : 0;
  if (episodes > 0) {
    const ok = await confirmDialog({
      title: "生成报审包",
      body:
        `《${drama.name}》共 ${episodes} 集，按每集 5 积分计算，将消耗 ${episodes * 5} 积分。\n` +
        "生成由服务端完成，所以如果生成失败，这些积分会自动退还。",
      confirmLabel: "生成",
    });
    if (!ok) return;
  }
  if (!beginTask(`正在探测并生成《${drama.name}》的报审包`)) return;
  try {
    const result = await call("generate_package", {
      root: state.root,
      name: drama.name,
      outputDir: $("#output-dir").value.trim() || null,
      outputRoot: state.settings?.outputDir ?? null,
      copyEpisodes: $("#copy-episodes").checked,
      aiGenerated: $("#ai-generated").checked,
      ffprobePath: effectiveFfprobe(),
      rules: effectiveRules(),
      costSheetScan: $("#scan-cost-sheet").value.trim() || null,
      ownershipScan: $("#scan-ownership").value.trim() || null,
      costSheet: {
        companyName: $("#company-name").value.trim(),
        totalInvestmentWan: cost.investment,
        actorRatioPercent: cost.ratio,
        contact: $("#contact").value.trim(),
      },
    });
    // 逐份标明签章方式：混合签章时整个包不能只写一个结论，
    // 否则使用者会以为「有扫描件 = 整包都盖好了」。
    const signers = Array.isArray(result.documentSigners) ? result.documentSigners : [];
    const documents = signers.length
      ? `<br />盖章文档：<ul class="signer-list">${signers
          .map((item) => {
            const scanned = item.signer === "scan";
            return `<li class="${scanned ? "status-pass" : "status-fail"}">${esc(
              item.document,
            )} — ${scanned ? "扫描件签章" : "<strong>占位章 · 未签署 · 无法律效力</strong>"}</li>`;
          })
          .join("")}</ul>`
      : "";
    // 没有逐份信息时（旧版服务端）退回原来的整包提示。
    const unsigned =
      signers.length === 0 && result.placeholder
        ? `<br /><strong>注意：盖的是占位章，未签署，无法律效力。</strong>正式报审前请换成加盖公章的版本：
        ${result.unsignedDocuments.map((path) => esc(path)).join("、")}`
        : "";
    const stillUnsigned = signers.filter((item) => item.signer !== "scan").length;
    renderResult("#generate-result", {
      tone: result.placeholder ? "warn" : "ok",
      html: `<strong>报审包已生成</strong><br />
        位置：${esc(result.packageDir)}<br />
        文件数：${result.fileCount} · 总大小：${formatBytes(Number(result.totalBytes))}${documents}${unsigned}${
          stillUnsigned > 0
            ? `<br /><strong>还有 ${stillUnsigned} 份是占位章，正式报审前要换成已盖章的版本。</strong>`
            : ""
        }`,
      paths: [["package", result.packageDir]],
    });
    toast(
      result.placeholder
        ? "报审包已生成（仍有占位章，未签署的部分无法律效力）"
        : "报审包生成完成",
    );
    // 流水多了一条：首页的引导和近 7 天图表跟着更新。
    loadHome();
  } catch (error) {
    const message = error instanceof CommandError ? L.errorText(error) : String(error);
    // 「目录已存在」是可控的：后端没有动任何文件，把旧包位置摆出来，
    // 直接给「打开旧包 / 换个目录」两条路，而不是让人自己去找那个路径。
    if (/已存在/.test(message)) {
      const existing = effectivePackageDir();
      renderResult("#generate-result", {
        tone: "error",
        html: `<strong>报审包目录已存在，本次未生成任何文件</strong><br />
          ${esc(message)}<br />
          旧目录不会被改动：可以先打开确认，也可以换一个输出目录后重试。`,
        paths: existing ? [["package", existing]] : [],
      });
      toast("报审包目录已存在：已标出旧包位置", true, {
        label: "换个输出目录",
        run: () => browse("#output-dir"),
      });
    } else {
      renderResult("#generate-result", {
        tone: "error",
        html: `<strong>生成失败</strong><br />${esc(message)}`,
      });
      reportError(error);
    }
  } finally {
    endTask();
  }
}

function selectedDrama() {
  if (state.selected == null) throw new Error("请先在左侧选择一部剧");
  return state.dramas[state.selected];
}

/**
 * 本次生成的报审包目录，与后端 `dossier_core::resolve_product_dir` 的三层规则保持一致：
 * 本次覆盖 → 设置里的「默认输出目录」/剧名/报审包 → 剧目目录/报审包。
 * 用在「目录已存在」的提示里，避免把用户指到一个错误的位置。
 */
function effectivePackageDir() {
  const override = $("#output-dir").value.trim();
  if (override) return stripPath(override);
  const drama = state.selected == null ? null : state.dramas[state.selected];
  if (!drama) return "";
  const root = state.settings?.outputDir?.trim();
  // 后端会用剧名拼一层；这里只做展示，剧名原样带入即可（后端会再净化一次）。
  if (root) return `${stripPath(root)}/${drama.name}/报审包`;
  return `${stripPath(drama.dir)}/报审包`;
}

function transcodeTarget() {
  const number = (selector, fallback) => {
    const value = $(selector).value.trim();
    return value === "" ? fallback : Number(value);
  };
  return {
    container: $("#transcode-container").value.trim() || "mp4",
    maxBitrateBps: Math.round(number("#transcode-bitrate", 8) * 1e6),
    maxWidth: number("#transcode-width", 1080),
    maxHeight: number("#transcode-height", 1920),
  };
}

async function transcodeAdvice() {
  if (!beginTask("正在生成转码建议（需要逐集探测）")) return;
  try {
    const drama = selectedDrama();
    const payload = await call("transcode_advice", {
      root: state.root,
      name: drama.name,
      target: transcodeTarget(),
      ffprobePath: effectiveFfprobe(),
    });
    const plan = payload.plan;
    const rows = plan.episodes
      .filter((episode) => episode.needsTranscode)
      .map(
        (episode) => `<tr>
          <td>${esc(episode.episodeNumber ?? "?")}</td>
          <td>${esc(episode.fileName)}</td>
          <td>${esc(episode.reasons.join("；"))}</td>
        </tr>`,
      )
      .join("");
    renderResult("#transcode-result", {
      tone: plan.transcodeCount ? "warn" : "ok",
      html: `
      <strong>共 ${plan.episodes.length} 集：需转码 ${plan.transcodeCount} 集，已合规 ${plan.compliantCount} 集</strong><br />
      预计输出目录：${esc(plan.outputDir)}<br />
      ${
        rows
          ? `<table><thead><tr><th>集号</th><th>文件</th><th>原因</th></tr></thead><tbody>${rows}</tbody></table>`
          : "<p>所有分集都符合目标规格。</p>"
      }
      ${plan.transcodeCount ? `<details><summary>批量脚本预览</summary><pre>${esc(payload.script)}</pre></details>` : ""}`,
      paths: [["transcode-output", plan.outputDir]],
    });
  } catch (error) {
    reportError(error);
  } finally {
    endTask();
  }
}

async function diffReportPackages() {
  const left = $("#diff-left").value.trim();
  const right = $("#diff-right").value.trim();
  setFieldError("#diff-left", left ? null : "请选择改稿前的报审包目录（目录里要有 manifest.json）。");
  setFieldError("#diff-right", right ? null : "请选择改稿后的报审包目录（目录里要有 manifest.json）。");
  if (!left || !right) {
    toast("请先选择要对比的两份报审包目录", true);
    return;
  }
  try {
    const result = await call("diff_report_packages", { leftDir: left, rightDir: right });
    const rows = result.files
      .map(
        (file) => `<tr>
          <td>${esc(file.path)}</td>
          <td>${esc(file.kind)}</td>
          <td>${Number(file.sizeDeltaBytes) >= 0 ? "+" : ""}${file.sizeDeltaBytes}</td>
          <td>${esc(file.detail)}</td>
        </tr>`,
      )
      .join("");
    renderResult("#diff-result", {
      tone: result.files.length ? "warn" : "ok",
      html: `
      <strong>${esc(result.summary)}</strong><br />
      ${esc(result.leftGeneratedAt)} → ${esc(result.rightGeneratedAt)}<br />
      签章：左侧「${esc(L.signingLabel(result.leftSigning))}」 · 右侧「${esc(
        L.signingLabel(result.rightSigning),
      )}」${
        result.leftSigning !== result.rightSigning
          ? "<br /><strong>两侧签章方式不同，对比时不要只看文件是否一致。</strong>"
          : ""
      }<br />
      ${
        rows
          ? `<table><thead><tr><th>路径</th><th>变化</th><th>字节</th><th>说明</th></tr></thead><tbody>${rows}</tbody></table>`
          : "<p>两份报审包内容一致。</p>"
      }`,
    });
  } catch (error) {
    reportError(error);
  }
}

async function loadPlatformProfiles() {
  try {
    const profiles = await call("platform_profiles");
    const select = $("#platform-select");
    select.innerHTML = profiles
      .map((profile) => `<option value="${esc(profile.id)}">${esc(profile.name)}</option>`)
      .join("");
  } catch (error) {
    reportError(error);
  }
}

async function platformReview() {
  if (!beginTask("正在按平台档案逐集对照")) return;
  try {
    const drama = selectedDrama();
    const review = await call("platform_review", {
      root: state.root,
      name: drama.name,
      profileId: $("#platform-select").value,
      ffprobePath: effectiveFfprobe(),
    });
    const body = review.episodes
      .map((episode) => {
        const failed = episode.checks.filter((check) => check.status !== "pass");
        const detail = failed.length
          ? failed.map((check) => `${check.label}：${check.detail}`).join("；")
          : "全部通过";
        return `<tr>
          <td>${esc(episode.episodeNumber ?? "?")}</td>
          <td>${esc(episode.fileName)}</td>
          <td>${esc(detail)}</td>
        </tr>`;
      })
      .join("");
    renderResult("#platform-result", {
      tone: review.failCount ? "error" : review.warnCount ? "warn" : "ok",
      html: `
      <strong>${esc(review.profileName)}：${review.clean ? "无阻断项" : `${review.failCount} 项阻断`}，${review.warnCount} 项警告</strong><br />
      <table><thead><tr><th>集号</th><th>文件</th><th>需关注</th></tr></thead><tbody>${body}</tbody></table>
      ${review.notes.length ? `<p>${esc(review.notes.join("；"))}</p>` : ""}`,
    });
  } catch (error) {
    reportError(error);
  } finally {
    endTask();
  }
}

async function exportDraft() {
  if (!beginTask("正在导出剪映草稿")) return;
  try {
    const drama = selectedDrama();
    const result = await call("export_draft", {
      root: state.root,
      name: drama.name,
      options: { outputDir: $("#draft-dir").value.trim() || null },
      ffprobePath: effectiveFfprobe(),
    });
    renderResult("#draft-result", {
      tone: "ok",
      html: `
      <strong>草稿已导出</strong><br />
      目录：${esc(result.draftDir)}<br />
      片段数：${result.segmentCount} · 总时长：${Number(result.durationSec).toFixed(1)} 秒`,
      paths: [["draft", result.draftDir]],
    });
    $("#draft-dir").value = result.draftDir;
  } catch (error) {
    reportError(error);
  } finally {
    endTask();
  }
}

async function importDraft() {
  const draftDir = $("#draft-dir").value.trim();
  setFieldError("#draft-dir", draftDir ? null : "请选择要读回的草稿目录。");
  if (!draftDir) {
    toast("请先选择草稿目录", true);
    return;
  }
  if (!beginTask("正在读取剪映草稿")) return;
  try {
    const draft = await call("import_draft", { draftDir });
    const rows = draft.segments
      .map(
        (segment) => `<tr>
          <td>${segment.order}</td>
          <td>${esc(segment.name)}</td>
          <td>${Number(segment.startSec).toFixed(2)}s</td>
          <td>${Number(segment.durationSec).toFixed(2)}s</td>
        </tr>`,
      )
      .join("");
    renderResult("#draft-result", {
      tone: "ok",
      html: `
      <strong>草稿：${esc(draft.draftName || "（未命名）")}</strong> · ${draft.width}×${draft.height} @ ${draft.fps}fps<br />
      总时长：${Number(draft.durationSec).toFixed(1)} 秒<br />
      <table><thead><tr><th>#</th><th>素材</th><th>时间线起点</th><th>时长</th></tr></thead><tbody>${rows}</tbody></table>`,
    });
  } catch (error) {
    reportError(error);
  } finally {
    endTask();
  }
}

async function generatePublishCopy() {
  try {
    const path = $("#publish-copy-path").value.trim();
    if (!path) throw new Error("请填写挂载文案 CSV 输出路径。");
    const drama = selectedDrama();
    const result = await call("generate_mount_copy_csv", {
      root: state.root,
      name: drama.name,
      outputPath: path,
      ffprobePath: $("#ffprobe-path").value.trim() || null,
    });
    $("#publish-copy-path").value = result.path;
    state.publishRows = await call("load_mount_copy", { path: result.path });
    renderPublishCopy(`<strong>模板已生成</strong><br />行数：${result.rowCount} · SHA-256：<code>${esc(result.sha256)}</code>`);
  } catch (error) {
    reportError(error);
  }
}

async function loadPublishCopy() {
  try {
    const path = $("#publish-copy-path").value.trim();
    if (!path) throw new Error("请填写挂载文案 CSV 路径。");
    state.publishRows = await call("load_mount_copy", { path });
    renderPublishCopy();
  } catch (error) {
    reportError(error);
  }
}

function renderPublishCopy(message = "") {
  const rows = state.publishRows;
  const body = rows.map((row, index) => `
    <tr>
      <td>${esc(row.episodeNumber ?? "—")}</td>
      <td>${esc(row.sourceFile)}</td>
      <td><input data-publish-index="${index}" data-publish-field="shortTitle" value="${esc(row.shortTitle)}" /></td>
      <td><textarea data-publish-index="${index}" data-publish-field="description">${esc(row.description)}</textarea></td>
      <td><textarea data-publish-index="${index}" data-publish-field="mountCopy">${esc(row.mountCopy)}</textarea></td>
      <td><input data-publish-index="${index}" data-publish-field="dramaSearchName" value="${esc(row.dramaSearchName)}" /></td>
      <td><select data-publish-index="${index}" data-publish-field="status">
        ${["draft", "ready", "submitted", "confirmed", "failed"].map((status) => `<option value="${status}" ${row.status === status ? "selected" : ""}>${status}</option>`).join("")}
      </select></td>
      <td><input data-publish-index="${index}" data-publish-field="resultNote" value="${esc(row.resultNote)}" /></td>
    </tr>`).join("");
  $("#publish-copy-result").innerHTML = `
    ${message}
    <strong>文案表：${rows.length} 行</strong>
    <div class="table-scroll"><table><thead><tr><th>集号</th><th>成片</th><th>短标题</th><th>描述</th><th>挂载文案</th><th>剧集检索名</th><th>本地状态</th><th>结果备注</th></tr></thead><tbody>${body}</tbody></table></div>
    <div class="option-row"><button id="publish-copy-save" class="success">保存 CSV</button></div>
    <p class="hint">状态只用于本地学习记录；本项目没有外部平台发布器。</p>`;
  $("#publish-copy-result").classList.remove("hidden");
  $("#publish-copy-result").querySelectorAll("[data-publish-index]").forEach((input) => {
    input.addEventListener("input", () => {
      const index = Number(input.dataset.publishIndex);
      state.publishRows[index][input.dataset.publishField] = input.value;
    });
    input.addEventListener("change", () => {
      const index = Number(input.dataset.publishIndex);
      state.publishRows[index][input.dataset.publishField] = input.value;
    });
  });
  $("#publish-copy-save").addEventListener("click", savePublishCopy);
}

async function previewPublishCopy() {
  try {
    const path = $("#publish-copy-path").value.trim();
    if (!path) throw new Error("请先填写并读取挂载文案 CSV 路径。");
    const drama = selectedDrama();
    const preview = await call("preview_mount_copy_publish", {
      path,
      batchId: `${Date.now()}`,
      dramaName: drama.name,
    });
    const rows = preview.rows.map((row) => `<tr>
      <td>${esc(row.sourceFile)}</td><td>${esc(row.status)}</td>
      <td>${row.eligible ? "满足" : "待补齐"}</td><td>${esc(row.reason)}</td>
    </tr>`).join("");
    const result = `<h4>本地 dry-run：${preview.canSubmit ? "前置条件满足" : "存在待处理项"}</h4>
      <p>可提交 ${preview.eligibleCount}/${preview.rowCount}；实际提交：否（wouldSubmit=false）。</p>
      <div class="table-scroll"><table><thead><tr><th>成片</th><th>状态</th><th>校验</th><th>原因</th></tr></thead><tbody>${rows}</tbody></table></div>`;
    renderPublishCopy(result);
  } catch (error) {
    reportError(error);
  }
}

async function savePublishCopy() {
  try {
    const path = $("#publish-copy-path").value.trim();
    if (!path) throw new Error("请填写挂载文案 CSV 路径。");
    const result = await call("save_mount_copy", { path, rows: state.publishRows });
    toast(`已保存 ${result.rowCount} 行文案，SHA-256 ${result.sha256.slice(0, 12)}…`);
  } catch (error) {
    reportError(error);
  }
}

// ============ 工具箱：区域擦除（M4b-2） ============
//
// 工作流：取一帧 → 自动识别候选区域 → 在画布上拖动微调 → 左右对比 → 勾选权利确认 → 批量。
// 坐标一律在「原始分辨率」下保存，画布上只做换算（换算规则在 logic.js，有单测）。

/** 擦除画布的状态。 */
const eraseState = {
  /** 取帧对应的分集路径（批量时取的是同一集的同一秒）。 */
  framePath: null,
  /** 原始画面尺寸（取帧时由 `img.naturalWidth/Height` 反推）。 */
  sourceSize: { width: 0, height: 0 },
  /** 画布上显示的画面尺寸。 */
  viewSize: { width: 0, height: 0 },
  /** 原始分辨率下的区域。 */
  regions: [],
  /** 正在拖的状态：`{ mode, index, from, original }`。 */
  drag: null,
};

const ERASE_FRAME_MAX_WIDTH = 720;

/** 当前剧的第 1 集（取帧预览用）。 */
function eraseEpisodeList() {
  const drama = selectedDrama();
  return drama && drama.episodes ? drama.episodes : [];
}

/**
 * 画布尺寸：**像素缓冲**按显示尺寸算，**显示尺寸**交给 CSS（按栏宽自适应）。
 *
 * 早先这里把 width/height 都写成像素、还用 inline style 固定住，结果是两栏并排时
 * 每栏只有两百来像素宽，画布被挤成细长条，框虽然画了却挤得看不清。
 * 现在只定缓冲尺寸（canvas.width/height 决定绘制坐标系），显示由 CSS 的
 * `width:100%` + `aspect-ratio` 决定；换算用 getBoundingClientRect 读实际显示尺寸。
 */
function syncCanvasSize(sourceSize, viewSize) {
  const canvas = $("#erase-canvas");
  // 缓冲尺寸按「显示尺寸 × 设备像素比」定，保证清晰度；坐标换算另外用 CSS 尺寸做。
  const dpr = Math.min(window.devicePixelRatio || 1, 2);
  canvas.width = Math.max(1, Math.round(viewSize.width * dpr));
  canvas.height = Math.max(1, Math.round(viewSize.height * dpr));
  canvas.style.aspectRatio = `${viewSize.width} / ${viewSize.height}`;
  // 清掉旧版本留下的 inline 像素宽高（否则会盖住 CSS 的自适应）。
  canvas.style.width = "";
  canvas.style.height = "";
  const frame = $("#erase-frame");
  frame.style.width = "";
  frame.style.height = "";
  const wrap = $("#erase-canvas-wrap");
  wrap.style.width = "";
  wrap.style.height = "";
  wrap.classList.add("has-frame");
  // 记一下原始与显示尺寸的对应关系，拖动时要用。
  eraseState.sourceSize = sourceSize;
  eraseState.viewSize = viewSize;
}

/** 重画画布上的区域框。 */
function drawEraseRegions() {
  const canvas = $("#erase-canvas");
  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  const { viewSize, sourceSize } = eraseState;
  // 显示尺寸（CSS 决定）与缓冲尺寸（DPR 决定）可能不同：绘制前把坐标整体缩放过去。
  const rect = canvas.getBoundingClientRect();
  const scaleX = rect.width > 0 ? canvas.width / rect.width : 1;
  const scaleY = rect.height > 0 ? canvas.height / rect.height : 1;
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  if (!viewSize.width || !sourceSize.width) return;
  ctx.setTransform(scaleX, 0, 0, scaleY, 0, 0);
  eraseState.regions.forEach((region, index) => {
    const box = L.toViewRect(region, viewSize, sourceSize);
    ctx.strokeStyle = "#5b8cff";
    ctx.lineWidth = 2;
    ctx.strokeRect(box.x, box.y, box.width, box.height);
    // 右下角的缩放把手。
    ctx.fillStyle = "#5b8cff";
    ctx.fillRect(box.x + box.width - 6, box.y + box.height - 6, 8, 8);
    // 序号：多框时便于对照列表。
    ctx.fillStyle = "#e8edf5";
    ctx.font = "14px sans-serif";
    ctx.fillText(String(index + 1), box.x + 4, Math.max(14, box.y - 4));
  });
  ctx.setTransform(1, 0, 0, 1, 0, 0);
  $("#erase-region-summary").textContent = L.regionSummary(eraseState.regions);
}

/** 画布上的鼠标点 → 画布坐标。 */
function canvasPoint(event) {
  const canvas = $("#erase-canvas");
  const rect = canvas.getBoundingClientRect();
  return { x: event.clientX - rect.left, y: event.clientY - rect.top };
}

/** 取一帧：按当前剧第 1 集的指定秒数。 */
async function loadEraseFrame() {
  const drama = selectedDrama();
  if (!drama) {
    toast("请先在剧目库里选一部剧", true);
    return;
  }
  const episodes = eraseEpisodeList();
  if (episodes.length === 0) {
    toast("这部剧没有分集", true);
    return;
  }
  const seconds = Number($("#erase-frame-sec").value || 0);
  if (!beginTask("正在取帧")) return;
  try {
    const first = episodes[0];
    const dataUrl = await call("video_frame", {
      path: first.path,
      seconds,
      maxWidth: ERASE_FRAME_MAX_WIDTH,
    });
    const image = $("#erase-frame");
    await new Promise((resolve, reject) => {
      image.onload = resolve;
      image.onerror = () => reject(new Error("取到的帧无法显示"));
      image.src = dataUrl;
    });
    image.classList.remove("hidden");
    $("#erase-empty").classList.add("hidden");
    const viewWidth = image.naturalWidth;
    const viewHeight = image.naturalHeight;
    // 反推原始尺寸：预览是等比缩到 maxWidth 的，高度按原始比例算过。
    const source = probeSourceSize(first.path);
    if (!source) {
      // 拿不到原始分辨率就不能往下走：区域会按预览坐标提交，擦的位置会偏。
      // 明确让用户先去探测，而不是给出一个「看起来能跑但擦错地方」的结果。
      toast("还没有这部剧的分集规格，请先到「剧目库」点「探测并检查」再取帧", true);
      return;
    }
    syncCanvasSize(source, { width: viewWidth, height: viewHeight });
    eraseState.framePath = first.path;
    eraseState.regions = [];
    drawEraseRegions();
    toast(`已取 ${seconds} 秒处的画面`);
  } catch (error) {
    reportError(error);
  } finally {
    endTask();
  }
}

/**
 * 取帧时 ffmpeg 缩到 720 宽，这里要还原原始宽高。
 *
 * 探测结果（`state.analysis.probes`）里带着每集的规格，按**文件名**对上那一集。
 * 拿不到时返回 null：调用方会退回「原始 = 预览」（换算比例 1），区域就会按预览坐标提交——
 * 那样在原始分辨率不是 720 宽时会偏，所以要尽量拿到真值。
 */
function probeSourceSize(path) {
  const probes = state.analysis && state.analysis.probes;
  if (!Array.isArray(probes)) return null;
  const name = path.split(/[\\/]/).pop();
  const probe = probes.find((item) => item && item.fileName === name);
  const spec = probe && probe.spec;
  if (spec && spec.width && spec.height) {
    return { width: Number(spec.width), height: Number(spec.height) };
  }
  return null;
}

/** 自动识别静止区域，填进画布（由人微调）。 */
async function detectEraseRegions() {
  const drama = selectedDrama();
  if (!drama) {
    toast("请先在剧目库里选一部剧", true);
    return;
  }
  const episodes = eraseEpisodeList();
  if (episodes.length === 0) {
    toast("这部剧没有分集", true);
    return;
  }
  const samples = Number($("#erase-samples").value || 30);
  if (!beginTask("正在自动识别静止区域")) return;
  try {
    const report = await call("detect_still_regions", {
      path: episodes[0].path,
      samples,
      toolPath: $("#erase-tool-path").value.trim() || null,
    });
    const candidates = L.acceptableCandidates(report.candidates);
    if (candidates.length === 0) {
      toast("没有找到「位置固定、内容不变」的区域；可以手动在画面上拖框", true);
      eraseState.regions = [];
    } else {
      eraseState.regions = candidates.map((item) => ({
        x: item.x,
        y: item.y,
        width: item.width,
        height: item.height,
      }));
      toast(`识别到 ${candidates.length} 个候选区域，可在画面上拖动微调`);
    }
    // 只更新「原始分辨率」这一半：画布的**显示尺寸**必须保持取帧时算好的值。
    // 这里如果拿 report 尺寸当显示尺寸传进去，画布会被撑到原始分辨率，
    // 画面和框都会跑到可见区域外面（框其实画了，只是看不见）。
    if (report.width && report.height) {
      eraseState.sourceSize = { width: report.width, height: report.height };
      const view = eraseState.viewSize;
      // 还没取过帧（viewSize 为空）时，才按 report 尺寸把画布建起来。
      if (!view.width || !view.height) {
        syncCanvasSize(
          { width: report.width, height: report.height },
          { width: report.width, height: report.height },
        );
      }
    }
    drawEraseRegions();
  } catch (error) {
    reportError(error);
  } finally {
    endTask();
  }
}

/** 先做一小段擦除，再取同一时刻的帧，左右对比。 */
async function previewErase() {
  if (eraseState.regions.length === 0) {
    toast("先识别或拖出至少一个区域", true);
    return;
  }
  if (!eraseState.framePath) {
    toast("先取一帧再预览对比", true);
    return;
  }
  const seconds = Number($("#erase-frame-sec").value || 0);
  const start = Math.max(0, seconds - 2);
  const duration = 4;
  if (!beginTask("正在做小段擦除对比（只处理 4 秒）")) return;
  try {
    const preview = await call("erase_preview", {
      input: eraseState.framePath,
      seconds,
      startSec: start,
      durationSec: duration,
      regions: eraseState.regions,
      toolPath: $("#erase-tool-path").value.trim() || null,
    });
    const image = $("#erase-preview");
    image.src = preview;
    image.classList.remove("hidden");
    $("#erase-preview-empty").classList.add("hidden");
    toast("已生成对比帧（右图是擦除后）");
  } catch (error) {
    reportError(error);
  } finally {
    endTask();
  }
}

/** 批量擦除当前剧的所有集（走任务队列）。 */
async function startEraseBatch() {
  const drama = selectedDrama();
  if (!drama) {
    toast("请先在剧目库里选一部剧", true);
    return;
  }
  if (eraseState.regions.length === 0) {
    toast("先识别或拖出至少一个区域", true);
    return;
  }
  if (!$("#erase-rights").checked) {
    toast("开始批量擦除前必须勾选「我确认对该素材拥有权利」", true);
    return;
  }
  const episodes = eraseEpisodeList().map((item) => item.fileName);
  if (episodes.length === 0) {
    toast("这部剧没有分集", true);
    return;
  }
  try {
    const view = await call("erase_batch_start", {
      request: {
        dramaName: drama.name,
        dramaDir: drama.dir,
        outputDir: eraseOutputDir(drama),
        episodes,
        regions: eraseState.regions,
        rightsConfirmed: true,
        rightsSha256: null,
        toolPath: $("#erase-tool-path").value.trim() || null,
      },
    });
    const node = $("#erase-job-result");
    node.innerHTML = `
      <strong>已加入任务队列</strong><br />
      任务：${esc(view.title)}<br />
      输出目录：${esc(view.outputDir)}<br />
      进度到侧栏底部的「任务」里看（能暂停、终止、继续）。
    `;
    node.classList.remove("hidden");
    toast("已开始批量擦除");
  } catch (error) {
    reportError(error);
  }
}

/** 擦除产物目录：和「处理版本」同一套规则。 */
function eraseOutputDir(drama) {
  const base = state.outputRoot || drama.dir;
  return `${base}/${drama.name}/擦除输出`;
}

/** 清空区域。 */
function clearEraseRegions() {
  eraseState.regions = [];
  drawEraseRegions();
  toast("已清空区域");
}

/** 绑定画布上的拖动：新建 / 移动 / 缩放。 */
function bindEraseCanvas() {
  const canvas = $("#erase-canvas");
  canvas.addEventListener("mousedown", (event) => {
    if (!eraseState.framePath) return;
    const point = canvasPoint(event);
    const viewRegions = eraseState.regions.map((region) =>
      L.toViewRect(region, eraseState.viewSize, eraseState.sourceSize),
    );
    const hit = L.hitTestRegions(point, viewRegions, 12);
    if (hit) {
      eraseState.drag = {
        mode: hit.part,
        index: hit.index,
        from: point,
        original: { ...eraseState.regions[hit.index] },
      };
    } else {
      eraseState.drag = { mode: "new", index: -1, from: point, original: null };
    }
    event.preventDefault();
  });

  canvas.addEventListener("mousemove", (event) => {
    const drag = eraseState.drag;
    if (!drag) return;
    const point = canvasPoint(event);
    if (drag.mode === "new") {
      const rect = L.normalizeRect(drag.from, point);
      const source = L.toSourceRect(rect, eraseState.viewSize, eraseState.sourceSize);
      if (L.rectIsUsable(rect)) {
        // 新建中的框实时显示：先放进区域列表的末尾预览。
        const rest = eraseState.regions.slice(0, drag.index);
        eraseState.regions = [...rest, L.clampRect(source, eraseState.sourceSize)];
      }
      drawEraseRegions();
      return;
    }
    if (drag.mode === "move") {
      const dx = (point.x - drag.from.x) * (eraseState.sourceSize.width / eraseState.viewSize.width);
      const dy = (point.y - drag.from.y) * (eraseState.sourceSize.height / eraseState.viewSize.height);
      eraseState.regions[drag.index] = L.clampRect(
        {
          x: drag.original.x + dx,
          y: drag.original.y + dy,
          width: drag.original.width,
          height: drag.original.height,
        },
        eraseState.sourceSize,
      );
      drawEraseRegions();
      return;
    }
    if (drag.mode === "resize") {
      const dx = (point.x - drag.from.x) * (eraseState.sourceSize.width / eraseState.viewSize.width);
      const dy = (point.y - drag.from.y) * (eraseState.sourceSize.height / eraseState.viewSize.height);
      eraseState.regions[drag.index] = L.clampRect(
        {
          x: drag.original.x,
          y: drag.original.y,
          width: Math.max(8, drag.original.width + dx),
          height: Math.max(8, drag.original.height + dy),
        },
        eraseState.sourceSize,
      );
      drawEraseRegions();
    }
  });

  const finish = () => {
    if (!eraseState.drag) return;
    // 太小的框丢掉（多半是误点）。
    if (eraseState.drag.mode === "new") {
      eraseState.regions = eraseState.regions.filter(
        (region) => region.width >= 8 && region.height >= 8,
      );
    }
    eraseState.drag = null;
    drawEraseRegions();
  };
  canvas.addEventListener("mouseup", finish);
  canvas.addEventListener("mouseleave", finish);
}
async function browse(targetId) {
  try {
    const folder = await call("pick_folder");
    if (folder) $(targetId).value = folder;
  } catch (error) {
    reportError(error);
  }
}

// ============ 工具箱：批量改名 ============

/** 改名按钮的可点状态只由「有计划 + 有待改项 + 没有别的任务在跑」决定。 */
function syncRenameApplyState() {
  const pending = renamePlan ? renamePlan.items.filter((item) => !item.unchanged).length : 0;
  $("#rename-apply").disabled = !pending || Boolean(state.task);
}

async function buildRenamePlan() {
  if (!beginTask("正在读取剧目文件")) return;
  try {
    const drama = selectedDrama();
    renamePlan = await call("list_rename_plan", {
      root: state.root,
      name: drama.name,
    });
    const rows = renamePlan.items
      .map((item) => `<tr>
          <td>${esc(item.from)}</td>
          <td>${item.unchanged ? "—" : esc(item.to)}</td>
          <td>${item.unchanged ? "已合规" : "待改名"}</td>
        </tr>`)
      .join("");
    const skipped = renamePlan.skipped.length
      ? `<p>无法解析集号、保持不动：${esc(renamePlan.skipped.join("、"))}</p>`
      : "";
    const pending = renamePlan.items.filter((item) => !item.unchanged).length;
    renderResult("#rename-result", {
      tone: pending ? "warn" : "ok",
      html: `
      <strong>共 ${renamePlan.items.length} 集：待改名 ${pending} 集，已合规 ${
        renamePlan.items.length - pending
      } 集</strong>
      <table><thead><tr><th>当前文件名</th><th>目标文件名</th><th>状态</th></tr></thead><tbody>${rows}</tbody></table>
      ${skipped}`,
    });
  } catch (error) {
    reportError(error);
  } finally {
    endTask();
    syncRenameApplyState();
  }
}

async function applyRename() {
  if (!renamePlan) return;
  const items = renamePlan.items
    .filter((item) => !item.unchanged)
    .map(({ from, to }) => ({ from, to }));
  if (!items.length) {
    toast("没有需要改名的文件");
    return;
  }
  if (!beginTask("正在批量改名")) return;
  let renamed = 0;
  try {
    const drama = selectedDrama();
    renamed = await call("apply_rename", {
      root: state.root,
      name: drama.name,
      items,
    });
    // 文件名已经变了，旧计划立刻作废：endTask 会把「应用改名」重新置灰，
    // 下面再重建一份新计划，避免拿着过期计划再点一次。
    renamePlan = null;
    toast(`已重命名 ${renamed} 个文件；建议重新扫描并检查`);
  } catch (error) {
    reportError(error);
  } finally {
    endTask();
  }
  if (renamed) {
    await buildRenamePlan();
  }
}

// ============ 工具箱：码率修复 ============

/** 码率修复按钮的可点状态：有低于安全线的分集、且当前没有别的任务在跑。 */
function syncBitrateFixState() {
  const fixable = (bitrateItems ?? []).filter((item) => item.needsFix).length;
  $("#bitrate-fix").disabled = !fixable || Boolean(state.task);
}

function bitrateMinBps() {
  const value = Number($("#bitrate-min").value);
  if (!(value > 0)) {
    setFieldError("#bitrate-min", "码率安全线必须是大于 0 的数字。");
    throw new Error("码率安全线必须是大于 0 的数字。");
  }
  setFieldError("#bitrate-min", null);
  return Math.round(value * 1e6);
}

async function probeBitrate() {
  let minKbps;
  try {
    minKbps = Math.round(bitrateMinBps() / 1000);
  } catch (error) {
    reportError(error);
    return;
  }
  if (!beginTask("正在检测分集码率")) return;
  try {
    const drama = selectedDrama();
    bitrateItems = await call("probe_bitrate", {
      root: state.root,
      name: drama.name,
      minKbps,
      ffprobePath: effectiveFfprobe(),
    });
    const rows = bitrateItems
      .map((item) => {
        const kbps = item.bitrateBps == null ? null : Number(item.bitrateBps);
        const status = kbps == null ? "unknown" : item.needsFix ? "fail" : "pass";
        return `<tr>
          <td>${esc(item.fileName)}</td>
          <td>${kbps == null ? "未知" : `${(kbps / 1000).toFixed(0)} kbps`}</td>
          <td class="status-${status}">${
            kbps == null ? "探测不到码率" : item.needsFix ? "低于安全线" : "达标"
          }</td>
        </tr>`;
      })
      .join("");
    const fixable = bitrateItems.filter((item) => item.needsFix).length;
    renderResult("#bitrate-result", {
      tone: fixable ? "warn" : "ok",
      html: `
      <strong>共 ${bitrateItems.length} 集：${fixable} 集低于安全线</strong>
      <table><thead><tr><th>文件</th><th>码率</th><th>状态</th></tr></thead><tbody>${rows}</tbody></table>
      ${fixable ? "<p>下一步会重编码这些分集并替换原文件，点按钮后会再确认一次。</p>" : ""}`,
    });
  } catch (error) {
    reportError(error);
  } finally {
    endTask();
    syncBitrateFixState();
  }
}

/** 第一段确认：把「会发生什么」讲清楚，用户点了确认才真正动手。 */
function requestBitrateFix() {
  const targets = (bitrateItems ?? []).filter((item) => item.needsFix);
  if (!targets.length) {
    toast("没有需要修复的分集", true);
    return;
  }
  bitratePending = targets;
  $("#bitrate-confirm-text").innerHTML = `即将重编码 <strong>${targets.length}</strong> 集：
    ${esc(targets.slice(0, 5).map((item) => item.fileName).join("、"))}${
      targets.length > 5 ? ` 等 ${targets.length} 个文件` : ""
    }。<br />
    <strong>原文件会被替换</strong>（视频与音频都会重新编码），<strong>没有备份、不能撤销</strong>。
    建议先备份，或先在副本上试一集。`;
  // 弹窗关掉后要把焦点还给触发它的按钮（openDialog 记着），否则键盘用户会被丢回页面开头。
  const opener = $("#bitrate-fix");
  $("#bitrate-fix").disabled = true;
  openDialog($("#bitrate-confirm"), {
    modal: false,
    opener,
    initialFocus: $("#bitrate-confirm-yes"),
    onCancel: cancelBitrateFix,
  });
}

function cancelBitrateFix() {
  bitratePending = null;
  // 先让按钮恢复可聚焦状态（上面刚把它禁用），再关弹窗还焦点。
  syncBitrateFixState();
  closeDialog($("#bitrate-confirm"));
}

async function applyBitrateFix() {
  if (!bitratePending) return;
  const targets = bitratePending;
  bitratePending = null;
  // 走确认路径时不还焦点：紧接着就会开始长任务并禁用按钮。
  closeDialog($("#bitrate-confirm"), { restoreFocus: false });
  syncBitrateFixState();
  if (!beginTask(`正在重编码 ${targets.length} 集（原文件会被替换）`)) return;
  try {
    const drama = selectedDrama();
    const report = await call("fix_bitrate", {
      root: state.root,
      name: drama.name,
      minKbps: Math.round(bitrateMinBps() / 1000),
      ffprobePath: effectiveFfprobe(),
    });
    const fixedRows = report.fixed
      .map(
        (item) =>
          `<tr><td>${esc(item.fileName)}</td><td>${item.fromKbps} → ${
            item.toKbps ?? "?"
          } kbps</td></tr>`,
      )
      .join("");
    const failedRows = report.failed
      .map((item) => `<tr><td>${esc(item.fileName)}</td><td>${esc(item.reason)}</td></tr>`)
      .join("");
    renderResult("#bitrate-result", {
      tone: report.failed.length ? "error" : "ok",
      html: `
      <strong>修复完成：成功 ${report.fixed.length} 集，失败 ${report.failed.length} 集，达标未动 ${report.untouched} 集</strong>
      ${fixedRows ? `<table><thead><tr><th>已修复</th><th>码率变化</th></tr></thead><tbody>${fixedRows}</tbody></table>` : ""}
      ${failedRows ? `<table><thead><tr><th>失败</th><th>原因</th></tr></thead><tbody>${failedRows}</tbody></table>` : ""}`,
      paths: [["drama", state.dramas[state.selected]?.dir]],
    });
    toast(report.failed.length ? "码率修复结束，有分集失败" : "码率修复结束", report.failed.length > 0);
    // 结果已变，旧的分级数据不再可信，要求重新检测。
    bitrateItems = null;
  } catch (error) {
    reportError(error);
  } finally {
    endTask();
    syncBitrateFixState();
  }
}

// ============ 工具箱：执行转码（微调 / 重组） ============

/**
 * 轻量路径归一：统一分隔符、丢掉空段与「.」段（和 core::same_path 的口径一致），
 * 用来在界面上先拦一次明显的「输出 = 素材目录」。
 * 不解析软链接也不展开「..」，真正会不会覆盖原文件由后端规范化后判定。
 */
function stripPath(value) {
  const raw = String(value ?? "").trim().replace(/\\/g, "/");
  const parts = raw.split("/").filter((part) => part !== "" && part !== ".");
  if (raw.startsWith("/")) parts.unshift("");
  return parts.join("/") || (raw.startsWith("/") ? "/" : "");
}

/**
 * 「执行转码」的输出目录：留空 = 默认写到剧目目录下的子目录；
 * 填了就和素材目录比一次，相同则就地拦下（否则会把输出写回原素材）。
 */
function transcodeOutputOverride() {
  const value = $("#exec-output").value.trim();
  setFieldError("#exec-output", null);
  if (!value) return null;
  const drama = state.dramas[state.selected];
  if (drama && stripPath(value) === stripPath(drama.dir)) {
    setFieldError(
      "#exec-output",
      "输出目录不能就是素材目录，那会覆盖原素材；留空会自动写到剧目目录的子目录。",
    );
    return undefined;
  }
  return value;
}

async function executeTranscode() {
  const mode = $("#exec-mode").value;
  const targetRaw = $("#exec-target").value.trim();
  setFieldError("#exec-target", null);
  if (targetRaw !== "" && !(Number.isInteger(Number(targetRaw)) && Number(targetRaw) > 0)) {
    setFieldError("#exec-target", "目标集数必须是大于 0 的整数。");
    toast("请先修正标红的输入项", true);
    return;
  }
  const outputDir = transcodeOutputOverride();
  if (outputDir === undefined) {
    toast("输出目录不能是素材目录，请换一个目录", true);
    return;
  }
  const modeLabel = mode === "recut" ? "分集重组" : "智能微调";
  if (!beginTask(`正在执行${modeLabel}（全剧重编码，可能需要几分钟）`)) return;
  try {
    const drama = selectedDrama();
    const report = await call("transcode_execute", {
      root: state.root,
      name: drama.name,
      outputRoot: state.settings?.outputDir ?? null,
      request: {
        mode,
        targetCount: targetRaw === "" ? null : Number(targetRaw),
        outputDir,
      },
      ffprobePath: effectiveFfprobe(),
    });
    const rows = report.items
      .map(
        (item) => `<tr>
          <td>${esc(item.source)}</td>
          <td>${esc(item.output)}</td>
          <td>${esc(item.detail)}</td>
        </tr>`,
      )
      .join("");
    renderResult("#exec-result", {
      tone: "ok",
      html: `
      <strong>${esc(report.note)}</strong><br />
      输出目录：${esc(report.outputDir)}
      ${
        rows
          ? `<table><thead><tr><th>来源</th><th>输出</th><th>说明</th></tr></thead><tbody>${rows}</tbody></table>`
          : ""
      }`,
      paths: [["exec-output", report.outputDir]],
    });
    toast("转码完成");
  } catch (error) {
    renderResult("#exec-result", {
      tone: "error",
      html: `<strong>转码失败</strong><br />${esc(String(error))}`,
    });
    reportError(error);
  } finally {
    endTask();
  }
}

// ============ 运行环境（全局横幅 + 头部状态 + 设置-本机环境） ============

async function checkRuntime(force = false) {
  if (state.runtime && !force) {
    renderRuntimeStatus(state.runtime);
    return;
  }
  try {
    const status = await call("runtime_status", { ffprobePath: effectiveFfprobe() });
    state.runtime = status;
    renderRuntimeStatus(status);
  } catch (error) {
    reportError(error);
  }
}

function renderRuntimeStatus(status) {
  renderEnvBanner(status);
  renderRuntimeList(status);
  renderEnvChip(status);
  renderRuntimeInstallEntry(status);
}

/** 非 macOS/Windows 没有可用下载源，按竖必然失败，改为给出对应平台的手动安装指引。 */
function manualInstallHint(platform) {
  if (platform === "windows") {
    // Windows 走一键安装（gyan.dev → BtbN 两个来源），这条只在装了旧系统（缺 curl.exe/tar.exe）时出现。
    return "Windows 一键配置需要系统自带的 curl.exe 与 tar.exe（Windows 10 1803 及以上）：请先更新系统，或执行「winget install Gyan.FFmpeg」/ 从 https://www.gyan.dev/ffmpeg/builds/ 下载压缩包，解压后把其中的 bin 目录加入 PATH。";
  }
  if (platform === "linux") {
    return "Linux 暂不支持一键配置：Debian/Ubuntu 执行「sudo apt install ffmpeg」，Fedora/RHEL 执行「sudo dnf install ffmpeg」。";
  }
  return "当前平台暂不支持一键配置，请手动安装 ffmpeg（含 ffprobe）。";
}

/** 设置页的「一键配置运行环境」入口：只在有下载源的平台显示。 */
function renderRuntimeInstallEntry(status) {
  const supported = status.installSupported === true;
  $("#runtime-install").classList.toggle("hidden", !supported);
  const hint = $("#runtime-install-hint");
  hint.classList.toggle("hidden", supported);
  hint.textContent = supported ? "" : manualInstallHint(status.platform);
}

/** 头部常驻状态：缺什么一眼可见（与全局横幅同一口径），点一下跳到本机环境详情。 */
function renderEnvChip(status) {
  const missing = [status.ffmpeg, status.ffprobe].filter(
    (tool) => tool && (!tool.found || !tool.ok),
  );
  const chip = $("#env-chip");
  chip.classList.remove("ok", "warn", "error");
  if (!missing.length) {
    chip.classList.add("ok");
    chip.textContent = "本机环境就绪";
    chip.title = `ffmpeg ${status.ffmpeg?.version ?? "?"} · ffprobe ${status.ffprobe?.version ?? "?"}`;
    // 原生 title 只在鼠标悬停时出现，键盘用户读不到，补一份可朗读的说明。
    $("#env-chip-detail").textContent = chip.title;
    return;
  }
  // 找到了但不可用（例如路径指向别的程序）也算未就绪，避免状态说假话。
  const unusable = missing.some((tool) => tool.found);
  chip.classList.add(unusable ? "warn" : "error");
  chip.textContent = unusable
    ? `环境异常：${missing.map((tool) => tool.name).join("、")}`
    : `缺少 ${missing.map((tool) => tool.name).join("、")}`;
  chip.title = status.python && !status.python.found
    ? "另外未检测到 python（只影响模型类工具）"
    : "点击查看本机环境详情";
  $("#env-chip-detail").textContent = chip.title;
}

function renderEnvBanner(status) {
  const tools = [status.ffmpeg, status.ffprobe];
  const missing = tools.filter((tool) => !tool.found || !tool.ok);
  const node = $("#env-banner");
  if (!missing.length) {
    // 就绪后不占版面；完整探测结果在「设置 → 本机环境」。
    node.classList.add("hidden");
    node.innerHTML = "";
    return;
  }
  node.classList.remove("hidden", "ok", "warn", "error");
  node.classList.add("error");
  const missingText = `运行环境未就绪：缺少 ${esc(
    missing.map((tool) => tool.name).join("、"),
  )}。`;
  if (status.installSupported !== true) {
    // 没有下载源时不给按钮：点了必然报错，比不提供入口更糟。
    node.innerHTML = `${missingText}${esc(
      manualInstallHint(status.platform),
    )}装好后在「设置 → 本机环境」点「重新检测」。`;
    return;
  }
  node.innerHTML = `${missingText}点击「一键配置运行环境」自动下载安装（约 25 MB，需要网络）；也可以在「设置 → 本机环境」手动指定路径。<button id="env-install" class="ghost">一键配置运行环境</button>`;
}

// 一键配置运行环境：后台自动下载安装 ffmpeg/ffprobe 并写回设置，无需手动配置。
// evermeet.cx 单连接限流，全剧下载可能需要几分钟，期间按钮保持禁用。
async function runtimeInstall() {
  // 横幅 / 设置页按钮文案不同（一键配置运行环境 / 下载运行环境），各自保存原文案。
  const buttons = [$("#env-install"), $("#runtime-install"), $("#gate-install")].filter(Boolean);
  const original = new Map(buttons.map((button) => [button, button.textContent]));
  for (const button of buttons) {
    button.disabled = true;
    button.textContent = "正在配置…（下载安装中，请稍候）";
  }
  try {
    const report = await call("install_runtime");
    toast(report.note);
  } catch (error) {
    reportError(error);
  } finally {
    for (const button of buttons) {
      button.disabled = false;
      button.textContent = original.get(button);
    }
    // 无论成败都刷新：成功后回填自动写入的路径并重测，失败时也展示最新状态。
    await loadSettings();
    await checkRuntime(true);
  }
}

function renderRuntimeList(status) {
  const tools = [status.ffmpeg, status.ffprobe];
  const missing = tools.filter((tool) => !tool.found || !tool.ok);
  const rows = [status.ffmpeg, status.ffprobe, status.python]
    .map((tool) => {
      const stateClass = tool.found ? (tool.ok ? "pass" : "warn") : "fail";
      return `<tr>
        <td>${esc(tool.name)}</td>
        <td class="status-${stateClass}">${tool.found ? "已找到" : "未找到"}</td>
        <td>${esc(tool.version ?? tool.detail ?? "")}</td>
      </tr>`;
    })
    .join("");
  $("#runtime-list").innerHTML = `
    <table><thead><tr><th>组件</th><th>状态</th><th>版本 / 说明</th></tr></thead><tbody>${rows}</tbody></table>
  `;
  // 竞品样式：引擎未就绪时显示红色错误行（错误码 RUNTIME_NOT_INSTALLED）。
  const errorNode = $("#runtime-error");
  if (missing.length) {
    errorNode.textContent = `RUNTIME_NOT_INSTALLED：缺少 ${missing
      .map((tool) => tool.name)
      .join("、")}。点击「下载运行环境」自动安装（约 25 MB，需要网络）。`;
    errorNode.classList.remove("hidden");
  } else {
    errorNode.classList.add("hidden");
    errorNode.textContent = "";
  }
  // 关于页的引擎徽章与转码模块的整页阻断都跟随运行环境状态。
  const engineBadge = $("#about-engine");
  if (engineBadge) {
    engineBadge.textContent = missing.length ? "未就绪" : "已是最新版";
    engineBadge.className = `badge ${missing.length ? "status-fail" : "status-ready"}`;
  }
  renderTranscodeGate();
  renderVersionGate();
}

// ============ 账号：登录闸门、状态提示、账号菜单 ============
// 账号状态以 Rust 端为准（account_status / account-changed 事件），这里只负责显示。
// 真正的功能拦截在 src-tauri/src/account/gate.rs，前端被改了也绕不过去。

const accountUi = {
  // 当前显示的是哪一屏：restoring / auth / lock / app。
  screen: "restoring",
  // 登录区当前是哪张卡：login / register / reset。
  authView: "login",
  // 用户关掉过的横幅（只在本次运行里记着）。
  dismissed: new Set(),
  // 验证码冷却：purpose → 截止时间（毫秒）。
  cooldowns: {},
  cooldownTimer: null,
  // 名额满时自动弹设备管理，同一次只弹一回。
  activationPrompted: false,
};

/** 切换整屏。离开主界面时关掉所有弹窗和菜单，免得盖在登录页上。 */
function setScreen(screen) {
  if (accountUi.screen === screen) return;
  accountUi.screen = screen;
  $("#restoring-view").classList.toggle("hidden", screen !== "restoring");
  $("#lock-view").classList.toggle("hidden", screen !== "lock");
  $("#auth-view").classList.toggle("hidden", screen !== "auth");
  const inApp = screen === "app";
  if (!inApp) {
    closeAllDialogs();
    closeAccountMenu();
    $("#settings-view").classList.add("hidden");
  }
  document.querySelector(".app").classList.toggle("hidden", !inApp);
  if (screen === "auth") switchAuthView(accountUi.authView);
  if (screen === "lock") $("#lock-retry").focus();
}

/** 收到新的账号状态：决定显示哪一屏，刷新所有和账号有关的地方。 */
function applyAccount(status) {
  if (!status) return;
  const previous = state.account;
  state.account = status;
  const view = L.accountView(status);
  if (view === "auth") renderAuthNotice(status);
  if (view === "lock") renderLockPage(status);
  setScreen(view);
  renderAccountChip();
  renderAccountBanners();
  renderAccountSettings();
  if (view === "app") {
    refreshModuleLock();
    promptActivation(status);
  }
  // 刚登录进来（包括注册、启动恢复成功）：读一遍首页要的数据。
  if (view === "app" && !previous?.loggedIn && status.loggedIn) loadHome();
  // 登录态可用时顺手拉一次余额：首页引导的第 2 步与「去充值」按钮要知道有没有积分。
  // 失败不打扰用户（可能是断网），余额保持未知，扣费仍然以服务端为准。
  if (view === "app" && status.loggedIn) {
    call("credits_overview")
      .then((overview) => {
        state.creditBalance = Number(overview.balance) || 0;
        renderBalance(state.creditBalance);
        renderHome();
      })
      .catch(() => {
        state.creditBalance = null;
      });
  }
  if (!status.pendingActivation) accountUi.activationPrompted = false;
}

function renderAccountChip() {
  const status = state.account;
  const badge = L.accountBadge(status, nowSecs());
  $("#chip-email").textContent = status?.loggedIn ? status.email : "未登录";
  // 侧栏窄，长邮箱会被省略号截断：鼠标悬停能看到完整的。
  $("#chip-email").title = status?.loggedIn ? status.email : "";
  const node = $("#chip-badge");
  node.textContent = badge.text;
  const tone = { ok: "status-ready", warn: "status-warn", err: "status-fail" }[badge.tone] ?? "";
  node.className = `badge ${tone}`;
  // 余额挂在 title 与 aria-label 上：徽章文字要留给使用权状态，
  // 但用户扫一眼账号区时也该知道还剩多少积分。
  const balanceText =
    state.creditBalance == null ? "" : `，剩余 ${state.creditBalance} 积分`;
  $("#chip-email").title = status?.loggedIn ? `${status.email}${balanceText}` : "";
  $("#account-chip").setAttribute(
    "aria-label",
    status?.loggedIn
      ? `账号 ${status.email}，${badge.text}${balanceText}，打开账号菜单`
      : "未登录",
  );
}

function renderAccountBanners() {
  const banners = L.accountBanners(state.account, nowSecs(), accountUi.dismissed);
  $("#account-banners").innerHTML = banners
    .map((banner) => {
      const action = banner.action
        ? `<button type="button" class="ghost" data-banner-action="${esc(banner.action)}">${esc(banner.actionLabel)}</button>`
        : "";
      const close = banner.dismissible
        ? `<button type="button" class="ghost" data-banner-dismiss="${esc(banner.id)}" aria-label="关闭这条提示">关闭</button>`
        : "";
      return `<div class="env-banner ${esc(banner.tone)}" role="status">${esc(banner.text)}${action}${close}</div>`;
    })
    .join("");
}
/** 登录页顶部的说明：为什么回到了登录页（被解绑、注销、会话失效）。 */
function renderAuthNotice(status) {
  const text =
    status.lockReason === "device_unbound"
      ? status.message
      : status.signedOutReason || status.storageWarning || "";
  const node = $("#auth-notice");
  node.textContent = text ?? "";
  node.classList.toggle("hidden", !text);
}

function renderLockPage(status) {
  const page = L.lockPage(status);
  $("#lock-title").textContent = page.title;
  $("#lock-message").textContent = page.message;
  $("#lock-retry").textContent = page.actionLabel || "重试";
  $("#lock-retry").classList.toggle("hidden", !page.action);
  $("#lock-logout").classList.toggle("hidden", !page.logout);
}

/** 名额满了：自动打开设备管理，让用户选一台解绑。 */
function promptActivation(status) {
  if (!status.pendingActivation || accountUi.activationPrompted) return;
  accountUi.activationPrompted = true;
  openDevicesDialog({ activation: true });
}

/** 功能闸门拒绝了某个命令。 */
async function handleLicenseLocked(error) {
  await refreshAccount();
  const actions = {
    no_entitlement: { label: "兑换卡密", run: openRedeemDialog },
    entitlement_expired: { label: "兑换卡密续期", run: openRedeemDialog },
    needs_activation: { label: "管理设备", run: () => openDevicesDialog({ activation: true }) },
    needs_online: { label: "重新连接", run: retryAccount },
  };
  toast(error.message, true, actions[error.reason] ?? null);
}

async function refreshAccount() {
  if (!invoke) return;
  try {
    applyAccount(await call("account_status"));
  } catch (error) {
    // 连账号状态都拿不到（后端出错）：不能停在「正在恢复登录」，给一个能重试的锁定页。
    if (!state.account) {
      renderLockPage({ lockReason: null, message: `读取登录状态失败：${String(error)}`, loggedIn: false });
      setScreen("lock");
    } else {
      toast(String(error), true);
    }
  }
}

/** 「重试 / 重新连接」：后端按情况重新自检钥匙串或联网刷新。 */
async function retryAccount() {
  try {
    const status = await call("account_retry");
    applyAccount(status);
    if (status.tier === "online") toast("已连上服务端");
    else if (status.tier === "offline") toast("还是连不上服务端，请检查网络", true);
  } catch (error) {
    toast(L.errorText(error), true);
  }
}

// ---- 登录 / 注册 / 重设密码 ----

const AUTH_FORMS = { login: "#auth-login-form", register: "#auth-register-form", reset: "#auth-reset-form" };

function switchAuthView(name) {
  accountUi.authView = name;
  for (const [key, selector] of Object.entries(AUTH_FORMS)) {
    $(selector).classList.toggle("hidden", key !== name);
  }
  ["#login-error", "#register-error", "#reset-error"].forEach((selector) => showFormError(selector, null));
  // 已经填过的邮箱带到下一张卡上，省得重输。
  const email = ["#login-email", "#register-email", "#reset-email"].map((s) => $(s).value.trim()).find(Boolean);
  const target = { login: "#login-email", register: "#register-email", reset: "#reset-email" }[name];
  if (email && !$(target).value) $(target).value = email;
  const first = $(AUTH_FORMS[name]).querySelector("input");
  first?.focus();
  renderCodeButtons();
}

function showFormError(selector, message) {
  const node = $(selector);
  node.textContent = message ?? "";
  node.classList.toggle("hidden", !message);
}

/** 按住提交按钮防重复点击，结束后恢复。 */
async function withBusy(button, run) {
  if (button.disabled) return;
  button.disabled = true;
  button.setAttribute("aria-busy", "true");
  try {
    await run();
  } finally {
    button.removeAttribute("aria-busy");
    button.disabled = false;
    syncRegisterSubmit();
  }
}

/** 密码长度按字符数算（和服务端一样），不按 UTF-16 码元。 */
const passwordLength = (value) => Array.from(value).length;

function checkNewPassword(password) {
  const length = passwordLength(password);
  return length >= 8 && length <= 128 ? null : "密码长度要在 8 到 128 个字符之间";
}
// 验证码：60 秒冷却，以服务端返回的 cooldown_seconds 为准；被限流时按 retryAfter 等。

function startCooldown(purpose, seconds) {
  accountUi.cooldowns[purpose] = Date.now() + seconds * 1000;
  renderCodeButtons();
  if (!accountUi.cooldownTimer) {
    accountUi.cooldownTimer = setInterval(() => {
      renderCodeButtons();
      const active = Object.values(accountUi.cooldowns).some((until) => until > Date.now());
      if (!active) {
        clearInterval(accountUi.cooldownTimer);
        accountUi.cooldownTimer = null;
      }
    }, 1000);
  }
}

function renderCodeButtons() {
  $$("[data-send-code]").forEach((button) => {
    if (button.getAttribute("aria-busy") === "true") return;
    const until = accountUi.cooldowns[button.dataset.sendCode] ?? 0;
    const label = L.codeButtonLabel((until - Date.now()) / 1000);
    button.textContent = label.text;
    button.disabled = label.disabled;
  });
}

async function sendCode(button) {
  const purpose = button.dataset.sendCode;
  const isRegister = purpose === "register";
  const emailInput = $(isRegister ? "#register-email" : "#reset-email");
  const errorSelector = isRegister ? "#register-error" : "#reset-error";
  const email = emailInput.value.trim();
  if (!email.includes("@")) {
    showFormError(errorSelector, "请先填写正确的邮箱");
    emailInput.focus();
    return;
  }
  showFormError(errorSelector, null);
  button.disabled = true;
  button.setAttribute("aria-busy", "true");
  try {
    const reply = await call("account_send_code", { args: { email, purpose } });
    startCooldown(purpose, Number(reply?.cooldown_seconds) || 60);
    toast(`验证码已发到 ${email}，10 分钟内有效`);
    $(isRegister ? "#register-code" : "#reset-code").focus();
  } catch (error) {
    showFormError(errorSelector, L.errorText(error));
    if (error?.code === "rate_limited" && error.retryAfter) startCooldown(purpose, error.retryAfter);
  } finally {
    button.removeAttribute("aria-busy");
    renderCodeButtons();
  }
}

function syncRegisterSubmit() {
  $("#register-submit").disabled = !$("#register-terms").checked;
}

async function registerAccount(event) {
  event.preventDefault();
  const email = $("#register-email").value.trim();
  const code = $("#register-code").value.trim();
  const password = $("#register-password").value;
  // 邀请码可选：留空就是不填。填了但格式不对也不拦——服务端会当成无效码忽略，
  // 邀请是附加动作，不该因为一个手滑的邀请码把人挡在注册外面。
  const inviteCode = $("#register-invite").value.trim();
  if (!$("#register-terms").checked) return showFormError("#register-error", "请先阅读并勾选同意条款");
  if (!email.includes("@")) return showFormError("#register-error", "请填写正确的邮箱");
  if (!/^\d{6}$/.test(code)) return showFormError("#register-error", "验证码是 6 位数字");
  const problem = checkNewPassword(password);
  if (problem) return showFormError("#register-error", problem);
  showFormError("#register-error", null);
  await withBusy($("#register-submit"), async () => {
    try {
      const status = await call("account_register", {
        args: { email, code, password, acceptTerms: true, inviteCode },
      });
      $("#register-password").value = "";
      $("#register-code").value = "";
      $("#register-invite").value = "";
      applyAccount(status);
      toast(`注册成功，已登录：${status.email}`);
    } catch (error) {
      showFormError("#register-error", L.errorText(error));
    }
  });
}

async function loginAccount(event) {
  event.preventDefault();
  const email = $("#login-email").value.trim();
  const password = $("#login-password").value;
  if (!email || !password) return showFormError("#login-error", "请填写邮箱和密码");
  showFormError("#login-error", null);
  await withBusy($("#login-submit"), async () => {
    try {
      const status = await call("account_login", { email, password });
      $("#login-password").value = "";
      applyAccount(status);
      toast(`欢迎回来，${status.email}`);
    } catch (error) {
      showFormError("#login-error", L.errorText(error));
    }
  });
}

async function resetPassword(event) {
  event.preventDefault();
  const email = $("#reset-email").value.trim();
  const code = $("#reset-code").value.trim();
  const newPassword = $("#reset-password").value;
  if (!email.includes("@")) return showFormError("#reset-error", "请填写正确的邮箱");
  if (!/^\d{6}$/.test(code)) return showFormError("#reset-error", "验证码是 6 位数字");
  const problem = checkNewPassword(newPassword);
  if (problem) return showFormError("#reset-error", problem);
  showFormError("#reset-error", null);
  await withBusy($("#reset-submit"), async () => {
    try {
      await call("account_reset_password", { args: { email, code, newPassword } });
      $("#reset-password").value = "";
      $("#reset-code").value = "";
      $("#login-email").value = email;
      switchAuthView("login");
      const notice = $("#auth-notice");
      notice.textContent = "密码已重设，请用新密码登录";
      notice.classList.remove("hidden");
    } catch (error) {
      showFormError("#reset-error", L.errorText(error));
    }
  });
}
async function logoutAccount() {
  const ok = await confirmDialog({
    title: "退出登录",
    body: "退出后这台电脑仍然占着账号的设备名额。要换电脑，请在「设备管理」里解绑这台电脑，而不是只退出登录。",
    confirmLabel: "退出登录",
  });
  if (!ok) return;
  try {
    applyAccount(await call("account_logout"));
    toast("已退出登录");
  } catch (error) {
    toast(L.errorText(error), true);
  }
}

// ---- 账号菜单（侧栏底部） ----

function openAccountMenu() {
  if (!state.account?.loggedIn) return;
  $("#account-menu").classList.remove("hidden");
  $("#account-chip").setAttribute("aria-expanded", "true");
  $("#account-menu [role=menuitem]")?.focus();
}

function closeAccountMenu({ restoreFocus = false } = {}) {
  const menu = $("#account-menu");
  if (menu.classList.contains("hidden")) return;
  menu.classList.add("hidden");
  $("#account-chip").setAttribute("aria-expanded", "false");
  if (restoreFocus) $("#account-chip").focus();
}

/** 菜单里的键盘：Esc 关闭，上下方向键在选项间移动。 */
function handleAccountMenuKeys(event) {
  const menu = $("#account-menu");
  if (menu.classList.contains("hidden")) return false;
  if (event.key === "Escape") {
    event.preventDefault();
    closeAccountMenu({ restoreFocus: true });
    return true;
  }
  if (event.key === "ArrowDown" || event.key === "ArrowUp") {
    event.preventDefault();
    const items = $$("#account-menu [role=menuitem]");
    const index = items.indexOf(document.activeElement);
    const step = event.key === "ArrowDown" ? 1 : -1;
    items[(index + step + items.length) % items.length].focus();
    return true;
  }
  if (event.key === "Tab") closeAccountMenu();
  return false;
}

/** 账号菜单、首页、设置页里 `data-account-action` 的按钮都走这里。 */
function runAccountAction(action) {
  closeAccountMenu();
  if (action === "credits") openCreditsDialog();
  else if (action === "redeem") openRedeemDialog();
  else if (action === "devices") openDevicesDialog({ activation: false });
  else if (action === "settings") {
    state.settingsSub = "account";
    switchModule("settings");
  } else if (action === "logout") logoutAccount();
}

// ---- 积分与充值（M5a） ----

/** 积分弹窗的状态：当前下单的订单号（用来轮询 / 主动查单）。 */
const creditsState = { outTradeNo: null, channel: null };

function openCreditsDialog() {
  if (!state.account?.loggedIn) return;
  showFormError("#credits-error", null);
  $("#credits-checkout").classList.add("hidden");
  $("#credits-balance").textContent = "—";
  $("#credits-history").innerHTML = '<tr><td colspan="3" class="empty">正在读取…</td></tr>';
  $("#invite-code").value = "";
  $("#invite-stats").textContent = "—";
  openDialog($("#dialog-credits"), { initialFocus: $("#credits-refresh") });
  loadCredits();
  loadInvites();
  loadTiers();
}

/** 余额与最近流水。 */
async function loadCredits() {
  try {
    const overview = await call("credits_overview");
    renderBalance(overview.balance);
    renderCreditHistory(overview.history || []);
  } catch (error) {
    showFormError("#credits-error", L.errorText(error));
  }
}

function renderBalance(balance) {
  $("#credits-balance").textContent = String(balance);
  // 账号区的余额提示由 renderAccountChip 统一维护（避免两处各写一遍 title 互相覆盖）。
  renderAccountChip();
}

/** 流水：变动带正负号，退还与奖励用不同颜色。 */
function renderCreditHistory(items) {
  const body = $("#credits-history");
  if (!items || items.length === 0) {
    body.innerHTML = '<tr><td colspan="3" class="empty">还没有积分记录。</td></tr>';
    return;
  }
  body.innerHTML = items
    .map((item) => {
      const delta = Number(item.delta) || 0;
      const tone = delta > 0 ? "status-pass" : "status-fail";
      return `<tr>
        <td>${esc(formatDateTime(item.createdAt))}</td>
        <td class="${tone}">${delta > 0 ? "+" : ""}${delta}</td>
        <td>${esc(describeCredit(item))}</td>
      </tr>`;
    })
    .join("");
}

/** 一条流水的人类可读说明：种类 + 备注，预扣状态单独标出来。 */
function describeCredit(item) {
  const kind = item.kind;
  const memo = item.memo || "";
  if (kind === "hold") {
    const state = item.settledState;
    if (state === "reserved") return `预扣中：${memo}`;
    if (state === "settled") return `已消费：${memo}`;
    if (state === "released") return `已退还：${memo}`;
  }
  if (kind === "grant") return memo || "充值";
  if (kind === "reward") return memo || "奖励";
  if (kind === "release") return memo || "退还";
  if (kind === "expire") return memo || "预扣超时退还";
  return memo || kind;
}

/** 充值档位。 */
async function loadTiers() {
  const node = $("#credits-tiers");
  try {
    const payload = await call("recharge_tiers");
    const items = payload.items || [];
    if (items.length === 0) {
      node.textContent = "还没有可用的充值档位。";
      return;
    }
    node.innerHTML = items
      .map(
        (tier) =>
          `<button type="button" class="primary" data-tier="${esc(tier.id)}" data-credits="${tier.credits}">
            ${tier.credits} 积分 · ${formatMoney(tier.amountFen)}
          </button>`,
      )
      .join("");
  } catch (error) {
    node.textContent = L.errorText(error);
  }
}

/** 分换算成「元」。 */
function formatMoney(fen) {
  return `${(Number(fen) / 100).toFixed(2)} 元`;
}

/** 下单：拿回怎么去付。 */
async function startRecharge(tierId) {
  showFormError("#credits-error", null);
  try {
    const order = await call("create_recharge_order", { tierId, channel: "mock" });
    creditsState.outTradeNo = order.outTradeNo;
    creditsState.channel = order.channel;
    const node = $("#credits-checkout");
    // 收银台地址做成可点的链接：本地模拟支付时用户要在浏览器里打开它才能「付款」。
    // 只显示一段死文本，本地闭环就走不下去。
    //
    // 只把 http/https 渲染成链接：地址虽然来自我们的服务端，但它是后端按环境拼的，
    // 一律按不可信处理——`javascript:` 之类的协议绝不能进 href。
    const rawTarget = String(order.checkout.payload ?? "");
    const linkable = /^https?:\/\//i.test(rawTarget);
    const payTarget =
      order.checkout.kind === "redirect"
        ? (linkable
            ? `<a href="${esc(rawTarget)}" target="_blank" rel="noreferrer noopener">打开收银台付款</a><br />`
            : "") + `<span class="hint">收银台地址：<code>${esc(rawTarget)}</code></span>`
        : `二维码内容：<code>${esc(rawTarget)}</code>`;
    // Mock 渠道没有「渠道侧」可查，点「我已完成支付」只会拿到 503。
    // 与其让用户点了才发现，不如提前说清楚：去收银台页面点付款，回来刷新即可。
    const isMock = order.channel === "mock";
    node.innerHTML = `
      <strong>订单已创建</strong><br />
      应付：${formatMoney(order.amountFen)} · 到账：${order.credits} 积分<br />
      ${payTarget}<br />
      <span class="hint">当前是本地模拟支付（没有接真实渠道）。实际使用时这里会跳转到收银台或显示二维码。</span>
      <div class="option-row">
        <button id="credits-paid" type="button" class="success">我已完成支付</button>
      </div>
      ${
        isMock
          ? `<span class="hint">模拟支付没有渠道侧可查单，点「我已完成支付」会提示查不了。
             请在上面的收银台页面点「支付成功」，再点弹窗顶部的「刷新」。</span>`
          : ""
      }
    `;
    node.classList.remove("hidden");
    $("#credits-paid").addEventListener("click", reconcileRecharge);
  } catch (error) {
    showFormError("#credits-error", L.errorText(error));
  }
}

/** 「我已完成支付」：主动查单（回调丢失的兜底）。 */
async function reconcileRecharge() {
  if (!creditsState.outTradeNo) return;
  try {
    const result = await call("reconcile_recharge", { outTradeNo: creditsState.outTradeNo });
    if (result.credited > 0) {
      toast(`充值成功，到账 ${result.credited} 积分`);
    } else if (result.already) {
      toast("这笔订单已经到账过了");
    } else {
      toast("还没有查到支付结果，请稍后再试或联系客服", true);
    }
    loadCredits();
  } catch (error) {
    // Mock 渠道查不了单：如实显示服务端的说明，不假装「已支付」。
    showFormError("#credits-error", L.errorText(error));
  }
}

/** 邀请码与战绩。 */
async function loadInvites() {
  try {
    const info = await call("invite_status");
    $("#invite-code").value = info.code || "";
    $("#invite-stats").textContent =
      `已邀请 ${info.invited} 人 · 已得奖励 ${info.rewards} 次（共 ${info.earnedCredits} 积分）。` +
      `邀请人每成功一次得 ${info.inviterCredits} 积分，被邀请人得 ${info.inviteeCredits} 积分，` +
      `每人最多奖励 ${info.maxRewards} 次。`;
  } catch (error) {
    $("#invite-stats").textContent = L.errorText(error);
  }
}

// ---- 服务端处理（M5b-8） ----
//
// 这条链路是：传素材 → 提交任务 → 轮询状态 → 下载产物。
// 视频不经过本地 ffmpeg，全部在服务端跑；本机只负责传上去和取回来。
//
// 与本机处理的区别：本机处理视频不出这台电脑、断网也能跑；
// 服务端处理要联网、要花积分，但不用等本机 CPU。

/** 上传进度事件：Rust 侧每传完一块发一次。 */
function handleServerUploadProgress(progress) {
  if (!serverState.upload || serverState.upload.uploadId !== progress.uploadId) return;
  serverState.upload.progress = progress;
  renderServerUpload();
}

const serverState = {
  // 当前这副素材的上传会话（upload_id + 块大小 + 已确认的素材 id）
  upload: null,
  // 正在跟踪的任务
  task: null,
  // 轮询定时器
  timer: null,
  busy: false,
};

/** 切进「服务端处理」时调用：复位界面到当前剧目对应的状态。 */
function enterServerModule() {
  renderServerContext();
  renderServerUpload();
  renderServerTask();
}

function renderServerContext() {
  const selected = state.selected == null ? null : state.dramas[state.selected];
  const node = $("#server-context");
  if (!selected) {
    node.textContent = "尚未选择剧目——请先到「剧目库」扫描并点选一部剧。";
    node.classList.add("empty");
    $("#server-upload-start").disabled = true;
    $("#server-submit").disabled = true;
    return;
  }
  const episodes = selected.episodes || [];
  node.classList.remove("empty");
  node.textContent =
    `${selected.name || "未命名"} · ${episodes.length} 集 · 合计 ${formatBytes(
      episodes.reduce((sum, item) => sum + (Number(item.sizeBytes) || 0), 0),
    )}`;
  // 传好了才能提交；上传中不能重复点。
  $("#server-upload-start").disabled = serverState.busy || Boolean(serverState.upload?.assetId);
  $("#server-submit").disabled = !serverState.upload?.assetId || serverState.busy;
}

function renderServerUpload() {
  const badge = $("#server-upload-badge");
  const panel = $("#server-upload-panel");
  const upload = serverState.upload;
  if (!upload) {
    badge.textContent = "未开始";
    badge.className = "badge";
    panel.classList.add("hidden");
    $("#server-upload-abort").disabled = true;
    return;
  }
  panel.classList.remove("hidden");
  if (upload.assetId) {
    badge.textContent = "已上传";
    badge.className = "badge success";
    panel.innerHTML = `<strong>素材已就绪</strong><br /><span class="hint">素材编号：<code>${esc(
      upload.assetId,
    )}</code></span>`;
    $("#server-upload-abort").disabled = true;
    return;
  }
  badge.textContent = "上传中";
  badge.className = "badge";
  const progress = upload.progress || { totalChunks: 0, sentChunks: 0, skippedChunks: 0 };
  panel.innerHTML = `
    <strong>${esc(L.uploadText(progress))}</strong>
    <progress max="100" value="${L.uploadPercent(progress)}"></progress>
    <span class="hint">${L.uploadPercent(progress)}%</span>
  `;
  $("#server-upload-abort").disabled = false;
}

function renderServerTask() {
  const node = $("#server-task");
  const artifacts = $("#server-artifacts");
  const task = serverState.task;
  if (!task) {
    node.classList.add("hidden");
    artifacts.classList.add("hidden");
    return;
  }
  const state = L.serverStateLabel(task.state);
  node.classList.remove("hidden");
  node.innerHTML = `
    <strong class="badge ${esc(state.tone)}">${esc(state.label)}</strong>
    <span class="hint">任务 <code>${esc(task.id || "")}</code></span>
    ${
      task.error?.message
        ? `<br /><span class="field-error">${esc(task.error.message)}</span>`
        : ""
    }
    <div class="option-row">
      <button id="server-task-cancel" class="ghost" type="button">${
        L.serverTaskActions(task).cancel ? "取消任务" : "已结束"
      }</button>
    </div>
    <span class="hint">${
      L.serverTaskFinished(task)
        ? "任务已结束，不再刷新。"
        : "正在跟踪进度，结束后会自动停止刷新。"
    }</span>
  `;
  const cancel = $("#server-task-cancel");
  if (cancel) {
    cancel.disabled = !L.serverTaskActions(task).cancel;
    cancel.addEventListener("click", cancelServerTask);
  }

  const outputs = (task.artifacts || []).filter((item) => item.kind === "output");
  if (L.serverTaskActions(task).download && outputs.length) {
    artifacts.classList.remove("hidden");
    artifacts.innerHTML = `
      <strong>产物 ${outputs.length} 个</strong>
      <div class="option-row">
        ${outputs
          .map(
            (item, index) =>
              `<button class="success" type="button" data-artifact="${esc(item.id)}">下载第 ${
                index + 1
              } 个（${formatBytes(item.size_bytes)}）</button>`,
          )
          .join("")}
      </div>
      <span class="hint">下载链接是短时的，过期后点「刷新」重新获取。</span>
    `;
    artifacts.querySelectorAll("[data-artifact]").forEach((button) => {
      button.addEventListener("click", () => downloadServerArtifact(button.dataset.artifact));
    });
  } else {
    artifacts.classList.add("hidden");
  }
}

async function startServerUpload() {
  const selected = state.selected == null ? null : state.dramas[state.selected];
  if (!selected) return;
  const episodes = selected.episodes || [];
  if (!episodes.length) {
    showFormError("#server-upload-error", "这部剧还没有分集");
    return;
  }
  // 这一版一次只处理一集：服务端按集计价，多集上传要排队，
  // 先把「传一集 → 处理一集 → 下载」走通，多集等后面再扩。
  const first = episodes[0];
  if (!first.path) {
    showFormError("#server-upload-error", "这一集没有本机路径，无法上传");
    return;
  }
  showFormError("#server-upload-error", null);
  serverState.busy = true;
  $("#server-upload-start").disabled = true;
  try {
    // 断点续传：已经有**同一集**未完成的上传会话时直接续传，不重新开会话。
    // 判据抽在 logic.js 的 uploadResumePlan 里（可单测）：误判成「新开」会让用户
    // 从头再传几个 GB，「断点续传」就成了空话。
    const plan = L.uploadResumePlan(serverState.upload, first.path);
    let session = plan.resume
      ? { upload_id: plan.uploadId, chunk_size: plan.chunkSize, total_chunks: plan.totalChunks }
      : null;
    if (!session) {
      session = await call("server_upload_start", {
        path: first.path,
        name: first.fileName || first.name || "第 1 集",
        filename: first.fileName || first.name || "ep01.mp4",
      });
      serverState.upload = {
        uploadId: session.upload_id,
        chunkSize: session.chunk_size,
        totalChunks: session.total_chunks,
        progress: { totalChunks: session.total_chunks, sentChunks: 0, skippedChunks: 0 },
        assetId: null,
        // 记住这一集：换了一集就要重开会话，不能接着传别的文件。
        sourcePath: first.path,
      };
      renderServerUpload();
    }

    await call("server_upload_chunks", {
      uploadId: session.upload_id,
      path: first.path,
      chunkSize: session.chunk_size,
    });
    const done = await call("server_upload_complete", { uploadId: session.upload_id });
    serverState.upload.assetId = done.asset_id;
    toast("素材已上传到服务端");
  } catch (error) {
    showFormError("#server-upload-error", L.errorText(error));
  } finally {
    serverState.busy = false;
    renderServerContext();
    renderServerUpload();
  }
}

async function abortServerUpload() {
  const upload = serverState.upload;
  if (!upload?.uploadId) return;
  try {
    await call("server_upload_abort", { uploadId: upload.uploadId });
    serverState.upload = null;
    toast("已放弃这次上传，服务端的分片已删除");
  } catch (error) {
    showFormError("#server-upload-error", L.errorText(error));
  } finally {
    renderServerContext();
    renderServerUpload();
  }
}

async function submitServerTask() {
  const upload = serverState.upload;
  if (!upload?.assetId) return;
  showFormError("#server-submit-error", null);
  serverState.busy = true;
  $("#server-submit").disabled = true;
  try {
    const task = await call("server_submit_task", {
      assetIds: [upload.assetId],
      recipeId: $("#server-recipe").value,
    });
    serverState.task = task;
    renderServerTask();
    startServerPolling();
    toast("已提交到服务端处理");
  } catch (error) {
    // 积分不足是这里最常见的错误：把服务端给的数字原样显示出来。
    showFormError("#server-submit-error", L.errorText(error));
  } finally {
    serverState.busy = false;
    renderServerContext();
  }
}

function startServerPolling() {
  stopServerPolling();
  serverState.timer = setInterval(refreshServerTask, 3000);
}

function stopServerPolling() {
  if (serverState.timer) {
    clearInterval(serverState.timer);
    serverState.timer = null;
  }
}

async function refreshServerTask() {
  const task = serverState.task;
  if (!task?.id) return stopServerPolling();
  try {
    const fresh = await call("server_task", { taskId: task.id });
    serverState.task = fresh;
    renderServerTask();
    // 结束了就不用再问：省请求，也让界面稳定下来。
    if (L.serverTaskFinished(fresh)) stopServerPolling();
  } catch (error) {
    stopServerPolling();
    showFormError("#server-submit-error", L.errorText(error));
  }
}

async function cancelServerTask() {
  const task = serverState.task;
  if (!task?.id) return;
  try {
    await call("server_cancel_task", { taskId: task.id });
    toast("已请求取消；预扣的积分会退回来");
    await refreshServerTask();
  } catch (error) {
    showFormError("#server-submit-error", L.errorText(error));
  }
}

async function downloadServerArtifact(artifactId) {
  try {
    const info = await call("server_artifact", { artifactId });
    // 服务端给的是短时链接。这里不自己写下载器（要处理断点、磁盘、进度），
    // 而是把链接显示出来交给用户点开——和「模拟收银台」同一个做法，
    // 也让「短时有效」这件事看得见。
    const node = $("#server-artifacts");
    node.classList.remove("hidden");
    // 只把 http/https 渲染成链接：地址由服务端按环境拼，一律按不可信处理。
    const url = String(info.download_url ?? "");
    const linkable = /^https?:\/\//i.test(url);
    node.insertAdjacentHTML(
      "beforeend",
      `<div class="result">${
        linkable
          ? `<a href="${esc(url)}" target="_blank" rel="noreferrer noopener">点这里下载</a>`
          : ""
      }<br /><span class="hint">下载链接（15 分钟内有效）：<code>${esc(url)}</code></span></div>`,
    );
  } catch (error) {
    showFormError("#server-submit-error", L.errorText(error));
  }
}

// ---- 卡密兑换 ----

function openRedeemDialog() {
  if (!state.account?.loggedIn) return;
  $("#redeem-code").value = "";
  showFormError("#redeem-error", null);
  $("#redeem-success").classList.add("hidden");
  $("#redeem-code").removeAttribute("aria-invalid");
  openDialog($("#dialog-redeem"), { initialFocus: $("#redeem-code") });
}

function formatRedeemInput() {
  const input = $("#redeem-code");
  const { value, caret } = L.formatCardInput(input.value, input.selectionStart);
  if (value !== input.value) {
    input.value = value;
    input.setSelectionRange(caret, caret);
  }
  input.removeAttribute("aria-invalid");
  showFormError("#redeem-error", null);
}

async function redeemCard(event) {
  event.preventDefault();
  const input = $("#redeem-code");
  const checked = L.validateCard(input.value);
  if (!checked.ok) {
    input.setAttribute("aria-invalid", "true");
    showFormError("#redeem-error", checked.message);
    input.focus();
    return;
  }
  $("#redeem-success").classList.add("hidden");
  await withBusy($("#redeem-submit"), async () => {
    try {
      const reply = await call("license_redeem", { code: checked.code });
      applyAccount(reply.status);
      const until = L.entitlementText(reply.status, nowSecs());
      const text = reply.alreadyRedeemed
        ? `这张卡之前已经兑换过了，没有重复加时间。当前：${until}`
        : `兑换成功。当前：${until}`;
      const node = $("#redeem-success");
      node.textContent = text;
      node.classList.remove("hidden");
      input.value = "";
      toast(text);
    } catch (error) {
      input.setAttribute("aria-invalid", "true");
      showFormError("#redeem-error", L.errorText(error));
    }
  });
}
// ---- 设备管理 ----
// `activation: true`：名额满了、这台电脑还没绑上，用户要先解绑一台再绑定本机。

const devicesUi = { activation: false, list: null };

async function openDevicesDialog({ activation }) {
  if (!state.account?.loggedIn) return;
  devicesUi.activation = Boolean(activation) || Boolean(state.account.pendingActivation);
  $("#devices-title").textContent = devicesUi.activation ? "电脑数量已满" : "设备管理";
  $("#devices-activate").classList.toggle("hidden", !devicesUi.activation);
  $("#devices-logout").classList.toggle("hidden", !devicesUi.activation);
  showFormError("#devices-error", null);
  const dialog = $("#dialog-devices");
  if (dialog.classList.contains("hidden")) openDialog(dialog, {});
  await loadDevices();
}

async function loadDevices() {
  $("#devices-body").innerHTML = '<tr><td colspan="5" class="empty">正在读取…</td></tr>';
  try {
    devicesUi.list = await call("device_list");
    renderDevices();
  } catch (error) {
    // 名额满时后端在状态里带了一份设备列表，联网读失败就先用它。
    const pending = state.account?.pendingActivation;
    if (pending?.devices) {
      devicesUi.list = pending;
      renderDevices();
    } else {
      $("#devices-body").innerHTML = '<tr><td colspan="5" class="empty">没读到设备列表。</td></tr>';
    }
    showFormError("#devices-error", L.errorText(error));
  }
}

function renderDevices() {
  const list = devicesUi.list;
  const devices = L.sortDevices(list?.devices);
  const quota = L.unbindQuotaText(list?.unbind_quota);
  const limit = list?.device_limit ?? 1;
  $("#devices-desc").textContent = devicesUi.activation
    ? `这个账号最多同时绑定 ${limit} 台电脑，现在已经满了。解绑一台不用的电脑，再绑定这台。${quota}。`
    : `这个账号最多同时绑定 ${limit} 台电脑。${quota}。解绑这台电脑等于退出登录。`;
  $("#devices-body").innerHTML =
    devices
      .map(
        (device) => `<tr>
          <td>${esc(device.name)}${
            device.current ? ' <span class="badge status-ready">本机</span>' : ""
          }<div class="hint">${esc(L.platformLabel(device.platform))}</div></td>
          <td class="mono">${esc(device.code)}</td>
          <td>${esc(L.formatIso(device.bound_at))}</td>
          <td>${esc(L.formatIso(device.last_seen_at))}</td>
          <td><button type="button" class="ghost" data-unbind="${esc(device.id)}">解绑</button></td>
        </tr>`,
      )
      .join("") || '<tr><td colspan="5" class="empty">没有绑定的电脑。</td></tr>';
}

async function unbindDevice(id) {
  const device = devicesUi.list?.devices?.find((item) => item.id === id);
  if (!device) return;
  const quota = L.unbindQuotaText(devicesUi.list?.unbind_quota);
  const self = device.current
    ? "这是本机：解绑后这台电脑会立即退出登录。"
    : "那台电脑会立即退出登录；它要是一直断网，最多还能离线用到凭证到期（72 小时内）。";
  const ok = await confirmDialog({
    title: "解绑电脑",
    body: `确定解绑「${device.name}」（${device.code}）？${self}${quota}。`,
    confirmLabel: "解绑",
  });
  if (!ok) return;
  try {
    const status = await call("device_unbind", { id });
    toast("已解绑");
    applyAccount(status);
    if (device.current || !status.loggedIn) return;
    // 在等名额的话，后端刷新时会自动重新绑定本机；绑上了就关掉弹窗。
    if (devicesUi.activation && !status.pendingActivation) {
      closeDialog($("#dialog-devices"));
      toast("已绑定这台电脑");
      return;
    }
    await loadDevices();
  } catch (error) {
    showFormError("#devices-error", L.errorText(error));
    // 会话失效（被别处解绑）时，状态已经变了。
    refreshAccount();
  }
}

async function activateDevice() {
  await withBusy($("#devices-activate"), async () => {
    try {
      const status = await call("device_activate");
      applyAccount(status);
      if (status.pendingActivation) {
        showFormError("#devices-error", "电脑数量还是满的，请先解绑一台");
        await loadDevices();
      } else {
        closeDialog($("#dialog-devices"));
        toast("已绑定这台电脑");
      }
    } catch (error) {
      showFormError("#devices-error", L.errorText(error));
    }
  });
}
// ---- 设置：账号与授权 ----

function tierText(status) {
  if (!status?.loggedIn) return "—";
  if (status.tier === "online") return "已联网";
  if (status.tier === "offline") {
    const left = status.offlineUntil != null ? status.offlineUntil - nowSecs() : status.offlineRemainingSecs;
    return `离线（还能用 ${L.formatRemaining(left)}）`;
  }
  return status.message || "已锁定";
}

function renderAccountSettings() {
  const status = state.account;
  const entitlement = status?.loggedIn ? L.entitlementText(status, nowSecs()) : "—";
  $("#account-email").textContent = status?.loggedIn ? status.email : "—";
  $("#account-entitlement").textContent = entitlement;
  $("#billing-entitlement").textContent = entitlement;
  $("#account-device-code").textContent = status?.deviceCode ?? "—";
  $("#account-tier").textContent = tierText(status);
  // 首页欢迎卡。
  $("#home-welcome").textContent = status?.loggedIn ? `欢迎回来，${status.email}` : "欢迎使用剧备";
  $("#home-entitlement").textContent = status?.loggedIn ? `使用权：${entitlement}` : "";
}

async function copyDeviceCode() {
  const code = state.account?.deviceCode;
  if (!code) return;
  const copied = await copyText(code);
  toast(copied ? "机器码已复制" : `复制失败，请手动复制：${code}`, !copied);
}

async function syncLicense() {
  await withBusy($("#account-sync"), async () => {
    try {
      const status = await call("license_refresh");
      applyAccount(status);
      toast(status.tier === "online" ? "授权已刷新" : tierText(status), status.tier !== "online");
    } catch (error) {
      toast(L.errorText(error), true);
    }
  });
}

async function exportAccountData() {
  await withBusy($("#account-export"), async () => {
    try {
      const path = await call("account_export");
      if (path) toast(`个人数据已保存到 ${path}`);
    } catch (error) {
      toast(L.errorText(error), true);
    }
  });
}

function closePasswordForm() {
  $("#password-change-form").classList.add("hidden");
  $("#open-change-password").setAttribute("aria-expanded", "false");
  $("#old-password").value = "";
  $("#new-password").value = "";
  showFormError("#password-change-error", null);
}

async function changePassword(event) {
  event.preventDefault();
  const currentPassword = $("#old-password").value;
  const newPassword = $("#new-password").value;
  if (!currentPassword) return showFormError("#password-change-error", "请输入当前密码");
  const problem = checkNewPassword(newPassword);
  if (problem) return showFormError("#password-change-error", problem);
  showFormError("#password-change-error", null);
  await withBusy($("#change-password-submit"), async () => {
    try {
      applyAccount(await call("account_change_password", { args: { currentPassword, newPassword } }));
      closePasswordForm();
      toast("密码已修改，其他电脑上的登录已失效");
    } catch (error) {
      showFormError("#password-change-error", L.errorText(error));
    }
  });
}

// ---- 注销账号 ----

function openDeleteDialog() {
  if (!state.account?.loggedIn) return;
  $("#delete-password").value = "";
  $("#delete-email").value = "";
  showFormError("#delete-error", null);
  syncDeleteSubmit();
  openDialog($("#dialog-delete"), { initialFocus: $("#delete-password") });
}

function syncDeleteSubmit() {
  $("#delete-submit").disabled = !L.canConfirmDelete(
    state.account?.email,
    $("#delete-email").value,
    $("#delete-password").value,
  );
}

async function deleteAccount(event) {
  event.preventDefault();
  if (!L.canConfirmDelete(state.account?.email, $("#delete-email").value, $("#delete-password").value)) return;
  await withBusy($("#delete-submit"), async () => {
    try {
      const status = await call("account_delete", { password: $("#delete-password").value });
      $("#delete-password").value = "";
      applyAccount(status);
      toast("账号已注销");
    } catch (error) {
      showFormError("#delete-error", L.errorText(error));
    }
  });
  syncDeleteSubmit();
}
// ============ 弹窗体系 ============
// 所有弹窗走 openDialog / closeDialog：
// - 打开时把焦点放进弹窗，关闭时还给打开它的按钮；
// - Esc 关闭（等同取消），Tab / Shift+Tab 在弹窗内循环；
// - 模态弹窗打开时，背后的内容设为 inert：读屏和鼠标都碰不到。
// 可以叠：设备管理里点「解绑」，确认弹窗叠在上面，关掉后焦点回到「解绑」按钮。

const dialogStack = [];

function openDialog(node, { modal = true, opener = document.activeElement, initialFocus = null, onCancel = null } = {}) {
  if (dialogStack.some((entry) => entry.node === node)) return;
  dialogStack.push({ node, modal, opener, onCancel });
  node.classList.remove("hidden");
  syncInert();
  const target = initialFocus ?? focusableIn(node)[0];
  target?.focus();
}

function closeDialog(node, { restoreFocus = true } = {}) {
  const index = dialogStack.findIndex((entry) => entry.node === node);
  node.classList.add("hidden");
  if (index < 0) return;
  const [entry] = dialogStack.splice(index, 1);
  syncInert();
  if (!restoreFocus) return;
  const back = entry.opener;
  const usable = (element) =>
    element && document.contains(element) && !element.disabled && element.getClientRects().length > 0;
  // 打开它的按钮可能已经不在了（例如兑换成功后遮罩消失）：退回到当前模块的导航项，别让焦点掉到页面开头。
  const target = usable(back) ? back : $(".nav-item.active");
  if (usable(target)) target.focus();
}

function closeAllDialogs() {
  while (dialogStack.length) {
    const entry = dialogStack[dialogStack.length - 1];
    if (entry.onCancel) entry.onCancel();
    closeDialog(entry.node, { restoreFocus: false });
  }
}

/** Esc 和「取消 / 关闭」按钮走同一条路：有 onCancel 的交给它（它负责关），没有的直接关。 */
function cancelDialog(entry) {
  if (entry.onCancel) entry.onCancel();
  else closeDialog(entry.node);
}

function focusableIn(node) {
  const selector = "button, [href], input, select, textarea, [tabindex]:not([tabindex='-1'])";
  return Array.from(node.querySelectorAll(selector)).filter(
    (element) => !element.disabled && element.getClientRects().length > 0,
  );
}

/** 最上面的模态弹窗之外，body 下的其它内容都设为 inert（提示条除外，读屏要能听到）。 */
function syncInert() {
  const top = [...dialogStack].reverse().find((entry) => entry.modal);
  for (const child of document.body.children) {
    if (child.tagName === "SCRIPT" || child.id === "toast") continue;
    child.inert = Boolean(top) && child !== top.node;
  }
}

function handleDialogKeys(event) {
  const entry = dialogStack[dialogStack.length - 1];
  if (!entry) return;
  if (event.key === "Escape") {
    event.preventDefault();
    cancelDialog(entry);
    return;
  }
  if (event.key !== "Tab") return;
  const focusables = focusableIn(entry.node);
  if (!focusables.length) return;
  const first = focusables[0];
  const last = focusables[focusables.length - 1];
  const active = document.activeElement;
  const inside = Boolean(active) && entry.node.contains(active);
  if (event.shiftKey ? active === first || !inside : active === last || !inside) {
    event.preventDefault();
    (event.shiftKey ? last : first).focus();
  }
}

/** 二次确认，返回 Promise<boolean>。 */
function confirmDialog({ title, body, confirmLabel = "确认", danger = true }) {
  return new Promise((resolve) => {
    const node = $("#dialog-confirm");
    // 同一时间只问一件事：已经有确认弹窗开着，就当这次没确认。
    if (!node.classList.contains("hidden")) {
      resolve(false);
      return;
    }
    const yes = $("#confirm-yes");
    $("#confirm-title").textContent = title;
    $("#confirm-body").textContent = body;
    yes.textContent = confirmLabel;
    yes.className = danger ? "danger" : "primary";
    const finish = (answer) => {
      yes.onclick = null;
      closeDialog(node);
      resolve(answer);
    };
    yes.onclick = () => finish(true);
    // 默认焦点放在「取消」上：危险操作不该一按回车就执行。
    openDialog(node, { initialFocus: $("#confirm-no"), onCancel: () => finish(false) });
  });
}

// ============ 亮暗主题 ============

function applyTheme(theme) {
  document.documentElement.dataset.theme = theme;
  try {
    localStorage.setItem(L.THEME_KEY, theme);
  } catch {
    // 存不下就只在本次生效。
  }
  renderThemeToggles();
}

function renderThemeToggles() {
  // 按钮文字说的是「点了会怎样」，读屏读到的也是这句，不另设 aria-label（可见文字和可读名称要一致）。
  const light = document.documentElement.dataset.theme === "light";
  $$("[data-theme-toggle]").forEach((button) => {
    button.textContent = light ? "切换暗色" : "切换亮色";
  });
}
// ============ 首页：新手引导、近 7 天、公告 ============

const GUIDE_COLLAPSED_KEY = "dossier.guideCollapsed";

function guideCollapsed() {
  try {
    return localStorage.getItem(GUIDE_COLLAPSED_KEY) === "1";
  } catch {
    return false;
  }
}

function setGuideCollapsed(collapsed) {
  try {
    if (collapsed) localStorage.setItem(GUIDE_COLLAPSED_KEY, "1");
    else localStorage.removeItem(GUIDE_COLLAPSED_KEY);
  } catch {
    // 存不下就只在本次生效。
  }
  renderHome();
  (collapsed ? $("#guide-reopen") : $("#guide-collapse")).focus();
}

/** 读首页要的数据（生成流水），再画。流水是本地文件，不需要使用权也能读。 */
async function loadHome() {
  if (invoke) {
    try {
      const payload = await call("ledger_list");
      state.ledger = payload.records ?? [];
    } catch (error) {
      reportError(error);
    }
  }
  renderHome();
}

function renderHome() {
  const entitled = ["online", "offline"].includes(state.account?.tier);
  const steps = L.guideSteps({
    entitled,
    libraryRoot: state.settings?.libraryRoot,
    dramaCount: state.dramas.length,
    ledgerCount: state.ledger.length,
    jianyingReady: Boolean(state.settings?.jianyingDraftDir) || state.jianyingFound,
    creditBalance: state.creditBalance,
  });
  const progress = L.guideProgress(steps);
  const collapsed = guideCollapsed();
  const visible = L.guideVisible(state.ledger.length, collapsed);
  $("#home-guide").classList.toggle("hidden", !visible);
  // 自动收起（已经生成过报审包）时不提示「重新打开」；手动收起的才给入口。
  $("#guide-reopen-row").classList.toggle("hidden", visible || !collapsed);
  $("#guide-progress").textContent = `${progress.done}/${progress.total}`;
  $("#guide-steps").innerHTML = steps
    .map((step, index) => {
      const mark = step.done ? "✓" : String(index + 1);
      const status = step.done ? "已完成" : step.disabled ? "即将推出" : "未完成";
      const button = step.disabled
        ? `<button type="button" class="ghost" disabled>${esc(step.actionLabel)}</button>`
        : step.done
          ? ""
          : `<button type="button" class="ghost" data-onboard-action="${esc(step.action)}">${esc(step.actionLabel)}</button>`;
      return `<li class="guide-step ${step.done ? "done" : ""} ${step.disabled ? "disabled" : ""}">
        <span class="guide-mark" aria-hidden="true">${mark}</span>
        <div class="guide-body">
          <p class="guide-title">${esc(step.title)}<span class="sr-only">（${status}）</span></p>
          <p class="guide-detail">${esc(step.detail)}</p>
          ${button}
        </div>
      </li>`;
    })
    .join("");
  renderChart();
  renderAnnouncement();
}

function runOnboardAction(action) {
  if (action === "redeem") openRedeemDialog();
  // 积分没有独立模块页（只有账号菜单里的弹窗），所以不能走 switchModule：
  // 那会把工作区切成一个不存在的 module-credits，界面变空白。
  else if (action === "credits") openCreditsDialog();
  else if (action === "jianying") {
    state.settingsSub = "jianying";
    switchModule("settings");
  } else switchModule(action);
}

function renderChart() {
  const days = L.last7Days(state.ledger);
  const chart = L.barChart(days);
  const bars = chart.bars
    .map(
      (bar) => `<g>
        <rect class="chart-bar" x="${bar.x}" y="${bar.y}" width="${bar.width}" height="${Math.max(bar.height, 0)}" rx="3"></rect>
        <text class="chart-value" x="${bar.x + bar.width / 2}" y="${bar.y - 4}" text-anchor="middle">${bar.count || ""}</text>
        <text class="chart-label" x="${bar.x + bar.width / 2}" y="${chart.height - 6}" text-anchor="middle">${esc(bar.label)}</text>
      </g>`,
    )
    .join("");
  $("#chart-svg").innerHTML = `<svg viewBox="0 0 ${chart.width} ${chart.height}" role="presentation" focusable="false">
    <line class="chart-axis" x1="0" x2="${chart.width}" y1="${chart.baseline}" y2="${chart.baseline}"></line>
    ${bars}
  </svg>`;
  $("#chart-total").textContent = `共 ${chart.total} 次`;
  $("#chart-table tbody").innerHTML = days
    .map((day) => `<tr><td>${esc(day.key)}</td><td>${day.count}</td></tr>`)
    .join("");
}
// 公告轮播：6 秒一条；鼠标悬停或焦点在里面时暂停；用户点了「暂停」就一直停着；
// 系统设置了「减少动态效果」时不自动轮播。
const carousel = {
  index: 0,
  timer: null,
  paused: false,
  hovering: false,
  reducedMotion: window.matchMedia?.("(prefers-reduced-motion: reduce)") ?? null,
};

function renderAnnouncement() {
  const items = L.ANNOUNCEMENTS;
  const item = items[carousel.index % items.length];
  $("#announce-body").innerHTML = `<h4>${esc(item.title)}</h4><p>${esc(item.body)}</p>`;
  $("#announce-index").textContent = `${(carousel.index % items.length) + 1} / ${items.length}`;
  const stopped = carousel.paused || carousel.reducedMotion?.matches;
  $("#announce-toggle").textContent = stopped ? "播放" : "暂停";
  $("#announce-toggle").setAttribute("aria-pressed", String(Boolean(stopped)));
  scheduleAnnouncement();
}

function stepAnnouncement(delta, { manual = false } = {}) {
  const count = L.ANNOUNCEMENTS.length;
  carousel.index = (carousel.index + delta + count) % count;
  // 手动翻页时读屏要读出新内容；自动轮播时不打扰。
  $("#announce-body").setAttribute("aria-live", manual ? "polite" : "off");
  renderAnnouncement();
}

function scheduleAnnouncement() {
  clearTimeout(carousel.timer);
  carousel.timer = null;
  const homeVisible = !$("#module-home").classList.contains("hidden") && accountUi.screen === "app";
  if (carousel.paused || carousel.hovering || carousel.reducedMotion?.matches || !homeVisible) return;
  carousel.timer = setTimeout(() => stepAnnouncement(1), 6000);
}

function toggleAnnouncement() {
  if (carousel.reducedMotion?.matches) {
    toast("系统设置了「减少动态效果」，公告不自动轮播；可以用左右按钮翻页");
    return;
  }
  carousel.paused = !carousel.paused;
  renderAnnouncement();
}

// ============ 启动 ============

/** 启动：先等 Rust 端恢复登录（restoring），期间显示「正在恢复登录」。 */
async function boot() {
  renderThemeToggles();
  if (!invoke) {
    // 浏览器静态预览（无 Tauri IPC）：跳过登录直接展示主界面，便于布局验证。
    setScreen("app");
    switchModule("home");
    return;
  }
  if (tauriEvent?.listen) {
    await tauriEvent.listen("account-changed", (event) => applyAccount(event.payload));
  }
  await refreshAccount();
  // 事件可能在监听注册之前就发过了：恢复没做完时，每 1.5 秒再问一次，直到做完。
  while (state.account?.restoring) {
    await new Promise((resolve) => setTimeout(resolve, 1500));
    await refreshAccount();
  }
  // 离线剩余时间、快到期提醒按分钟走，不等后端事件。
  setInterval(() => {
    if (!state.account?.loggedIn) return;
    renderAccountChip();
    renderAccountBanners();
    renderAccountSettings();
  }, 60_000);
}

// ============ 设置：独立视图（七子页） ============

function openSettingsView() {
  document.querySelector(".app").classList.add("hidden");
  $("#settings-view").classList.remove("hidden");
  switchSettingsSub(state.settingsSub ?? "account");
  refreshSettingsData();
}

function closeSettingsView() {
  $("#settings-view")?.classList.add("hidden");
  // 只有在主界面这一屏才把主界面露出来：登录页、锁定页上不能透出主界面。
  if (accountUi.screen === "app") document.querySelector(".app")?.classList.remove("hidden");
}

function switchSettingsSub(name) {
  state.settingsSub = name;
  document.querySelectorAll(".settings-nav").forEach((node) => {
    node.classList.toggle("active", node.dataset.sub === name);
  });
  document.querySelectorAll(".settings-page").forEach((node) => {
    node.classList.toggle("hidden", node.id !== `sub-${name}`);
  });
}

// 打开设置视图时刷新各子页数据（账单流水、账号状态、工作目录回显等）。
async function refreshSettingsData() {
  if (state.root) $("#env-workdir").value = state.root;
  if (invoke) {
    await refreshAccount();
    await loadLedger();
  }
}

async function switchToSubAnd(name) {
  switchSettingsSub(name);
  if (name === "billing") await loadLedger();
}

// ---- 设置：账单（生成流水） ----

async function loadLedger() {
  if (!invoke) return;
  try {
    const payload = await call("ledger_list");
    $("#billing-month-count").textContent = String(payload.monthCount);
    $("#ledger-path").textContent = payload.path;
    const rows = payload.records
      .slice(0, 50)
      .map(
        (record) => `<tr>
          <td>${esc(record.ts.replace("T", " "))}</td>
          <td>${esc(record.dramaName)}</td>
          <td>${record.episodeCount}</td>
          <td>${formatBytes(Number(record.sizeBytes))}</td>
          <td class="mono">${esc(record.packagePath)}</td>
        </tr>`,
      )
      .join("");
    $("#ledger-body").innerHTML = rows ||
      '<tr><td colspan="5" class="empty">暂无生成记录。</td></tr>';
  } catch (error) {
    toast(String(error), true);
  }
}

// 修改密码、导出、注销见上面「设置：账号与授权」。
// ---- 设置：剪映 ----

async function jianyingDetect() {
  try {
    const result = await call("jianying_detect");
    const badge = $("#jianying-status");
    badge.textContent = result.installed ? "已安装" : "未安装";
    badge.className = `badge ${result.installed ? "status-ready" : "status-fail"}`;
    if (result.installed) {
      $("#jianying-dir").value = result.installDir;
      $("#jianying-version").value = result.version ?? "未知";
    } else {
      $("#jianying-version").value = "";
      toast(`未在 ${result.installDir} 找到剪映专业版`, true);
    }
    if (result.draftDir) $("#jianying-draft-dir").placeholder = result.draftDir;
    // 引导第 5 步：探测到草稿目录就算完成。
    state.jianyingFound = Boolean(result.draftDirExists);
  } catch (error) {
    reportError(error);
  }
}

async function jianyingSave() {
  // 目录字段复用 saveSettings 的带回链路（jianyingDir / jianyingDraftDir）。
  await saveSettings();
}

// ---- 设置：AI 模板 ----

async function aiSave() {
  await saveSettings();
}

// ---- 设置：本机环境（工作目录） ----

async function envWorkdirPick() {
  try {
    const folder = await call("pick_folder");
    if (!folder) return;
    $("#env-workdir").value = folder;
    // 工作目录与「剧目库」根目录是同一个概念，同步后提示重新扫描。
    $("#library-root").value = folder;
    toast("工作目录已同步到剧目库，请回「剧目库」重新扫描");
  } catch (error) {
    toast(String(error), true);
  }
}

function envWorkdirApply() {
  const folder = $("#env-workdir").value.trim();
  if (!folder) return;
  $("#library-root").value = folder;
  toast("工作目录已同步到剧目库，请回「剧目库」重新扫描");
}

// ============ 挂载：四步流程 ============

function switchMountStep(step) {
  state.mountStep = step;
  document.querySelectorAll("#mount-steps .step").forEach((node) => {
    node.classList.toggle("active", Number(node.dataset.step) === step);
  });
  for (let index = 1; index <= 4; index += 1) {
    $(`#mount-step-${index}`).classList.toggle("hidden", index !== step);
  }
  // 到执行步时展示当前计划摘要。
  if (step === 4) renderMountPlanSummary();
}

// 步骤 1 的剧目展示与上下文 chip 同步。
function refreshMountContext() {
  const selected = state.selected == null ? null : state.dramas[state.selected];
  const node = $("#mount-drama");
  if (!node || !selected) {
    if (node) {
      node.textContent = "尚未选择剧目。";
      node.classList.add("empty");
    }
    return;
  }
  node.textContent = `当前剧目：${selected.name}（${selected.episodes.length} 集）`;
  node.classList.remove("empty");
}

function requireMountDrama() {
  if (state.selected == null) {
    toast("请先到「剧目库」选择剧目", true);
    return null;
  }
  return state.dramas[state.selected];
}

// 绑定当前剧目：后端在剧目目录创建/复用挂载文案.csv，并保存挂载计划。
async function mountBind() {
  const drama = requireMountDrama();
  if (!drama) return;
  try {
    const check = await call("mount_csv_template", {
      root: state.root,
      dramaName: drama.name,
    });
    renderMountCsvCheck(check);
    $("#mount-csv-content").value = check.content ?? "";
    $("#mount-csv-path").textContent = check.csvPath;
    $("#mount-csv-save").disabled = false;
    state.mountPlan = await call("mount_plan_save", {
      root: state.root,
      dramaName: drama.name,
    });
    toast("已绑定当前剧目，文案模板就绪");
  } catch (error) {
    reportError(error);
  }
}

// 步骤 2：生成 / 打开模板（已存在则复用）。
async function mountCsvGenerate() {
  const drama = requireMountDrama();
  if (!drama) return;
  try {
    const check = await call("mount_csv_template", {
      root: state.root,
      dramaName: drama.name,
    });
    $("#mount-csv-content").value = check.content ?? "";
    renderMountCsvCheck(check);
    $("#mount-csv-path").textContent = check.csvPath;
    $("#mount-csv-save").disabled = false;
  } catch (error) {
    reportError(error);
  }
}

// 步骤 2：保存编辑内容并校验。
async function mountCsvSave() {
  const drama = requireMountDrama();
  if (!drama) return;
  try {
    const check = await call("mount_csv_save", {
      root: state.root,
      dramaName: drama.name,
      content: $("#mount-csv-content").value,
    });
    renderMountCsvCheck(check);
    $("#mount-csv-path").textContent = check.csvPath;
    $("#mount-csv-save").disabled = false;
  } catch (error) {
    reportError(error);
  }
}

function renderMountCsvCheck(payload) {
  const check = payload.check;
  const node = $("#mount-csv-result");
  const issues = (check.issues ?? [])
    .map((issue) => `<li>第 ${issue.row} 行：${esc(issue.message)}</li>`)
    .join("");
  node.innerHTML = `
    <strong class="${check.ok ? "status-pass" : "status-fail"}">
      ${check.ok ? "校验通过" : `发现 ${check.issues.length} 个问题`}
    </strong>（${check.rows} 行）<br />
    位置：${esc(payload.csvPath)}
    ${issues ? `<ul class="issue-list">${issues}</ul>` : ""}
  `;
  node.classList.remove("hidden");
}

function renderMountPlanSummary() {
  const drama = state.selected == null ? null : state.dramas[state.selected];
  const node = $("#mount-plan-summary");
  if (!drama && !state.mountPlan) {
    node.classList.add("hidden");
    return;
  }
  const plan = state.mountPlan;
  node.innerHTML = `
    <strong>挂载计划</strong><br />
    剧目：${esc(plan?.dramaName ?? drama?.name ?? "—")}<br />
    文案：${esc(plan?.csvPath ?? "尚未绑定")}<br />
    ${plan?.savedAt ? `保存时间：${esc(plan.savedAt)}` : ""}
  `;
  node.classList.remove("hidden");
}

// 步骤 4：保存计划（云端版接入后在此触发上传）。
async function mountPlanSave() {
  const drama = requireMountDrama();
  if (!drama) return;
  try {
    state.mountPlan = await call("mount_plan_save", {
      root: state.root,
      dramaName: drama.name,
    });
    renderMountPlanSummary();
    toast("挂载计划已保存到本机");
  } catch (error) {
    reportError(error);
  }
}

// ============ 转码：模式卡片与整页阻断 ============

function selectTranscodeMode(mode) {
  $("#exec-mode").value = mode;
  $("#mode-nudge").classList.toggle("active", mode === "nudge");
  $("#mode-recut").classList.toggle("active", mode === "recut");
  // 目标集数只对分集重组有意义。
  $("#exec-target-wrap").classList.toggle("hidden", mode !== "recut");
}

// 运行环境未就绪时整页阻断（竞品行为）。
function renderTranscodeGate() {
  const gate = $("#transcode-gate");
  const body = $("#transcode-body");
  if (!gate || !body) return;
  const tools = state.runtime ? [state.runtime.ffmpeg, state.runtime.ffprobe] : [];
  const ready = tools.length > 0 && tools.every((tool) => tool.found && tool.ok);
  gate.classList.toggle("hidden", ready);
  body.classList.toggle("hidden", !ready);
}

// ============ 处理版本（M4b-1） ============
//
// 流程：权属材料 → 档位 → 模板（存剧目目录的「版本模板.json」）→ 预览计划（只在本机算）→ 开始生成。
// 门槛都在后端（version.rs 的 prepare_version）：这里的校验只是让人早点看到问题，后端还会再查一遍。

/** 换剧、重新扫描后清空：别拿着上一部剧的计划点「开始生成」。 */
function resetVersionState() {
  state.version.drama = null;
  state.version.template = null;
  state.version.rights = null;
  state.version.plan = null;
  state.version.planKey = null;
  clearResult("#version-result");
  clearResult("#rights-status");
  syncVersionStartState();
}

/** 预览计划绑定的输入：剧、档位、模板（已保存的）。三者变了，旧计划作废。 */
function versionPlanKey() {
  const drama = state.version.drama;
  if (!drama) return null;
  return JSON.stringify([state.root, drama, state.version.strength, state.version.template]);
}

/** 「开始生成」只在「有计划、计划没有问题、计划没过期、表单没有未保存的改动、没有别的长任务」时可点。 */
function syncVersionStartState() {
  const button = $("#version-start");
  if (!button) return;
  const plan = state.version.plan;
  const fresh = plan && state.version.planKey === versionPlanKey();
  const dirty = !$("#template-dirty").classList.contains("hidden");
  button.disabled = Boolean(state.task) || !fresh || plan.problems.length > 0 || dirty;
}

function renderVersionGate() {
  const tools = state.runtime ? [state.runtime.ffmpeg, state.runtime.ffprobe] : [];
  const ready = tools.length > 0 && tools.every((tool) => tool.found && tool.ok);
  $("#version-gate").classList.toggle("hidden", ready);
  $("#version-body").classList.toggle("hidden", !ready);
}

function renderStrengths() {
  $("#version-strengths").innerHTML = L.STRENGTHS.map(
    (item) => `
      <button type="button" class="mode-card ${item.value === state.version.strength ? "active" : ""}"
        role="radio" aria-checked="${item.value === state.version.strength}" data-strength="${esc(item.value)}">
        <strong>${esc(item.label)}</strong>
        <span>${esc(item.detail)}</span>
      </button>`,
  ).join("");
}

function selectStrength(value) {
  if (!L.STRENGTHS.some((item) => item.value === value)) return;
  state.version.strength = value;
  renderStrengths();
  syncVersionStartState();
}

/** 切进模块或换剧时：读权属状态和模板。读失败如实显示，不当作「没有」。 */
async function enterVersionModule() {
  renderVersionGate();
  renderStrengths();
  const drama = state.selected == null ? null : state.dramas[state.selected];
  if (!drama) {
    $("#version-main").classList.add("hidden");
    $("#rights-declare").disabled = true;
    $("#rights-badge").textContent = "未选剧目";
    $("#rights-badge").className = "badge";
    return;
  }
  $("#rights-declare").disabled = false;
  if (state.version.drama === drama.name && state.version.template) {
    renderRights();
    return;
  }
  const root = state.root;
  try {
    const [rights, template] = await Promise.all([
      call("rights_status", { root, name: drama.name }),
      call("version_template_load", { root, name: drama.name }),
    ]);
    // 等待期间换了剧：这次结果作废。
    if (state.dramas[state.selected]?.name !== drama.name || state.root !== root) return;
    state.version.drama = drama.name;
    state.version.rights = rights;
    state.version.template = template;
    state.version.plan = null;
    fillTemplateForm(template);
    renderRights();
  } catch (error) {
    $("#version-main").classList.add("hidden");
    renderResult("#rights-status", {
      tone: "error",
      html: `<strong>读取这部剧的处理版本设置失败</strong><br />${esc(errorMessage(error))}`,
    });
    reportError(error);
  }
}

function errorMessage(error) {
  return error instanceof CommandError ? L.errorText(error) : String(error);
}

/** 权属区：没声明 / 文件变了 → 只显示声明区；声明有效 → 展开后面的步骤。 */
function renderRights() {
  const rights = state.version.rights;
  const badge = $("#rights-badge");
  const declared = rights?.declared;
  if (!declared) {
    badge.textContent = "未声明";
    badge.className = "badge status-warn";
    clearResult("#rights-status");
    $("#rights-declare").textContent = "选择文件并声明";
    $("#version-main").classList.add("hidden");
    return;
  }
  const when = L.formatIso(declared.declaredAt);
  if (rights.changed) {
    badge.textContent = "需要重新声明";
    badge.className = "badge status-fail";
    renderResult("#rights-status", {
      tone: "error",
      html: `<strong>${esc(rights.message ?? "权属材料文件变了，请重新声明")}</strong><br />
        上次声明：${esc(declared.file)}（${esc(when)}）`,
    });
    $("#rights-declare").textContent = "重新选择并声明";
    $("#version-main").classList.add("hidden");
    return;
  }
  badge.textContent = "已声明";
  badge.className = "badge status-ready";
  renderResult("#rights-status", {
    tone: "ok",
    html: `已声明：${esc(declared.file)}<br />
      声明时间：${esc(when)} · 校验值 ${esc(String(declared.sha256).slice(0, 16))}…`,
  });
  $("#rights-declare").textContent = "换一份材料重新声明";
  $("#version-main").classList.remove("hidden");
  syncVersionStartState();
}

async function declareRights() {
  let drama;
  try {
    drama = selectedDrama();
  } catch (error) {
    reportError(error);
    return;
  }
  try {
    const file = await call("pick_rights_file");
    if (!file) return;
    const ok = await confirmDialog({
      title: "声明权属材料",
      body: `确认《${drama.name}》的版权登记证书或授权书是这份文件：\n${file}\n\n程序只记录文件位置和校验值，不核验真伪；声明会记入本机的处理台账。`,
      confirmLabel: "确认声明",
      danger: false,
    });
    if (!ok) return;
    const rights = await call("rights_declare", { root: state.root, name: drama.name, file });
    // 换了材料，任务哈希就变了：旧计划作废。
    state.version.rights = rights;
    state.version.plan = null;
    clearResult("#version-result");
    renderRights();
    toast("权属材料已声明");
  } catch (error) {
    reportError(error);
  }
}

// ---- 模板表单 ----

const CORNERS = ["top_left", "top_right", "bottom_left", "bottom_right"];
const COLOR_PRESETS = ["original", "warm", "cool", "high_contrast", "low_saturation"];

const TEMPLATE_FIELDS = {
  coverAtSec: "#tpl-cover-at",
  subtitleSize: "#tpl-subtitle-size",
  watermarkOpacity: "#tpl-watermark-opacity",
  musicVolume: "#tpl-music-volume",
  aiContentProducer: "#tpl-ai-producer",
  watermarkCorner: "#tpl-watermark-corner",
  musicLicensed: "#tpl-music-licensed",
  recutCount: "#tpl-recut-count",
  subtitleDir: "#tpl-subtitle-dir",
};

function fillTemplateForm(template) {
  const text = (selector, value) => {
    $(selector).value = value ?? "";
  };
  text("#tpl-intro", template.introPath);
  text("#tpl-outro", template.outroPath);
  text("#tpl-cover-at", template.coverAtSec);
  text("#tpl-cover-text", template.coverText);
  text("#tpl-subtitle-dir", template.subtitleDir);
  text("#tpl-subtitle-size", template.subtitleSize);
  text("#tpl-subtitle-margin", template.subtitleMarginV);
  $("#tpl-color").value = COLOR_PRESETS.includes(template.colorPreset) ? template.colorPreset : "original";
  text("#tpl-watermark", template.watermarkText);
  $("#tpl-watermark-corner").value = CORNERS.includes(template.watermarkCorner)
    ? template.watermarkCorner
    : "top_right";
  text("#tpl-watermark-opacity", template.watermarkOpacity);
  $("#tpl-ai").checked = Boolean(template.aiMarked);
  text("#tpl-ai-producer", template.aiContentProducer);
  $("#tpl-recut").checked = Boolean(template.recut);
  text("#tpl-recut-count", template.recutCount);
  text("#tpl-music", template.musicPath);
  text("#tpl-music-volume", template.musicVolume);
  $("#tpl-music-licensed").checked = Boolean(template.musicLicensed);
  clearFieldErrors(Object.values(TEMPLATE_FIELDS));
  syncTemplateToggles();
  refreshTemplateDirty();
}

/** 读表单。空的数字框按「没填」处理（Number("") 是 0，不能直接用）。 */
function readTemplateForm() {
  const text = (selector) => $(selector).value.trim();
  const optional = (selector) => text(selector) || null;
  const number = (selector) => (text(selector) === "" ? null : Number(text(selector)));
  return {
    introPath: optional("#tpl-intro"),
    outroPath: optional("#tpl-outro"),
    coverAtSec: number("#tpl-cover-at") ?? 1,
    coverText: optional("#tpl-cover-text"),
    subtitleDir: optional("#tpl-subtitle-dir"),
    subtitleSize: number("#tpl-subtitle-size") ?? 56,
    subtitleMarginV: number("#tpl-subtitle-margin"),
    colorPreset: $("#tpl-color").value,
    watermarkText: text("#tpl-watermark"),
    watermarkCorner: $("#tpl-watermark-corner").value,
    watermarkOpacity: number("#tpl-watermark-opacity") ?? 0.5,
    aiMarked: $("#tpl-ai").checked,
    aiContentProducer: text("#tpl-ai-producer"),
    recut: $("#tpl-recut").checked,
    recutCount: $("#tpl-recut").checked ? number("#tpl-recut-count") : null,
    musicPath: optional("#tpl-music"),
    musicVolume: number("#tpl-music-volume") ?? 0.5,
    musicLicensed: $("#tpl-music-licensed").checked,
  };
}

/** AI 字段和「重新分几集」只在勾了对应选项时出现；勾了 AI 时右下角不可选。 */
function syncTemplateToggles() {
  const ai = $("#tpl-ai").checked;
  $("#tpl-ai-fields").classList.toggle("hidden", !ai);
  $("#tpl-recut-count-wrap").classList.toggle("hidden", !$("#tpl-recut").checked);
  const corner = $("#tpl-watermark-corner");
  const bottomRight = corner.querySelector('option[value="bottom_right"]');
  bottomRight.disabled = ai;
  bottomRight.textContent = ai ? "右下角（AI 标识占用）" : "右下角";
  if (ai && corner.value === "bottom_right") corner.value = "bottom_left";
}

/** 表单和已保存的模板不一样时亮「有改动未保存」：预览和生成只认已保存的模板。 */
function refreshTemplateDirty() {
  const saved = state.version.template;
  const dirty = Boolean(saved) && JSON.stringify(readTemplateForm()) !== JSON.stringify(normalizeTemplate(saved));
  $("#template-dirty").classList.toggle("hidden", !dirty);
  syncVersionStartState();
}

/** 后端返回的模板按表单的读法规整一遍（字段顺序、缺省值），才能和表单比较。 */
function normalizeTemplate(template) {
  return {
    introPath: template.introPath ?? null,
    outroPath: template.outroPath ?? null,
    coverAtSec: template.coverAtSec ?? 1,
    coverText: template.coverText || null,
    subtitleDir: template.subtitleDir ?? null,
    subtitleSize: template.subtitleSize ?? 56,
    subtitleMarginV: template.subtitleMarginV ?? null,
    colorPreset: template.colorPreset ?? "original",
    watermarkText: template.watermarkText ?? "",
    watermarkCorner: template.watermarkCorner ?? "top_right",
    watermarkOpacity: template.watermarkOpacity ?? 0.5,
    aiMarked: Boolean(template.aiMarked),
    aiContentProducer: template.aiContentProducer ?? "",
    recut: Boolean(template.recut),
    recutCount: template.recut ? template.recutCount ?? null : null,
    musicPath: template.musicPath ?? null,
    musicVolume: template.musicVolume ?? 0.5,
    musicLicensed: Boolean(template.musicLicensed),
  };
}

async function saveTemplate() {
  let drama;
  try {
    drama = selectedDrama();
  } catch (error) {
    reportError(error);
    return;
  }
  const template = readTemplateForm();
  clearFieldErrors(Object.values(TEMPLATE_FIELDS));
  const problems = L.templateProblems(template);
  problems.forEach(([field, message]) => setFieldError(TEMPLATE_FIELDS[field], message));
  if (problems.length) {
    toast(problems[0][1], true);
    return;
  }
  try {
    const saved = await call("version_template_save", { root: state.root, name: drama.name, template });
    state.version.template = saved;
    state.version.plan = null;
    clearResult("#version-result");
    fillTemplateForm(saved);
    toast("模板已保存到剧目目录的「版本模板.json」");
  } catch (error) {
    reportError(error);
  }
}

async function pickAsset(button) {
  try {
    const path = await call("pick_version_asset", { kind: button.dataset.pickAsset });
    if (!path) return;
    $(button.dataset.pickTarget).value = path;
    refreshTemplateDirty();
  } catch (error) {
    reportError(error);
  }
}

// ---- 预览计划与开始生成 ----

function renderPlan(plan) {
  const totalSec = plan.episodes.reduce((sum, episode) => sum + Number(episode.durationSec || 0), 0);
  const rows = plan.episodes
    .map(
      (episode) => `<tr>
        <td>${esc(episode.name)}</td>
        <td>${Number(episode.durationSec).toFixed(1)} 秒</td>
        <td>${esc(episode.width)}×${esc(episode.height)}</td>
        <td>${episode.subtitle ? "有" : "—"}</td>
      </tr>`,
    )
    .join("");
  const steps = plan.steps.map((step) => `<li>${esc(step)}</li>`).join("");
  const problems = plan.problems.length
    ? `<strong>还不能开始生成，请先解决：</strong>
       <ul class="plan-list">${plan.problems.map((problem) => `<li>${esc(problem)}</li>`).join("")}</ul>`
    : `<strong>计划可以执行</strong>（${esc(strengthLabel(plan.strength))}档，${plan.episodes.length} 集，原片共约 ${Math.round(totalSec / 60)} 分钟）`;
  renderResult("#version-result", {
    tone: plan.problems.length ? "error" : "ok",
    html: `${problems}<br />
      输出位置：${esc(plan.outputDir)}
      <p>每集要做的处理：</p><ul class="plan-list">${steps}</ul>
      <div class="table-wrap"><table>
        <thead><tr><th>分集</th><th>时长</th><th>原分辨率</th><th>字幕</th></tr></thead>
        <tbody>${rows || '<tr><td colspan="4" class="empty">没有分集</td></tr>'}</tbody>
      </table></div>`,
  });
}

function strengthLabel(value) {
  return L.STRENGTHS.find((item) => item.value === value)?.label ?? value;
}

async function planVersion() {
  let drama;
  try {
    drama = selectedDrama();
  } catch (error) {
    reportError(error);
    return;
  }
  if (!$("#template-dirty").classList.contains("hidden")) {
    toast("模板有改动还没保存：预览只用已保存的模板，请先保存或放弃改动", true);
    return;
  }
  if (!beginTask(`正在核对《${drama.name}》的分集（只在本机计算，不扣次数）`)) return;
  const key = versionPlanKey();
  try {
    const plan = await call("version_plan", {
      root: state.root,
      name: drama.name,
      strength: state.version.strength,
      outputRoot: state.settings?.outputDir ?? null,
    });
    state.version.plan = plan;
    state.version.planKey = key;
    renderPlan(plan);
  } catch (error) {
    state.version.plan = null;
    renderResult("#version-result", {
      tone: "error",
      html: `<strong>预览失败</strong><br />${esc(errorMessage(error))}`,
    });
    reportProbeFailure(error);
  } finally {
    endTask();
  }
}

async function startVersion() {
  let drama;
  try {
    drama = selectedDrama();
  } catch (error) {
    reportError(error);
    return;
  }
  const plan = state.version.plan;
  if (!plan || state.version.planKey !== versionPlanKey() || plan.problems.length) {
    toast("请先重新预览计划", true);
    return;
  }
  const ok = await confirmDialog({
    title: "开始生成处理版本",
    body:
      `《${drama.name}》· ${strengthLabel(plan.strength)}档 · ${plan.episodes.length} 集\n` +
      `输出到：${plan.outputDir}\n\n` +
      `积分：本剧 ${plan.episodes.length} 集，按每集 5 积分计算，预计消耗 ` +
      `${plan.episodes.length * 5} 积分（准确数额以服务端签发时为准）。\n` +
      `这笔积分在开始处理前就会扣掉。如果处理中途失败，积分不退——` +
      `视频只在本机处理、服务端看不到处理结果，无法判断真假。\n\n` +
      "开始后会向服务端申请一份处理配方并立即用掉。配方只能用一次：中途暂停、终止可以继续，" +
      "但如果要换档位或改模板，就得删掉这次的版本目录重新开始。",
    confirmLabel: "申请配方并开始",
    danger: false,
  });
  if (!ok) return;
  if (!beginTask(`正在为《${drama.name}》申请处理配方`)) return;
  try {
    const job = await call("version_start", {
      root: state.root,
      name: drama.name,
      strength: plan.strength,
      outputRoot: state.settings?.outputDir ?? null,
    });
    state.jobs = L.mergeJob(state.jobs, job);
    renderJobs();
    // 版本目录生成后，这份计划就不能再用了（后端会因为「已有处理版本」拒绝）。
    state.version.plan = null;
    renderResult("#version-result", {
      tone: "ok",
      html: `<strong>已加入任务队列</strong><br />
        ${esc(job.title)}：共 ${job.total} 步，进度在「任务」里看，可以随时暂停或终止。`,
    });
    toast("处理版本已开始生成", false, { label: "查看任务", run: () => switchModule("jobs") });
  } catch (error) {
    renderResult("#version-result", {
      tone: "error",
      html: `<strong>没有开始生成</strong><br />${esc(errorMessage(error))}`,
    });
    reportError(error);
  } finally {
    endTask();
  }
}

// ============ 任务（M4b-1） ============

async function loadJobs() {
  if (!invoke) return;
  try {
    state.jobs = await call("job_list");
    renderJobs();
  } catch (error) {
    reportError(error);
  }
}

function renderJobsBadge() {
  const count = L.activeJobCount(state.jobs);
  const badge = $("#jobs-count");
  badge.textContent = String(count);
  badge.classList.toggle("hidden", count === 0);
}

function renderJobs() {
  renderJobsBadge();
  const list = $("#jobs-list");
  if (!state.jobs.length) {
    list.innerHTML = '<div class="empty">还没有任务。在「处理版本」里开始生成后，任务会出现在这里。</div>';
    return;
  }
  list.innerHTML = state.jobs.map(renderJobCard).join("");
}

function renderJobCard(job) {
  const status = L.jobStateLabel(job.state);
  const tone = { ok: "status-ready", warn: "status-warn", err: "status-fail" }[status.tone] ?? "";
  const actions = L.jobActions(job);
  const key = `job-${job.id}`;
  state.paths[key] = job.outputDir;
  const buttons = [
    actions.pause ? `<button class="ghost" type="button" data-job-action="pause" data-job-id="${esc(job.id)}">暂停</button>` : "",
    actions.resume ? `<button class="primary" type="button" data-job-action="resume" data-job-id="${esc(job.id)}">继续</button>` : "",
    actions.terminate ? `<button class="danger" type="button" data-job-action="terminate" data-job-id="${esc(job.id)}">终止</button>` : "",
    `<button class="ghost" type="button" data-open-key="${esc(key)}">打开目录</button>`,
  ].join("");
  const steps = job.steps
    .map(
      (step) => `<tr>
        <td>${esc(step.name)}</td>
        <td class="${step.status === "failed" ? "status-fail" : step.status === "done" ? "status-pass" : ""}">${esc(L.stepStateLabel(step.status))}</td>
        <td>${esc(step.detail ?? "")}</td>
      </tr>`,
    )
    .join("");
  const open = state.jobsOpen.has(job.id) ? " open" : "";
  return `<article class="job-card" aria-label="${esc(job.title)}">
    <div class="job-head">
      <strong>${esc(job.title)}</strong>
      <span class="badge ${tone}">${esc(status.label)}</span>
      ${buttons}
    </div>
    <div class="job-track" role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${L.jobPercent(job)}">
      <span style="width: ${L.jobPercent(job)}%"></span>
    </div>
    <div class="job-meta">${esc(L.jobProgressText(job))} · 创建于 ${esc(L.formatIso(job.createdAt))} · ${esc(job.outputDir)}</div>
    ${job.error ? `<div class="job-error">${esc(job.error)}</div>` : ""}
    <details data-job-details="${esc(job.id)}"${open}>
      <summary>每集情况</summary>
      <div class="table-wrap"><table>
        <thead><tr><th>步骤</th><th>状态</th><th>说明</th></tr></thead>
        <tbody>${steps}</tbody>
      </table></div>
    </details>
  </article>`;
}

async function runJobAction(action, id) {
  const job = state.jobs.find((item) => item.id === id);
  if (!job) return;
  if (action === "terminate") {
    const ok = await confirmDialog({
      title: "终止任务",
      body: `终止「${job.title}」：正在处理的这一集立刻停下，半成品会删掉；已经做完的集保留，之后可以「继续」。`,
      confirmLabel: "终止",
    });
    if (!ok) return;
  }
  const command = { pause: "job_pause", resume: "job_resume", terminate: "job_terminate" }[action];
  if (!command) return;
  try {
    const view = await call(command, { id });
    state.jobs = L.mergeJob(state.jobs, view);
    renderJobs();
    if (action === "pause" && view.state === "running") toast("会在当前这一集做完后暂停");
  } catch (error) {
    reportError(error);
  }
}

function handleJobProgress(view) {
  const previous = state.jobs.find((job) => job.id === view?.id);
  state.jobs = L.mergeJob(state.jobs, view);
  renderJobs();
  if (previous && previous.state !== view.state) {
    if (view.state === "done") toast(`「${view.title}」已完成`);
    if (view.state === "failed") toast(`「${view.title}」失败：${view.error ?? "原因未知"}`, true);
  }
}

// ============ 关于 ============

async function loadVersion() {
  try {
    const version = await window.__TAURI__?.app?.getVersion?.();
    $("#about-version").textContent = version ?? "未知";
  } catch {
    $("#about-version").textContent = "未知";
  }
}

// ============ 事件绑定 ============

$("#browse-root").addEventListener("click", () => browse("#library-root"));
document.querySelectorAll(".nav-item").forEach((node) => {
  node.addEventListener("click", () => switchModule(node.dataset.module));
});
$("#env-chip").addEventListener("click", () => {
  gotoRuntimeSettings();
});
// ============ 登录 / 注册 / 重设密码 ============
$$("[data-auth-view]").forEach((node) => {
  node.addEventListener("click", () => {
    $("#auth-notice").classList.add("hidden");
    switchAuthView(node.dataset.authView);
  });
});
$$("[data-send-code]").forEach((node) => node.addEventListener("click", () => sendCode(node)));
$("#auth-login-form").addEventListener("submit", loginAccount);
$("#auth-register-form").addEventListener("submit", registerAccount);
$("#auth-reset-form").addEventListener("submit", resetPassword);
$("#register-terms").addEventListener("change", syncRegisterSubmit);
$("#register-code").addEventListener("input", (event) => {
  event.target.value = event.target.value.replace(/\D/g, "").slice(0, 6);
});
$("#reset-code").addEventListener("input", (event) => {
  event.target.value = event.target.value.replace(/\D/g, "").slice(0, 6);
});
// ============ 锁定页 / 账号横幅 / 账号菜单 ============
$("#lock-retry").addEventListener("click", () => (state.account ? retryAccount() : refreshAccount()));
$("#lock-logout").addEventListener("click", logoutAccount);
$("#account-banners").addEventListener("click", (event) => {
  const action = event.target.closest("[data-banner-action]")?.dataset.bannerAction;
  if (action === "retry") retryAccount();
  else if (action === "redeem") openRedeemDialog();
  const dismiss = event.target.closest("[data-banner-dismiss]")?.dataset.bannerDismiss;
  if (dismiss) {
    accountUi.dismissed.add(dismiss);
    renderAccountBanners();
  }
});
$("#account-chip").addEventListener("click", (event) => {
  event.stopPropagation();
  if ($("#account-menu").classList.contains("hidden")) openAccountMenu();
  else closeAccountMenu({ restoreFocus: true });
});
$("#locked-action").addEventListener("click", () => {
  if (state.account?.lockReason === "needs_activation") openDevicesDialog({ activation: true });
  else openRedeemDialog();
});
// ============ 弹窗 ============
$("#dialog-redeem form").addEventListener("submit", redeemCard);
$("#redeem-code").addEventListener("input", formatRedeemInput);
$("#devices-body").addEventListener("click", (event) => {
  const id = event.target.closest("[data-unbind]")?.dataset.unbind;
  if (id) unbindDevice(id);
});
$("#devices-activate").addEventListener("click", activateDevice);
$("#devices-logout").addEventListener("click", logoutAccount);
$("#dialog-delete form").addEventListener("submit", deleteAccount);
["#delete-password", "#delete-email"].forEach((selector) => {
  $(selector).addEventListener("input", syncDeleteSubmit);
});
// 弹窗里的「关闭 / 取消」：和 Esc 走同一条路。
document.addEventListener("click", (event) => {
  const close = event.target.closest("[data-dialog-close]");
  if (!close) return;
  const entry = dialogStack.find((item) => item.node.contains(close));
  if (entry) cancelDialog(entry);
});
// 点遮罩空白处等于取消（点在弹窗本身上不算）。
$$(".modal").forEach((node) => {
  node.addEventListener("click", (event) => {
    if (event.target !== node) return;
    const entry = dialogStack.find((item) => item.node === node);
    if (entry) cancelDialog(entry);
  });
});
// ============ 主题 ============
$$("[data-theme-toggle]").forEach((node) => {
  node.addEventListener("click", () =>
    applyTheme(document.documentElement.dataset.theme === "light" ? "dark" : "light"),
  );
});
// ============ 首页 ============
$("#guide-collapse").addEventListener("click", () => setGuideCollapsed(true));
$("#guide-reopen").addEventListener("click", () => setGuideCollapsed(false));
$("#guide-steps").addEventListener("click", (event) => {
  const action = event.target.closest("[data-onboard-action]")?.dataset.onboardAction;
  if (action) runOnboardAction(action);
});
$("#announce-prev").addEventListener("click", () => stepAnnouncement(-1, { manual: true }));
$("#announce-next").addEventListener("click", () => stepAnnouncement(1, { manual: true }));
$("#announce-toggle").addEventListener("click", toggleAnnouncement);
const announce = $(".announce");
["mouseenter", "focusin"].forEach((type) =>
  announce.addEventListener(type, () => {
    carousel.hovering = true;
    scheduleAnnouncement();
  }),
);
["mouseleave", "focusout"].forEach((type) =>
  announce.addEventListener(type, () => {
    carousel.hovering = announce.matches(":hover") || announce.contains(document.activeElement);
    scheduleAnnouncement();
  }),
);
carousel.reducedMotion?.addEventListener?.("change", renderAnnouncement);
// ============ 设置独立视图 ============
$("#settings-back").addEventListener("click", () => {
  switchModule(state.module === "settings" ? "home" : state.module);
});
document.querySelectorAll(".settings-nav").forEach((node) => {
  node.addEventListener("click", () => switchToSubAnd(node.dataset.sub));
});
$("#password-change-form").addEventListener("submit", changePassword);
$("#open-change-password").addEventListener("click", () => {
  const form = $("#password-change-form");
  if (!form.classList.contains("hidden")) return closePasswordForm();
  form.classList.remove("hidden");
  $("#open-change-password").setAttribute("aria-expanded", "true");
  $("#old-password").focus();
});
$("#cancel-change-password").addEventListener("click", closePasswordForm);
$("#logout").addEventListener("click", logoutAccount);
$("#copy-device-code").addEventListener("click", copyDeviceCode);
$("#account-sync").addEventListener("click", syncLicense);
$("#account-export").addEventListener("click", exportAccountData);
$("#open-delete-account").addEventListener("click", openDeleteDialog);
$("#ledger-refresh").addEventListener("click", () => loadLedger());
$("#jianying-detect").addEventListener("click", jianyingDetect);
$("#jianying-save").addEventListener("click", jianyingSave);
$("#ai-save").addEventListener("click", aiSave);
$("#env-workdir-pick").addEventListener("click", envWorkdirPick);
$("#env-workdir").addEventListener("keydown", (event) => {
  if (event.key === "Enter") envWorkdirApply();
});
// ============ 挂载 ============
document.querySelectorAll("#mount-steps .step").forEach((node) => {
  node.addEventListener("click", () => switchMountStep(Number(node.dataset.step)));
});
$("#mount-bind").addEventListener("click", mountBind);
$("#mount-csv-generate").addEventListener("click", mountCsvGenerate);
$("#mount-csv-save").addEventListener("click", mountCsvSave);
$("#mount-plan-save").addEventListener("click", mountPlanSave);
// ============ 转码 ============
$("#mode-nudge").addEventListener("click", () => selectTranscodeMode("nudge"));
$("#mode-recut").addEventListener("click", () => selectTranscodeMode("recut"));
$("#gate-install").addEventListener("click", runtimeInstall);
// ============ 原有模块 ============
$("#rename-plan").addEventListener("click", buildRenamePlan);
$("#rename-apply").addEventListener("click", applyRename);
$("#bitrate-probe").addEventListener("click", probeBitrate);
$("#bitrate-fix").addEventListener("click", requestBitrateFix);
$("#bitrate-confirm-yes").addEventListener("click", applyBitrateFix);
$("#bitrate-confirm-no").addEventListener("click", cancelBitrateFix);

// 键盘：账号菜单开着时先交给菜单，否则交给最上面的弹窗（Esc 取消、Tab 循环）。
document.addEventListener("keydown", (event) => {
  if (handleAccountMenuKeys(event)) return;
  handleDialogKeys(event);
});
// 点菜单外面就关掉菜单。
document.addEventListener("click", (event) => {
  if (!event.target.closest("#account-menu, #account-chip")) closeAccountMenu();
});
$("#exec-run").addEventListener("click", executeTranscode);
$("#runtime-recheck").addEventListener("click", () => checkRuntime(true));
$("#runtime-install").addEventListener("click", runtimeInstall);
// 横幅里的安装按钮随 innerHTML 重建，用事件委托绑定。
$("#env-banner").addEventListener("click", (event) => {
  if (event.target instanceof Element && event.target.id === "env-install") {
    runtimeInstall();
  }
});
$("#erase-load-frame").addEventListener("click", loadEraseFrame);
$("#erase-detect").addEventListener("click", detectEraseRegions);
$("#erase-clear").addEventListener("click", clearEraseRegions);
$("#erase-preview-run").addEventListener("click", previewErase);
$("#erase-batch").addEventListener("click", startEraseBatch);
bindEraseCanvas();
$("#transcode-plan").addEventListener("click", transcodeAdvice);
$("#diff-run").addEventListener("click", diffReportPackages);
$("#diff-left-pick").addEventListener("click", () => browse("#diff-left"));
$("#diff-right-pick").addEventListener("click", () => browse("#diff-right"));
$("#platform-run").addEventListener("click", platformReview);
$("#draft-pick").addEventListener("click", () => browse("#draft-dir"));
$("#draft-export").addEventListener("click", exportDraft);
$("#draft-import").addEventListener("click", importDraft);
$("#publish-copy-generate").addEventListener("click", generatePublishCopy);
$("#publish-copy-load").addEventListener("click", loadPublishCopy);
$("#publish-copy-preview").addEventListener("click", previewPublishCopy);
$("#browse-output").addEventListener("click", () => browse("#output-dir"));
$("#credits-refresh").addEventListener("click", () => {
  loadCredits();
  loadInvites();
});
// 档位按钮是动态生成的，用事件委托。
$("#credits-tiers").addEventListener("click", (event) => {
  const button = event.target.closest("[data-tier]");
  if (button) startRecharge(button.dataset.tier);
});
$("#invite-copy").addEventListener("click", async () => {
  const code = $("#invite-code").value.trim();
  if (!code) return;
  try {
    await navigator.clipboard.writeText(code);
    toast("邀请码已复制");
  } catch {
    // 没有剪贴板权限时退回「选中让用户自己复制」。
    $("#invite-code").select();
    toast("请按 Ctrl+C / Cmd+C 复制选中的邀请码");
  }
});
// 扫描件：选了就填进只读输入框，结果里会逐份标明签章方式。
async function pickScan(targetId) {
  try {
    const file = await call("pick_scan_file");
    if (file) $(targetId).value = file;
  } catch (error) {
    reportError(error);
  }
}
$("#scan-cost-pick").addEventListener("click", () => pickScan("#scan-cost-sheet"));
$("#scan-ownership-pick").addEventListener("click", () => pickScan("#scan-ownership"));
$("#scan-cost-clear").addEventListener("click", () => {
  $("#scan-cost-sheet").value = "";
});
$("#scan-ownership-clear").addEventListener("click", () => {
  $("#scan-ownership").value = "";
});
$("#scan").addEventListener("click", scan);
$("#analyze").addEventListener("click", analyze);
$("#generate").addEventListener("click", generate);
$("#save-settings").addEventListener("click", saveSettings);
$("#save-settings-env").addEventListener("click", saveSettings);
$("#settings-font-pick").addEventListener("click", async () => {
  try {
    const path = await call("pick_version_asset", { kind: "font" });
    if (!path) return;
    $("#settings-font").value = path;
    refreshDirtyBadge();
  } catch (error) {
    reportError(error);
  }
});
// 处理版本（M4b-1）。
$("#version-gate-install").addEventListener("click", runtimeInstall);
$("#rights-declare").addEventListener("click", declareRights);
$("#template-save").addEventListener("click", saveTemplate);
$("#template-reload").addEventListener("click", () => {
  if (state.version.template) fillTemplateForm(state.version.template);
});
$("#version-plan").addEventListener("click", planVersion);
$("#version-start").addEventListener("click", startVersion);
$("#jobs-refresh").addEventListener("click", loadJobs);
// 服务端处理（M5b-8）
$("#server-upload-start").addEventListener("click", startServerUpload);
$("#server-upload-abort").addEventListener("click", abortServerUpload);
$("#server-submit").addEventListener("click", submitServerTask);
$("#server-refresh").addEventListener("click", refreshServerTask);
// 模板表单任一改动：刷新「未保存」、清字段错误、同步联动开关。
$$("#module-version input, #module-version select").forEach((node) => {
  const onChange = () => {
    setFieldError(`#${node.id}`, null);
    syncTemplateToggles();
    refreshTemplateDirty();
  };
  node.addEventListener("input", onChange);
  node.addEventListener("change", onChange);
});
// 记住哪些任务展开了「每集情况」：进度事件会重画列表，不能每次都收起来。
$("#jobs-list").addEventListener(
  "toggle",
  (event) => {
    const id = event.target?.dataset?.jobDetails;
    if (!id) return;
    if (event.target.open) state.jobsOpen.add(id);
    else state.jobsOpen.delete(id);
  },
  true,
);
$("#reset-settings").addEventListener("click", () => {
  fillRules(DEFAULT_RULES);
  $("#settings-output").value = "";
  refreshDirtyBadge();
  // ffprobe 路径在「本机环境」里，可能是一键配置装出来的，这里不动它。
  toast("已恢复默认阈值；点击「保存设置」后生效");
});
$("#library-root").addEventListener("keydown", (event) => {
  if (event.key === "Enter") scan();
});
$("#library-root").addEventListener("input", renderEmptyGuide);

// 设置页任一改动都刷新「未保存」标记，避免改了以为已经生效。
["#rule-min-episodes", "#rule-min-duration", "#rule-max-duration", "#rule-max-bitrate",
  "#rule-prefer-portrait", "#settings-output", "#ffprobe-path", "#settings-font",
  "#settings-software-encode",
].forEach((selector) => {
  const node = $(selector);
  node.addEventListener("input", refreshDirtyBadge);
  node.addEventListener("change", refreshDirtyBadge);
});

// 用户改了「本次覆盖」的路径就清掉旧的字段错误提示。
["#exec-output", "#exec-target", "#erase-input", "#erase-output", "#erase-regions",
  "#total-investment", "#actor-ratio", "#bitrate-min", "#diff-left", "#diff-right",
  "#draft-dir", "#rule-min-episodes", "#rule-min-duration", "#rule-max-duration",
  "#rule-max-bitrate",
].forEach((selector) => {
  $(selector).addEventListener("input", () => setFieldError(selector, null));
});

// 结果区里的「打开目录 / 复制路径」用事件委托，避免把路径拼进 HTML。
document.addEventListener("click", (event) => {
  const guideButton = event.target.closest("[data-guide-action]");
  if (guideButton) {
    if (guideButton.dataset.guideAction === "browse") browse("#library-root");
    else if (guideButton.dataset.guideAction === "scan") scan();
    return;
  }
  const accountButton = event.target.closest("[data-account-action]");
  if (accountButton) {
    runAccountAction(accountButton.dataset.accountAction);
    return;
  }
  const jobButton = event.target.closest("[data-job-action]");
  if (jobButton) {
    runJobAction(jobButton.dataset.jobAction, jobButton.dataset.jobId);
    return;
  }
  const strengthButton = event.target.closest("[data-strength]");
  if (strengthButton) {
    selectStrength(strengthButton.dataset.strength);
    return;
  }
  const assetButton = event.target.closest("[data-pick-asset]");
  if (assetButton) {
    pickAsset(assetButton);
    return;
  }
  const folderButton = event.target.closest("[data-pick-folder]");
  if (folderButton) {
    browse(folderButton.dataset.pickFolder).then(refreshTemplateDirty);
    return;
  }
  const gotoButton = event.target.closest("[data-goto-module]");
  if (gotoButton) {
    switchModule(gotoButton.dataset.gotoModule);
    return;
  }
  const openButton = event.target.closest("[data-open-key]");
  if (openButton) {
    openPath(openButton.dataset.openKey);
    return;
  }
  const copyButton = event.target.closest("[data-copy-key]");
  if (copyButton) {
    copyPath(copyButton.dataset.copyKey);
  }
});

// 后端逐集报进度，界面据此画确定进度条。
if (tauriEvent?.listen) {
  tauriEvent.listen("probe-progress", (event) => handleProbeProgress(event.payload));
  tauriEvent.listen("task-progress", (event) => handleTaskProgress(event.payload));
  tauriEvent.listen("erase-progress", (event) => handleEraseProgress(event.payload));
  tauriEvent.listen("job-progress", (event) => handleJobProgress(event.payload));
  tauriEvent.listen("server-upload-progress", (event) => handleServerUploadProgress(event.payload));
}

// 启动：先等恢复登录，再决定显示登录页、锁定页还是主界面（浏览器预览无 IPC 时直接进主界面便于布局验证）。
boot();
if (invoke) {
  // 设置、本机环境、平台规则都是本机信息，不需要登录（闸门里是「不设闸门」）。
  loadSettings().then(() => {
    checkRuntime();
    // 引导「准备素材」「剪映」两步看设置里记下的目录。
    if (state.module === "home") renderHome();
  });
  loadPlatformProfiles();
  // 重启后没做完的任务显示为「已暂停」：侧栏数字要一开始就对。
  loadJobs();
}
loadVersion();

// 首屏就画好三步引导，不必等设置读完（读失败时页面也不该是一片空白）。
renderEmptyGuide();
