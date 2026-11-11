// tvctl search youtube: results from the search page's initial data.
// Runs as the body of `async (limit, kind) => { ... }`; returns [{kind, title, url, ...}].
const deadline = Date.now() + 15000;
while (!window.ytInitialData && Date.now() < deadline) await new Promise((r) => setTimeout(r, 100));
const data = window.ytInitialData;
if (!data) throw new Error("YouTube search page did not load");
const text = (t) => t?.simpleText ?? t?.runs?.map((r) => r.text).join("") ?? null;
const results = [];
const walk = (node) => {
  if (results.length >= limit || node === null || typeof node !== "object") return;
  if (node.videoRenderer) {
    const v = node.videoRenderer;
    const live = (v.badges ?? []).some((b) => b.metadataBadgeRenderer?.style === "BADGE_STYLE_TYPE_LIVE_NOW");
    results.push({
      kind: "video",
      title: text(v.title),
      url: `https://www.youtube.com/watch?v=${v.videoId}`,
      channel: text(v.ownerText),
      duration: text(v.lengthText),
      published: text(v.publishedTimeText),
      views: text(v.shortViewCountText) ?? text(v.viewCountText),
      ...(live ? { live: true } : {}),
    });
    return;
  }
  for (const value of Array.isArray(node) ? node : Object.values(node)) walk(value);
};
walk(data.contents);
return results;
