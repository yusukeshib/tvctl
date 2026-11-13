// Chrome's display-mode media query covers native window fullscreen as well as
// the Fullscreen API. Install in every frame so embedded players work too.
if (!window.__tvctlCursor) window.__tvctlCursor = (() => {
  const DELAY_MS = 10000;
  const fullscreen = matchMedia("(display-mode: fullscreen)");
  let timer = null;
  let style = null;
  let pointer = null;

  function restore() {
    if (timer !== null) clearTimeout(timer);
    timer = null;
    if (style) style.remove();
  }

  function active() {
    return fullscreen.matches || !!document.fullscreenElement;
  }

  function wake() {
    restore();
    if (!active()) return;
    timer = setTimeout(() => {
      timer = null;
      if (!active()) return;
      if (!style) {
        style = document.createElement("style");
        style.textContent = "*, *::before, *::after { cursor: none !important; }";
      }
      document.documentElement.appendChild(style);
    }, DELAY_MS);
  }

  window.addEventListener("mousemove", (e) => {
    // Ignore synthetic moves caused by controls appearing under a still mouse.
    const next = [e.screenX, e.screenY];
    if (pointer && next[0] === pointer[0] && next[1] === pointer[1]) return;
    pointer = next;
    wake();
  }, true);
  fullscreen.addEventListener("change", wake);
  document.addEventListener("fullscreenchange", wake);
  wake();

  return { wake };
})();
