// drama-cloud Web 客户端。
//
// 为什么放在独立文件而不是内联在 index.html：服务端对页面下发
// `Content-Security-Policy: script-src 'self'`，内联脚本和 onclick= 一律不执行，
// 这样即使某处漏了转义，注入的 HTML 也跑不起脚本、读不走 localStorage 里的令牌。
//
// 往 innerHTML 里拼任何来自服务器的值（素材名、错误信息、配方号……）都必须先过 esc()。

const $ = (id) => document.getElementById(id);

/** HTML 转义：拼进 innerHTML 的动态值一律先过这里。 */
function esc(value) {
  return String(value ?? "").replace(/[&<>"']/g, (ch) => (
    { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[ch]
  ));
}
const state = { token: localStorage.getItem("dramaToken") || "", assets: [], recipes: [], taskId: null, timer: null };

function headers(extra) {
  const h = Object.assign({}, extra || {});
  if (state.token) h["Authorization"] = "Bearer " + state.token;
  return h;
}

async function api(path, options) {
  const res = await fetch(path, Object.assign({ headers: headers() }, options || {}));
  const text = await res.text();
  let body;
  try { body = text ? JSON.parse(text) : null; } catch (e) { body = text; }
  if (!res.ok) {
    const detail = (body && body.detail) || body || {};
    const message = detail.message || detail.error || res.statusText;
    throw new Error(res.status + " · " + message);
  }
  return body;
}

function show(target, node) { $(target).innerHTML = ""; $(target).appendChild(node); }
function pre(value) { const el = document.createElement("pre"); el.textContent =
  typeof value === "string" ? value : JSON.stringify(value, null, 2); return el; }
function pill(state) { const s = document.createElement("span"); s.className = "pill state-" + state;
  s.textContent = state; return s; }

// ---------- 身份 ----------

async function connect() {
  if (!state.token) { $("whoami").textContent = "尚未设置令牌"; return; }
  try {
    const me = await api("/api/v1/auth/whoami");
    $("whoami").innerHTML = "";
    $("whoami").append("当前身份：", document.createTextNode(me.name + " / " + me.role));
    document.querySelector("#who").textContent = me.name + " · " + me.role;
    await refreshStatus(); await loadRecipes(); await loadAssets(); await loadReviews();
  } catch (error) {
    $("whoami").innerHTML = '<span class="err">' + esc(error.message) + "</span>";
  }
}

$("save").onclick = () => {
  state.token = $("token").value.trim();
  localStorage.setItem("dramaToken", state.token);
  connect();
};
$("forget").onclick = () => {
  state.token = ""; localStorage.removeItem("dramaToken"); $("token").value = "";
  $("whoami").textContent = "已清除"; $("status").textContent = "未连接";
};

// ---------- 状态 ----------

async function refreshStatus() {
  const data = await api("/api/v1/system/status");
  $("status").innerHTML = "";
  const grid = document.createElement("div"); grid.className = "kvs";
  const rows = [
    ["Worker", data.worker_version], ["引擎", data.engine_version],
    ["ffmpeg", data.ffmpeg.replace(/^ffmpeg version /, "").slice(0, 48)],
    ["队列深度", data.queue_depth], ["存储占用", (data.storage.objects_bytes / 1048576).toFixed(1) + " MB"],
    ["模块数", data.modules.length],
  ];
  for (const [k, v] of rows) { const a = document.createElement("div"); a.textContent = k;
    const b = document.createElement("div"); b.textContent = String(v); grid.append(a, b); }
  $("status").appendChild(grid);
  $("status").appendChild(pre(data.tasks_by_state));
}
$("refresh").onclick = () => refreshStatus().catch((e) => alert(e.message));

// ---------- 上传 ----------

$("upload").onclick = async () => {
  const file = $("file").files[0];
  if (!file) { alert("请先选择文件"); return; }
  const button = $("upload"); button.disabled = true;
  try {
    const name = $("assetName").value.trim() || file.name;
    const session = await api("/api/v1/uploads", {
      method: "POST",
      headers: headers({ "Content-Type": "application/json" }),
      body: JSON.stringify({ name, role: $("assetRole").value, filename: file.name }),
    });
    const res = await fetch(session.upload_url + "?token=" + encodeURIComponent(session.token), {
      method: "PUT", headers: headers({ "Content-Type": "application/octet-stream" }), body: file,
    });
    const body = await res.json();
    if (!res.ok) throw new Error(JSON.stringify(body));
    show("uploadResult", pre(body.asset));
    await loadAssets();
  } catch (error) { show("uploadResult", pre("上传失败：" + error.message)); }
  finally { button.disabled = false; }
};

async function loadAssets() {
  const data = await api("/api/v1/assets");
  state.assets = data.assets;
  $("assets").innerHTML = "";
  const table = document.createElement("table");
  table.innerHTML = "<tr><th>ID</th><th>名称</th><th>角色</th><th>大小</th></tr>";
  for (const asset of data.assets) {
    const tr = document.createElement("tr");
    tr.innerHTML = `<td><code>${esc(asset.id)}</code></td><td>${esc(asset.name)}</td>` +
                   `<td>${esc(asset.declaration.role || "video")}</td>` +
                   `<td>${esc((asset.size_bytes / 1048576).toFixed(1))} MB</td>`;
    table.appendChild(tr);
  }
  $("assets").appendChild(table);
  const select = $("inputs"); select.innerHTML = "";
  for (const asset of data.assets) {
    const option = document.createElement("option");
    option.value = asset.id;
    option.textContent = `${asset.name} · ${asset.declaration.role || "video"} · ${asset.id}`;
    select.appendChild(option);
  }
}

// ---------- 配方 ----------

async function loadRecipes() {
  const data = await api("/api/v1/recipes");
  state.recipes = data.recipes;
  const select = $("recipe"); select.innerHTML = "";
  const seen = new Set();
  for (const row of data.recipes) {
    if (row.status !== "published" || seen.has(row.recipe_id)) continue;
    seen.add(row.recipe_id);
    const option = document.createElement("option");
    option.value = row.recipe_id; option.textContent = `${row.recipe_id}@${row.version}（${row.purpose}）`;
    select.appendChild(option);
  }
  if (!select.options.length) {
    const option = document.createElement("option");
    option.value = ""; option.textContent = "没有已发布配方，请先运行 drama-cloud seed";
    select.appendChild(option);
  }
}

// ---------- 任务 ----------

$("submit").onclick = async () => {
  const inputs = Array.from($("inputs").selectedOptions).map((o) => o.value);
  if (!inputs.length) { alert("请选择输入素材"); return; }
  const recipeId = $("recipe").value;
  if (!recipeId) { alert("没有可用配方"); return; }
  let userParams = null;
  const raw = $("userParams").value.trim();
  if (raw) { try { userParams = JSON.parse(raw); } catch (e) { alert("参数覆盖不是合法 JSON"); return; } }
  try {
    const task = await api("/api/v1/tasks", {
      method: "POST",
      headers: headers({ "Content-Type": "application/json" }),
      body: JSON.stringify({ asset_ids: inputs, recipe_id: recipeId, user_params: userParams,
                             idempotency_key: $("idem").value.trim() || null }),
    });
    state.taskId = task.id; $("taskId").value = task.id;
    show("taskResult", pre({ id: task.id, state: task.state, recipe: task.recipe_id + "@" + task.recipe_version }));
    await loadTask(); await loadTasks();
  } catch (error) { show("taskResult", pre("创建失败：" + error.message)); }
};

$("watch").onclick = () => { state.taskId = $("taskId").value.trim(); loadTask().catch((e) => {
  show("taskDetail", pre(e.message)); }); };
$("cancel").onclick = async () => {
  if (!state.taskId) return;
  try { await api(`/api/v1/tasks/${state.taskId}/cancel`, { method: "POST" }); await loadTask(); }
  catch (e) { alert(e.message); }
};
$("retry").onclick = async () => {
  if (!state.taskId) return;
  try { await api(`/api/v1/tasks/${state.taskId}/retry`, { method: "POST" }); await loadTask(); }
  catch (e) { alert(e.message); }
};

async function loadTask() {
  if (!state.taskId) return;
  const task = await api("/api/v1/tasks/" + state.taskId);
  $("taskDetail").innerHTML = "";
  const head = document.createElement("div");
  head.append(pill(task.state));
  const info = document.createElement("div"); info.style.marginTop = "8px";
  info.innerHTML =
    `<div class="kvs"><div>配方</div><div>${esc(task.recipe_id)}@${esc(task.recipe_version)}</div>` +
    `<div>尝试次数</div><div>${esc(task.attempts)}</div>` +
    `<div>产物哈希</div><div><code>${esc(task.recipe_sha256.slice(0, 16))}…</code></div>` +
    `<div>进度</div><div>${esc((task.progress && task.progress.stage) || "-")}</div>` +
    (task.error ? `<div>错误</div><div class="err">${esc(task.error.class)}<br>${esc(task.error.message)}</div>` : "") +
    "</div>";
  $("taskDetail").append(head, info, pre(task.progress));
  renderArtifacts(task.artifacts || []);
  schedulePoll(task.state);
}

let pollTimer = null;

function schedulePoll(current) {
  if (pollTimer) { clearTimeout(pollTimer); pollTimer = null; }
  if (["SUCCEEDED", "FAILED", "CANCELLED"].includes(current)) return;
  pollTimer = setTimeout(() => { loadTask().catch(() => {}); }, 2500);
}

function renderArtifacts(artifacts) {
  $("artifacts").innerHTML = "";
  if (!artifacts.length) { $("artifacts").textContent = "暂无产物"; return; }
  const table = document.createElement("table");
  table.innerHTML = "<tr><th>类型</th><th>大小</th><th>SHA256</th><th></th></tr>";
  for (const artifact of artifacts) {
    const tr = document.createElement("tr");
    tr.innerHTML = `<td>${esc(artifact.kind)}</td><td>${esc((artifact.size_bytes / 1048576).toFixed(2))} MB</td>` +
                   `<td><code>${esc(artifact.sha256.slice(0, 12))}…</code></td>`;
    const cell = document.createElement("td");
    const link = document.createElement("a");
    link.textContent = "获取下载链接";
    link.href = "#";
    link.onclick = async (event) => {
      event.preventDefault();
      const detail = await api("/api/v1/artifacts/" + artifact.id);
      window.open(detail.download_url, "_blank");
    };
    cell.appendChild(link); tr.appendChild(cell); table.appendChild(tr);
  }
  $("artifacts").appendChild(table);
}

async function loadTasks() {
  const data = await api("/api/v1/tasks?limit=20");
  const table = document.createElement("table");
  table.innerHTML = "<tr><th>状态</th><th>任务</th><th>配方</th><th>创建时间</th></tr>";
  for (const task of data.tasks) {
    const tr = document.createElement("tr");
    tr.innerHTML = `<td></td><td><code>${esc(task.id)}</code></td>` +
                   `<td>${esc(task.recipe_id)}@${esc(task.recipe_version)}</td><td>${esc(task.created_at)}</td>`;
    tr.firstChild.appendChild(pill(task.state));
    tr.style.cursor = "pointer";
    tr.onclick = () => { state.taskId = task.id; $("taskId").value = task.id; loadTask(); };
    table.appendChild(tr);
  }
  $("tasks").innerHTML = ""; $("tasks").appendChild(table);
}

// ---------- 人工复核队列（确认流分支B） ----------

function reviewRow(item) {
  const tr = document.createElement("tr");
  const metrics = (item.detail && item.detail.inpaint && item.detail.inpaint.summary) || {};
  const parts = [];
  if (metrics.residual_max_ratio != null) parts.push("残留 " + (metrics.residual_max_ratio * 100).toFixed(2) + "%");
  if (metrics.flicker_max_index != null) parts.push("闪烁 " + metrics.flicker_max_index.toFixed(2));
  if (metrics.seam_max_score != null) parts.push("接缝 " + metrics.seam_max_score.toFixed(2));
  if (metrics.ssim_min != null) parts.push("SSIM " + Number(metrics.ssim_min).toFixed(3));

  const cells = [
    item.severity, item.reason,
    item.artifact_id ? "候选产物 " + item.artifact_id : "无产物",
    parts.join(" · ") || "—",
  ];
  for (const value of cells) {
    const td = document.createElement("td");
    td.textContent = value;
    tr.appendChild(td);
  }
  const actions = document.createElement("td");
  actions.style.whiteSpace = "nowrap";

  const detailBtn = document.createElement("button");
  detailBtn.className = "ghost";
  detailBtn.textContent = "详情";
  detailBtn.onclick = () => showReview(item.id);

  const approveBtn = document.createElement("button");
  approveBtn.textContent = "标记通过";
  approveBtn.disabled = !item.artifact_id;
  approveBtn.onclick = () => decideReview(item.id, "approve");

  const rejectBtn = document.createElement("button");
  rejectBtn.className = "ghost";
  rejectBtn.textContent = "标记失败";
  rejectBtn.onclick = () => decideReview(item.id, "reject");

  const rerunBtn = document.createElement("button");
  rerunBtn.className = "ghost";
  rerunBtn.textContent = "修正后重跑";
  rerunBtn.onclick = () => rerunReview(item.id);

  actions.append(detailBtn, approveBtn, rejectBtn, rerunBtn);
  tr.appendChild(actions);
  return tr;
}

async function loadReviews() {
  if (!state.token) { return; }
  const status = $("reviewStatus").value;
  const data = await api("/api/v1/reviews?status=" + encodeURIComponent(status) + "&limit=20");
  const box = $("reviews");
  box.innerHTML = "";
  if (!data.reviews.length) {
    box.textContent = "没有复核记录（status=" + status + "）";
    return;
  }
  const table = document.createElement("table");
  table.innerHTML = "<tr><th>级别</th><th>原因</th><th>产物</th><th>指标</th><th>动作</th></tr>";
  for (const item of data.reviews) table.appendChild(reviewRow(item));
  box.appendChild(table);
}

async function showReview(id) {
  const data = await api("/api/v1/reviews/" + encodeURIComponent(id));
  show("reviewDetail", pre(data));
}

async function decideReview(id, decision) {
  const note = window.prompt(decision === "approve" ? "放行说明（会写进审计）" : "判定失败的原因", "");
  if (note === null) return;
  try {
    const data = await api("/api/v1/reviews/" + encodeURIComponent(id) + "/decision", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ decision: decision, note: note }),
    });
    show("reviewDetail", pre(data));
    await loadReviews();
  } catch (error) {
    $("reviewDetail").innerHTML = '<span class="err">' + esc(error.message) + "</span>";
  }
}

async function rerunReview(id) {
  const raw = window.prompt(
    '修正 video.inpaint 参数（JSON，例如 {"dilate": 8, "feather": 4}）',
    '{"dilate": 8}'
  );
  if (raw === null) return;
  let params;
  try {
    params = JSON.parse(raw);
  } catch (e) {
    $("reviewDetail").innerHTML = '<span class="err">参数不是合法 JSON</span>';
    return;
  }
  try {
    const data = await api("/api/v1/reviews/" + encodeURIComponent(id) + "/rerun", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ user_params: { erase: params }, note: "人工修正 mask 后重跑" }),
    });
    show("reviewDetail", pre(data));
    await loadReviews();
  } catch (error) {
    $("reviewDetail").innerHTML = '<span class="err">' + esc(error.message) + "</span>";
  }
}

$("reviewsRefresh").onclick = () => loadReviews().catch((error) => {
  $("reviews").innerHTML = '<span class="err">' + esc(error.message) + "</span>";
});
$("reviewStatus").onchange = () => loadReviews().catch(() => {});

// ---------- 启动 ----------

if (state.token) { $("token").value = state.token; connect(); } else { $("whoami").textContent = "尚未设置令牌"; }
