// tvctl search primevideo: titles on the Prime Video search page of the site's store.
// Runs as the body of `async (limit, kind) => { ... }`; returns [{kind, title, url}].
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const pattern = /\/gp\/video\/detail\/([A-Z0-9]{10})/;
const links = () => [...document.querySelectorAll("a[href]")].filter((a) => pattern.test(a.getAttribute("href")));
const deadline = Date.now() + 15000;
let last = -1;
while (Date.now() < deadline) {
  const n = links().length;
  if (n > 0 && n === last) break;
  last = n;
  await sleep(n > 0 ? 400 : 150);
}
if (/\/ap\/signin/.test(location.pathname)) throw new Error("not signed in to Prime Video (tvctl auth login primevideo)");
const seen = new Set();
const results = [];
for (const a of links()) {
  const id = a.getAttribute("href").match(pattern)[1];
  const title = (a.getAttribute("aria-label") || a.textContent || a.querySelector("img")?.alt || "").trim().replace(/\s+/g, " ");
  if (seen.has(id) || !title || /watchlist/i.test(title)) continue;
  seen.add(id);
  results.push({ kind: "title", title, url: `${location.origin}/gp/video/detail/${id}` });
  if (results.length >= limit) break;
}
return results;
