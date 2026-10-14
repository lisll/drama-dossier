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
  module: "library",
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
};

let renamePlan = null;
let bitrateItems = null;
let bitratePending = null;
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

function toast(message, isError = false) {
  const node = $("#toast");
  // 颜色之外再给一个文字信号：色觉差异、投屏或截图里也能分辨成功与失败。
  node.textContent = isError ? `出错了：${message}` : message;
  node.classList.toggle("error", isError);
  node.classList.remove("hidden");
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => node.classList.add("hidden"), isError ? 9000 : 4500);
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
    toast(String(error), true);
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

/** 生成页的「本次覆盖」输入只放占位提示，默认值始终来自已保存的设置。 */
function renderDefaultHints() {
  const output = state.settings?.outputDir;
  $("#output-dir").placeholder = output
    ? `留空时用设置里的：${output}`
    : "留空时生成到剧目目录/报审包";
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
  });
}

/** 表单与已保存的设置不一致时给出「有改动未保存」提示。 */
function refreshDirtyBadge() {
  const saved = JSON.stringify({
    rules: { ...DEFAULT_RULES, ...(state.settings?.rules ?? {}) },
    ffprobe: state.settings?.ffprobePath ?? "",
    output: state.settings?.outputDir ?? "",
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
    renderDefaultHints();
    refreshDirtyBadge();
    if (payload.warning) toast(payload.warning, true);
  } catch (error) {
    toast(String(error), true);
  }
}

async function saveSettings() {
  let rules;
  try {
    rules = readRulesForm(true);
  } catch (error) {
    toast(String(error), true);
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

function switchModule(name) {
  state.module = name;
  document.querySelectorAll(".nav-item").forEach((node) => {
    node.classList.toggle("active", node.dataset.module === name);
  });
  document.querySelectorAll(".module").forEach((node) => {
    node.classList.toggle("hidden", node.id !== `module-${name}`);
  });
}

function renderContexts() {
  const chip = $("#current-drama");
  const selected = state.selected == null ? null : state.dramas[state.selected];
  if (!selected) {
    chip.classList.add("hidden");
    const emptyText =
      "尚未选择剧目——请先到「剧目库」扫描并点选一部剧。";
    $("#generate-context").textContent = emptyText;
    $("#toolbox-context").textContent = emptyText;
    $("#generate-context").classList.add("empty");
    $("#toolbox-context").classList.add("empty");
    return;
  }
  chip.textContent = `当前剧目：${selected.name}`;
  chip.classList.remove("hidden");
  for (const id of ["#generate-context", "#toolbox-context"]) {
    const node = $(id);
    node.textContent = `当前剧目：${selected.name}（${selected.episodes.length} 集）`;
    node.classList.remove("empty");
  }
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
    toast(`已发现 ${state.dramas.length} 个剧目`);
    // 记住素材根目录：下次启动直接回填，不用每次重新找。
    await rememberRoot(root);
  } catch (error) {
    toast(String(error), true);
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
    );
    return;
  }
  toast(message, true);
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
    renderResult("#generate-result", {
      tone: "error",
      html: `<strong>生成失败</strong><br />${esc(String(error))}`,
    });
    toast(String(error), true);
  } finally {
    endTask();
  }
}

function selectedDrama() {
  if (state.selected == null) throw new Error("请先在左侧选择一部剧");
  return state.dramas[state.selected];
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
    toast(String(error), true);
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
    toast(String(error), true);
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
    toast(String(error), true);
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
    toast(String(error), true);
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
    toast(String(error), true);
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
    toast(String(error), true);
  } finally {
    endTask();
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
    toast(String(error), true);
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
    toast(String(error), true);
  }
}

async function planErase() {
  const request = readEraseRequest();
  if (!request) return;
  try {
    const plan = await call("plan_erase", {
      request,
      toolPath: $("#erase-tool-path").value.trim() || null,
    });
    renderErasePlan(plan);
  } catch (error) {
    toast(String(error), true);
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
    toast(String(error), true);
  } finally {
    endTask();
  }
}

async function browse(targetId) {
  try {
    const folder = await call("pick_folder");
    if (folder) $(targetId).value = folder;
  } catch (error) {
    toast(String(error), true);
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
    toast(String(error), true);
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
    toast(String(error), true);
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
    toast(String(error), true);
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
    toast(String(error), true);
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
  $("#bitrate-confirm").classList.remove("hidden");
  $("#bitrate-fix").disabled = true;
  $("#bitrate-confirm-yes").focus();
}

function cancelBitrateFix() {
  bitratePending = null;
  $("#bitrate-confirm").classList.add("hidden");
  syncBitrateFixState();
}

async function applyBitrateFix() {
  if (!bitratePending) return;
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
    toast(String(error), true);
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
    toast(String(error), true);
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
    toast(String(error), true);
  }
}

function renderRuntimeStatus(status) {
  renderEnvBanner(status);
  renderRuntimeList(status);
  renderEnvChip(status);
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
  node.innerHTML = `运行环境未就绪：缺少 ${esc(
    missing.map((tool) => tool.name).join("、"),
  )}。点击「一键配置运行环境」自动下载安装（约 25 MB，需要网络）；也可以在「设置 → 本机环境」手动指定路径。<button id="env-install" class="ghost">一键配置运行环境</button>`;
}

// 一键配置运行环境：后台自动下载安装 ffmpeg/ffprobe 并写回设置，无需手动配置。
// evermeet.cx 单连接限流，全剧下载可能需要几分钟，期间按钮保持禁用。
async function runtimeInstall() {
  const buttons = [$("#env-install"), $("#runtime-install")].filter(Boolean);
  for (const button of buttons) {
    button.disabled = true;
    button.textContent = "正在配置…（下载安装中，请稍候）";
  }
  try {
    const report = await call("install_runtime");
    toast(report.note);
  } catch (error) {
    toast(String(error), true);
  } finally {
    for (const button of buttons) {
      button.disabled = false;
      button.textContent = "一键配置运行环境";
    }
    // 无论成败都刷新：成功后回填自动写入的路径并重测，失败时也展示最新状态。
    await loadSettings();
    await checkRuntime(true);
  }
}

function renderRuntimeList(status) {
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
  switchModule("settings");
  $("#runtime-list").closest(".panel")?.scrollIntoView({ behavior: "smooth", block: "center" });
});
$("#rename-plan").addEventListener("click", buildRenamePlan);
$("#rename-apply").addEventListener("click", applyRename);
$("#bitrate-probe").addEventListener("click", probeBitrate);
$("#bitrate-fix").addEventListener("click", requestBitrateFix);
$("#bitrate-confirm-yes").addEventListener("click", applyBitrateFix);
$("#bitrate-confirm-no").addEventListener("click", cancelBitrateFix);
$("#exec-run").addEventListener("click", executeTranscode);
$("#runtime-recheck").addEventListener("click", () => checkRuntime(true));
$("#runtime-install").addEventListener("click", runtimeInstall);
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
    toast(String(error), true);
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

// 设置页任一改动都刷新「未保存」标记，避免改了以为已经生效。
["#rule-min-episodes", "#rule-min-duration", "#rule-max-duration", "#rule-max-bitrate",
  "#rule-prefer-portrait", "#settings-output", "#ffprobe-path",
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
}

if (invoke) {
  loadSettings().then(() => checkRuntime());
  loadPlatformProfiles();
  loadVersion();
}
