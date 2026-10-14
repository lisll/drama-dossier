const { invoke } = window.__TAURI__?.core ?? {};

// 与 dossier-core 的 ChecklistRules::default() 保持一致。
const DEFAULT_RULES = {
  minEpisodes: 4,
  minDurationSec: 31,
  maxDurationSec: 480,
  maxBitrateBps: 8_000_000,
  preferPortrait: true,
};

const state = {
  root: "",
  dramas: [],
  selected: null,
  analysis: null,
  module: "library",
  runtime: null,
};
let renamePlan = null;
let bitrateItems = null;
// 最近一次从配置文件读到的设置：保存时用它带回 ffmpegPath（界面上没有对应输入框）。
let loadedSettings = null;

const $ = (selector) => document.querySelector(selector);
const esc = (value) =>
  String(value ?? "").replace(
    /[&<>'"]/g,
    (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", '"': "&quot;" })[c],
  );

function toast(message, isError = false) {
  const node = $("#toast");
  node.textContent = message;
  node.classList.toggle("error", isError);
  node.classList.remove("hidden");
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => node.classList.add("hidden"), 4500);
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

function rulesFromForm() {
  const raw = (id) => {
    const value = $(id).value.trim();
    return value === "" ? null : Number(value);
  };
  const episodes = raw("#rule-min-episodes") ?? DEFAULT_RULES.minEpisodes;
  const minDuration = raw("#rule-min-duration") ?? DEFAULT_RULES.minDurationSec;
  const maxDuration = raw("#rule-max-duration") ?? DEFAULT_RULES.maxDurationSec;
  const maxBitrateMbps = raw("#rule-max-bitrate");
  const rules = {
    minEpisodes: episodes,
    minDurationSec: minDuration,
    maxDurationSec: maxDuration,
    maxBitrateBps:
      maxBitrateMbps === null
        ? DEFAULT_RULES.maxBitrateBps
        : Math.round(maxBitrateMbps * 1e6),
    preferPortrait: $("#rule-prefer-portrait").checked,
  };

  if (!Number.isInteger(rules.minEpisodes) || rules.minEpisodes <= 0) {
    throw new Error("最少集数必须是大于 0 的整数。");
  }
  if (!(rules.minDurationSec > 0)) {
    throw new Error("单集时长下限必须是大于 0 的秒数。");
  }
  if (!(rules.maxDurationSec > rules.minDurationSec)) {
    throw new Error("单集时长上限必须大于下限。");
  }
  if (!(rules.maxBitrateBps > 0)) {
    throw new Error("码率上限必须大于 0。");
  }
  return rules;
}

function fillRules(rules) {
  const merged = { ...DEFAULT_RULES, ...(rules ?? {}) };
  $("#rule-min-episodes").value = merged.minEpisodes;
  $("#rule-min-duration").value = merged.minDurationSec;
  $("#rule-max-duration").value = merged.maxDurationSec;
  $("#rule-max-bitrate").value = Number((merged.maxBitrateBps / 1e6).toFixed(3));
  $("#rule-prefer-portrait").checked = merged.preferPortrait !== false;
}

async function loadSettings() {
  try {
    const payload = await call("load_app_settings");
    const settings = payload.settings ?? {};
    loadedSettings = settings;
    $("#settings-path").textContent = payload.path;
    fillRules(settings.rules);
    if (settings.ffprobePath) $("#ffprobe-path").value = settings.ffprobePath;
    if (settings.outputDir) $("#output-dir").value = settings.outputDir;
    if (payload.warning) toast(payload.warning, true);
  } catch (error) {
    toast(String(error), true);
  }
}

async function saveSettings() {
  let rules;
  try {
    rules = rulesFromForm();
  } catch (error) {
    toast(String(error), true);
    return;
  }
  try {
    const path = await call("save_app_settings", {
      settings: {
        rules,
        // ffmpegPath 由「一键配置运行环境」写入，界面无输入框，保存时原样带回避免丢失。
        ffmpegPath: loadedSettings?.ffmpegPath ?? null,
        ffprobePath: $("#ffprobe-path").value.trim() || null,
        outputDir: $("#output-dir").value.trim() || null,
      },
    });
    $("#settings-path").textContent = path;
    toast("设置已保存到本机配置文件");
  } catch (error) {
    toast(String(error), true);
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
  // 剧目切换后，工具箱里的旧计划全部作废。
  renamePlan = null;
  bitrateItems = null;
  $("#rename-apply").disabled = true;
  $("#bitrate-fix").disabled = true;
  $("#rename-result").classList.add("hidden");
  $("#bitrate-result").classList.add("hidden");
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
            <span class="status-${esc(item.status)}">${esc(item.status)}</span>
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
  try {
    state.dramas = await call("scan_library", { root });
    state.selected = null;
    state.analysis = null;
    $("#detail-view").classList.add("hidden");
    $("#detail-placeholder").classList.remove("hidden");
    renderDramas();
    renderContexts();
    toast(`已发现 ${state.dramas.length} 个剧目`);
  } catch (error) {
    toast(String(error), true);
  }
}

async function analyze() {
  if (state.selected == null) return;
  const drama = state.dramas[state.selected];
  try {
    state.analysis = await call("analyze_drama", {
      root: state.root,
      name: drama.name,
      ffprobePath: $("#ffprobe-path").value.trim() || null,
      rules: rulesFromForm(),
    });
    renderChecklist(state.analysis.checklist);
    renderEpisodes(state.analysis.probes);
    toast(state.analysis.checklist.ready ? "检查完成，材料可用" : "检查完成，请处理缺失或警告项");
  } catch (error) {
    toast(String(error), true);
  }
}

async function generate() {
  if (state.selected == null) return;
  const drama = state.dramas[state.selected];
  const investment = $("#total-investment").value;
  const ratio = $("#actor-ratio").value;
  try {
    const result = await call("generate_package", {
      root: state.root,
      name: drama.name,
      outputDir: $("#output-dir").value.trim() || null,
      copyEpisodes: $("#copy-episodes").checked,
      aiGenerated: $("#ai-generated").checked,
      ffprobePath: $("#ffprobe-path").value.trim() || null,
      rules: rulesFromForm(),
      costSheet: {
        companyName: $("#company-name").value.trim(),
        totalInvestmentWan: investment === "" ? null : Number(investment),
        actorRatioPercent: ratio === "" ? null : Number(ratio),
        contact: $("#contact").value.trim(),
      },
    });
    const resultNode = $("#generate-result");
    resultNode.innerHTML = `
      <strong>报审包已生成</strong><br />
      位置：${esc(result.packageDir)}<br />
      文件数：${result.fileCount} · 总大小：${formatBytes(Number(result.totalBytes))}
    `;
    resultNode.classList.remove("hidden");
    toast("报审包生成完成");
  } catch (error) {
    toast(String(error), true);
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
  try {
    const drama = selectedDrama();
    const payload = await call("transcode_advice", {
      root: state.root,
      name: drama.name,
      target: transcodeTarget(),
      ffprobePath: $("#ffprobe-path").value.trim() || null,
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
    const node = $("#transcode-result");
    node.innerHTML = `
      <strong>共 ${plan.episodes.length} 集：需转码 ${plan.transcodeCount} 集，已合规 ${plan.compliantCount} 集</strong><br />
      输出目录：${esc(plan.outputDir)}<br />
      ${
        rows
          ? `<table><thead><tr><th>集号</th><th>文件</th><th>原因</th></tr></thead><tbody>${rows}</tbody></table>`
          : "<p>所有分集都符合目标规格。</p>"
      }
      ${plan.transcodeCount ? `<details><summary>批量脚本预览</summary><pre>${esc(payload.script)}</pre></details>` : ""}
    `;
    node.classList.remove("hidden");
  } catch (error) {
    toast(String(error), true);
  }
}

async function diffReportPackages() {
  try {
    const result = await call("diff_report_packages", {
      leftDir: $("#diff-left").value.trim(),
      rightDir: $("#diff-right").value.trim(),
    });
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
    const node = $("#diff-result");
    node.innerHTML = `
      <strong>${esc(result.summary)}</strong><br />
      ${esc(result.leftGeneratedAt)} → ${esc(result.rightGeneratedAt)}<br />
      ${
        rows
          ? `<table><thead><tr><th>路径</th><th>变化</th><th>字节</th><th>说明</th></tr></thead><tbody>${rows}</tbody></table>`
          : "<p>两份报审包内容一致。</p>"
      }
    `;
    node.classList.remove("hidden");
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
  try {
    const drama = selectedDrama();
    const review = await call("platform_review", {
      root: state.root,
      name: drama.name,
      profileId: $("#platform-select").value,
      ffprobePath: $("#ffprobe-path").value.trim() || null,
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
    const node = $("#platform-result");
    node.innerHTML = `
      <strong>${esc(review.profileName)}：${review.clean ? "无阻断项" : `${review.failCount} 项阻断`}，${review.warnCount} 项警告</strong><br />
      <table><thead><tr><th>集号</th><th>文件</th><th>需关注</th></tr></thead><tbody>${body}</tbody></table>
      ${review.notes.length ? `<p>${esc(review.notes.join("；"))}</p>` : ""}
    `;
    node.classList.remove("hidden");
  } catch (error) {
    toast(String(error), true);
  }
}

async function exportDraft() {
  try {
    const drama = selectedDrama();
    const result = await call("export_draft", {
      root: state.root,
      name: drama.name,
      options: { outputDir: $("#draft-dir").value.trim() || null },
      ffprobePath: $("#ffprobe-path").value.trim() || null,
    });
    const node = $("#draft-result");
    node.innerHTML = `
      <strong>草稿已导出</strong><br />
      目录：${esc(result.draftDir)}<br />
      片段数：${result.segmentCount} · 总时长：${Number(result.durationSec).toFixed(1)} 秒
    `;
    node.classList.remove("hidden");
    $("#draft-dir").value = result.draftDir;
  } catch (error) {
    toast(String(error), true);
  }
}

async function importDraft() {
  try {
    const draft = await call("import_draft", { draftDir: $("#draft-dir").value.trim() });
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
    const node = $("#draft-result");
    node.innerHTML = `
      <strong>草稿：${esc(draft.draftName || "（未命名）")}</strong> · ${draft.width}×${draft.height} @ ${draft.fps}fps<br />
      总时长：${Number(draft.durationSec).toFixed(1)} 秒<br />
      <table><thead><tr><th>#</th><th>素材</th><th>时间线起点</th><th>时长</th></tr></thead><tbody>${rows}</tbody></table>
    `;
    node.classList.remove("hidden");
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
  try {
    const plan = await call("plan_erase", {
      request: eraseRequest(),
      toolPath: $("#erase-tool-path").value.trim() || null,
    });
    renderErasePlan(plan);
  } catch (error) {
    toast(String(error), true);
  }
}

async function runErase() {
  const button = $("#erase-run");
  button.disabled = true;
  try {
    const run = await call("run_erase", {
      request: eraseRequest(),
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
    button.disabled = false;
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

async function buildRenamePlan() {
  try {
    const drama = selectedDrama();
    renamePlan = await call("list_rename_plan", {
      root: state.root,
      name: drama.name,
    });
    const rows = renamePlan.items
      .map(
        (item) => `<tr>
          <td>${esc(item.from)}</td>
          <td>${item.unchanged ? "—" : esc(item.to)}</td>
          <td>${item.unchanged ? "已合规" : "待改名"}</td>
        </tr>`,
      )
      .join("");
    const skipped = renamePlan.skipped.length
      ? `<p>无法解析集号、保持不动：${esc(renamePlan.skipped.join("、"))}</p>`
      : "";
    const pending = renamePlan.items.filter((item) => !item.unchanged).length;
    const node = $("#rename-result");
    node.innerHTML = `
      <strong>共 ${renamePlan.items.length} 集：待改名 ${pending} 集，已合规 ${
        renamePlan.items.length - pending
      } 集</strong>
      <table><thead><tr><th>当前文件名</th><th>目标文件名</th><th>状态</th></tr></thead><tbody>${rows}</tbody></table>
      ${skipped}
    `;
    node.classList.remove("hidden");
    $("#rename-apply").disabled = pending === 0;
  } catch (error) {
    toast(String(error), true);
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
  const button = $("#rename-apply");
  button.disabled = true;
  try {
    const drama = selectedDrama();
    const renamed = await call("apply_rename", {
      root: state.root,
      name: drama.name,
      items,
    });
    toast(`已重命名 ${renamed} 个文件；建议重新扫描并检查`);
    await buildRenamePlan();
  } catch (error) {
    toast(String(error), true);
  } finally {
    button.disabled = false;
  }
}

// ============ 工具箱：码率修复 ============

function bitrateMinBps() {
  const value = Number($("#bitrate-min").value);
  if (!(value > 0)) throw new Error("码率安全线必须大于 0。");
  return Math.round(value * 1e6);
}

async function probeBitrate() {
  try {
    const drama = selectedDrama();
    bitrateItems = await call("probe_bitrate", {
      root: state.root,
      name: drama.name,
      minKbps: Math.round(bitrateMinBps() / 1000),
      ffprobePath: $("#ffprobe-path").value.trim() || null,
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
    const node = $("#bitrate-result");
    node.innerHTML = `
      <strong>共 ${bitrateItems.length} 集：${fixable} 集低于安全线</strong>
      <table><thead><tr><th>文件</th><th>码率</th><th>状态</th></tr></thead><tbody>${rows}</tbody></table>
    `;
    node.classList.remove("hidden");
    $("#bitrate-fix").disabled = fixable === 0;
  } catch (error) {
    toast(String(error), true);
  }
}

async function fixBitrate() {
  const button = $("#bitrate-fix");
  button.disabled = true;
  button.textContent = "正在重编码…";
  try {
    const drama = selectedDrama();
    const report = await call("fix_bitrate", {
      root: state.root,
      name: drama.name,
      minKbps: Math.round(bitrateMinBps() / 1000),
      ffprobePath: $("#ffprobe-path").value.trim() || null,
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
    const node = $("#bitrate-result");
    node.innerHTML = `
      <strong>修复完成：成功 ${report.fixed.length} 集，失败 ${report.failed.length} 集，达标未动 ${report.untouched} 集</strong>
      ${fixedRows ? `<table><thead><tr><th>已修复</th><th>码率变化</th></tr></thead><tbody>${fixedRows}</tbody></table>` : ""}
      ${failedRows ? `<table><thead><tr><th>失败</th><th>原因</th></tr></thead><tbody>${failedRows}</tbody></table>` : ""}
    `;
    node.classList.remove("hidden");
    toast("码率修复结束");
  } catch (error) {
    toast(String(error), true);
  } finally {
    button.textContent = "修复不达标分集";
    button.disabled = false;
  }
}

// ============ 工具箱：执行转码（微调 / 重组） ============

async function executeTranscode() {
  const button = $("#exec-run");
  const mode = $("#exec-mode").value;
  const targetRaw = $("#exec-target").value.trim();
  button.disabled = true;
  button.textContent = "正在转码…（全剧重编码，可能需要几分钟）";
  try {
    const drama = selectedDrama();
    const report = await call("transcode_execute", {
      root: state.root,
      name: drama.name,
      request: {
        mode,
        targetCount: targetRaw === "" ? null : Number(targetRaw),
        outputDir: $("#exec-output").value.trim() || null,
      },
      ffprobePath: $("#ffprobe-path").value.trim() || null,
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
    const node = $("#exec-result");
    node.innerHTML = `
      <strong>${esc(report.note)}</strong><br />
      输出目录：${esc(report.outputDir)}
      ${
        rows
          ? `<table><thead><tr><th>来源</th><th>输出</th><th>说明</th></tr></thead><tbody>${rows}</tbody></table>`
          : ""
      }
    `;
    node.classList.remove("hidden");
    toast("转码完成");
  } catch (error) {
    toast(String(error), true);
  } finally {
    button.disabled = false;
    button.textContent = "执行转码";
  }
}

// ============ 运行环境（全局横幅 + 设置-本机环境） ============

async function checkRuntime(force = false) {
  if (state.runtime && !force) {
    renderEnvBanner(state.runtime);
    renderRuntimeList(state.runtime);
    return;
  }
  try {
    const status = await call("runtime_status", {
      ffprobePath: $("#ffprobe-path").value.trim() || null,
    });
    state.runtime = status;
    renderEnvBanner(status);
    renderRuntimeList(status);
  } catch (error) {
    toast(String(error), true);
  }
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

$("#browse-root").addEventListener("click", () => browse("#library-root"));
document.querySelectorAll(".nav-item").forEach((node) => {
  node.addEventListener("click", () => switchModule(node.dataset.module));
});
$("#rename-plan").addEventListener("click", buildRenamePlan);
$("#rename-apply").addEventListener("click", applyRename);
$("#bitrate-probe").addEventListener("click", probeBitrate);
$("#bitrate-fix").addEventListener("click", fixBitrate);
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
  $("#ffprobe-path").value = "";
  $("#output-dir").value = "";
  toast("已恢复默认阈值；点击「保存设置」后写入配置文件");
});
$("#library-root").addEventListener("keydown", (event) => {
  if (event.key === "Enter") scan();
});

if (invoke) {
  loadSettings();
  loadPlatformProfiles();
  loadVersion();
  checkRuntime();
}
