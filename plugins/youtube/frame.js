// Injected into every frame; only acts inside the YouTube embed iframe.
// Gives the parent page access to internal player functions the IFrame API
// no longer exposes (e.g. quality selection).
(() => {
  if (!/(^|\.)youtube(-nocookie)?\.com$/.test(location.hostname) || !location.pathname.startsWith("/embed/")) return;
  window.addEventListener("message", (e) => {
    const d = e.data;
    if (!d || typeof d !== "object" || !d.__tvctl) return;
    let result = null;
    let error = null;
    try {
      const mp = document.getElementById("movie_player");
      if (!mp) throw new Error("player element not found");
      switch (d.op) {
        case "quality":
          if (d.level === "auto") mp.setPlaybackQualityRange("auto", "auto");
          else mp.setPlaybackQualityRange(d.level, d.level);
          break;
        case "info":
          break;
        default:
          throw new Error("unknown op " + d.op);
      }
      result = {
        quality: mp.getPlaybackQuality(),
        available: mp.getAvailableQualityLevels(),
      };
    } catch (err) {
      error = String((err && err.message) || err);
    }
    e.source.postMessage({ __tvctlReply: d.id, result, error }, "*");
  });
})();
