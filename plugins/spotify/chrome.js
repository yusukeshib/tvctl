// Spotify Web Player adapter (Chrome backend). The web player has no public
// JavaScript API, so this drives its UI controls, found by `data-testid`.
window.__mediactl = (() => {
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

  function parseClock(s) {
    if (!s) return null;
    const parts = s.trim().replace(/^-/, "").split(":").map(Number);
    if (parts.some(isNaN)) return null;
    return parts.reduce((t, v) => t * 60 + v, 0);
  }

  const footer = () => $("[data-testid='now-playing-bar']") || $("footer") || document;
  const playPause = () => $("[data-testid='control-button-playpause']", footer());
  const isPlaying = () => {
    const b = playPause();
    if (!b) return false;
    const label = (b.getAttribute("aria-label") || "").toLowerCase();
    return label.includes("pause") || label.includes("一時停止");
  };
  const progressInput = () => $("[data-testid='playback-progressbar'] input[type='range']", footer());
  const volumeInput = () => $("[data-testid='volume-bar'] input[type='range']", footer());
  const muteButton = () => $("[data-testid='volume-bar-toggle-mute-button']", footer());

  function setRange(input, value) {
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set;
    setter.call(input, String(value));
    input.dispatchEvent(new Event("input", { bubbles: true }));
    input.dispatchEvent(new Event("change", { bubbles: true }));
    // Finish the "drag" so the slider does not keep capturing later clicks.
    for (const type of ["pointerup", "mouseup"]) {
      input.dispatchEvent(new PointerEvent(type, { bubbles: true }));
    }
    input.blur();
  }

  function nowPlaying() {
    const title = $("[data-testid='context-item-info-title']", footer());
    const artists = $("[data-testid='context-item-info-subtitles']", footer());
    return {
      title: title ? title.textContent.trim() : null,
      artist: artists ? artists.textContent.trim() : null,
    };
  }

  function volumeState() {
    const v = volumeInput();
    if (!v) return { volume: null, muted: null };
    const max = Number(v.max) || 1;
    const volume = Math.round((Number(v.value) / max) * 100);
    // Spotify mutes by moving the slider to 0; the button label can lag behind.
    return { volume, muted: volume === 0 };
  }

  function status() {
    const pos = $("[data-testid='playback-position']", footer());
    const dur = $("[data-testid='playback-duration']", footer());
    const np = nowPlaying();
    const { volume, muted } = volumeState();
    return {
      loaded: !!playPause(),
      url: location.href,
      title: np.title,
      artist: np.artist,
      state: !playPause() ? "unstarted" : isPlaying() ? "playing" : "paused",
      current_time: parseClock(pos && pos.textContent),
      duration: parseClock(dur && dur.textContent),
      volume,
      muted,
      logged_in: !$("[data-testid='login-button']"),
    };
  }

  function click(el, what) {
    if (!el) throw new Error(`${what} control not found`);
    if (el.disabled) throw new Error(`${what} is not available now`);
    el.click();
  }

  async function settle(pred) {
    if (pred) await waitFor(pred, 5000);
    else await sleep(400);
    return status();
  }

  // The big green play button of the page's entity (track, album, playlist...).
  const pagePlayButton = () =>
    $("[data-testid='action-bar-row'] [data-testid='play-button']") ||
    $("main [data-testid='play-button']");

  // Track links autoplay (Spotify redirects them to the album with
  // `highlight=...&autoplay_ok=1`); other pages are started with their play button.
  async function open(a) {
    if (!(await waitFor(() => playPause() && pagePlayButton(), 20000))) {
      throw new Error("Spotify player did not load (are you logged in? `mediactl auth login spotify`)");
    }
    const kind = new URL(a.url).pathname.split("/").filter(Boolean).find((p) => !p.startsWith("intl-"));
    const autoplays = kind === "track" || location.search.includes("autoplay_ok");
    if (autoplays) {
      // The footer may still show the account's previous playback; the linked
      // track's autoplay is recognized by playback starting from the beginning.
      const started = () => isPlaying() && (status().current_time ?? 99) <= 5;
      if (!(await waitFor(started, 10000))) click(playPause(), "play");
    } else if (!a.paused) {
      click(pagePlayButton(), "play");
    }
    if (!a.paused || autoplays) await waitFor(() => isPlaying() && nowPlaying().title, 10000);
    if (a.start) await seekTo(a.start);
    if (a.paused && isPlaying()) {
      click(playPause(), "pause");
      await waitFor(() => !isPlaying(), 5000);
    }
    return status();
  }

  async function seekTo(t) {
    const input = progressInput();
    if (!input) throw new Error("progress bar not found");
    const max = Number(input.max);
    // The range is in milliseconds when max is large, otherwise in seconds.
    const unit = max > 10000 ? 1000 : 1;
    const target = Math.max(0, Math.min(t * unit, max));
    setRange(input, target);
    await sleep(600);
    return target / unit;
  }

  // Spotify sometimes ignores the first click on the mute button (e.g. right
  // after the volume slider changed), so click until the state changes.
  async function setMuted(want) {
    for (let i = 0; i < 3 && volumeState().muted !== want; i++) {
      click(muteButton(), "mute");
      await waitFor(() => volumeState().muted === want, 600);
    }
    if (volumeState().muted !== want) throw new Error(`could not ${want ? "mute" : "unmute"}`);
  }

  async function run(a) {
    if (a.action === "open") return open(a);
    if (a.action === "status") return status();
    switch (a.action) {
      case "play":
        if (!isPlaying()) click(playPause(), "play");
        return settle(isPlaying);
      case "pause":
        if (isPlaying()) click(playPause(), "pause");
        return settle(() => !isPlaying());
      case "toggle": {
        const was = isPlaying();
        click(playPause(), "play/pause");
        return settle(() => isPlaying() !== was);
      }
      case "next": click($("[data-testid='control-button-skip-forward']", footer()), "next"); return settle();
      case "previous": click($("[data-testid='control-button-skip-back']", footer()), "previous"); return settle();
      case "seek": {
        const cur = status().current_time || 0;
        const t = await seekTo(a.relative ? cur + a.seconds : a.seconds);
        return { ...status(), current_time: t };
      }
      case "volume": {
        const v = volumeInput();
        if (a.level != null) {
          if (!v) throw new Error("volume control not found");
          setRange(v, (a.level / 100) * (Number(v.max) || 1));
        }
        if (a.muted != null) await setMuted(a.muted);
        return settle();
      }
      default:
        throw new Error(`${a.action} is not supported by the spotify plugin`);
    }
  }

  return { run };
})();
