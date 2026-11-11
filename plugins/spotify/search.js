// tvctl search spotify: links on the web player's search page, in page order
// (top result, songs, artists, albums, playlists...).
// Runs as the body of `async (limit, kind) => { ... }` (kind: the --kind page, or null);
// returns [{kind, title, url, by?}].
const pattern = /^\/(?:intl-[a-z-]+\/)?(track|album|artist|playlist|episode|show)\/([A-Za-z0-9]+)/;
const links = () =>
  [...document.querySelectorAll("main a[href]")].filter((a) => pattern.test(new URL(a.href, location.href).pathname));
const deadline = Date.now() + 15000;
let last = -1;
// Wait for results, then until the list stops growing.
while (Date.now() < deadline) {
  const n = links().length;
  if (n > 0 && n === last) break;
  last = n;
  await new Promise((r) => setTimeout(r, n > 0 ? 300 : 150));
}
const seen = new Set();
const results = [];
for (const a of links()) {
  const [, type, id] = new URL(a.href, location.href).pathname.match(pattern);
  const url = `https://open.spotify.com/${type}/${id}`;
  const title = a.textContent.trim();
  if (seen.has(url) || title === "" || (kind && type !== kind)) continue;
  seen.add(url);
  // The artist/owner links next to a track or album title.
  const row = a.closest('[role="row"], [data-encore-id="card"], [data-testid="top-result-card"]');
  const by = row
    ? [...row.querySelectorAll('a[href*="/artist/"], a[href*="/user/"]')]
        .filter((b) => b !== a)
        .map((b) => b.textContent.trim())
        .filter((s, i, all) => s && all.indexOf(s) === i)
        .join(", ")
    : "";
  results.push({ kind: type, title, url, ...(by ? { by } : {}) });
  if (results.length >= limit) break;
}
if (results.length === 0 && document.querySelector('[data-testid="login-button"]') && !document.querySelector("main"))
  throw new Error("Spotify search needs a login: tvctl auth login spotify");
return results;
