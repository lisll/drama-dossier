// 报审包生成流程的前端检查：用最小 DOM 桩跑 main.js 里的 generate()。
//
// 覆盖的点（都是会真的丢数据的地方）：
//   - 首次生成必须不带 overwrite，避免未经确认就覆盖旧包；
//   - 收到「已存在」时给出的按钮要反映设置里的默认覆盖方式；
//   - 点确认后带上 overwrite 与 overwriteMode；
//   - 确认的是「那个路径」：确认后又改了输出目录，新路径不能带 overwrite（防「确认 A 却覆盖 B」）；
//   - 结果里如实说明覆盖方式与证明材料数量。
//
// 运行：node scripts/tests/test_generate_flow.js（已接入 scripts/check.sh）

const fs = require("fs");
const path = require("path");
const assert = require("assert");

const repo = path.resolve(__dirname, "..", "..");
const src = fs.readFileSync(path.join(repo, "src", "main.js"), "utf8");

/** 从 main.js 里按函数名抽出源码（配平花括号），不必引入整个模块。
 *  保留 `async` 关键字：丢了它，函数体里的 await 会变成语法错误。 */
function grabFunction(name) {
  const asyncStart = src.indexOf(`async function ${name}(`);
  const start = asyncStart >= 0 ? asyncStart : src.indexOf(`function ${name}(`);
  if (start < 0) throw new Error(`main.js 里找不到函数 ${name}`);
  let depth = 0;
  for (let at = src.indexOf("{", start); at < src.length; at += 1) {
    if (src[at] === "{") depth += 1;
    else if (src[at] === "}") {
      depth -= 1;
      if (depth === 0) return src.slice(start, at + 1);
    }
  }
  throw new Error(`函数 ${name} 的大括号没有配平`);
}

const modeLabels = src.match(/const OVERWRITE_MODE_LABELS = \{[\s\S]*?\};/);
if (!modeLabels) throw new Error("main.js 里找不到 OVERWRITE_MODE_LABELS");
const existsMark = src.match(/const PACKAGE_EXISTS_MARK = "([^"]+)";/);
if (!existsMark) throw new Error("main.js 里找不到 PACKAGE_EXISTS_MARK");
// 前后端约定同一个前缀：后端改了而前端没跟，覆盖按钮会悄悄消失。
{
  const rust = fs.readFileSync(path.join(repo, "crates", "dossier-core", "src", "package.rs"), "utf8");
  const backend = rust.match(/pub const PACKAGE_EXISTS_MARK: &str = "([^"]+)";/);
  if (!backend || backend[1].trim() !== existsMark[1]) {
    throw new Error(`前后端 PACKAGE_EXISTS_MARK 不一致：后端 ${backend?.[1]}，前端 ${existsMark[1]}`);
  }
}
if (!src.includes('$("#generate").addEventListener("click", () => generate());')) {
  throw new Error("「生成材料包」按钮没有绑定 generate()");
}

function makeWorld() {
  const values = {
    "#output-dir": "",
    "#copy-episodes": false,
    "#ai-generated": false,
    "#ai-label-confirmed": false,
    "#with-proof": true,
    "#overwrite-default": "standardOnly",
    "#company-name": "测试文化",
    "#total-investment": "",
    "#actor-ratio": "",
    "#contact": "138",
    "#proof-source-dir": "",
  };
  const nodes = new Map();
  const world = {
    values,
    calls: [],
    toasts: [],
    lastResult: null,
    failNext: true,
  };
  global.$ = (selector) => {
    if (!nodes.has(selector)) {
      nodes.set(selector, {
        get value() {
          return values[selector] ?? "";
        },
        set value(next) {
          values[selector] = next;
        },
        get checked() {
          return values[selector];
        },
        set checked(next) {
          values[selector] = next;
        },
        className: "",
        innerHTML: "",
        classList: { add() {}, remove() {} },
      });
    }
    return nodes.get(selector);
  };
  global.esc = (value) => String(value ?? "");
  global.formatBytes = (n) => `${n}B`;
  global.reportError = () => {};
  global.toast = (message, isError, action) => world.toasts.push({ message, isError, action });
  global.beginTask = () => true;
  global.endTask = () => {};
  global.validateCostForm = () => ({ investment: null, ratio: null });
  global.effectiveFfprobe = () => null;
  global.effectiveRules = () => null;
  global.stripPath = (value) => value.replace(/\/+$/, "");
  global.renderResult = (selector, options) => {
    world.lastResult = { selector, options };
  };
  global.state = {
    root: "/素材",
    selected: 0,
    dramas: [
      { name: "剧A", dir: "/素材/剧A" },
      { name: "剧B", dir: "/素材/剧B" },
    ],
    settings: { deepOutputDir: null },
  };
  global.browse = () => {};
  global.effectivePackageDir = () => values["#output-dir"].trim() || "/素材/剧A/报审包";
  global.call = async (command, args) => {
    world.calls.push(args);
    if (command !== "generate_package") throw new Error(`意外的命令：${command}`);
    if (!args.overwrite && world.failNext) {
      world.failNext = false;
      throw new Error(
        "[PACKAGE_EXISTS] 报审包目录已存在：/素材/剧A/报审包。可以选择覆盖旧包，或换一个输出目录。",
      );
    }
    if (world.failWith) {
      const message = world.failWith;
      world.failWith = null;
      throw new Error(message);
    }
    if (world.onCall) {
      const hook = world.onCall;
      world.onCall = null;
      hook();
    }
    return {
      packageDir: "/素材/剧A/报审包",
      fileCount: 3,
      totalBytes: 10,
      overwrite: args.overwrite
        ? { replaced: 3, removed: ["06_分集视频/第02集.mp4"], kept: ["我放的说明.txt"], dropped: 0 }
        : null,
      proof: { files: [{}, {}], note: null },
    };
  };
  return world;
}

function loadApi() {
  const code = [
    modeLabels[0],
    existsMark[0],
    grabFunction("generate"),
    grabFunction("sameDrama"),
    grabFunction("stripExistsMark"),
    grabFunction("overwriteModeLabel"),
    grabFunction("overwriteSummary"),
    grabFunction("proofSummary"),
    "return { generate, overwriteModeLabel, overwriteSummary, proofSummary };",
  ].join("\n");
  return new Function(code)();
}

async function main() {
  const world = makeWorld();
  const api = loadApi();

  // 1. 首次生成：不带 overwrite，默认带上证明。
  await api.generate();
  assert.strictEqual(world.calls[0].overwrite, false, "首次生成不能带 overwrite");
  assert.strictEqual(world.calls[0].withProof, true, "默认应带上证明与过程图");
  assert.strictEqual(world.calls[0].proofSourceDir, null, "留空时交给后端按设置推算");

  // 2. 「已存在」时给的按钮要跟着默认覆盖方式走。
  const action = world.toasts.at(-1).action;
  assert.ok(action, "收到「已存在」时要给出覆盖入口");
  assert.ok(
    action.label.includes("只替换标准文件"),
    `按钮文案应反映默认方式，实际：${action.label}`,
  );

  // 3. 点确认：带 overwrite + 模式，结果里说明覆盖方式与证明数量。
  await action.run();
  const confirmed = world.calls.at(-1);
  assert.strictEqual(confirmed.overwrite, true);
  assert.strictEqual(confirmed.overwriteMode, "standardOnly");
  assert.ok(world.lastResult.options.html.includes("只替换标准文件"), "结果要说明覆盖方式");
  assert.ok(world.lastResult.options.html.includes("证明与过程图：2 个"), "结果要说明证明数量");

  // 4. 拿到确认按钮后路径被改：旧按钮不能去覆盖新路径。
  world.calls.length = 0;
  world.values["#output-dir"] = "";
  world.failNext = true;
  await api.generate(); // 目标 /素材/剧A/报审包，已存在 → 拿到确认按钮
  const staleAction = world.toasts.at(-1).action;
  assert.ok(staleAction, "应给出覆盖入口");
  world.values["#output-dir"] = "/别处/报审包"; // 用户改了目录，但按钮还是旧的
  await staleAction.run();
  assert.strictEqual(
    world.calls.at(-1).overwrite,
    false,
    "旧按钮确认的是原路径，路径改过之后不能覆盖",
  );
  // 改回原路径再点旧按钮：仍然只对原路径生效。
  world.values["#output-dir"] = "";
  await staleAction.run();
  assert.strictEqual(world.calls.at(-1).overwrite, true, "路径与确认时一致才覆盖");

  // 4b. 显示时去掉前缀：结果区与提示里都不能出现 [PACKAGE_EXISTS]。
  world.failNext = true;
  await api.generate();
  assert.ok(!world.lastResult.options.html.includes("[PACKAGE_EXISTS]"), "结果区要去掉前缀");
  assert.ok(!String(world.toasts.at(-1).message).includes("[PACKAGE_EXISTS]"), "提示要去掉前缀");
  const crossDramaAction = world.toasts.at(-1).action;

  // 4c. 系统错误文案里碰巧含「已存在」（中文 Windows 的错误 183）：不能给覆盖按钮。
  world.failNext = false;
  world.failWith = "Error: 当文件已存在时，无法创建该文件。 (os error 183)";
  world.toasts.length = 0;
  await api.generate();
  assert.ok(!world.toasts.at(-1)?.action, "系统 183 文案不能弹出覆盖按钮");
  assert.ok(world.lastResult.options.html.includes("生成失败"));

  // 4d. 确认绑定剧目：拿到剧A的覆盖按钮后切到剧B，点按钮不能覆盖。
  global.state.selected = 1;
  world.calls.length = 0;
  await crossDramaAction.run();
  assert.strictEqual(world.calls.length, 0, "切换剧目后，旧剧的覆盖确认要作废，不能发起生成");
  assert.ok(String(world.toasts.at(-1).message).includes("已切换剧目"));
  global.state.selected = 0;

  // 4e. 生成期间切换了剧目：结果不渲染到新剧下面。
  world.lastResult = null;
  world.onCall = () => {
    global.state.selected = 1;
  };
  await api.generate();
  assert.strictEqual(world.lastResult, null, "切换剧目后不能把旧剧的结果渲染到新剧");
  assert.ok(String(world.toasts.at(-1).message).includes("剧A"), "要提示是哪部剧的结果");
  global.state.selected = 0;

  // 5. 摘要文案。
  assert.ok(
    api.overwriteSummary({ overwrite: { dropped: 7, replaced: 9 } }, true).includes("7 个文件"),
    "整体替换要说明旧包里有几个文件被换掉",
  );
  assert.ok(api.overwriteSummary({ overwrite: { replaced: 3 } }, true).includes("已覆盖旧包"));
  assert.strictEqual(api.overwriteSummary(null, false), "未覆盖旧包（新建）");
  assert.strictEqual(
    api.proofSummary({ files: [], note: "没有找到深度处理输出目录" }),
    "证明与过程图：未放入。没有找到深度处理输出目录",
  );
  assert.strictEqual(api.proofSummary({ files: [{}, {}] }), "证明与过程图：2 个（07_证明与过程图/）");

  console.log("报审包生成流程检查通过");
}

main().catch((error) => {
  console.error(`报审包生成流程检查失败：${error.message}`);
  process.exit(1);
});
