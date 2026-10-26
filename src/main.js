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
  account: null, // 最近一次 account_* 命令回包（含 loggedIn / email / path）。
  mountStep: 1,
  mountPlan: null,
  publishRows: [],
  // 设置独立视图当前子页（account/billing/jianying/ai/env/rules/about）。
  settingsSub: null,
};

let renamePlan = null;
let bitrateItems = null;
let bitratePending = null;
// 本地演示验证码：前端生成、toast 回显（云端版改为服务端下发）。
let demoVerifyCode = "";
/// 打开码率确认弹窗前聚焦的元素，关闭后要还回去。
let bitrateConfirmReturnFocus = null;
// 最近一次从配置文件读到的设置：保存时用它带回 ffmpegPath（界面上没有对应输入框）。
let loadedSettings = null;

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
  const message = String(error);
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

/** 进度卡：标题只放任务说明；「已用 n 秒」单独放右侧 meta 位，长文件名不会把计时挤没。 */
function renderProgressLabel() {
  const seconds = state.taskStartedAt
    ? Math.round((Date.now() - state.taskStartedAt) / 1000)
    : 0;
  $("#progress-label").textContent = state.taskLabel ?? "";
  $("#progress-meta").textContent =
    state.task && seconds >= 1 ? `已用 ${seconds} 秒` : "";
}

function setProgress({ label, done = null, total = null }) {
  state.taskLabel = label ?? state.taskLabel;
  const node = $("#progress");
  node.classList.remove("hidden");
  const percent = $("#progress-percent");
  if (done != null && total) {
    node.classList.remove("indeterminate");
    const value = Math.min(Math.round((done / total) * 100), 100);
    $("#progress-fill").style.width = `${value}%`;
    percent.textContent = `${value}%`;
  } else {
    node.classList.add("indeterminate");
    $("#progress-fill").style.width = "";
    percent.textContent = "—";
  }
  renderProgressLabel();
}

function hideProgress() {
  const node = $("#progress");
  node.classList.add("hidden");
  node.classList.remove("indeterminate");
  $("#progress-fill").style.width = "0";
  $("#progress-percent").textContent = "—";
  $("#progress-meta").textContent = "";
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
          ? "探测完成，正在写入报审包与清单（复制分集时较慢）"
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

async function call(command, args) {
  if (!invoke) {
    throw new Error("请在 DramaDossier 桌面应用中运行界面。");
  }
  return invoke(command, args);
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
  // 深度处理合并后 #exec-output 已下架，输出目录提示改由「设置 → 转码设置」承担。
  $("#deep-output-dir").placeholder = root
    ? `留空时写到 ${root}/<剧名>/深度处理`
    : "留空时写到 剧目目录/深度处理";
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
    deepOutput: $("#deep-output-dir")?.value.trim() ?? "",
    recutTarget: $("#recut-target-count")?.value.trim() ?? "",
    llm: [
      $("#llm-base-url")?.value.trim() ?? "",
      $("#llm-api-key")?.value.trim() ?? "",
      $("#llm-model")?.value.trim() ?? "",
    ].join("|"),
    img: [
      $("#img-base-url")?.value.trim() ?? "",
      $("#img-api-key")?.value.trim() ?? "",
      $("#img-model")?.value.trim() ?? "",
    ].join("|"),
  });
}

/** 表单与已保存的设置不一致时给出「有改动未保存」提示。 */
function refreshDirtyBadge() {
  const saved = JSON.stringify({
    rules: { ...DEFAULT_RULES, ...(state.settings?.rules ?? {}) },
    ffprobe: state.settings?.ffprobePath ?? "",
    output: state.settings?.outputDir ?? "",
    deepOutput: state.settings?.deepOutputDir ?? "",
    recutTarget: state.settings?.recutTargetCount == null ? "" : String(state.settings.recutTargetCount),
    llm: [
      state.settings?.llmBaseUrl ?? "",
      state.settings?.llmApiKey ?? "",
      state.settings?.llmModel ?? "",
    ].join("|"),
    img: [
      state.settings?.imgBaseUrl ?? "",
      state.settings?.imgApiKey ?? "",
      state.settings?.imgModel ?? "",
    ].join("|"),
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
    // 转码设置（深度处理）与 AI 接口配置。
    if (settings.deepOutputDir) $("#deep-output-dir").value = settings.deepOutputDir;
    if (settings.recutTargetCount != null)
      $("#recut-target-count").value = String(settings.recutTargetCount);
    $("#drama-title").value = settings.dramaTitle ?? "";
    $("#drama-synopsis").value = settings.dramaSynopsis ?? "";
    $("#llm-base-url").value = settings.llmBaseUrl ?? "";
    $("#llm-api-key").value = settings.llmApiKey ?? "";
    $("#llm-model").value = settings.llmModel ?? "";
    $("#img-base-url").value = settings.imgBaseUrl ?? "";
    $("#img-api-key").value = settings.imgApiKey ?? "";
    $("#img-model").value = settings.imgModel ?? "";
    // 海报已在历史会话生成过时直接回显预览。
    if (settings.posterPath && settings.posterPath !== $("#poster-path").dataset.saved) {
      $("#poster-path").textContent = settings.posterPath;
      $("#poster-path").dataset.saved = settings.posterPath;
    }
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
    const settings = buildSettingsFromForm(rules);
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

/**
 * 从表单收集完整设置对象（saveSettings 与 AI 静默持久化共用），
 * 保证任何一条保存路径都不会遗漏字段而抹掉其他配置。
 */
function buildSettingsFromForm(rules) {
  return {
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
    // 转码设置（深度处理）+ 剧名/简介/海报 + AI 接口（云端版接入前的自配方案）。
    deepOutputDir: $("#deep-output-dir").value.trim() || null,
    recutTargetCount:
      $("#recut-target-count").value.trim() === ""
        ? null
        : Number($("#recut-target-count").value.trim()),
    dramaTitle: $("#drama-title").value.trim() || null,
    dramaSynopsis: $("#drama-synopsis").value.trim() || null,
    posterPath: state.settings?.posterPath ?? null,
    llmBaseUrl: $("#llm-base-url").value.trim() || null,
    llmApiKey: $("#llm-api-key").value.trim() || null,
    llmModel: $("#llm-model").value.trim() || null,
    imgBaseUrl: $("#img-base-url").value.trim() || null,
    imgApiKey: $("#img-api-key").value.trim() || null,
    imgModel: $("#img-model").value.trim() || null,
  };
}

/**
 * 静默持久化剧名/简介等 AI 元数据：AI 生成海报前调用，
 * 后端从落盘设置读取剧名与简介来拼生图提示词。
 */
async function persistDramaMeta() {
  let rules;
  try {
    rules = readRulesForm(false);
  } catch {
    rules = state.settings?.rules ?? null;
  }
  const settings = buildSettingsFromForm(rules);
  state.settings = settings;
  loadedSettings = settings;
  await call("save_app_settings", { settings });
  refreshDirtyBadge();
}

// ============ AI 生成：剧名 / 简介 / 海报（OpenAI 兼容自配 API） ============

/** 生成剧名或简介并回填输入框；kind 传 title / synopsis，素材取对方字段的当前值。 */
async function aiGenerateText(kind) {
  const title = $("#drama-title").value.trim();
  const synopsis = $("#drama-synopsis").value.trim();
  // 素材：生成剧名用简介，生成简介用剧名 + 已写了一半的简介；两者全空时给出引导。
  const context =
    kind === "title"
      ? synopsis || title
      : [title, synopsis].filter(Boolean).join("。");
  if (!context) {
    toast(
      kind === "title"
        ? "请先手动填写一些剧情简介作为素材，再生成剧名"
        : "请先填写剧名或简单描述剧情方向，再生成简介",
      true,
    );
    return;
  }
  if (!beginTask(kind === "title" ? "正在 AI 生成剧名" : "正在 AI 生成简介")) return;
  try {
    const text = await call("ai_generate_text", { kind, context });
    if (kind === "title") {
      $("#drama-title").value = text;
    } else {
      $("#drama-synopsis").value = text;
    }
    endTask();
    toast(kind === "title" ? "剧名已生成并回填" : "简介已生成并回填");
  } catch (error) {
    endTask();
    toast(String(error), true);
  }
}

/** 生成竖版海报：先静默落盘剧名/简介（生图提示词要用），再调用生图 API 并回显预览。 */
async function aiGeneratePoster() {
  if (!beginTask("正在 AI 生成海报（生图通常需要 10~30 秒）")) return;
  try {
    // 后端从设置读剧名/简介拼提示词，生成前先把表单内容静默保存。
    await persistDramaMeta();
    const outputDir = state.settings?.deepOutputDir ?? "";
    const poster = await call("ai_generate_poster", { outputDir });
    const preview = $("#poster-preview");
    preview.src = poster.dataUrl ?? "";
    $("#poster-preview-wrap").classList.remove("hidden");
    $("#poster-path").textContent = poster.path ?? "";
    $("#poster-path").dataset.saved = poster.path ?? "";
    state.settings = { ...(state.settings ?? {}), posterPath: poster.path ?? null };
    endTask();
    toast("海报已生成并保存");
  } catch (error) {
    endTask();
    toast(String(error), true);
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
    // 转码设置（深度处理）+ 剧名/简介/海报 + AI 接口，同样原样带回。
    deepOutputDir: state.settings?.deepOutputDir ?? null,
    recutTargetCount: state.settings?.recutTargetCount ?? null,
    dramaTitle: state.settings?.dramaTitle ?? null,
    dramaSynopsis: state.settings?.dramaSynopsis ?? null,
    posterPath: state.settings?.posterPath ?? null,
    llmBaseUrl: state.settings?.llmBaseUrl ?? null,
    llmApiKey: state.settings?.llmApiKey ?? null,
    llmModel: state.settings?.llmModel ?? null,
    imgBaseUrl: state.settings?.imgBaseUrl ?? null,
    imgApiKey: state.settings?.imgApiKey ?? null,
    imgModel: state.settings?.imgModel ?? null,
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
  // 首页「使用教程」与本页三步引导共用同一进度口径，一并刷新。
  renderTutorial();
}

/**
 * 首页「使用教程」六步手风琴：状态按本机实际进度刷新
 * （前两步与剧目库三步引导同口径，后面几步是固定的引导文案）。
 */
function renderTutorial() {
  const host = $("#tutorial-list");
  if (!host) return;
  const hasRoot = Boolean($("#library-root").value.trim());
  const hasDramas = state.dramas.length > 0;
  const steps = [
    {
      title: "选择素材目录",
      detail:
        "一部剧一个子目录，分集视频放子目录里（推荐命名「第01集.mp4」）；封面可叫「封面.jpg」，简介可叫「简介.txt」。",
      done: hasRoot,
      status: hasRoot ? "已完成" : "待准备",
      body: "素材目录是所有功能的起点：扫描剧目、生成报审包与挂载文案都基于它。",
      actions: [{ label: "去选择目录", guideAction: "browse" }],
    },
    {
      title: "扫描并检查剧目",
      detail: "扫描后列出每个剧目与集数，点选剧目再「探测并检查」确认材料可用。",
      done: hasDramas,
      status: hasDramas ? "已完成" : hasRoot ? "待扫描" : "待准备",
      body: "探测每集的时长、分辨率与码率，检查结果会逐项列出是否通过。",
      actions: [{ label: "去剧目库", goto: "library" }],
    },
    {
      title: "生成报审包",
      detail: "填写出品信息后一键生成内容权属声明、简介、成本图等报审材料。",
      done: false,
      status: "待选择",
      body: "报审包只生成在本地目录；「转码」页第一张卡就是生成入口。",
      actions: [{ label: "去转码", goto: "transcode" }],
    },
    {
      title: "转码处理",
      detail: "智能微帧 / 分集重组两种模式，输出到新目录，绝不改动原素材。",
      done: false,
      status: "可选",
      body: "需要去重或调整分集结构时使用；全剧重编码耗时较长，可随时停止。",
      actions: [{ label: "去转码", goto: "transcode" }],
    },
    {
      title: "分轨与字幕擦除",
      detail: "擦除硬字幕与角标，为草稿准备干净视频；只处理自有素材。",
      done: false,
      status: "可选",
      body: "「分轨」页提供字幕擦除、V5 去重验证等后期工具。",
      actions: [{ label: "去分轨", goto: "track" }],
    },
    {
      title: "制作剪映草稿",
      detail: "导出分集顺序与时长到剪映草稿目录，在剪映里继续精修。",
      done: false,
      status: "待准备",
      body: "先在「设置 → 剪映」确认草稿目录，再到「分轨」页导出草稿。",
      actions: [
        { label: "去分轨", goto: "track" },
        { label: "去剪映设置", gotoSub: "jianying" },
      ],
    },
  ];
  const doneCount = steps.filter((step) => step.done).length;
  const countNode = $("#tutorial-count");
  if (countNode) {
    countNode.textContent = doneCount ? `已完成 ${doneCount}/6` : "待开始";
  }
  host.innerHTML = steps
    .map((step, index) => {
      const actions = step.actions
        .map((action) =>
          action.guideAction
            ? `<button class="ghost" data-guide-action="${action.guideAction}">${esc(action.label)}</button>`
            : action.gotoSub
              ? `<button class="ghost" data-goto-sub="${action.gotoSub}">${esc(action.label)}</button>`
              : `<button class="ghost" data-goto="${action.goto}">${esc(action.label)}</button>`,
        )
        .join("");
      return `<details class="tut-row ${step.done ? "done" : ""}">
        <summary>
          <span class="tut-mark" aria-hidden="true">${step.done ? "✓" : index + 1}</span>
          <span class="tut-title">${esc(step.title)}</span>
          <span class="tut-status">${esc(step.status)}</span>
        </summary>
        <div class="tut-body">
          <p>${esc(step.body)}</p>
          <div class="option-row">${actions}</div>
        </div>
      </details>`;
    })
    .join("");
}

/// 剧目或目录一变，工具箱里的旧计划一律作废（别让人拿着上一部剧的计划点执行）。
function invalidatePlans() {
  renamePlan = null;
  bitrateItems = null;
  bitratePending = null;
  $("#rename-apply").disabled = true;
  $("#bitrate-confirm").classList.add("hidden");
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
  renderDramas();
  renderContexts();
}

// ============ 模块导航 ============

// 内容区页头标题：与侧边栏导航一一对应。
const MODULE_TITLES = {
  home: "首页",
  library: "剧目库",
  transcode: "转码",
  track: "分轨",
  toolbox: "工具箱",
};

function switchModule(name) {
  // 设置是独立视图：替换整个工作区，对照竞品「← 返回应用」布局。
  if (name === "settings") {
    openSettingsView();
    return;
  }
  closeSettingsView();
  state.module = name;
  $("#page-title").textContent = MODULE_TITLES[name] ?? name;
  document.querySelectorAll(".nav-item").forEach((node) => {
    node.classList.toggle("active", node.dataset.module === name);
  });
  document.querySelectorAll(".module").forEach((node) => {
    node.classList.toggle("hidden", node.id !== `module-${name}`);
  });
  // 转码模块需要运行环境；切进去时按当前状态刷新阻断层。
  if (name === "transcode") renderTranscodeGate();
}

function renderContexts() {
  const chip = $("#current-drama");
  const selected = state.selected == null ? null : state.dramas[state.selected];
  if (!selected) {
    chip.classList.add("hidden");
    const emptyText =
      "尚未选择剧目——请先到「剧目库」扫描并点选一部剧。";
    for (const id of ["#toolbox-context", "#transcode-context", "#track-context"]) {
      const node = $(id);
      node.textContent = emptyText;
      node.classList.add("empty");
    }
    refreshMountContext();
    return;
  }
  chip.textContent = `当前剧目：${selected.name}`;
  chip.classList.remove("hidden");
  for (const id of ["#toolbox-context", "#transcode-context", "#track-context"]) {
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
  if (investment !== null && !(investment >= 0)) {
    problems.push(["#total-investment", "投资总额必须是不小于 0 的数字。"]);
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
      costSheet: {
        companyName: $("#company-name").value.trim(),
        totalInvestmentWan: cost.investment,
        actorRatioPercent: cost.ratio,
        contact: $("#contact").value.trim(),
      },
    });
    renderResult("#generate-result", {
      tone: "ok",
      html: `<strong>报审包已生成</strong><br />
        位置：${esc(result.packageDir)}<br />
        文件数：${result.fileCount} · 总大小：${formatBytes(Number(result.totalBytes))}`,
      paths: [["package", result.packageDir]],
    });
    toast("报审包生成完成");
  } catch (error) {
    const message = String(error);
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

/**
 * V5 去重端到端验证（技术验证）：调用仓库内 cloud 虚拟环境跑两轮编码 CLI。
 * 完成后展示成片位置、manifest 关键摘要与运行日志尾部；输出目录里另有
 * episode-1.log（per-episode 日志）、checkpoint.json（断点续跑进度）可回溯。
 */
async function runDedupDemo() {
  const input = $("#dedup-demo-input").value.trim();
  const outputDir = $("#dedup-demo-output").value.trim();
  setFieldError("#dedup-demo-input", input ? null : "请填写输入视频路径。");
  setFieldError("#dedup-demo-output", outputDir ? null : "请填写输出目录。");
  if (!input || !outputDir) {
    toast("请先填写输入视频与输出目录", true);
    return;
  }
  if (!beginTask("正在执行 V5 去重验证（两轮编码）")) return;
  try {
    const result = await call("dedup_demo_execute", {
      input,
      outputDir,
      seed: Number($("#dedup-demo-seed").value) || 0,
      series: $("#dedup-demo-series").value.trim() || null,
      badge: $("#dedup-demo-badge").value.trim() || null,
      watermark: $("#dedup-demo-watermark").value.trim() || null,
      timeStretch: $("#dedup-demo-stretch").value,
    });
    const manifest = result.manifest ?? {};
    const tags = manifest.metadataTags ?? {};
    const logTail = (result.logLines ?? []).slice(-10).join("\n");
    renderResult("#dedup-demo-result", {
      tone: "ok",
      html: `
      <strong>V5 验证完成</strong><br />
      成片：${esc(result.finalVideo)}<br />
      referenceSha256：<code>${esc(String(manifest.referenceSha256 ?? "?"))}</code><br />
      伪装机型：${esc(String(tags.make ?? "?"))} · encoder：${esc(String(tags.encoder ?? "?"))}<br />
      输出目录内含 episode-1.log / checkpoint.json / final-manifest.json<br />
      <details><summary>运行日志（尾部 10 行）</summary><pre>${esc(logTail)}</pre></details>`,
      paths: [["dedupDemoOutput", result.outputDir]],
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
    toast(String(error), true);
  }
}

async function loadPublishCopy() {
  try {
    const path = $("#publish-copy-path").value.trim();
    if (!path) throw new Error("请填写挂载文案 CSV 路径。");
    state.publishRows = await call("load_mount_copy", { path });
    renderPublishCopy();
  } catch (error) {
    toast(String(error), true);
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
    toast(String(error), true);
  }
}

async function savePublishCopy() {
  try {
    const path = $("#publish-copy-path").value.trim();
    if (!path) throw new Error("请填写挂载文案 CSV 路径。");
    const result = await call("save_mount_copy", { path, rows: state.publishRows });
    toast(`已保存 ${result.rowCount} 行文案，SHA-256 ${result.sha256.slice(0, 12)}…`);
  } catch (error) {
    toast(String(error), true);
  }
}

function parseEraseRegions() {
  const raw = $("#erase-regions").value.trim();
  if (!raw) return [];
  return raw
    .split(/[;；\n]/)
    .map((chunk) => chunk.trim())
    .filter(Boolean)
    .map((chunk) => {
      const parts = chunk.split(/[,，\s]+/).map(Number);
      if (parts.length !== 4 || parts.some((value) => !Number.isFinite(value))) {
        throw new Error(`区域格式应为 x,y,w,h：${chunk}`);
      }
      const [x, y, width, height] = parts;
      return { x, y, width, height };
    });
}

function eraseRequest() {
  const optionalNumber = (selector) => {
    const value = $(selector).value.trim();
    return value === "" ? null : Number(value);
  };
  const workerArgs = $("#erase-worker-args")
    .value.split(/[,，]/)
    .map((item) => item.trim())
    .filter(Boolean);
  return {
    input: $("#erase-input").value.trim(),
    output: $("#erase-output").value.trim(),
    regions: parseEraseRegions(),
    detector: $("#erase-detector").value,
    inpainter: $("#erase-inpainter").value,
    dilate: Number($("#erase-dilate").value || 4),
    feather: Number($("#erase-feather").value || 2),
    context: Number($("#erase-context").value || 1.6),
    start: optionalNumber("#erase-start"),
    duration: optionalNumber("#erase-duration"),
    persistentWorker: $("#erase-persistent").checked,
    workerProgram: $("#erase-worker-program").value.trim() || null,
    workerArgs,
  };
}

/** 就地校验擦除表单：输入/输出必填、且不能是同一个文件（后端也会再拦一次）。 */
function eraseIoProblems(request) {
  clearFieldErrors(["#erase-input", "#erase-output"]);
  const problems = [];
  if (!request.input) problems.push(["#erase-input", "请选择要擦除的输入视频。"]);
  if (!request.output) problems.push(["#erase-output", "请选择擦除后的输出文件。"]);
  if (request.input && request.output && stripPath(request.input) === stripPath(request.output)) {
    problems.push(["#erase-output", "输出文件不能和输入文件相同，请换一个输出文件名。"]);
  }
  problems.forEach(([selector, message]) => setFieldError(selector, message));
  return problems.length === 0;
}

/** 读擦除表单：区域格式或路径不对时给出就地提示，返回 null 表示先别执行。 */
function readEraseRequest() {
  let request;
  try {
    request = eraseRequest();
  } catch (error) {
    setFieldError("#erase-regions", String(error));
    reportError(error);
    return null;
  }
  if (!eraseIoProblems(request)) {
    toast("请先修正标红的输入或输出路径", true);
    return null;
  }
  return request;
}

function renderErasePlan(plan) {
  const node = $("#erase-result");
  node.innerHTML = `
    <strong>将要执行的命令</strong><br />
    <code>${esc(plan.commandLine)}</code><br />
    处理日志：${esc(plan.reportPath)}${
      plan.requiresWorker ? " · 需要模型 worker" : ""
    }
  `;
  node.classList.remove("hidden");
}

async function checkEraseStatus() {
  try {
    const status = await call("erase_status", {
      toolPath: $("#erase-tool-path").value.trim() || null,
    });
    const node = $("#erase-result");
    node.innerHTML = `
      <strong>擦除工具：${status.available ? "可用" : "不可用"}</strong><br />
      ${status.tool ? `路径：<code>${esc(status.tool)}</code><br />` : ""}
      ${esc(status.message)}
    `;
    node.classList.remove("hidden");
    toast(status.available ? "擦除工具可用" : "未找到擦除工具", !status.available);
  } catch (error) {
    reportError(error);
  }
}

async function planErase() {
  const request = readEraseRequest();
  if (!request) return;
  try {
    const plan = await call("plan_erase", {
      request,
      toolPath: $("#erase-tool-path").value.trim() || null,
      ffprobePath: effectiveFfprobe(),
    });
    renderErasePlan(plan);
  } catch (error) {
    reportError(error);
  }
}

async function runErase() {
  const request = readEraseRequest();
  if (!request) return;
  if (!beginTask("正在擦除（可能需要较长时间，请勿关闭窗口）")) return;
  try {
    const run = await call("run_erase", {
      request,
      toolPath: $("#erase-tool-path").value.trim() || null,
      ffprobePath: effectiveFfprobe(),
      timeoutSeconds: 1800,
    });
    const report = run.report ?? {};
    const verification = report.verification ?? {};
    const residual = verification.residual ?? {};
    const ssim = verification.ssim ?? {};
    const node = $("#erase-result");
    node.innerHTML = `
      <strong>擦除结论：${esc(run.outcomeLabel)}（退出码 ${run.exitCode}）</strong><br />
      ${verification.verdict ? `工具自检：${esc(String(verification.verdict))}<br />` : ""}
      ${
        residual.maxRatio != null
          ? `残留峰值：${(Number(residual.maxRatio) * 100).toFixed(2)}%<br />`
          : ""
      }
      ${ssim.minSsim != null ? `结构相似度最低：${Number(ssim.minSsim).toFixed(3)}<br />` : ""}
      ${run.stderrTail ? `<pre>${esc(run.stderrTail)}</pre>` : ""}
      处理日志：${esc(run.reportPath ?? "")}
    `;
    node.classList.remove("hidden");
    toast(`擦除结束：${run.outcomeLabel}`);
  } catch (error) {
    reportError(error);
  } finally {
    endTask();
  }
}

/** 通用路径选择：mode="folder" 选目录（默认），mode="file" 选视频文件（pick_video 带 mp4 等扩展名过滤）。 */
async function browse(targetId, mode = "folder") {
  try {
    const picked = await call(mode === "file" ? "pick_video" : "pick_folder");
    if (picked) $(targetId).value = picked;
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
  // 弹窗关掉后要把焦点还给触发它的按钮，否则键盘用户会被丢回页面开头。
  bitrateConfirmReturnFocus = $("#bitrate-fix");
  $("#bitrate-confirm").classList.remove("hidden");
  $("#bitrate-fix").disabled = true;
  $("#bitrate-confirm-yes").focus();
}

function cancelBitrateFix() {
  bitratePending = null;
  $("#bitrate-confirm").classList.add("hidden");
  syncBitrateFixState();
  // 先让按钮恢复可聚焦状态（上面可能刚把它禁用），再把焦点还回去。
  const back = bitrateConfirmReturnFocus;
  bitrateConfirmReturnFocus = null;
  if (back && !back.disabled) back.focus();
}

async function applyBitrateFix() {
  if (!bitratePending) return;
  // 走确认路径时不还焦点：紧接着就会开始长任务并禁用按钮。
  bitrateConfirmReturnFocus = null;
  cancelBitrateFix();
  if (!beginTask(`正在重编码 ${bitratePending.length} 集（原文件会被替换）`)) return;
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
 * 深度处理合并后输出目录改由「设置 → 转码设置」下发，此函数仅保留备查（旧双档位时代）。
 */
// function transcodeOutputOverride() {
//   const value = $("#exec-output").value.trim();
//   setFieldError("#exec-output", null);
//   if (!value) return null;
//   const drama = state.dramas[state.selected];
//   if (drama && stripPath(value) === stripPath(drama.dir)) {
//     setFieldError(
//       "#exec-output",
//       "输出目录不能就是素材目录，那会覆盖原素材；留空会自动写到剧目目录的子目录。",
//     );
//     return undefined;
//   }
//   return value;
// }

async function executeTranscode() {
  // 深度处理（重组 + 微帧合并）：目标集数与输出目录一律以设置为准，
  // 前端不传，后端从 settings.deep_output_dir / recut_target_count 读取。
  const mode = $("#exec-mode").value || "deep";
  const outputDir = null;
  const modeLabel = "深度处理（重组 + 微帧）";
  if (!beginTask(`正在执行${modeLabel}（两轮全剧重编码，可能需要较长时间）`)) return;
  const stopButton = $("#exec-stop");
  stopButton.classList.remove("hidden");
  stopButton.disabled = false;
  stopButton.textContent = "停止转码";
  try {
    const drama = selectedDrama();
    const report = await call("transcode_execute", {
      root: state.root,
      name: drama.name,
      outputRoot: state.settings?.outputDir ?? null,
      request: {
        mode,
        targetCount: null,
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
    const message = String(error);
    const stopped = message.includes("停止");
    renderResult("#exec-result", {
      tone: stopped ? "warn" : "error",
      html: `<strong>${stopped ? "已停止转码" : "转码失败"}</strong><br />${esc(message)}`,
    });
    if (stopped) {
      toast("转码已停止，本次输出目录已删除");
    } else {
      reportError(error);
    }
  } finally {
    stopButton.classList.add("hidden");
    stopButton.disabled = false;
    stopButton.textContent = "停止转码";
    endTask();
  }
}

// ============ 运行环境（全局横幅 + 头部状态 + 设置-本机环境） ============

async function checkRuntime(force = false) {
  if (state.runtime && !force) {
    renderRuntimeStatus(state.runtime);
    return true;
  }
  try {
    const status = await call("runtime_status", { ffprobePath: effectiveFfprobe() });
    state.runtime = status;
    renderRuntimeStatus(status);
    return true;
  } catch (error) {
    reportError(error);
    return false;
  }
}

function renderRuntimeStatus(status) {
  renderEnvBanner(status);
  renderRuntimeList(status);
  renderEnvChip(status);
  renderRuntimeInstallEntry(status);
}

/** 没有「一键配置」的平台（现仅 Linux），按键必然失败，改为给出手动安装指引。 */
function manualInstallHint(platform) {
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
// evermeet.cx 单连接限流，全剧下载可能需要几分钟；期间用弹窗展示进度（竞品同款交互）。
function formatBytes(bytes) {
  if (!Number.isFinite(bytes) || bytes <= 0) return "待计算";
  if (bytes >= 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${Math.max(1, Math.round(bytes / 1024))} KB`;
}

// 把弹窗切成「进行中」初始态：标题/副标题/统计/进度条全部复位。
function openInstallModal() {
  $("#install-overlay").classList.remove("hidden");
  const modal = document.querySelector(".install-modal");
  modal.classList.remove("failed");
  $("#install-icon").textContent = "⬇";
  $("#install-title").textContent = "正在安装本机引擎";
  $("#install-subtitle").textContent = "正在连接下载源。";
  $("#install-total").textContent = "待计算";
  $("#install-downloaded").textContent = "待计算";
  $("#install-component").textContent = "准备中";
  $("#install-percent").textContent = "—";
  const bar = $("#install-bar");
  bar.classList.add("indeterminate");
  bar.style.width = "";
  $("#install-error").classList.add("hidden");
  $("#install-retry").classList.add("hidden");
  $("#install-close").classList.add("hidden");
}

// 进度事件驱动：需要下载 / 已下载 / 组件 + 百分比。
function handleInstallProgress(payload) {
  $("#install-component").textContent =
    `${payload.component} · ${payload.stageLabel}`;
  $("#install-subtitle").textContent =
    payload.stage === "finished"
      ? "安装完成。"
      : payload.stage === "verifying"
        ? "正在检查安装结果。"
        : payload.stage === "extracting"
          ? "正在解压安装。"
          : payload.stage === "installing"
            ? "正在安装（安装包内置资源）。"
            : `正在${payload.stageLabel}。`;
  if (payload.stage === "installing") {
    // 内置资源复制路径没有网络动作，统计区如实标注。
    $("#install-total").textContent = "内置资源";
    $("#install-downloaded").textContent = "内置资源";
  }
  if (payload.stage === "downloading" || payload.stage === "connecting") {
    const total = payload.totalBytes;
    const done = payload.downloadedBytes ?? 0;
    $("#install-total").textContent = total ? formatBytes(total) : "待计算";
    $("#install-downloaded").textContent = done ? formatBytes(done) : "待计算";
    const bar = $("#install-bar");
    if (total) {
      bar.classList.remove("indeterminate");
      const ratio = Math.min(100, (done / total) * 100);
      bar.style.width = `${ratio.toFixed(1)}%`;
      $("#install-percent").textContent = `${Math.round(ratio)}%`;
    }
  }
}

// 失败态：红图标 + 错误详情 + 「重新下载」/关闭。
function failInstallModal(message) {
  const modal = document.querySelector(".install-modal");
  modal.classList.add("failed");
  $("#install-icon").textContent = "✕";
  $("#install-title").textContent = "引擎安装未完成";
  const firstLine = String(message).split("\n").find(Boolean) ?? "安装失败。";
  $("#install-subtitle").textContent = firstLine;
  $("#install-error").textContent = String(message);
  $("#install-error").classList.remove("hidden");
  $("#install-retry").classList.remove("hidden");
  $("#install-close").classList.remove("hidden");
  const bar = $("#install-bar");
  bar.classList.remove("indeterminate");
}

async function runtimeInstall() {
  openInstallModal();
  // 横幅 / 设置页按钮文案不同（一键配置运行环境 / 下载运行环境），各自保存原文案。
  const buttons = [$("#env-install"), $("#runtime-install"), $("#gate-install")].filter(Boolean);
  const original = new Map(buttons.map((button) => [button, button.textContent]));
  for (const button of buttons) {
    button.disabled = true;
    button.textContent = "正在配置…（下载安装中，请稍候）";
  }
  const unlisten = await tauriEvent.listen("install-progress", (event) =>
    handleInstallProgress(event.payload),
  );
  try {
    const report = await call("install_runtime");
    // 成功：进度条推满，短暂展示后自动收起。
    const bar = $("#install-bar");
    bar.classList.remove("indeterminate");
    bar.style.width = "100%";
    $("#install-percent").textContent = "100%";
    $("#install-component").textContent = "完成";
    $("#install-subtitle").textContent = "安装完成。";
    toast(report.note);
    await new Promise((resolve) => setTimeout(resolve, 700));
    $("#install-overlay").classList.add("hidden");
  } catch (error) {
    // 失败：弹窗保持打开（错误态 + 重新下载按钮），由用户手动关闭。
    failInstallModal(error);
  } finally {
    unlisten();
    for (const button of buttons) {
      button.disabled = false;
      button.textContent = original.get(button);
    }
    // 无论成败都刷新：成功后回填自动写入的路径并重测，失败时也展示最新状态。
    await loadSettings();
    await checkRuntime(true);
  }
}

// 失败弹窗上的「重新下载」= 原样再跑一次；「关闭」只收起弹窗。
$("#install-retry").addEventListener("click", () => runtimeInstall());
$("#install-close").addEventListener("click", () =>
  $("#install-overlay").classList.add("hidden"),
);

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
}

// ============ 登录 / 注册（本地演示版，接口与云端版一致） ============

function showAuthView() {
  $("#auth-view").classList.remove("hidden");
  document.querySelector(".app").classList.add("hidden");
  closeSettingsView();
}

function showApp() {
  $("#auth-view").classList.add("hidden");
  document.querySelector(".app").classList.remove("hidden");
}

function switchAuthCard(showRegister) {
  $("#auth-login-form").classList.toggle("hidden", showRegister);
  $("#auth-register-form").classList.toggle("hidden", !showRegister);
}

function renderAccountChip() {
  const account = state.account;
  const loggedIn = Boolean(account?.loggedIn);
  $("#chip-email").textContent = loggedIn ? account.email : "未登录";
  $("#chip-badge").textContent = loggedIn ? "已登录" : "离线";
  $("#chip-badge").className = `badge ${loggedIn ? "status-ready" : ""}`;
}

function applyAccount(payload) {
  state.account = payload;
  renderAccountChip();
  // 设置-账号子页同步。
  const emailNode = $("#account-email");
  if (emailNode) emailNode.textContent = payload?.loggedIn ? payload.email : "—";
  if (payload?.path) $("#account-path").textContent = payload.path;
}

async function sendVerifyCode() {
  const email = $("#register-email").value.trim();
  if (!email) {
    toast("请先填写邮箱", true);
    return;
  }
  // 本地演示：随机 6 位、toast 回显；云端版改为调用服务端发送。
  demoVerifyCode = String(Math.floor(100000 + Math.random() * 900000));
  toast(`演示验证码：${demoVerifyCode}（本地演示，未发送邮件）`);
}

async function registerAccount(event) {
  event.preventDefault();
  const email = $("#register-email").value.trim();
  const password = $("#register-password").value;
  const code = $("#register-code").value.trim();
  if (!/^\d{6}$/.test(code)) {
    toast("验证码应为 6 位数字", true);
    return;
  }
  if (demoVerifyCode && code !== demoVerifyCode) {
    toast("验证码不正确（演示验证码见 toast 提示）", true);
    return;
  }
  try {
    const payload = await call("account_register", {
      args: { email, password, code },
    });
    applyAccount(payload);
    showApp();
    toast(`注册成功，已登录：${payload.email}`);
    await afterLoginInit();
  } catch (error) {
    toast(String(error), true);
  }
}

async function loginAccount(event) {
  event.preventDefault();
  const email = $("#login-email").value.trim();
  const password = $("#login-password").value;
  try {
    const payload = await call("account_login", { email, password });
    applyAccount(payload);
    showApp();
    toast(`欢迎回来，${payload.email}`);
    await afterLoginInit();
  } catch (error) {
    toast(String(error), true);
  }
}

async function logoutAccount() {
  try {
    applyAccount(await call("account_logout"));
    switchAuthCard(false);
    showAuthView();
    toast("已退出登录");
  } catch (error) {
    toast(String(error), true);
  }
}

// 登录后（含注册即登录）需要初始化的数据。
async function afterLoginInit() {
  await loadSettings();
  await checkRuntime(true);
}

// 启动分流：未登录 → 登录卡；已登录 / 浏览器预览 → 主应用。
async function boot() {
  if (!invoke) {
    // 浏览器静态预览（无 Tauri IPC）：跳过登录直接展示主界面，便于布局验证。
    showApp();
    return;
  }
  try {
    applyAccount(await call("account_status"));
    if (state.account?.loggedIn) {
      showApp();
    } else {
      switchAuthCard(false);
      showAuthView();
    }
  } catch (error) {
    toast(String(error), true);
    showAuthView();
  }
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
  document.querySelector(".app")?.classList.remove("hidden");
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
    try {
      applyAccount(await call("account_status"));
    } catch {
      /* 未登录时保持占位文案 */
    }
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

// ---- 设置：修改密码 / 退出登录 ----

async function changePassword(event) {
  event.preventDefault();
  const oldPassword = $("#old-password").value;
  const newPassword = $("#new-password").value;
  try {
    const payload = await call("account_change_password", {
      args: { oldPassword, newPassword },
    });
    applyAccount(payload);
    $("#password-change-form").classList.add("hidden");
    $("#old-password").value = "";
    $("#new-password").value = "";
    toast("密码已修改");
  } catch (error) {
    toast(String(error), true);
  }
}

// ---- 设置：剪映 ----

async function jianyingDetect() {
  try {
    // 把输入框当前值一起传给后端：检测要响应"刚填还没保存"的路径。
    const result = await call("jianying_detect", {
      dir: $("#jianying-dir").value.trim() || null,
    });
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
  } catch (error) {
    toast(String(error), true);
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
    toast(String(error), true);
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
    toast(String(error), true);
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
    toast(String(error), true);
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
    toast(String(error), true);
  }
}

// ============ 转码：模式卡片与整页阻断 ============

// 旧双档位（智能微帧 / 分集重组）合并为「深度处理」后不再需要切换逻辑，注释保留备查。
// function selectTranscodeMode(mode) {
//   $("#exec-mode").value = mode;
//   $("#mode-nudge").classList.toggle("active", mode === "nudge");
//   $("#mode-recut").classList.toggle("active", mode === "recut");
//   // 目标集数只对分集重组有意义。
//   $("#exec-target-wrap").classList.toggle("hidden", mode !== "recut");
// }

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

// ============ 关于 ============

async function loadVersion() {
  try {
    const version = await window.__TAURI__?.app?.getVersion?.();
    $("#about-version").textContent = version ?? "未知";
    // 侧边栏品牌处的版本号（Grove 样式：品牌名下挂小字版本）。
    const side = $("#side-version");
    if (side) side.textContent = version ?? "…";
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
// ============ 登录 / 注册 ============
$("#goto-register").addEventListener("click", () => switchAuthCard(true));
$("#goto-login").addEventListener("click", () => switchAuthCard(false));
$("#forgot-password").addEventListener("click", () =>
  toast("找回密码需要邮件服务，云端版开放后可用", true),
);
$("#send-code").addEventListener("click", sendVerifyCode);
$("#auth-login-form").addEventListener("submit", loginAccount);
$("#auth-register-form").addEventListener("submit", registerAccount);
// ============ 设置独立视图 ============
$("#settings-back").addEventListener("click", () => {
  // 返回进入设置前的页面；异常兜底回首页。
  switchModule(state.module ?? "home");
});
document.querySelectorAll(".settings-nav").forEach((node) => {
  node.addEventListener("click", () => switchToSubAnd(node.dataset.sub));
});
$("#password-change-form").addEventListener("submit", changePassword);
$("#open-change-password").addEventListener("click", () => {
  $("#password-change-form").classList.toggle("hidden");
});
$("#cancel-change-password").addEventListener("click", () => {
  $("#password-change-form").classList.add("hidden");
  $("#old-password").value = "";
  $("#new-password").value = "";
});
$("#logout").addEventListener("click", logoutAccount);
$("#ledger-refresh").addEventListener("click", () => loadLedger());
$("#jianying-detect").addEventListener("click", jianyingDetect);
$("#jianying-save").addEventListener("click", jianyingSave);
$("#jianying-dir-pick").addEventListener("click", () => browse("#jianying-dir"));
$("#ai-save").addEventListener("click", aiSave);
$("#env-workdir-pick").addEventListener("click", envWorkdirPick);
$("#env-workdir").addEventListener("keydown", (event) => {
  if (event.key === "Enter") envWorkdirApply();
});
// ============ AI 生成（剧名 / 简介 / 海报）与转码设置 ============
$("#ai-title").addEventListener("click", () => aiGenerateText("title"));
$("#ai-synopsis").addEventListener("click", () => aiGenerateText("synopsis"));
$("#ai-poster").addEventListener("click", aiGeneratePoster);
$("#deep-output-pick").addEventListener("click", () => browse("#deep-output-dir"));
// ============ 挂载 ============
// 挂载与发布卡暂时下架（HTML 已整块注释）：功能函数保留，仅注释事件绑定，恢复时一并取消注释。
// document.querySelectorAll("#mount-steps .step").forEach((node) => {
//   node.addEventListener("click", () => switchMountStep(Number(node.dataset.step)));
// });
// $("#mount-bind").addEventListener("click", mountBind);
// $("#mount-csv-generate").addEventListener("click", mountCsvGenerate);
// $("#mount-csv-save").addEventListener("click", mountCsvSave);
// $("#mount-plan-save").addEventListener("click", mountPlanSave);
// ============ 转码 ============
// 旧双档位卡已合并为「深度处理」，切换绑定注释保留（HTML 中两张 mode-card 也已注释）。
// $("#mode-nudge").addEventListener("click", () => selectTranscodeMode("nudge"));
// $("#mode-recut").addEventListener("click", () => selectTranscodeMode("recut"));
$("#gate-install").addEventListener("click", runtimeInstall);
// ============ 原有模块 ============
$("#rename-plan").addEventListener("click", buildRenamePlan);
$("#rename-apply").addEventListener("click", applyRename);
$("#bitrate-probe").addEventListener("click", probeBitrate);
$("#bitrate-fix").addEventListener("click", requestBitrateFix);
$("#bitrate-confirm-yes").addEventListener("click", applyBitrateFix);
$("#bitrate-confirm-no").addEventListener("click", cancelBitrateFix);

/**
 * 确认弹窗的键盘行为：Esc 取消、Tab 在弹窗内循环。
 * `alertdialog` 只声明了语义，焦点仍然会跑到背后的表单上，必须自己兜住。
 */
function handleConfirmKeys(event) {
  const node = $("#bitrate-confirm");
  if (node.classList.contains("hidden")) return;
  if (event.key === "Escape") {
    event.preventDefault();
    cancelBitrateFix();
    return;
  }
  if (event.key !== "Tab") return;
  const focusables = $$("#bitrate-confirm button:not([disabled])");
  if (!focusables.length) return;
  const first = focusables[0];
  const last = focusables[focusables.length - 1];
  const active = document.activeElement;
  const inside = Boolean(active) && node.contains(active);
  if (event.shiftKey ? active === first || !inside : active === last || !inside) {
    event.preventDefault();
    (event.shiftKey ? last : first).focus();
  }
}

document.addEventListener("keydown", handleConfirmKeys);
$("#exec-run").addEventListener("click", executeTranscode);
// 停止转码：后端杀掉正在跑的 ffmpeg、清理本次输出目录，执行中的 transcode_execute 会随之返回。
$("#exec-stop").addEventListener("click", async (event) => {
  const button = event.currentTarget;
  button.disabled = true;
  button.textContent = "正在停止…";
  try {
    await invoke("transcode_stop");
    toast("已发送停止指令，正在终止 ffmpeg 并清理输出目录");
  } catch (error) {
    toast(error, true);
    button.disabled = false;
    button.textContent = "停止转码";
  }
});
// 「重新检测」要有可感知反馈：环境正常时结果不变，不加状态会像点了没反应。
$("#runtime-recheck").addEventListener("click", async (event) => {
  const button = event.currentTarget;
  button.disabled = true;
  button.textContent = "检测中…";
  try {
    const ok = await checkRuntime(true);
    if (ok) toast("本机环境检测完成");
  } finally {
    button.disabled = false;
    button.textContent = "重新检测";
  }
});
$("#runtime-install").addEventListener("click", runtimeInstall);
// 测试期排障：一键打开日志目录（~/Documents/剧备日志），把 dossier.log 发给维护者即可。
$("#open-log").addEventListener("click", () =>
  invoke("open_log_dir").catch((error) => toast(error, true)),
);
// 运行日志面板（对照 Grove 的可复制日志面板）：加载最近 200 条 + 一键复制。
async function refreshLogPanel() {
  const panel = $("#log-panel");
  try {
    const lines = await invoke("read_recent_logs", { limit: 200 });
    panel.textContent = lines.length ? lines.join("\n") : "日志为空。";
    panel.scrollTop = panel.scrollHeight;
  } catch (error) {
    panel.textContent = `加载失败：${error}`;
  }
}
$("#log-refresh").addEventListener("click", refreshLogPanel);
// 初次进入页面即加载一次（读取很轻，避免面板一直是「尚未加载」）。
refreshLogPanel();
$("#log-copy").addEventListener("click", async () => {
  const text = $("#log-panel").textContent || "";
  if (!text || text === "尚未加载。") return;
  try {
    await navigator.clipboard.writeText(text);
    toast("日志已复制到剪贴板");
  } catch (error) {
    toast(`复制失败：${error}`, true);
  }
});
// 横幅里的安装按钮随 innerHTML 重建，用事件委托绑定。
$("#env-banner").addEventListener("click", (event) => {
  if (event.target instanceof Element && event.target.id === "env-install") {
    runtimeInstall();
  }
});
$("#browse-erase-input").addEventListener("click", async () => {
  try {
    const file = await call("pick_video");
    if (file) $("#erase-input").value = file;
  } catch (error) {
    reportError(error);
  }
});
$("#erase-status").addEventListener("click", checkEraseStatus);
$("#erase-plan").addEventListener("click", planErase);
$("#erase-run").addEventListener("click", runErase);
$("#transcode-plan").addEventListener("click", transcodeAdvice);
$("#diff-run").addEventListener("click", diffReportPackages);
$("#diff-left-pick").addEventListener("click", () => browse("#diff-left"));
$("#diff-right-pick").addEventListener("click", () => browse("#diff-right"));
$("#platform-run").addEventListener("click", platformReview);
$("#draft-pick").addEventListener("click", () => browse("#draft-dir"));
$("#draft-export").addEventListener("click", exportDraft);
$("#draft-import").addEventListener("click", importDraft);
$("#dedup-demo-pick").addEventListener("click", () => browse("#dedup-demo-input", "file"));
$("#dedup-demo-run").addEventListener("click", runDedupDemo);
$("#publish-copy-generate").addEventListener("click", generatePublishCopy);
$("#publish-copy-load").addEventListener("click", loadPublishCopy);
$("#publish-copy-preview").addEventListener("click", previewPublishCopy);
$("#browse-output").addEventListener("click", () => browse("#output-dir"));
$("#scan").addEventListener("click", scan);
$("#analyze").addEventListener("click", analyze);
$("#generate").addEventListener("click", generate);
$("#save-settings").addEventListener("click", saveSettings);
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
  "#rule-prefer-portrait", "#settings-output", "#ffprobe-path",
].forEach((selector) => {
  const node = $(selector);
  node.addEventListener("input", refreshDirtyBadge);
  node.addEventListener("change", refreshDirtyBadge);
});

// 用户改了「本次覆盖」的路径就清掉旧的字段错误提示。
// （#exec-output / #exec-target 随双档位下架，已从清单移除。）
["#erase-input", "#erase-output", "#erase-regions",
  "#total-investment", "#actor-ratio", "#bitrate-min", "#diff-left", "#diff-right",
  "#draft-dir", "#dedup-demo-input", "#dedup-demo-output",
  "#rule-min-episodes", "#rule-min-duration", "#rule-max-duration",
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
  // 页内跳转（首页教程 / 横幅 CTA）：data-goto 切模块，data-goto-sub 直达设置子页。
  const gotoNode = event.target.closest("[data-goto]");
  if (gotoNode) {
    switchModule(gotoNode.dataset.goto);
    return;
  }
  const gotoSubNode = event.target.closest("[data-goto-sub]");
  if (gotoSubNode) {
    switchModule("settings");
    switchToSubAnd(gotoSubNode.dataset.gotoSub);
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
}

// 启动：决定显示登录卡还是主应用（浏览器预览无 IPC 时直接进主应用便于布局验证）。
boot();
if (invoke) {
  loadSettings().then(() => checkRuntime());
  loadPlatformProfiles();
}
loadVersion();

// 首屏就画好三步引导，不必等设置读完（读失败时页面也不该是一片空白）。
renderEmptyGuide();
