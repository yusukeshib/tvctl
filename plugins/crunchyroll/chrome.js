// Crunchyroll adapter (Chrome backend). Runs on crunchyroll.com/watch/<id>
// pages and drives the web player's own player object, reached through the
// view models of its React controls (`[data-testid=player-controls-root]`).
// Times in that API are seconds. Not a public API; the E2E tests detect
// breakage. Ads (ad-supported plans) are not skipped.
window.__tvctl = (() => {
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  async function waitFor(pred, ms) {
    const end = Date.now() + ms;
    while (Date.now() < end) {
      try { if (pred()) return true; } catch (_) {}
      await sleep(150);
    }
    try { return !!pred(); } catch (_) { return false; }
  }

  // The player fills the window; the rest of the watch page (header, episode
  // info, comments) is hidden behind it.
  function fillWindow() {
    if (!document.documentElement) {
      // Evaluated before the document has elements: style it once it has.
      document.addEventListener("DOMContentLoaded", fillWindow, { once: true });
      return;
    }
    if (document.getElementById("tvctl-crunchyroll-style")) return;
    const s = document.createElement("style");
    s.id = "tvctl-crunchyroll-style";
    s.textContent = `
      html, body { overflow: hidden !important; }
      .video-player-wrapper { position: fixed !important; inset: 0 !important; z-index: 2147483000 !important;
        width: 100vw !important; height: 100vh !important; max-width: none !important; margin: 0 !important;
        padding: 0 !important; background: #000 !important; }
      .video-player-wrapper .player-container, .video-player-wrapper .bitmovinplayer-container {
        width: 100% !important; height: 100% !important; max-width: none !important; max-height: none !important;
        padding: 0 !important; aspect-ratio: auto !important; }
      .video-player-wrapper video { width: 100% !important; height: 100% !important; object-fit: contain !important; }
    `;
    (document.head || document.documentElement).appendChild(s);
  }
  fillWindow();

  // `/watch/<id>[/<slug>]` or `/series/<id>[/<slug>]`, optionally after a
  // language prefix (`/ja/watch/...`).
  function parsePath(pathname) {
    const parts = pathname.split("/").filter(Boolean);
    const i = parts.findIndex((p) => p === "watch" || p === "series");
    const id = i >= 0 && i <= 1 ? parts[i + 1] : null;
    return id && /^[A-Z0-9]+$/.test(id) ? { kind: parts[i], id } : null;
  }

  function parseUrl(raw) {
    const p = parsePath(new URL(raw).pathname);
    if (!p) {
      throw new Error(`not a Crunchyroll episode or series URL: ${raw} (expected https://www.crunchyroll.com/watch/<id> or /series/<id>)`);
    }
    return p;
  }

  // Current value of one of the player's observables (they emit it on subscribe).
  function current(obs) {
    let v;
    obs.subscribe((x) => { v = x; }).unsubscribe();
    return v;
  }

  function viewModels() {
    const root = document.querySelector("[data-testid='player-controls-root']");
    if (!root) return null;
    const key = Object.keys(root).find((k) => k.startsWith("__reactFiber"));
    for (let f = key && root[key], i = 0; f && i < 40; f = f.return, i++) {
      const vm = f.memoizedProps && f.memoizedProps.viewModels;
      if (vm && vm.trackSelectionVM) return vm;
    }
    return null;
  }

  // The player and its loaded episode, or null.
  function session() {
    const vm = viewModels();
    const player = vm && vm.trackSelectionVM._player;
    const orch = player && player.playerOrchestrator;
    const engine = orch && orch._mediaEngine;
    const model = engine && engine._activeVideoModel;
    return model ? { vm, player, orch, engine, model } : null;
  }

  const playerState = (s) => s.orch._playerStateMachine.getCurrentState();
  const isLoaded = (s) => !!s && ["playing", "paused", "rebuffering", "stopped"].includes(playerState(s));

  function requirePlayer() {
    const s = session();
    if (!isLoaded(s)) throw new Error("player not ready (is an episode playing?)");
    return s;
  }

  function stateOf(s) {
    const st = playerState(s);
    const v = document.querySelector("video");
    if (st === "stopped" || (v && v.ended)) return "ended";
    if (st === "paused") return "paused";
    if (st === "playing" && !current(s.vm.bufferingVM.isRebuffering$)) return "playing";
    return "buffering";
  }

  const streamTime = (s) => s.engine.getCurrentPlayheadPosition();
  function contentTime(s) {
    const t = streamTime(s);
    try {
      const c = s.orch._playerTime.streamTimeToContentTime(t);
      if (typeof c === "number" && !Number.isNaN(c)) return c;
    } catch (_) {}
    return t;
  }

  // Ads are stitched into the stream next to the content (element type 0).
  function adShowing(s) {
    try {
      const pt = s.orch._playerTime;
      if (!(pt.stitchedElements || []).some((e) => e.type !== 0)) return false;
      return !pt.isTimelineTimeInContent(pt.streamTimeToTimelineTime(streamTime(s)));
    } catch (_) {
      return false;
    }
  }

  const episodeNumber = (x) => (x == null || x === "" ? null : /^\d+(\.\d+)?$/.test(String(x)) ? Number(x) : String(x));

  // Title and number of an episode from a link card on the page ("E2 - Title").
  function cardInfo(id) {
    for (const a of document.querySelectorAll(`a[href*="/watch/${id}"]`)) {
      const m = /^E(\S+)\s+-\s+(.+)$/m.exec((a.innerText || "").trim());
      if (m) return { title: m[2].trim(), number: episodeNumber(m[1]) };
    }
    return { title: null, number: null };
  }

  function describe(s) {
    const md = s.model.assetMetadata || {};
    const out = { video_id: s.model.id, title: md.seriesTitle || md.title || null, type: md.contentType || null };
    if (md.contentType === "episode") {
      out.episode = { id: s.model.id, title: md.title || null, number: episodeNumber(md.episodeDisplayString) };
      const next = s.vm.nextEpisodeVM._nextEpisodeGuid;
      out.next_episode = next ? { id: next, ...cardInfo(next) } : null;
    }
    return out;
  }

  const textInfo = (t) => (t && t.language !== "none" ? { language: t.language, name: t.displayName || null } : null);
  const audioInfo = (t) => (t ? { id: t.guid || null, language: t.language, name: t.displayName || null, type: t.role || null } : null);
  const textTracks = (s) => current(s.vm.trackSelectionVM.availableTextTracks$) || [];
  const audioTracks = (s) => (current(s.vm.trackSelectionVM.availableAudioTracks$) || []).filter((t) => t.canUse !== false);
  const qualities = (s) => [...s.orch.playbackQualityOrchestrator.bucketToQualityMap.keys()].map((b) => b.toLowerCase());

  // Cloudflare's bot check ("Just a moment...") instead of the page.
  const challenged = () => /^just a moment/i.test(document.title) || !!document.querySelector("#challenge-form, .cf-turnstile, script[src*='challenges.cloudflare.com']");
  const CHALLENGE =
    "Crunchyroll's Cloudflare check (\"Verify you are human\") blocks tvctl's Chrome: run `tvctl auth login crunchyroll`, open any Crunchyroll episode in that window, pass the check, quit that Chrome (Cmd+Q) and try again";
  const notFound = () => /page not found/i.test(document.title);
  const needsPremium = () => /^\/(payments|premium)(\/|$)/.test(location.pathname);
  // Logged out, the header has sign-up / log-in links to sso.crunchyroll.com.
  const signedOut = () => !!document.querySelector("a[href*='sso.crunchyroll.com/authorize']");

  // Why there is no playback (a Crunchyroll error or a blocked stream), or null.
  function playerError(s) {
    if (notFound()) return "page not found (check the id in the URL)";
    if (needsPremium()) return "this title needs Crunchyroll Premium";
    if (document.querySelector(".erc-blocked-stream")) {
      return signedOut()
        ? "not signed in to Crunchyroll in tvctl's Chrome profile (run `tvctl auth login crunchyroll`)"
        : "this title is not available to this account (it may need Premium or not be offered in your country)";
    }
    if (!s) return null;
    try {
      if (!current(s.vm.errorOverlayVM.isVisible$)) return null;
      const d = current(s.vm.errorOverlayVM.errorDetails$) || {};
      const msg = [d.title, d.message, d.description].filter((x) => typeof x === "string" && x).join(": ");
      const code = d.code || d.errorCode;
      return (msg || "playback error") + (code ? ` (${code})` : "");
    } catch (_) {
      return null;
    }
  }

  function status() {
    const s = session();
    if (!isLoaded(s)) {
      const err = playerError(s);
      return { loaded: false, url: location.href, title: document.title, ...(err ? { error: err } : {}) };
    }
    const out = {
      loaded: true,
      url: location.href,
      ...describe(s),
      state: stateOf(s),
      current_time: contentTime(s),
      duration: (s.model.assetMetadata || {}).contentDuration || current(s.vm.timelineScrubberVM.duration$) || null,
      volume: Math.round((current(s.engine.volume$) ?? 1) * 100),
      muted: !!current(s.vm.volumeVM.isMuted$),
      playback_rate: current(s.vm.playbackSpeedMenuVM.selectedRate$),
      subtitles: textInfo(current(s.vm.trackSelectionVM.activeTextTrack$)),
      audio: audioInfo(current(s.vm.trackSelectionVM.activeAudioTrack$)),
      quality: (s.orch.playbackQualityOrchestrator.currentBucket || "").toLowerCase() || null,
      available_qualities: qualities(s),
      ad_showing: adShowing(s),
    };
    const err = playerError(s);
    if (err) out.error = err;
    if (pending) out.pending = pending;
    if (lastError) { out.warning = lastError; lastError = null; }
    return out;
  }

  // Settings requested while an ad plays, applied when the content resumes.
  let pending = null;
  let pendingTimer = null;
  let lastError = null;
  function defer(what) {
    pending = { ...(pending || {}), ...what };
    if (pendingTimer) return;
    pendingTimer = setInterval(async () => {
      const s = session();
      if (!isLoaded(s) || adShowing(s) || !pending) return;
      const p = pending;
      pending = null;
      clearInterval(pendingTimer);
      pendingTimer = null;
      try {
        if (p.subtitles) await subtitles(p.subtitles);
        if (p.seconds != null) await seekToStart(p.seconds);
      } catch (e) {
        lastError = String(e.message || e);
      }
    }, 500);
  }

  function refuseDuringAd(s, what) {
    if (adShowing(s)) throw new Error(`${what} is not available while an ad plays; try again after it`);
  }

  // The link of the series page's Start/Continue watching button.
  function playButtonHref() {
    const a = document.querySelector(".top-actions-wrapper a[href*='/watch/']");
    return a ? new URL(a.getAttribute("href"), location.href).href : null;
  }

  async function open(a) {
    parseUrl(a.url);
    const here = parsePath(location.pathname);
    if (!here || here.kind !== "watch") {
      // A series page plays like its Start/Continue button.
      let href = null;
      await waitFor(() => notFound() || needsPremium() || (href = playButtonHref()), 15000);
      const err = playerError(null);
      if (err) throw new Error("Crunchyroll: " + err);
      if (challenged()) throw new Error(CHALLENGE);
      if (here && here.kind === "series" && href) return { navigate: href };
      if (signedOut()) {
        throw new Error("not signed in to Crunchyroll in tvctl's Chrome profile (run `tvctl auth login crunchyroll`)");
      }
      throw new Error(`Crunchyroll showed ${location.href} instead of a player`);
    }
    const failed = () => playerError(session());
    const ready = await waitFor(() => failed() || isLoaded(session()), 25000);
    if (failed()) throw new Error("Crunchyroll: " + failed());
    if (!ready && challenged()) throw new Error(CHALLENGE);
    if (!ready) {
      // E.g. the site stuck on its loading spinner, which happens when
      // Cloudflare throttles this browser's requests.
      throw new Error("Crunchyroll player did not load (are you logged in? `tvctl auth login crunchyroll`; if the page keeps showing a spinner, Crunchyroll may be throttling this browser: open an episode in `tvctl auth login crunchyroll`, quit that Chrome and retry later)");
    }
    let s = session();
    // Wait for real playback (time moving), so later commands are not
    // overridden by the player finishing its start-up.
    const t0 = streamTime(s);
    if (playerState(s) === "paused") s.player.play();
    await waitFor(() => failed() || adShowing(session()) || streamTime(session()) > t0 + 0.3, 10000);
    if (failed()) throw new Error("Crunchyroll: " + failed());
    s = session();
    let warning = null;
    if (adShowing(s)) {
      // Applied when the ad ends.
      const later = {};
      if (a.start != null) later.seconds = a.start;
      if (a.subtitles) later.subtitles = a.subtitles;
      if (Object.keys(later).length) defer(later);
    } else {
      // Subtitles first: switching them reloads the stream.
      if (a.subtitles) {
        try { await subtitles(a.subtitles); } catch (e) { warning = "subtitles: " + (e.message || e); }
      }
      if (a.start != null) await seekToStart(a.start);
    }
    if (a.paused) {
      session().player.pause();
      await waitFor(() => stateOf(session()) === "paused", 5000);
    }
    const st = status();
    return warning ? { ...st, warning } : st;
  }

  // The player may still jump to the account's resume point right after
  // playback starts, undoing an early seek; re-apply the start until it holds.
  async function seekToStart(seconds) {
    for (let i = 0; i < 3; i++) {
      await seekTo(session(), seconds);
      await sleep(1000);
      if (Math.abs(contentTime(session()) - seconds) < 10) return;
    }
  }

  async function seekTo(s, seconds) {
    const dur = (s.model.assetMetadata || {}).contentDuration;
    let t = Math.max(0, seconds);
    if (dur) t = Math.min(t, dur - 1);
    s.orch.seekToContentTime(t);
    await waitFor(() => Math.abs(contentTime(session()) - t) < 3, 8000);
    return t;
  }

  function matchTrack(tracks, want, rank) {
    const w = want.toLowerCase();
    const sorted = [...tracks].sort((x, y) => rank(x) - rank(y));
    const lang = (t) => String(t.language || "").toLowerCase();
    return (
      sorted.find((t) => String(t.guid || "").toLowerCase() === w) ||
      sorted.find((t) => lang(t) === w) ||
      (!w.includes("-") && sorted.find((t) => lang(t).split("-")[0] === w)) ||
      sorted.find((t) => String(t.displayName || "").toLowerCase() === w)
    );
  }

  const noRank = () => 0;
  // Dubs and the original before audio description.
  const audioRank = (t) => (t.role === "description" ? 1 : 0);

  async function subtitles(lang) {
    const s = requirePlayer();
    refuseDuringAd(s, "subtitles");
    const all = textTracks(s);
    const tracks = all.filter((t) => t.language !== "none");
    if (lang === "list") return { ...status(), tracks: tracks.map(textInfo) };
    let target;
    if (lang === "off") {
      target = all.find((t) => t.language === "none");
      if (!target) throw new Error("no way to turn subtitles off");
    } else if (lang === "on") {
      const ui = (s.model.initialProfile || {}).uiLanguage || navigator.language || "en-US";
      target = matchTrack(tracks, ui, noRank) || matchTrack(tracks, ui.split("-")[0], noRank) ||
        matchTrack(tracks, "en", noRank) || tracks[0];
      if (!target) throw new Error("this title has no subtitles");
    } else {
      target = matchTrack(tracks, lang, noRank);
      if (!target) {
        throw new Error(`no subtitles for "${lang}" (available: ${tracks.map((t) => t.language).join(", ")})`);
      }
    }
    const active = () => (current(session().vm.trackSelectionVM.activeTextTrack$) || {}).language;
    if (active() !== target.language) {
      // The track object must be the player's own (it checks identity).
      s.player.setTextTrack(target);
      if (!(await waitFor(() => active() === target.language, 10000))) {
        throw new Error(`could not switch subtitles to ${target.displayName}`);
      }
      // Subtitles are burned in: the player reloads the stream.
      await waitFor(() => stateOf(session()) !== "buffering", 8000);
    }
    return status();
  }

  async function audio(lang) {
    const s = requirePlayer();
    refuseDuringAd(s, "audio");
    const tracks = audioTracks(s);
    if (lang === "list") return { ...status(), audio_tracks: tracks.map(audioInfo) };
    if (tracks.length <= 1) throw new Error("this title has only one audio track");
    const target = matchTrack(tracks, lang, audioRank);
    if (!target) {
      throw new Error(`no audio track for "${lang}" (available: ${[...new Set(tracks.map((t) => t.language))].join(", ")})`);
    }
    if (s.model.id === target.guid) return status();
    // Each dub is its own episode; the player loads it in place.
    s.player.setAudioTrack(target);
    const switched = await waitFor(() => {
      const n = session();
      return isLoaded(n) && n.model.id === target.guid && stateOf(n) !== "buffering";
    }, 15000);
    if (!switched) throw new Error(`could not switch audio to ${target.displayName}`);
    return status();
  }

  async function next() {
    const s = requirePlayer();
    const to = s.vm.nextEpisodeVM._nextEpisodeGuid;
    if (!to) throw new Error("there is no next episode");
    const from = s.model.id;
    s.vm.nextEpisodeVM.loadNextEpisode();
    const moved = await waitFor(() => {
      const n = session();
      return isLoaded(n) && n.model.id !== from;
    }, 15000);
    if (!moved) throw new Error("the next episode did not start");
    await waitFor(() => stateOf(session()) === "playing", 5000);
    return status();
  }

  async function settle(want) {
    if (want) await waitFor(() => stateOf(session()) === want, 5000);
    else await sleep(300);
    return status();
  }

  async function run(a) {
    fillWindow();
    if (a.action === "open") return open(a);
    if (a.action === "status") return status();
    const s = requirePlayer();
    const p = s.player;
    switch (a.action) {
      case "play": p.play(); return settle("playing");
      case "pause": p.pause(); return settle("paused");
      case "toggle":
        if (stateOf(s) === "paused" || stateOf(s) === "ended") { p.play(); return settle("playing"); }
        p.pause();
        return settle("paused");
      case "next": return next();
      case "seek": {
        refuseDuringAd(s, "seeking");
        const t = await seekTo(s, a.relative ? contentTime(s) + a.seconds : a.seconds);
        return { ...status(), current_time: t };
      }
      case "subtitles": return subtitles(a.lang);
      case "audio": return audio(a.lang);
      case "volume":
        if (a.level != null) p.setVolume(Math.max(0, Math.min(100, a.level)));
        if (a.muted != null) p.setMute(a.muted);
        if (a.level != null) {
          await waitFor(() => Math.round(current(session().engine.volume$) * 100) === Math.max(0, Math.min(100, a.level)), 3000);
        }
        if (a.muted != null) await waitFor(() => !!current(session().vm.volumeVM.isMuted$) === a.muted, 3000);
        return status();
      case "speed": {
        refuseDuringAd(s, "speed");
        const rates = current(s.vm.playbackSpeedMenuVM.availableRates$) || [];
        if (!rates.includes(a.rate)) throw new Error(`speed ${a.rate} not available (available: ${rates.join(", ")})`);
        s.vm.playbackSpeedMenuVM.selectPlaybackSpeed(a.rate);
        // While paused the player applies the rate when playback resumes.
        if (!(await waitFor(() => current(session().vm.playbackSpeedMenuVM.selectedRate$) === a.rate, 3000))) {
          throw new Error(`could not set speed ${a.rate}`);
        }
        return status();
      }
      case "quality": {
        const levels = qualities(s);
        const want = String(a.level || "").toLowerCase();
        if (!levels.includes(want)) throw new Error(`quality "${a.level}" not available (available: ${levels.join(", ")})`);
        s.vm.settingsVM.setPlaybackQualityBucket(want.toUpperCase());
        await waitFor(() => s.orch.playbackQualityOrchestrator.currentBucket === want.toUpperCase(), 3000);
        return status();
      }
      default:
        throw new Error(`${a.action} is not supported by the crunchyroll plugin`);
    }
  }

  return { run };
})();
