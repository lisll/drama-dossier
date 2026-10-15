// 在 <head> 里同步执行：样式表生效前就定好主题，避免先闪一下错误的颜色。
// CSP 不允许内联脚本，所以单独成文件。依赖先加载的 logic.js。
(() => {
  let stored = null;
  try {
    stored = window.localStorage.getItem(window.DossierLogic.THEME_KEY);
  } catch {
    // 存储不可用（极少见）：按默认暗色。
  }
  document.documentElement.dataset.theme = window.DossierLogic.resolveTheme(stored);
})();
