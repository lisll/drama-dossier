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

  function show(status) {
    // 已在页面上就只更新提示；被移除过（isConnected 为假）则重新创建，不能因为留着旧引用就不再遮挡
    if (overlay && overlay.isConnected) {
      overlay.querySelector(".license-msg").textContent = status.message || "";
      return;
    }
    overlay = el("div", { className: "license-overlay", id: "license-overlay" });
    overlay.setAttribute("role", "dialog");
    overlay.setAttribute("aria-modal", "true");
    overlay.setAttribute("aria-labelledby", "license-title");

    const card = el("form", { className: "license-card", noValidate: true });
    card.append(
      el("h1", { id: "license-title" }, "激活剧备"),
      el("p", { className: "license-sub" }, "请输入购买获得的卡密。激活需要联网，之后离线也能使用，软件会定期自动续期。"),
    );

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
    const msg = el("p", { className: "license-msg" });
    msg.setAttribute("aria-live", "polite");
    msg.textContent = status.reason === "not_activated" ? "" : status.message || "";
    card.append(label, input, actions, msg);

    const machine = el("div", { className: "license-machine" });
    machine.append(el("div", {}, "本机机器码（换电脑或联系客服时需要）"));
    const row = el("div", { className: "license-machine-row" });
    const code = el("code", {}, status.machineCode || "读取失败");
    code.title = status.machineId || "";
    const copy = el("button", { type: "button" }, "复制完整机器码");
    copy.disabled = !status.machineId;
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

    if (!status.serverConfigured) {
      submit.disabled = true;
      setMsg(msg, "授权服务器未配置，请联系客服获取正式版本。");
    }

    card.addEventListener("submit", async (event) => {
      event.preventDefault();
      const value = input.value.trim();
      if (!value) {
        setMsg(msg, "请输入卡密");
        input.focus();
        return;
      }
      submit.disabled = true;
      submit.textContent = "激活中…";
      setMsg(msg, "");
      try {
        const result = await invoke("license_activate", { card: value });
        if (!result.active) throw new Error(result.message || "激活失败");
        setMsg(msg, `激活成功：${result.planLabel || ""}，正在进入…`, true);
        setTimeout(() => window.location.reload(), 600);
      } catch (error) {
        setMsg(msg, errorText(error));
        submit.disabled = false;
        submit.textContent = "激活";
        input.focus();
      }
    });

    overlay.append(card);
    document.body.append(overlay);
    input.focus();
  }

  function setMsg(node, text, ok = false) {
    node.textContent = text;
    node.classList.toggle("ok", ok);
  }

  async function check() {
    try {
      const status = await invoke("license_status");
      if (status.active) {
        if (overlay) window.location.reload();
        return;
      }
      show(status);
    } catch (error) {
      show({ message: errorText(error), serverConfigured: true });
    }
  }

  // 启动时立即检查；Rust 守护线程发现失效时会推送事件
  check();
  tauri.event?.listen?.("license-changed", (event) => {
    if (event?.payload && event.payload.active === false) show(event.payload);
  });
})();
