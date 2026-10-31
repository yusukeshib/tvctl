// YouTube adapter for the Chrome backend. Runs on www.youtube.com/watch pages and
// drives the page's own player element (#movie_player), which exposes the full
// internal player API.
window.__mediactl = (() => {
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const STATES = { "-1": "unstarted", 0: "ended", 1: "playing", 2: "paused", 3: "buffering", 5: "cued" };
  const mp = () => document.getElementById("movie_player");

  async function waitFor(pred, ms) {
    const end = Date.now() + ms;
    while (Date.now() < end) {
      try { if (pred()) return true; } catch (_) {}
      await sleep(100);
    }
    return pred();
  }

  function parseTime(s) {
    if (!s) return null;
    if (/^\d+(\.\d+)?s?$/.test(s)) return parseFloat(s);
    const m = /^(?:(\d+)h)?(?:(\d+)m)?(?:(\d+)s)?$/.exec(s);
    return m ? (+m[1] || 0) * 3600 + (+m[2] || 0) * 60 + (+m[3] || 0) : null;
  }

  function parseUrl(raw) {
    const u = new URL(raw);
    const host = u.hostname.replace(/^(www|m|music)\./, "");
    const parts = u.pathname.split("/").filter(Boolean);
    let id = null;
    if (host === "youtu.be") id = parts[0];
    else if (parts[0] === "watch") id = u.searchParams.get("v");
    else if (["shorts", "embed", "live", "v"].includes(parts[0])) id = parts[1];
    if (!id || !/^[A-Za-z0-9_-]{11}$/.test(id)) throw new Error("could not find a video id in " + raw);
    return { id, start: parseTime(u.searchParams.get("t") || u.searchParams.get("start")) };
  }

  // Makes the player fill the window and hides the rest of the page.
  // Pure CSS so it keeps working when YouTube re-parents the player: everything
  // is made invisible except the player, and ancestors of the player lose
  // contain/transform (which would otherwise trap `position: fixed`).
  // The adapter is loaded into every document of the window, so this runs on
  // reloads too, and a MutationObserver restores the style if it is removed.
  let observer = null;
  function fillWindow() {
    if (!observer) {
      observer = new MutationObserver(() => {
        if (!document.getElementById("__mediactl_css")) addStyle();
      });
      observer.observe(document.documentElement, { childList: true, subtree: false });
    }
    addStyle();
    window.dispatchEvent(new Event("resize"));
  }

  function addStyle() {
    if (!document.getElementById("__mediactl_css")) {
      const s = document.createElement("style");
      s.id = "__mediactl_css";
      s.textContent = `
        html, body { overflow: hidden !important; background: #000 !important; }
        body * { visibility: hidden !important; }
        #movie_player, #movie_player * { visibility: visible !important; }
        :has(#movie_player) {
          contain: none !important; transform: none !important;
          filter: none !important; will-change: auto !important;
        }
        #movie_player {
          position: fixed !important; inset: 0 !important;
          width: 100vw !important; height: 100vh !important;
          z-index: 2147483646 !important; background: #000 !important;
        }
        #movie_player video { object-fit: contain !important; }
      `;
      document.documentElement.appendChild(s);
    }
  }

  // True when the player covers the viewport and the rest of the page is hidden.
  function fillsWindow() {
    const p = mp();
    if (!p || !document.getElementById("__mediactl_css")) return false;
    const r = p.getBoundingClientRect();
    const covers = r.left <= 0 && r.top <= 0 && r.right >= innerWidth && r.bottom >= innerHeight;
    const masthead = document.querySelector("#masthead-container, ytd-masthead");
    const hidden = !masthead || getComputedStyle(masthead).visibility === "hidden";
    return covers && hidden;
  }

  let subs = null;
  let pendingSubs = null;
  let listening = false;

  function listen(p) {
    if (listening) return;
    listening = true;
    p.addEventListener("onStateChange", (st) => {
      if (st === 1 && pendingSubs) {
        const lang = pendingSubs;
        pendingSubs = null;
        subtitles(lang).catch((err) => { subs = { language: lang, error: String(err.message || err) }; });
      }
    });
  }

  function summarizeTrack(t) {
    return {
      language: t.languageCode,
      name: t.displayName || t.languageName || null,
      auto_generated: t.kind === "asr",
      translated_to: t.translationLanguage ? t.translationLanguage.languageCode : null,
    };
  }

  function audioInfo(t) {
    // Internal track objects; accessor names may change.
    const info = t && (t.getLanguageInfo ? t.getLanguageInfo() : t.languageInfo || t);
    const get = (o, f, k) => (o && typeof o[f] === "function" ? o[f]() : o && o[k]);
    return {
      id: get(info, "getId", "id") || null,
      name: get(info, "getName", "name") || null,
      is_default: !!(info && (info.isDefault || (typeof info.getIsDefault === "function" && info.getIsDefault()))),
    };
  }

  function requirePlayer() {
    const p = mp();
    if (!p || !p.getPlayerState) throw new Error("player not ready");
    return p;
  }

  function status() {
    const p = mp();
    if (!p || !p.getPlayerState) return { loaded: false, url: location.href };
    const d = p.getVideoData() || {};
    const st = p.getPlayerState();
    let audio = null;
    try { audio = p.getAudioTrack ? audioInfo(p.getAudioTrack()) : null; } catch (_) {}
    return {
      loaded: true,
      video_id: d.video_id || new URL(location.href).searchParams.get("v"),
      title: d.title || null,
      author: d.author || null,
      url: location.href,
      state: STATES[st] ?? String(st),
      current_time: p.getCurrentTime(),
      duration: p.getDuration(),
      volume: p.getVolume(),
      muted: p.isMuted(),
      playback_rate: p.getPlaybackRate(),
      quality: p.getPlaybackQuality(),
      available_qualities: p.getAvailableQualityLevels(),
      subtitles: subs,
      audio,
      ad_showing: p.classList.contains("ad-showing"),
      fills_window: fillsWindow(),
      logged_in: !!(window.ytcfg && ytcfg.get && ytcfg.get("LOGGED_IN")),
    };
  }

  async function open(a) {
    const { id, start } = parseUrl(a.url);
    const here = new URL(location.href);
    if (here.pathname !== "/watch" || here.searchParams.get("v") !== id) {
      return { navigate: `https://www.youtube.com/watch?v=${id}` };
    }
    if (!(await waitFor(() => mp() && mp().getPlayerState, 20000))) throw new Error("YouTube player did not load");
    const p = mp();
    listen(p);
    fillWindow();
    const t = a.start ?? start;
    if (t) p.seekTo(t, true);
    if (a.paused) p.pauseVideo(); else p.playVideo();
    const wanted = a.paused ? [2, 5, -1] : [1];
    await waitFor(() => wanted.includes(p.getPlayerState()) || p.classList.contains("ad-showing"), 15000);
    if (a.subtitles) {
      try { await subtitles(a.subtitles); } catch (e) { return { ...status(), warning: "subtitles: " + (e.message || e) }; }
    }
    return status();
  }

  async function captionTracks(p) {
    p.loadModule("captions");
    let tracks = [];
    await waitFor(() => {
      tracks = p.getOption("captions", "tracklist") || [];
      return tracks.length > 0;
    }, 5000);
    return tracks;
  }

  async function subtitles(lang) {
    const p = requirePlayer();
    if (lang === "off") {
      try { p.setOption("captions", "track", {}); } catch (_) {}
      p.unloadModule("captions");
      subs = null;
      pendingSubs = null;
      await sleep(300);
      return status();
    }
    if ([-1, 5].includes(p.getPlayerState())) {
      if (lang === "list") throw new Error("subtitle tracks are available after playback starts");
      pendingSubs = lang;
      subs = { language: lang, pending: true };
      return status();
    }
    const tracks = await captionTracks(p);
    if (lang === "list") {
      const tl = p.getOption("captions", "translationLanguages") || [];
      return { ...status(), tracks: tracks.map(summarizeTrack), translation_languages: tl.map((l) => l.languageCode) };
    }
    if (!tracks.length) throw new Error("this video has no subtitles");
    const manual = tracks.find((t) => t.kind !== "asr") || tracks[0];
    const select = (track) => { p.setOption("captions", "track", track); subs = summarizeTrack(track); };
    if (lang === "on") {
      select(manual);
    } else {
      const want = lang.toLowerCase();
      const code = (t) => t.languageCode.toLowerCase();
      const exact =
        tracks.find((t) => code(t) === want && t.kind !== "asr") ||
        tracks.find((t) => code(t) === want) ||
        tracks.find((t) => code(t).split("-")[0] === want.split("-")[0]);
      if (exact) {
        select(exact);
      } else {
        const tl = p.getOption("captions", "translationLanguages") || [];
        const target = tl.find((l) => l.languageCode.toLowerCase() === want);
        if (!target) throw new Error(`no subtitles or auto-translation available for "${lang}"`);
        select({ ...manual, translationLanguage: target });
      }
    }
    await sleep(500);
    return status();
  }

  async function audio(lang) {
    const p = requirePlayer();
    let tracks = (p.getAvailableAudioTracks && p.getAvailableAudioTracks()) || [];
    // Single-track videos report no alternatives; list the current track instead.
    if (!tracks.length && p.getAudioTrack && p.getAudioTrack()) tracks = [p.getAudioTrack()];
    if (lang === "list") return { ...status(), audio_tracks: tracks.map(audioInfo) };
    if (tracks.length <= 1) throw new Error("this video has only one audio track");
    const want = lang.toLowerCase();
    const match = tracks.find((t) => {
      const i = audioInfo(t);
      const tid = String(i.id || "").toLowerCase();
      return tid === want || tid.split(".")[0] === want || tid.split(/[-.]/)[0] === want;
    });
    if (!match) throw new Error(`no audio track for "${lang}" (available: ${tracks.map((t) => audioInfo(t).id).join(", ")})`);
    p.setAudioTrack(match);
    await sleep(800);
    return status();
  }

  // Waits (bounded) for the player to reach `state`, e.g. through buffering.
  async function settleIn(p, state) {
    await waitFor(() => p.getPlayerState() === state || p.classList.contains("ad-showing"), 5000);
    return status();
  }

  async function settle() {
    await sleep(300);
    return status();
  }

  async function run(a) {
    if (a.action === "open") return open(a);
    if (a.action === "status") return status();
    const p = requirePlayer();
    switch (a.action) {
      case "play": p.playVideo(); return settleIn(p, 1);
      case "pause": p.pauseVideo(); return settleIn(p, 2);
      case "toggle":
        if (p.getPlayerState() === 1) { p.pauseVideo(); return settleIn(p, 2); }
        p.playVideo();
        return settleIn(p, 1);
      case "next": p.nextVideo(); return settle();
      case "previous": p.previousVideo(); return settle();
      case "seek": {
        let t = a.relative ? p.getCurrentTime() + a.seconds : a.seconds;
        const dur = p.getDuration();
        if (dur > 0) t = Math.min(t, dur);
        t = Math.max(0, t);
        p.seekTo(t, true);
        await sleep(300);
        return { ...status(), current_time: t };
      }
      case "subtitles": return subtitles(a.lang);
      case "audio": return audio(a.lang);
      case "volume":
        if (a.level != null) p.setVolume(a.level);
        if (a.muted === true) p.mute();
        if (a.muted === false) p.unMute();
        return settle();
      case "speed": p.setPlaybackRate(a.rate); return settle();
      case "quality": {
        const levels = p.getAvailableQualityLevels();
        if (a.level !== "auto" && levels.length && !levels.includes(a.level)) {
          throw new Error(`quality "${a.level}" not available (available: ${levels.join(", ")})`);
        }
        p.setPlaybackQualityRange(a.level, a.level);
        // The switch happens asynchronously; wait (bounded) until it is visible.
        if (a.level !== "auto") await waitFor(() => p.getPlaybackQuality() === a.level, 8000);
        else await sleep(500);
        return { ...status(), requested_quality: a.level };
      }
      default: throw new Error(`${a.action} is not supported by the youtube plugin`);
    }
  }

  // Loaded at document start on reloads/navigations: keep watch pages filled.
  if (location.pathname === "/watch" && document.documentElement) fillWindow();

  return { run };
})();
