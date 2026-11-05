// Blackout: covers a player page with black once playback has been paused or
// ended for a while. Moving the mouse over the window, clicking, scrolling or
// typing removes it (the input is not passed to the page), as does playback
// resuming or a tvctl command (`wake`); the wait then starts again.
// Installed in the main frame of every player page; reads the state from the
// plugin adapter (`window.__tvctl`). `__TVCTL_BLACKOUT_MS__` is replaced by the
// server.
if (window.top === window && !window.__tvctlBlackout) window.__tvctlBlackout = (() => {
  const DELAY_MS = __TVCTL_BLACKOUT_MS__;
  const POLL_MS = 1000;
  // Pointer movement smaller than this (in CSS pixels) is ignored: browsers send
  // synthetic mouse moves when the element under a resting pointer changes.
  const MOVE_PX = 4;
  const IDLE_STATES = ["paused", "ended"];

  let el = null;
  let shown = false;
  let idleSince = null;
  let pointer = null;
  let polling = false;

  // Tells a WebView host (window.ipc) about changes; Chrome hosts read `shown`.
  const notify = () => {
    try {
      if (window.ipc && window.ipc.postMessage) {
        window.ipc.postMessage(JSON.stringify({ type: "blackout", shown }));
      }
    } catch (_) {}
  };

  function overlay() {
    if (el) return el;
    el = document.createElement("div");
    el.id = "__tvctl_blackout";
    // A popover sits in the top layer, above fullscreen elements of the page.
    if ("popover" in el) el.popover = "manual";
    el.style.cssText = [
      "position:fixed", "inset:0", "width:100vw", "height:100vh",
      "max-width:none", "max-height:none", "margin:0", "padding:0", "border:0",
      "background:#000", "cursor:none", "z-index:2147483647", "display:none",
    ].map((d) => d + " !important").join(";");
    const dismiss = (e) => {
      e.preventDefault();
      e.stopPropagation();
      wake();
    };
    el.addEventListener("mousemove", (e) => {
      e.stopPropagation();
      if (!pointer) { pointer = [e.screenX, e.screenY]; return; }
      if (Math.hypot(e.screenX - pointer[0], e.screenY - pointer[1]) >= MOVE_PX) wake();
    });
    for (const type of ["mousedown", "mouseup", "click", "dblclick", "contextmenu", "wheel", "touchstart", "pointerdown"]) {
      el.addEventListener(type, dismiss, { passive: false });
    }
    return el;
  }

  function show() {
    const o = overlay();
    // Inside the fullscreen element if there is one (older engines without
    // popovers draw it there); otherwise at the end of the document.
    const parent = document.fullscreenElement && document.fullscreenElement.tagName !== "VIDEO"
      ? document.fullscreenElement : document.documentElement;
    if (o.parentNode !== parent) parent.appendChild(o);
    o.style.setProperty("display", "block", "important");
    if (o.showPopover) {
      try { if (o.matches(":popover-open")) o.hidePopover(); o.showPopover(); } catch (_) {}
    }
    pointer = null;
    if (!shown) { shown = true; notify(); }
  }

  function hide() {
    if (el) {
      if (el.hidePopover) { try { el.hidePopover(); } catch (_) {} }
      el.style.setProperty("display", "none", "important");
    }
    if (shown) { shown = false; notify(); }
  }

  // Activity: remove the blackout and restart the wait.
  function wake() {
    hide();
    if (idleSince !== null) idleSince = Date.now();
  }

  async function tick() {
    if (polling || !(window.__tvctl && window.__tvctl.run)) return;
    polling = true;
    let idle = false;
    try {
      const s = await window.__tvctl.run({ action: "status" });
      idle = !!s && IDLE_STATES.includes(s.state);
    } catch (_) {
      idle = false;
    } finally {
      polling = false;
    }
    if (!idle) {
      idleSince = null;
      hide();
      return;
    }
    if (idleSince === null) idleSince = Date.now();
    if (!shown && Date.now() - idleSince >= DELAY_MS) show();
  }

  window.addEventListener("keydown", (e) => {
    if (!shown) return;
    e.preventDefault();
    e.stopPropagation();
    wake();
  }, true);
  // Re-stack above an element that entered fullscreen while black.
  document.addEventListener("fullscreenchange", () => { if (shown) show(); });
  if (DELAY_MS > 0) setInterval(tick, POLL_MS);

  return {
    get shown() { return shown; },
    wake,
  };
})();
