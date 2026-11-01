// Netflix adapter (Chrome backend). Runs on netflix.com/watch/<id> pages and
// drives the page's internal player API
// (`netflix.appContext.state.playerApp.getAPI().videoPlayer`). Times in that
// API are milliseconds. Never set `<video>.currentTime` directly: Netflix
// stops playback with error M7375. Not a public API; the E2E tests detect
// breakage. Ads (plans with ads) are not skipped.
window.__tvctl = (() => {
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  // Rates offered by Netflix's speed menu.
  const SPEEDS = [0.5, 0.75, 1, 1.25, 1.5];

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
    const i = parts.findIndex((p) => p === "watch" || p === "title");
    const id = i >= 0 ? parts[i + 1] : null;
    if (!id || !/^\d+$/.test(id)) {
      throw new Error(`not a Netflix title URL: ${raw} (expected https://www.netflix.com/watch/<id> or /title/<id>)`);
    }
    const t = u.searchParams.get("t");
    return { id, start: t && /^\d+$/.test(t) ? Number(t) : null };
  }

  function app() {
    try { return window.netflix.appContext.state.playerApp; } catch (_) { return null; }
  }

  // The current player session (it changes when the next episode starts).
  function session() {
    const a = app();
    if (!a) return null;
    const api = a.getAPI().videoPlayer;
    const ids = api.getAllPlayerSessionIds().filter((id) => id.startsWith("watch"));
    const id = ids[ids.length - 1];
    if (!id) return null;
    const player = api.getVideoPlayerBySessionId(id);
    if (!player) return null;
    const state = a.getState().videoPlayer.playbackStateBySessionId[id] || {};
    return { id, player, state };
  }

  const isLoaded = (s) => !!s && s.state.loadedState === "LOADED" && s.player.isReady();

  function requirePlayer() {
    const s = session();
    if (!isLoaded(s)) throw new Error("player not ready (is a title playing?)");
    return s;
  }

  function stateOf(s) {
    const st = s.state;
    if (st.ended || s.player.isEnded()) return "ended";
    if (s.player.isPaused()) return "paused";
    if (st.seeking || st.stalled || (st.busyProgress ?? -1) >= 0) return "buffering";
    return "playing";
  }

  function adShowing(s) {
    try { return !!s.player.getAdManager().getPresentingAdBreak(); } catch (_) { return false; }
  }

  function video(s) {
    try {
      // getCurrentVideo is the episode; getVideo would be the show.
      const md = app().getState().videoPlayer.videoMetadata[String(s.player.getMovieId())];
      return md && (md.getCurrentVideo() || md.getVideo());
    } catch (_) {
      return null;
    }
  }

  function describe(v) {
    if (!v) return {};
    const call = (name) => { try { return typeof v[name] === "function" ? v[name]() : null; } catch (_) { return null; } };
    const out = { title: call("getTitle"), type: call("getType") };
    if (out.type === "show" || call("isEpisodic")) {
      const season = call("getSeason");
      const seasonName = season && typeof season.getTitle === "function" ? season.getTitle() : null;
      const index = call("getEpisodeIndex");
      out.episode = {
        id: call("getId"),
        title: call("getEpisodeTitle") || null,
        // Some shows hide episode numbers; fall back to the position in the season.
        number: Number(call("getEpisodeNumber")) || (typeof index === "number" ? index + 1 : null),
        season: seasonName,
      };
      const next = call("getNextEpisode");
      if (next) {
        const n = (name) => { try { return typeof next[name] === "function" ? next[name]() : null; } catch (_) { return null; } };
        out.next_episode = { id: n("getId"), title: n("getEpisodeTitle") || n("getTitle"), number: n("getEpisodeNumber") };
      } else {
        out.next_episode = null;
      }
    }
    return out;
  }

  const textInfo = (t) =>
    t && !t.isNoneTrack
      ? { id: t.trackId, language: t.bcp47 || null, name: t.displayName || null, type: t.trackType || null, forced: !!t.isForcedNarrative }
      : null;
  const audioInfo = (t) => (t ? { id: t.trackId, language: t.bcp47 || null, name: t.displayName || null } : null);

  // The error Netflix shows (e.g. too many streams), or null.
  function playerError(s) {
    let e = null;
    try { e = s && s.player.getError(); } catch (_) {}
    if (!e) {
      const errs = s && s.state.errors;
      if (errs && errs.length) e = errs[errs.length - 1];
    }
    if (!e) {
      const el = document.querySelector("[data-uia='player-error'], .nf-player-container .error-page, [data-uia='error-message-container']");
      if (el && el.innerText.trim()) return el.innerText.trim().replace(/\s+/g, " ").slice(0, 300);
      return null;
    }
    const msg = [e.title, e.message, e.errorMessage, e.description].filter((x) => typeof x === "string" && x).join(": ");
    const code = e.code || e.errorCode || e.displayCode;
    const shown = (document.querySelector(".nf-player-container") || {}).innerText;
    return (msg || (shown || "").trim().replace(/\s+/g, " ").slice(0, 300) || "playback error") + (code ? ` (${code})` : "");
  }

  function status() {
    const s = session();
    if (!isLoaded(s)) {
      const err = s && playerError(s);
      return { loaded: false, url: location.href, title: document.title, ...(err ? { error: err } : {}) };
    }
    const p = s.player;
    const out = {
      loaded: true,
      url: location.href,
      video_id: String(p.getMovieId()),
      ...describe(video(s)),
      state: stateOf(s),
      current_time: p.getCurrentTime() / 1000,
      duration: p.getDuration() / 1000,
      volume: Math.round(p.getVolume() * 100),
      muted: p.isMuted(),
      playback_rate: p.getPlaybackRate(),
      subtitles: textInfo(p.getTextTrack()),
      audio: audioInfo(p.getAudioTrack()),
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
      if (!isLoaded(s)) return;
      if (adShowing(s) || !pending) return;
      const p = pending;
      pending = null;
      clearInterval(pendingTimer);
      pendingTimer = null;
      try {
        if (p.seconds != null) await seekTo(s, p.seconds);
        if (p.subtitles) await subtitles(p.subtitles);
      } catch (e) {
        lastError = String(e.message || e);
      }
    }, 500);
  }

  function refuseDuringAd(s, what) {
    if (adShowing(s)) throw new Error(`${what} is not available while an ad plays; try again after it`);
  }

  async function open(a) {
    const { id, start } = parseUrl(a.url);
    // A /title/ (show or movie) id plays like the page's Play button; Netflix
    // then redirects /watch/<show id> to the episode, so any watch page is final.
    if (!location.pathname.startsWith("/watch/")) {
      return { navigate: `https://www.netflix.com/watch/${id}` };
    }
    const failed = () => playerError(session());
    const ready = await waitFor(() => failed() || isLoaded(session()), 30000);
    if (failed()) throw new Error("Netflix: " + failed());
    if (!ready) throw new Error("Netflix player did not load (are you logged in? `tvctl auth login netflix`)");
    let s = session();
    // Wait for real playback (time moving), so later commands are not
    // overridden by the player finishing its start-up.
    const t0 = s.player.getCurrentTime();
    if (s.player.isPaused()) s.player.play();
    await waitFor(() => failed() || adShowing(session()) || session().player.getCurrentTime() > t0 + 300, 20000);
    if (failed()) throw new Error("Netflix: " + failed());
    s = session();
    let warning = null;
    const t = a.start ?? start;
    if (adShowing(s)) {
      // Applied when the ad ends.
      const later = {};
      if (t != null) later.seconds = t;
      if (a.subtitles) later.subtitles = a.subtitles;
      if (Object.keys(later).length) defer(later);
    } else {
      if (t != null) await seekTo(s, t);
      if (a.subtitles) {
        try { await subtitles(a.subtitles); } catch (e) { warning = "subtitles: " + (e.message || e); }
      }
    }
    if (a.paused) {
      s.player.pause();
      await waitFor(() => session().player.isPaused(), 5000);
    }
    const st = status();
    return warning ? { ...st, warning } : st;
  }

  async function seekTo(s, seconds) {
    const dur = s.player.getDuration();
    let ms = Math.max(0, Math.round(seconds * 1000));
    if (dur) ms = Math.min(ms, dur - 1000);
    s.player.seek(ms);
    await waitFor(() => Math.abs(session().player.getCurrentTime() - ms) < 3000 && !session().state.seeking, 8000);
    return ms / 1000;
  }

  function matchTrack(tracks, want, rank) {
    const w = want.toLowerCase();
    const sorted = [...tracks].sort((x, y) => rank(x) - rank(y));
    const lang = (t) => String(t.bcp47 || "").toLowerCase();
    return (
      sorted.find((t) => String(t.trackId).toLowerCase() === w) ||
      sorted.find((t) => lang(t) === w) ||
      (!w.includes("-") && sorted.find((t) => lang(t).split("-")[0] === w)) ||
      sorted.find((t) => String(t.displayName || "").toLowerCase() === w)
    );
  }

  // Full subtitles before closed captions; forced-narrative tracks last.
  const textRank = (t) => (t.isForcedNarrative ? 2 : t.trackType === "PRIMARY" ? 0 : 1);
  // Regular audio before audio description.
  const audioRank = (t) => (/audio description/i.test(t.displayName || "") || /^A:\d+:2;/.test(t.trackId) ? 1 : 0);

  async function subtitles(lang) {
    const s = requirePlayer();
    refuseDuringAd(s, "subtitles");
    const all = s.player.getTextTrackList() || [];
    const tracks = all.filter((t) => !t.isNoneTrack);
    if (lang === "list") return { ...status(), tracks: tracks.map(textInfo) };
    let target;
    if (lang === "off") {
      target = all.find((t) => t.isNoneTrack);
      if (!target) throw new Error("no way to turn subtitles off");
    } else if (lang === "on") {
      const audio = s.player.getAudioTrack();
      const base = String((audio && audio.bcp47) || "en").split("-")[0];
      target = matchTrack(tracks, base, textRank) || tracks.sort((x, y) => textRank(x) - textRank(y))[0];
      if (!target) throw new Error("this title has no subtitles");
    } else {
      target = matchTrack(tracks, lang, textRank);
      if (!target) {
        throw new Error(`no subtitles for "${lang}" (available: ${[...new Set(tracks.map((t) => t.bcp47))].join(", ")})`);
      }
    }
    await s.player.setTextTrack(target);
    if (!(await waitFor(() => (session().player.getTextTrack() || {}).trackId === target.trackId, 5000))) {
      throw new Error(`could not switch subtitles to ${target.displayName}`);
    }
    return status();
  }

  async function audio(lang) {
    const s = requirePlayer();
    refuseDuringAd(s, "audio");
    const tracks = s.player.getAudioTrackList() || [];
    if (lang === "list") return { ...status(), audio_tracks: tracks.map(audioInfo) };
    if (tracks.length <= 1) throw new Error("this title has only one audio track");
    const target = matchTrack(tracks, lang, audioRank);
    if (!target) {
      throw new Error(`no audio track for "${lang}" (available: ${[...new Set(tracks.map((t) => t.bcp47))].join(", ")})`);
    }
    await s.player.setAudioTrack(target);
    if (!(await waitFor(() => (session().player.getAudioTrack() || {}).trackId === target.trackId, 8000))) {
      throw new Error(`could not switch audio to ${target.displayName}`);
    }
    return status();
  }

  async function next() {
    const s = requirePlayer();
    const info = describe(video(s));
    if (!info.next_episode) throw new Error("there is no next episode");
    const from = String(s.player.getMovieId());
    // The API's playNextEpisode only works from the post-play screen; the
    // player's "Next episode" button switches in place. Mouse movement shows it.
    const button = await revealControl("control-next");
    if (!button) throw new Error("next episode button not found");
    button.click();
    const moved = await waitFor(() => {
      const n = session();
      return isLoaded(n) && String(n.player.getMovieId()) !== from;
    }, 12000);
    if (!moved) throw new Error("the next episode did not start");
    await waitFor(() => !session().player.isPaused(), 5000);
    return status();
  }

  async function revealControl(uia) {
    const sel = `[data-uia='${uia}']`;
    const surface = document.querySelector("[data-uia='watch-video'], .watch-video") || document.body;
    for (let i = 0; i < 10 && !document.querySelector(sel); i++) {
      surface.dispatchEvent(new MouseEvent("mousemove", { bubbles: true, clientX: 200 + i * 7, clientY: 200 }));
      await sleep(200);
    }
    return document.querySelector(sel);
  }

  async function settle(want) {
    if (want) await waitFor(() => stateOf(session()) === want, 5000);
    else await sleep(300);
    return status();
  }

  async function run(a) {
    if (a.action === "open") return open(a);
    if (a.action === "status") return status();
    const s = requirePlayer();
    const p = s.player;
    switch (a.action) {
      case "play": p.play(); return settle("playing");
      case "pause": p.pause(); return settle("paused");
      case "toggle":
        if (p.isPaused()) { p.play(); return settle("playing"); }
        p.pause();
        return settle("paused");
      case "next": return next();
      case "seek": {
        refuseDuringAd(s, "seeking");
        const cur = p.getCurrentTime() / 1000;
        const t = await seekTo(s, a.relative ? cur + a.seconds : a.seconds);
        return { ...status(), current_time: t };
      }
      case "subtitles": return subtitles(a.lang);
      case "audio": return audio(a.lang);
      case "volume":
        if (a.level != null) p.setVolume(Math.max(0, Math.min(100, a.level)) / 100);
        if (a.muted != null) p.setMuted(a.muted);
        return settle();
      case "speed":
        refuseDuringAd(s, "speed");
        if (!SPEEDS.includes(a.rate)) throw new Error(`speed ${a.rate} not available (available: ${SPEEDS.join(", ")})`);
        p.setPlaybackRate(a.rate);
        if (!(await waitFor(() => session().player.getPlaybackRate() === a.rate, 3000))) {
          throw new Error(`could not set speed ${a.rate}`);
        }
        return status();
      default:
        throw new Error(`${a.action} is not supported by the netflix plugin`);
    }
  }

  return { run };
})();
