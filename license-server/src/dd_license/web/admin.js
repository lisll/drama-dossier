"use strict";
// 剧备授权管理后台。所有渲染都走 textContent / DOM API，不拼接 HTML（防 XSS，配合严格 CSP）。

const $ = (sel) => document.querySelector(sel);
const state = { csrf: "", username: "", plans: [], page: { batches: 1, machines: 1, audit: 1 }, machine: null, lastCodes: [] };

const PLAN_LABEL = { trial: "试用卡", month: "月卡", quarter: "季卡", year: "年卡", lifetime: "永久卡" };
const ACTION_LABEL = {
  activate: "激活", "activate.fail": "激活失败", "batch.create": "生成卡密", "batch.revoke": "作废批次",
  "card.revoke": "作废卡密", "machine.revoke": "作废电脑", "machine.unrevoke": "恢复电脑",
  "admin.login": "登录", "admin.login.fail": "登录失败", "admin.reauth.fail": "二次确认失败", "admin.password": "改密码",
  "admin.create": "创建管理员", "admin.disable": "禁用管理员", "admin.enable": "启用管理员", "admin.unlock": "解除锁定",
};

class ApiError extends Error {
  constructor(status, body) {
    super((body && body.message) || `请求失败（${status}）`);
    this.status = status;
    this.code = body && body.error;
  }
}

async function api(method, path, body) {
  const opts = { method, credentials: "same-origin", headers: {} };
  if (method !== "GET") {
    opts.headers["Content-Type"] = "application/json";
    opts.headers["X-CSRF-Token"] = state.csrf;
    opts.body = JSON.stringify(body || {});
  }
  const resp = await fetch(path, opts);
  let data = null;
  try { data = await resp.json(); } catch { data = null; }
  if (!resp.ok) {
    if (resp.status === 401 && path !== "/admin/api/login") showLogin("登录已过期，请重新登录");
    throw new ApiError(resp.status, data);
  }
  return data;
}

function fmtTime(ts) {
  if (!ts) return "-";
  const d = new Date(ts * 1000);
  const p = (n) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

function el(tag, text, cls) {
  const e = document.createElement(tag);
  if (text !== undefined && text !== null) e.textContent = String(text);
  if (cls) e.className = cls;
  return e;
}

function setMsg(node, text, ok = false) {
  node.textContent = text || "";
  node.classList.toggle("ok", ok);
}

function renderTable(table, headers, rows) {
  table.replaceChildren();
  const thead = el("thead");
  const tr = el("tr");
  headers.forEach((h) => tr.append(el("th", h)));
  thead.append(tr);
  const tbody = el("tbody");
  if (!rows.length) {
    const r = el("tr");
    const td = el("td", "暂无数据");
    td.colSpan = headers.length;
    r.append(td);
    tbody.append(r);
  }
  rows.forEach((cells) => {
    const r = el("tr");
    cells.forEach((c) => {
      if (c instanceof Node) {
        const td = el("td");
        td.append(c);
        r.append(td);
      } else if (c && typeof c === "object") {
        r.append(el("td", c.text, c.cls));
      } else {
        r.append(el("td", c));
      }
    });
    tbody.append(r);
  });
  table.append(thead, tbody);
}

function renderDl(dl, pairs) {
  dl.replaceChildren();
  pairs.forEach(([k, v, cls]) => {
    dl.append(el("dt", k));
    dl.append(el("dd", v, cls));
  });
}

function renderPager(node, data, key, reload) {
  node.replaceChildren();
  const pages = Math.max(1, Math.ceil(data.total / data.size));
  const prev = el("button", "上一页");
  prev.type = "button";
  prev.disabled = data.page <= 1;
  prev.onclick = () => { state.page[key] = data.page - 1; reload(); };
  const next = el("button", "下一页");
  next.type = "button";
  next.disabled = data.page >= pages;
  next.onclick = () => { state.page[key] = data.page + 1; reload(); };
  node.append(prev, el("span", `第 ${data.page} / ${pages} 页，共 ${data.total} 条`), next);
}

// ---------------------------------------------------------------- 对话框

function confirmDialog({ title, text, fields = [], okText = "确认" }) {
  const dlg = $("#dlg");
  $("#dlg-title").textContent = title;
  $("#dlg-text").textContent = text;
  $("#dlg-ok").textContent = okText;
  setMsg($("#dlg-msg"), "");
  const box = $("#dlg-fields");
  box.replaceChildren();
  const inputs = {};
  fields.forEach((f) => {
    const label = el("label", f.label);
    const input = el("input");
    input.type = f.type || "text";
    input.required = true;
    input.maxLength = f.maxLength || 256;
    if (f.autocomplete) input.autocomplete = f.autocomplete;
    label.append(input);
    box.append(label);
    inputs[f.name] = input;
  });
  return new Promise((resolve) => {
    dlg.onclose = () => {
      if (dlg.returnValue !== "ok") return resolve(null);
      const values = {};
      for (const [k, v] of Object.entries(inputs)) values[k] = v.value;
      resolve(values);
    };
    dlg.returnValue = "";
    dlg.showModal();
    const first = Object.values(inputs)[0];
    if (first) first.focus();
  });
}

// ---------------------------------------------------------------- 登录

function showLogin(msg) {
  state.csrf = "";
  $("#app-view").classList.add("hidden");
  $("#login-view").classList.remove("hidden");
  setMsg($("#login-msg"), msg || "");
  $("#login-user").focus();
}

function showApp(me) {
  state.csrf = me.csrf;
  state.username = me.username;
  $("#who").textContent = `管理员：${me.username}`;
  const banner = $("#banner");
  if (me.allow_any_ip) {
    banner.textContent = "警告：已开启 ALLOW_ANY_IP，任何 IP 都能看到本后台登录页。请尽快改用 IP 白名单。";
    banner.classList.remove("hidden");
  } else {
    banner.classList.add("hidden");
  }
  $("#login-view").classList.add("hidden");
  $("#app-view").classList.remove("hidden");
  switchTab("overview");
}

$("#login-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  // 按回车或脚本提交时 ev.submitter 可能为 null，直接取表单里的提交按钮
  const btn = ev.currentTarget.querySelector("button[type=submit]");
  btn.disabled = true;
  try {
    const me = await api("POST", "/admin/api/login", { username: $("#login-user").value, password: $("#login-pass").value });
    $("#login-pass").value = "";
    await loadPlans();
    showApp({ ...me, allow_any_ip: (await api("GET", "/admin/api/me")).allow_any_ip });
  } catch (e) {
    setMsg($("#login-msg"), e.message);
  } finally {
    btn.disabled = false;
  }
});

$("#logout").addEventListener("click", async () => {
  try { await api("POST", "/admin/api/logout"); } catch { /* 会话已失效时同样回到登录页 */ }
  showLogin("已退出登录");
});

// ---------------------------------------------------------------- 导航

function switchTab(name) {
  document.querySelectorAll("#nav button").forEach((b) => b.classList.toggle("active", b.dataset.tab === name));
  document.querySelectorAll(".tab").forEach((t) => t.classList.toggle("hidden", t.id !== `tab-${name}`));
  const loaders = { overview: loadOverview, batches: loadBatches, machines: loadMachines, audit: loadAudit };
  if (loaders[name]) loaders[name]().catch(reportError);
}

document.querySelectorAll("#nav button").forEach((b) => b.addEventListener("click", () => switchTab(b.dataset.tab)));

function reportError(e) {
  if (e instanceof ApiError && e.status === 401) return;
  const banner = $("#banner");
  banner.textContent = e.message;
  banner.classList.remove("hidden");
  setTimeout(() => banner.classList.add("hidden"), 6000);
}

// ---------------------------------------------------------------- 概览

async function loadOverview() {
  const s = await api("GET", "/admin/api/stats");
  const a = s.alerts_24h;
  const alerts = $("#alerts");
  alerts.replaceChildren();
  const line = (t, bad) => { const p = el("p", t, bad ? "warn" : ""); alerts.append(p); };
  line(`最近 24 小时：后台登录失败 ${a.admin_login_fail} 次，卡密激活失败 ${a.activate_fail} 次`, a.admin_login_fail >= 5 || a.activate_fail >= 50);
  line(`最近 24 小时登录 IP：${a.admin_login_ips.length ? a.admin_login_ips.join("、") : "无"}`, a.admin_login_ips.length > 1);
  renderTable($("#plan-stats"), ["卡种", "总数", "未用", "已用", "已作废"],
    Object.entries(s.plans).map(([code, p]) => [PLAN_LABEL[code] || code, p.total, p.unused, p.redeemed, p.revoked]));
  renderDl($("#machine-stats"), [
    ["已激活电脑", s.machines.activated], ["当前有效授权", s.machines.active], ["7 天内到期", s.machines.expiring_7d],
  ]);
  renderTable($("#recent"), ["时间", "机器码前缀", "详情"], s.recent_activations.map((r) => {
    let d = {};
    try { d = JSON.parse(r.detail); } catch { d = {}; }
    return [fmtTime(r.at), { text: r.mid_prefix, cls: "mono" }, `${PLAN_LABEL[d.plan] || d.plan || ""} → 到期 ${d.after ? fmtTime(d.after) : "永久"}`];
  }));
}

// ---------------------------------------------------------------- 生成

async function loadPlans() {
  state.plans = await api("GET", "/admin/api/plans");
  const sel = $("#gen-plan");
  sel.replaceChildren();
  state.plans.forEach((p) => {
    const o = el("option", `${p.label}（${p.days ? p.days + " 天" : "永久"}）`);
    o.value = p.code;
    sel.append(o);
  });
}

$("#gen-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  // 按回车或脚本提交时 ev.submitter 可能为 null，直接取表单里的提交按钮
  const btn = ev.currentTarget.querySelector("button[type=submit]");
  const msg = $("#gen-msg");
  const count = Number($("#gen-count").value);
  const plan = state.plans.find((p) => p.code === $("#gen-plan").value);
  if (!window.confirm(`确认生成 ${count} 张「${plan ? plan.label : ""}」？`)) return;
  btn.disabled = true;
  setMsg(msg, "生成中…", true);
  try {
    const r = await api("POST", "/admin/api/batches", {
      plan: $("#gen-plan").value, count, note: $("#gen-note").value, password: $("#gen-pass").value,
    });
    state.lastCodes = r.codes;
    state.lastBatch = r.batch;
    $("#gen-pass").value = "";
    $("#gen-batch").textContent = `#${r.batch}（${r.codes.length} 张）`;
    $("#gen-codes").value = r.codes.join("\n");
    $("#gen-result").classList.remove("hidden");
    setMsg(msg, "已生成，请立即保存", true);
  } catch (e) {
    setMsg(msg, e.message);
  } finally {
    btn.disabled = false;
  }
});

$("#gen-copy").addEventListener("click", async () => {
  try {
    await navigator.clipboard.writeText(state.lastCodes.join("\n"));
    setMsg($("#gen-msg"), "已复制到剪贴板", true);
  } catch {
    $("#gen-codes").select();
    setMsg($("#gen-msg"), "浏览器不允许直接复制，已为你选中文本，请按 Ctrl+C");
  }
});

$("#gen-download").addEventListener("click", () => {
  const plan = $("#gen-plan").value;
  const lines = ["code,plan,batch", ...state.lastCodes.map((c) => `${c},${plan},${state.lastBatch}`)];
  // CSV 在浏览器本地生成，服务器不留副本
  const blob = new Blob(["\ufeff" + lines.join("\r\n") + "\r\n"], { type: "text/csv;charset=utf-8" });
  const a = el("a");
  a.href = URL.createObjectURL(blob);
  a.download = `cards-batch-${state.lastBatch}-${plan}.csv`;
  document.body.append(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(a.href), 1000);
});

// ---------------------------------------------------------------- 批次

async function loadBatches() {
  const data = await api("GET", `/admin/api/batches?page=${state.page.batches}`);
  renderTable($("#batches"), ["编号", "卡种", "数量", "已用", "备注", "创建人", "创建时间", "状态", ""], data.items.map((b) => {
    let action = "";
    if (!b.revoked_at) {
      action = el("button", "作废整批", "danger");
      action.type = "button";
      action.onclick = () => revokeBatch(b);
    }
    return [b.id, PLAN_LABEL[b.plan] || b.plan, b.count, b.redeemed, b.note, b.created_by, fmtTime(b.created_at),
      b.revoked_at ? el("span", "已作废", "tag bad") : el("span", "正常", "tag ok"), action];
  }));
  renderPager($("#batches-pager"), data, "batches", loadBatches);
}

async function revokeBatch(b) {
  const ok = await confirmDialog({
    title: `作废批次 #${b.id}`,
    text: `该批次中尚未兑换的卡将全部失效（已兑换的 ${b.redeemed} 张不受影响）。此操作不可撤销。`,
    okText: "作废",
  });
  if (!ok) return;
  try {
    const r = await api("POST", `/admin/api/batches/${b.id}/revoke`);
    reportOk(`已作废 ${r.revoked} 张未兑换的卡`);
    loadBatches();
  } catch (e) { reportError(e); }
}

function reportOk(text) {
  const banner = $("#banner");
  banner.textContent = text;
  banner.classList.remove("hidden");
  banner.style.background = "var(--accent)";
  setTimeout(() => { banner.classList.add("hidden"); banner.style.background = ""; }, 4000);
}

// ---------------------------------------------------------------- 卡密查询

let lookedUp = null;
$("#lookup-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const msg = $("#lookup-msg");
  setMsg(msg, "");
  $("#lookup-result").classList.add("hidden");
  try {
    const r = await api("POST", "/admin/api/cards/lookup", { code: $("#lookup-code").value });
    lookedUp = $("#lookup-code").value;
    const statusText = { unused: "未使用", redeemed: "已兑换", revoked: "已作废" }[r.status];
    renderDl($("#lookup-dl"), [
      ["卡密", r.display, "mono"], ["状态", statusText], ["卡种", `${PLAN_LABEL[r.plan] || r.plan}（${r.days ? r.days + " 天" : "永久"}）`],
      ["批次", `#${r.batch_id} ${r.batch_note || ""}`], ["生成时间", fmtTime(r.created_at)],
      ["兑换时间", fmtTime(r.redeemed_at)], ["兑换电脑", r.redeemed_mid || "-", "mono"], ["作废时间", fmtTime(r.revoked_at)],
    ]);
    $("#lookup-revoke").classList.toggle("hidden", r.status !== "unused");
    $("#lookup-result").classList.remove("hidden");
  } catch (e) {
    setMsg(msg, e.message);
  }
});

$("#lookup-revoke").addEventListener("click", async () => {
  const ok = await confirmDialog({ title: "作废卡密", text: "作废后这张卡将无法再兑换，此操作不可撤销。", okText: "作废" });
  if (!ok) return;
  try {
    await api("POST", "/admin/api/cards/revoke", { code: lookedUp });
    reportOk("已作废");
    $("#lookup-form").requestSubmit();
  } catch (e) { reportError(e); }
});

// ---------------------------------------------------------------- 电脑

function machineStatus(m) {
  if (m.revoked_at) return el("span", "已作废", "tag bad");
  if (!m.plan) return el("span", "未激活", "tag");
  if (m.until && m.until * 1000 <= Date.now()) return el("span", "已过期", "tag bad");
  return el("span", "有效", "tag ok");
}

async function loadMachines() {
  const q = encodeURIComponent($("#machine-q").value.trim().toLowerCase());
  const st = encodeURIComponent($("#machine-status").value);
  const data = await api("GET", `/admin/api/machines?q=${q}&status=${st}&page=${state.page.machines}`);
  renderTable($("#machines"), ["编号", "机器码", "卡种", "到期", "首次激活", "最近续签", "版本", "状态", ""], data.items.map((m) => {
    const btn = el("button", "详情");
    btn.type = "button";
    btn.onclick = () => showMachine(m.id);
    return [m.id, { text: m.mid.slice(0, 16) + "…", cls: "mono" }, PLAN_LABEL[m.plan] || "-",
      m.plan === "lifetime" ? "永久" : fmtTime(m.until), fmtTime(m.first_activated_at), fmtTime(m.last_seen_at),
      m.last_app_version || "-", machineStatus(m), btn];
  }));
  renderPager($("#machines-pager"), data, "machines", loadMachines);
}

$("#machine-filter").addEventListener("submit", (ev) => { ev.preventDefault(); state.page.machines = 1; loadMachines().catch(reportError); });

async function showMachine(id) {
  const m = await api("GET", `/admin/api/machines/${id}`);
  state.machine = m;
  renderDl($("#machine-dl"), [
    ["编号", m.id], ["机器码", m.mid, "mono"], ["卡种", PLAN_LABEL[m.plan] || "-"],
    ["到期", m.plan === "lifetime" ? "永久" : fmtTime(m.until)], ["首次激活", fmtTime(m.first_activated_at)],
    ["最近续签", fmtTime(m.last_seen_at)], ["客户端版本", m.last_app_version || "-"],
    ["作废", m.revoked_at ? `${fmtTime(m.revoked_at)}：${m.revoke_reason || ""}` : "否"],
  ]);
  renderTable($("#machine-cards"), ["卡密摘要前缀", "卡种", "天数", "批次", "兑换时间"],
    m.cards.map((c) => [{ text: c.digest_prefix, cls: "mono" }, PLAN_LABEL[c.plan] || c.plan, c.days || "永久", `#${c.batch_id}`, fmtTime(c.redeemed_at)]));
  $("#m-revoke").classList.toggle("hidden", Boolean(m.revoked_at));
  $("#m-unrevoke").classList.toggle("hidden", !m.revoked_at);
  $("#machine-detail").classList.remove("hidden");
}

$("#m-revoke").addEventListener("click", async () => {
  const m = state.machine;
  const v = await confirmDialog({
    title: `作废电脑 #${m.id}`,
    text: "该电脑下次联网续签时立即锁定；如果一直不联网，最多还能再用 3 天（凭证有效期，可用 DD_LICENSE_TOKEN_DAYS 调整）。",
    fields: [{ name: "reason", label: "原因（必填，如：退款、盗刷）", maxLength: 200 }],
    okText: "作废",
  });
  if (!v) return;
  try {
    await api("POST", `/admin/api/machines/${m.id}/revoke`, { reason: v.reason });
    reportOk("已作废");
    showMachine(m.id);
    loadMachines();
  } catch (e) { reportError(e); }
});

$("#m-unrevoke").addEventListener("click", async () => {
  const m = state.machine;
  const ok = await confirmDialog({ title: `恢复电脑 #${m.id}`, text: "恢复后这台电脑可以正常续签。", okText: "恢复" });
  if (!ok) return;
  try {
    await api("POST", `/admin/api/machines/${m.id}/unrevoke`);
    reportOk("已恢复");
    showMachine(m.id);
    loadMachines();
  } catch (e) { reportError(e); }
});

// ---------------------------------------------------------------- 审计

async function loadAudit() {
  const action = encodeURIComponent($("#audit-action").value.trim());
  const actor = encodeURIComponent($("#audit-actor").value.trim());
  const data = await api("GET", `/admin/api/audit?action=${action}&actor=${actor}&page=${state.page.audit}`);
  renderTable($("#audit"), ["时间", "操作人", "操作", "IP", "机器码", "卡密", "详情"], data.items.map((r) => [
    fmtTime(r.at), r.actor, ACTION_LABEL[r.action] || r.action, r.ip, { text: r.mid_prefix || "", cls: "mono" },
    { text: r.card_prefix || "", cls: "mono" }, { text: r.detail === "{}" ? "" : r.detail, cls: "mono" },
  ]));
  renderPager($("#audit-pager"), data, "audit", loadAudit);
}

$("#audit-filter").addEventListener("submit", (ev) => { ev.preventDefault(); state.page.audit = 1; loadAudit().catch(reportError); });

// ---------------------------------------------------------------- 账号

$("#pw-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const msg = $("#pw-msg");
  if ($("#pw-new").value !== $("#pw-new2").value) return setMsg(msg, "两次输入的新密码不一致");
  try {
    await api("POST", "/admin/api/password", { old: $("#pw-old").value, new: $("#pw-new").value });
    ["#pw-old", "#pw-new", "#pw-new2"].forEach((s) => { $(s).value = ""; });
    showLogin("密码已修改，请用新密码重新登录");
  } catch (e) {
    setMsg(msg, e.message);
  }
});

// ---------------------------------------------------------------- 启动

(async () => {
  try {
    const me = await api("GET", "/admin/api/me");
    await loadPlans();
    showApp(me);
  } catch {
    showLogin();
  }
})();
