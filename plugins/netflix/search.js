// tvctl search netflix: titles on the search page.
// Runs as the body of `async (limit, kind) => { ... }`; returns [{kind, title, url}].
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const searchUrl = location.href;
const deadline = Date.now() + 15000;
const cards = () => [...document.querySelectorAll('a[href*="/watch/"], a[href*="jbv="], a[href*="/title/"]')];
const profileSelector = '[data-uia^="profile-selector+tile"], [data-uia="profile-link"]';
const gate = () => document.querySelector(`[data-uia="profile-gate-screen"], ${profileSelector}`);
while (Date.now() < deadline && cards().length === 0 && !gate()) await sleep(150);
if (cards().length === 0 && gate()) {
  // "Who's watching?": pick the first profile that is not a Kids profile, then search again.
  await sleep(500);
  const profiles = [...document.querySelectorAll(profileSelector)];
  const pick = profiles.find((p) => !/kids|キッズ/i.test(p.textContent)) ?? profiles[0];
  if (!pick) throw new Error("Netflix asks to pick a profile");
  pick.click();
  await sleep(2500);
  return { navigate: searchUrl };
}
if (/login/.test(location.pathname)) throw new Error("not signed in to Netflix (tvctl auth login netflix)");
// Results stream in; wait until the list stops growing.
let last = -1;
while (Date.now() < deadline) {
  const n = cards().length;
  if (n > 0 && n === last) break;
  last = n;
  await sleep(400);
}
const seen = new Set();
const results = [];
for (const a of cards()) {
  const href = new URL(a.href, location.href);
  const id = href.searchParams.get("jbv") ?? href.pathname.match(/\/(?:watch|title)\/(\d+)/)?.[1];
  if (!id || seen.has(id)) continue;
  seen.add(id);
  const title = (a.getAttribute("aria-label") || a.querySelector("img")?.alt || a.textContent || "").trim();
  if (!title) continue;
  results.push({ kind: "title", title, url: `https://www.netflix.com/title/${id}` });
  if (results.length >= limit) break;
}
return results;
