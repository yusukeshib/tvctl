// Amazon Prime Video adapter (Chrome backend). Runs on amazon.com detail pages
// (/gp/video/detail/<id>), starts the title with the page's play button and then
// drives the web player through its internal stores (playback, captions, audio,
// bandwidth capper, ...), which the player keeps in a React context. These are
// not public APIs; the E2E tests detect breakage.
//
// Titles with ads have a timeline of items: main-content items (contentId ending
// in `_main`, positions in title time) and ad items (their own time base). Ads
// are not skipped; while one plays, actions that need the main content are
// refused or deferred until it ends.
window.__tvctl = (() => {
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const $ = (sel, root = document) => root.querySelector(sel);

  async function waitFor(pred, ms) {
    const end = Date.now() + ms;
    while (Date.now() < end) {
      try { if (pred()) return true; } catch (_) {}
      await sleep(150);
    }
    try { return !!pred(); } catch (_) { return false; }
  }

  function parseUrl(raw) {
    const u = new URL(raw);
    const parts = u.pathname.split("/").filter(Boolean);
    const after = (name) => {
      const i = parts.indexOf(name);
      return i >= 0 ? parts[i + 1] : null;
    };
    const id = after("detail") || after("dp") || after("product");
    if (!id || !/^[A-Za-z0-9._-]+$/.test(id)) {
      throw new Error(`not a Prime Video title URL: ${raw} (expected https://www.amazon.com/gp/video/detail/<id>)`);
    }
    return { id };
  }

  // primevideo.com links are opened on amazon.com, where the US account is signed in.
  const isDetailPage = () =>
    /(^|\.)amazon\.com$/.test(location.hostname) && location.pathname.startsWith("/gp/video/detail/");

  // The player's store context, read from the React props of its container.
  let ctxCache = { el: null, ctx: null };
  function ctx() {
    const el = $("#dv-web-player .atvwebplayersdk-player-container");
    if (!el) return null;
    if (ctxCache.el === el && ctxCache.ctx) return ctxCache.ctx;
    const key = Object.keys(el).find((k) => k.startsWith("__reactInternalInstance") || k.startsWith("__reactFiber"));
    let f = key && el[key];
    for (let i = 0; f && i < 15; i++, f = f.return) {
      const props = f.memoizedProps || {};
      for (const p of [props.context, props.value]) {
        if (p && p.playback && p.captions && p.audioStream && p.player) {
          ctxCache = { el, ctx: p };
          return p;
        }
      }
    }
    return null;
  }

  const isOpen = (c) => !!(c && c.player && c.player.isPlayerOpen && !c.player.isPlayerDestroyed);

  function requirePlayer() {
    const c = ctx();
    if (!isOpen(c)) throw new Error("player not ready (is a title playing?)");
    return c;
  }

  const langOf = (s) => (s.language || s.id.split("_")[0] || "").toLowerCase();
  const summarize = (s) =>
    s && s.id !== "off" ? { id: s.id, language: langOf(s), name: s.label || null, type: s.type || null } : null;

  const timeData = (c) => c.player.currentTime || {};
  const items = (c) => (c.player.timeline && c.player.timeline.items) || [];
  const isMainItem = (item) => !!item && /_main$/.test(item.contentId || "");

  function inAd(c) {
    let info = null;
    try { info = c.adPlayback.getAdPlaybackInfo(); } catch (_) {}
    const item = items(c)[timeData(c).currentTimelineItemIndex];
    return !!info || (!!item && !isMainItem(item));
  }

  // Seconds left in the current ad break, if known.
  function adRemaining(c) {
    try {
      const brk = c.adPlayback.getAdBreakInfo();
      const clip = c.adPlayback.getAdPlaybackInfo();
      if (brk && clip) return Math.max(0, Math.round((brk.endPositionMs - clip.positionMs) / 1000));
    } catch (_) {}
    return null;
  }

  // Main-content position in ms. During an ad this is the position playback
  // resumes at (the last main-content position or the pending seek target).
  let lastMainMs = null;
  let lastTitleId = null;
  function positionMs(c) {
    const t = timeData(c);
    if (typeof t.currentPosition !== "number") return null;
    const titleId = currentTitleId(c);
    if (titleId !== lastTitleId) { lastTitleId = titleId; lastMainMs = null; }
    if (inAd(c)) return lastMainMs;
    lastMainMs = t.currentPosition;
    return lastMainMs;
  }

  function durationMs(c) {
    const end = Math.max(0, ...items(c).filter(isMainItem).map((i) => i.endTime || 0));
    return end > 0 ? end : null;
  }

  const stateOf = (c) => String(c.playback.getPlaybackState() || "unknown").toLowerCase();

  // Settings requested while an ad plays, applied when the main content resumes.
  let pending = null;
  let pendingTimer = null;
  function defer(what) {
    pending = { ...(pending || {}), ...what };
    if (pendingTimer) return;
    pendingTimer = setInterval(async () => {
      const c = ctx();
      if (!isOpen(c)) { clearInterval(pendingTimer); pendingTimer = null; pending = null; return; }
      if (inAd(c) || !pending) return;
      const p = pending;
      pending = null;
      clearInterval(pendingTimer);
      pendingTimer = null;
      try {
        if (p.seconds != null) await seekTo(c, p.seconds);
        if (p.subtitles) await subtitles(p.subtitles);
      } catch (e) {
        lastError = String(e.message || e);
      }
    }, 500);
  }
  let lastError = null;

  function status() {
    const c = ctx();
    if (!isOpen(c)) return { loaded: false, url: location.href, title: document.title };
    const info = (() => { try { return c.title.getTitleInfo() || {}; } catch (_) { return {}; } })();
    const pos = positionMs(c);
    const dur = durationMs(c);
    const ad = inAd(c);
    let audio = null;
    try { audio = summarize(c.audioStream.getAudioStreamInfo().activeStream); } catch (_) {}
    let subtitles = null;
    try {
      const cur = c.captions.getCurrentCaptionsStream();
      if (cur && cur.id !== "off") {
        const s = (c.captions.getTimedTextStreamInfo().streams || []).find((x) => x.id === cur.id) || cur;
        subtitles = summarize(s);
      }
    } catch (_) {}
    let quality = null;
    let qualities = [];
    try {
      quality = c.bandwidthCapper.getCurrentCapStrategy();
      qualities = c.bandwidthCapper.getQualityCaps().map((q) => q.id);
    } catch (_) {}
    const s = {
      loaded: true,
      url: location.href,
      title: info.title || null,
      title_id: info.titleId || null,
      type: info.entityType || info.type || null,
      state: stateOf(c),
      current_time: pos == null ? null : pos / 1000,
      duration: dur == null ? null : dur / 1000,
      volume: Math.round((c.playback.volume ?? 0) * 100),
      muted: !!c.playback.muted,
      playback_rate: c.playback.getPlaybackRate(),
      quality,
      available_qualities: qualities,
      // During ads the subtitle/audio lists are the ad's; report the title's.
      subtitles: ad ? undefined : subtitles,
      audio: ad ? undefined : audio,
      ad_showing: ad,
      ad_remaining: ad ? adRemaining(c) : null,
      next_episode: nextEpisode(c),
    };
    const err = playerError(c);
    if (err) s.error = err;
    if (pending) s.pending = pending;
    if (lastError) { s.warning = lastError; lastError = null; }
    return s;
  }

  // The error the player shows (e.g. "Maximum Streams Playing"), or null.
  function playerError(c) {
    if (!c || !c.error) return null;
    let e = null;
    try { e = c.error.currentLocalizedError || c.error.getCurrentError(); } catch (_) {}
    if (!e) return null;
    const text = [e.title, e.message, e.errorMessage, e.description]
      .filter((s) => typeof s === "string" && s).join(": ");
    const shown = (($("#dv-web-player") || {}).innerText || "").trim().replace(/\s+/g, " ").slice(0, 300);
    const code = e.errorCode || e.code || e.name;
    return (text || shown || "playback error") + (code ? ` (${code})` : "");
  }

  function currentTitleId(c) {
    try { return c.title.getTitleInfo().titleId || null; } catch (_) { return null; }
  }

  // Starts the next episode the way the player's "Next episode" button does.
  async function next(c) {
    const n = c.nextTitle.getNextEpisode();
    if (!n || !n.titleId) throw new Error("there is no next episode");
    c.nextTitle.changeAsin("Button", n.titleId);
    if (!(await waitFor(() => currentTitleId(c) === n.titleId && stateOf(c) === "playing", 20000))) {
      throw new Error("the next episode did not start");
    }
    return status();
  }

  function nextEpisode(c) {
    try {
      const n = c.nextTitle.getNextEpisode();
      return n ? { title_id: n.titleId || null, title: n.title || null } : null;
    } catch (_) {
      return null;
    }
  }

  function refuseDuringAd(c, what) {
    if (inAd(c)) {
      const left = adRemaining(c);
      throw new Error(`${what} is not available while an ad plays${left != null ? ` (about ${left}s left)` : ""}; try again after it`);
    }
  }

  const playButton = () =>
    $("[data-testid='dp-atf-play-button']") || $("[data-automation-id='dp-atf-play-button']");

  async function open(a) {
    const { id } = parseUrl(a.url);
    if (!isDetailPage()) return { navigate: `https://www.amazon.com/gp/video/detail/${id}` };
    if (!(await waitFor(() => playButton() || isOpen(ctx()), 20000))) {
      const signIn = [...document.querySelectorAll("a[href*='signin']")].some((e) => /sign in/i.test(e.textContent || ""));
      throw new Error(signIn
        ? "not signed in to Amazon (run `tvctl auth login primevideo`)"
        : "no play button on this page (the title may need to be rented, bought or subscribed to)");
    }
    if (!isOpen(ctx())) playButton().click();
    const failed = () => playerError(ctx());
    const started = await waitFor(
      () => failed() || (isOpen(ctx()) && typeof timeData(ctx()).currentPosition === "number"), 30000);
    if (failed()) throw new Error("Prime Video: " + failed());
    if (!started) throw new Error("Prime Video player did not start");
    let c = ctx();
    // Wait for real playback (the position moving), so later commands are not
    // overridden by the player finishing its start-up.
    const raw = () => timeData(c).currentPosition;
    const p0 = raw();
    await waitFor(() => failed() || (stateOf(c) === "playing" && raw() > p0 + 300), 20000);
    if (failed()) throw new Error("Prime Video: " + failed());
    // Title metadata arrives after playback starts (in a fresh store context).
    await waitFor(() => isOpen(ctx()) && ctx().title.getTitleInfo().title, 10000);
    c = ctx() || c;
    let warning = null;
    if (inAd(c)) {
      const later = {};
      if (a.start != null) { later.seconds = a.start; lastMainMs = a.start * 1000; }
      if (a.subtitles) later.subtitles = a.subtitles;
      if (Object.keys(later).length) defer(later);
    } else {
      if (a.start != null) await seekTo(c, a.start);
      if (a.subtitles) {
        try { await subtitles(a.subtitles); } catch (e) { warning = "subtitles: " + (e.message || e); }
      }
    }
    if (a.paused) {
      c.playback.pause();
      await waitFor(() => stateOf(c) === "paused", 5000);
    }
    const s = status();
    return warning ? { ...s, warning } : s;
  }

  // Seeks in main-content time. Seeking past an ad break plays that break first;
  // the position then reported is the target.
  async function seekTo(c, seconds) {
    const dur = durationMs(c);
    let ms = Math.max(0, seconds * 1000);
    if (dur) ms = Math.min(ms, dur - 1000);
    c.playback.seek(ms);
    lastMainMs = ms;
    await waitFor(() => inAd(c) || Math.abs(timeData(c).currentPosition - ms) < 3000, 8000);
    return ms / 1000;
  }

  function matchStream(streams, want, typeOrder) {
    const w = want.toLowerCase();
    const rank = (s) => {
      const i = typeOrder.indexOf(String(s.type || "").toLowerCase());
      return i < 0 ? typeOrder.length : i;
    };
    const sorted = [...streams].sort((x, y) => rank(x) - rank(y));
    return (
      sorted.find((s) => s.id.toLowerCase() === w) ||
      sorted.find((s) => langOf(s) === w) ||
      (!w.includes("-") && sorted.find((s) => langOf(s).split("-")[0] === w)) ||
      sorted.find((s) => (s.label || "").toLowerCase() === w)
    );
  }

  async function subtitles(lang) {
    const c = requirePlayer();
    refuseDuringAd(c, "subtitles");
    const streams = c.captions.getTimedTextStreamInfo().streams || [];
    if (lang === "list") return { ...status(), tracks: streams.map(summarize) };
    if (lang === "off") {
      // Through the track change (like the menu), so "off" is remembered.
      c.captions.tryTrackChange("off");
      await waitFor(() => c.captions.getCurrentCaptionsStream().id === "off", 3000);
      return status();
    }
    if (!streams.length) throw new Error("this title has no subtitles");
    let target;
    if (lang === "on") {
      const audio = c.audioStream.getAudioStreamInfo().activeStream;
      target = (audio && matchStream(streams, langOf(audio).split("-")[0], ["subtitle", "sdh"])) || streams[0];
    } else {
      target = matchStream(streams, lang, ["subtitle", "sdh"]);
      if (!target) {
        throw new Error(`no subtitles for "${lang}" (available: ${[...new Set(streams.map(langOf))].join(", ")})`);
      }
    }
    c.captions.tryTrackChange(target.id);
    if (!(await waitFor(() => c.captions.getCurrentCaptionsStream().id === target.id, 5000))) {
      throw new Error(`could not switch subtitles to ${target.id}`);
    }
    return status();
  }

  async function audio(lang) {
    const c = requirePlayer();
    refuseDuringAd(c, "audio");
    const streams = c.audioStream.getAudioStreamInfo().streams || [];
    if (lang === "list") return { ...status(), audio_tracks: streams.map(summarize) };
    if (streams.length <= 1) throw new Error("this title has only one audio track");
    const target = matchStream(streams, lang, ["dialog"]);
    if (!target) {
      throw new Error(`no audio track for "${lang}" (available: ${streams.map((s) => s.id).join(", ")})`);
    }
    c.audioStream.changeAudioStream(target.id);
    if (!(await waitFor(() => c.audioStream.getAudioStreamInfo().activeStream.id === target.id, 8000))) {
      throw new Error(`could not switch audio to ${target.id}`);
    }
    return status();
  }

  async function settle(c, state) {
    if (state) await waitFor(() => stateOf(c) === state, 5000);
    else await sleep(300);
    return status();
  }

  async function run(a) {
    if (a.action === "open") return open(a);
    if (a.action === "status") return status();
    const c = requirePlayer();
    switch (a.action) {
      case "play": c.playback.play(); return settle(c, "playing");
      case "pause": c.playback.pause(); return settle(c, "paused");
      case "toggle":
        if (stateOf(c) === "playing") { c.playback.pause(); return settle(c, "paused"); }
        c.playback.play();
        return settle(c, "playing");
      case "next": return next(c);
      case "seek": {
        refuseDuringAd(c, "seeking");
        const cur = (positionMs(c) || 0) / 1000;
        const t = await seekTo(c, a.relative ? cur + a.seconds : a.seconds);
        return { ...status(), current_time: t };
      }
      case "subtitles": return subtitles(a.lang);
      case "audio": return audio(a.lang);
      case "volume":
        if (a.level != null) c.playback.volume = Math.max(0, Math.min(100, a.level)) / 100;
        if (a.muted != null) c.playback.muted = a.muted;
        return settle(c);
      case "speed": {
        refuseDuringAd(c, "speed");
        const options = (c.speedControls.state && c.speedControls.state.options) || [];
        if (options.length && !options.includes(a.rate)) {
          throw new Error(`speed ${a.rate} not available (available: ${options.join(", ")})`);
        }
        c.playback.setPlaybackRate(a.rate);
        if (!(await waitFor(() => c.playback.getPlaybackRate() === a.rate, 3000))) {
          throw new Error(`could not set speed ${a.rate}`);
        }
        return status();
      }
      case "quality": {
        const levels = c.bandwidthCapper.getQualityCaps().map((q) => q.id);
        if (!levels.includes(a.level)) {
          throw new Error(`quality "${a.level}" not available (available: ${levels.join(", ")})`);
        }
        c.bandwidthCapper.setCapStrategy(a.level, true);
        return settle(c);
      }
      default:
        throw new Error(`${a.action} is not supported by the primevideo plugin`);
    }
  }

  // Stop the stream when the window closes, so it does not keep counting
  // against the account's limit of simultaneous streams.
  addEventListener("pagehide", () => {
    try { const c = ctx(); if (isOpen(c)) c.player.closePlayer(); } catch (_) {}
  });

  return { run };
})();
