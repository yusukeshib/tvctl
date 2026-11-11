// tvctl search crunchyroll: series and movies on the search page.
// Runs as the body of `async (limit, kind) => { ... }`; returns [{kind, title, url}].
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const t0 = Date.now();
// Cloudflare's automatic check passes within seconds; past that it wants a human (same as `open`).
while (/Just a moment/.test(document.title) && Date.now() - t0 < 12000) await sleep(300);
if (/Just a moment/.test(document.title))
  throw new Error(
    "Crunchyroll's Cloudflare check (\"Verify you are human\") blocks tvctl's Chrome: run `tvctl auth login crunchyroll`, open any Crunchyroll episode in that window, pass the check, quit that Chrome (Cmd+Q) and try again",
  );
const pattern = /^\/(?:[a-z]{2}(?:-[a-z]{2})?\/)?(series|watch|watch\/musicvideo|concert)\/([A-Z0-9]+)/i;
const links = () => [...document.querySelectorAll("a[href]")].filter((a) => pattern.test(new URL(a.href, location.href).pathname));
const deadline = Date.now() + 15000;
let last = -1;
while (Date.now() < deadline) {
  const n = links().length;
  if (n > 0 && n === last) break;
  last = n;
  await sleep(n > 0 ? 400 : 150);
}
const seen = new Set();
const results = [];
for (const a of links()) {
  const [, type, id] = new URL(a.href, location.href).pathname.match(pattern);
  const kind = type === "series" ? "series" : "video";
  const url = `https://www.crunchyroll.com/${type}/${id}`;
  const title = (a.getAttribute("title") || a.getAttribute("aria-label") || a.textContent || "").trim().replace(/\s+/g, " ");
  if (seen.has(url) || !title) continue;
  seen.add(url);
  results.push({ kind, title, url });
  if (results.length >= limit) break;
}
return results;
