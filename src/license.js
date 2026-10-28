// 卡密激活层。
//
// 未授权时在 <body> 末尾插入全屏激活层，盖住整个界面。这只是体验层：
// 真正的拦截在 Rust 端（总闸门 + 功能内验权 + 守护线程），删掉这一层什么也得不到。
// 续签由 Rust 守护线程负责，这里只监听它发出的 license-changed 事件。
// 不依赖、也不修改 main.js；激活成功后整页重载，让 main.js 在已授权状态下完整初始化。
(() => {
  "use strict";

  const tauri = window.__TAURI__;
  const invoke = tauri?.core?.invoke;
  if (!invoke) return; // 浏览器静态预览：与 main.js 的 boot 一致，什么都不做

  let overlay = null;

  function el(tag, props = {}, text) {
    const node = document.createElement(tag);
    Object.assign(node, props);
    if (text !== undefined) node.textContent = text;
    return node;
  }

  function errorText(error) {
    const text = String(error && error.message ? error.message : error);
    return text.replace(/^LICENSE_LOCKED:/, "");
  }

  // 这些原因说明本机有授权文件，联网续期就可能恢复
  const RETRYABLE = new Set(["expired", "unreadable", "unpersisted", "clock_rollback"]);

  function show(status) {
    // 已在页面上就只更新提示；被移除过（isConnected 为假）则重新创建，不能因为留着旧引用就不再遮挡
    if (!overlay || !overlay.isConnected) build();
    render(status);
  }

  // 按最新状态更新激活层：提示文字、按钮是否可用、是否显示「联网重试」
  function render(status) {
    const { msg, submit, retry, code, copy, badge, sub, input } = overlay.parts;
    setMsg(msg, status.reason === "not_activated" ? "" : status.message || "");
    code.textContent = status.machineCode || "读取失败";
    code.title = status.machineId || "";
    copy.disabled = !status.machineId;
    // 老版本后端没有 canActivate 字段时，退回只看 serverConfigured
    const canActivate = status.canActivate ?? status.serverConfigured !== false;
    submit.disabled = !canActivate;
    retry.hidden = !(status.serverConfigured !== false && RETRYABLE.has(status.reason));
    // TEST-MODE：测试版明确标出来，避免和正式版混淆
    badge.hidden = !status.testMode;
    if (status.testMode) {
      sub.textContent =
        "这是内部测试版，请输入发放人提供的内部测试卡密。激活后离线可用 30 天，到期后再输入一次即可续用。";
      input.placeholder = "DDK-XXXX-XXXX-XXXX-XXXX";
    }
    if (!canActivate) setMsg(msg, "授权服务器未配置，请联系客服获取正式版本。");
  }

  function build() {
    overlay = el("div", { className: "license-overlay", id: "license-overlay" });
    overlay.setAttribute("role", "dialog");
    overlay.setAttribute("aria-modal", "true");
    overlay.setAttribute("aria-labelledby", "license-title");

    const card = el("form", { className: "license-card", noValidate: true });
    const title = el("h1", { id: "license-title" }, "激活剧备");
    const badge = el("span", { className: "license-badge" }, "测试版授权");
    badge.hidden = true;
    title.append(badge);
    const sub = el(
      "p",
      { className: "license-sub" },
      "请输入购买获得的卡密。激活需要联网，之后离线也能使用，软件会定期自动续期。",
    );
    card.append(title, sub);

    const label = el("label", { htmlFor: "license-card-input" }, "卡密");
    const input = el("input", {
      id: "license-card-input",
      autocomplete: "off",
      spellcheck: false,
      maxLength: 40,
      placeholder: "DDK-XXXX-XXXX-XXXX-XXXX",
    });
    const submit = el("button", { type: "submit", className: "license-primary" }, "激活");
    const actions = el("div", { className: "license-actions" });
    actions.append(submit);
    // 已有授权文件、只是过期或暂时读不到时，可以直接联网续期，不必重新输卡密
    const retry = el("button", { type: "button", className: "license-retry" }, "联网重试");
    retry.hidden = true;
    actions.append(retry);
    const msg = el("p", { className: "license-msg" });
    msg.setAttribute("aria-live", "polite");
    card.append(label, input, actions, msg);

    const machine = el("div", { className: "license-machine" });
    machine.append(el("div", {}, "本机机器码（换电脑或联系客服时需要）"));
    const row = el("div", { className: "license-machine-row" });
    const code = el("code", {}, "读取中…");
    const copy = el("button", { type: "button" }, "复制完整机器码");
    copy.disabled = true;
    copy.addEventListener("click", async () => {
      try {
        const full = await invoke("license_machine_code");
        await navigator.clipboard.writeText(full);
        setMsg(msg, "完整机器码已复制", true);
      } catch (error) {
        setMsg(msg, `复制失败：${errorText(error)}`);
      }
    });
    row.append(code, copy);
    machine.append(row);
    card.append(machine);

    card.addEventListener("submit", async (event) => {
      event.preventDefault();
      const value = input.value.trim();
      if (!value) {
        setMsg(msg, "请输入卡密");
        input.focus();
        return;
      }
      await busy(submit, "激活中…", async () => {
        const result = await invoke("license_activate", { card: value });
        if (!result.active) throw new Error(result.message || "激活失败");
        enter(`激活成功：${result.planLabel || ""}，正在进入…`);
      });
    });

    retry.addEventListener("click", () =>
      busy(retry, "正在联网…", async () => {
        const result = await invoke("license_refresh");
        if (!result.active) throw new Error(result.message || "续期失败");
        enter("续期成功，正在进入…");
      }),
    );

    overlay.parts = { msg, submit, retry, code, copy, input, badge, sub };
    overlay.append(card);
    document.body.append(overlay);
    input.focus();
  }

  // 按钮执行期间禁用并改文字；失败时把错误显示出来并恢复按钮
  async function busy(button, label, action) {
    const { msg, input } = overlay.parts;
    const original = button.textContent;
    button.disabled = true;
    button.textContent = label;
    setMsg(msg, "");
    try {
      await action();
    } catch (error) {
      setMsg(msg, errorText(error));
      button.disabled = false;
      button.textContent = original;
      input.focus();
    }
  }

  // 授权已有效：整页重载，让 main.js 在已授权状态下完整初始化
  let entering = false;
  function enter(text) {
    if (entering) return;
    entering = true;
    if (overlay?.isConnected) setMsg(overlay.parts.msg, text, true);
    setTimeout(() => window.location.reload(), 600);
  }

  function setMsg(node, text, ok = false) {
    node.textContent = text;
    node.classList.toggle("ok", ok);
  }

  // 激活层在页面上 = 页面是在「未授权」状态下加载的，恢复有效后必须重载
  function onStatus(status) {
    if (status.active) {
      if (overlay) enter("授权已恢复，正在进入…");
      return;
    }
    show(status);
  }

  async function check() {
    try {
      onStatus(await invoke("license_status"));
    } catch (error) {
      show({ message: errorText(error), serverConfigured: true });
    }
  }

  // 启动时立即检查；Rust 端在「有效 ↔ 锁定」变化时推送完整状态（含后台续签成功）
  check();
  tauri.event?.listen?.("license-changed", (event) => {
    if (event?.payload) onStatus(event.payload);
  });
})();
