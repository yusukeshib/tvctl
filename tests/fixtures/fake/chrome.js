// Fake adapter used by the end-to-end tests. It implements the whole plugin
// contract on an in-memory player so tests are deterministic and offline.
// The same file serves both backends (webview.html loads it with <script src>).
if (!window.__tvctl) window.__tvctl = (() => {
  const DURATION = 600;
  const QUALITIES = ["hd1080", "hd720", "large", "medium", "small", "auto"];
  const TRACKS = [
    { language: "en", name: "English" },
    { language: "ja", name: "Japanese" },
  ];
  const AUDIO = ["en", "ja"];
  const s = {
    opened: null,
    state: "unstarted",
    current_time: 0,
    volume: 100,
    muted: false,
    playback_rate: 1,
    quality: "hd720",
    subtitles: null,
    audio: "en",
    item: 0,
  };

  function status() {
    return {
      loaded: !!s.opened,
      title: "Fake video",
      url: location.href,
      opened: s.opened,
      state: s.state,
      current_time: s.current_time,
      duration: DURATION,
      volume: s.volume,
      muted: s.muted,
      playback_rate: s.playback_rate,
      quality: s.quality,
      available_qualities: QUALITIES,
      subtitles: s.subtitles,
      audio: s.audio,
      item: s.item,
    };
  }

  function requireOpen() {
    if (!s.opened) throw new Error("no video loaded");
  }

  async function run(a) {
    switch (a.action) {
      case "open": {
        const target = new URL(a.url);
        // Exercise the "navigate first" protocol when the page is the target site.
        if (target.host === location.host && target.searchParams.has("redirect") && !location.search.includes("landed")) {
          target.searchParams.delete("redirect");
          target.searchParams.set("landed", "1");
          return { navigate: target.toString() };
        }
        if (target.searchParams.has("fail")) throw new Error("fake open failure");
        s.opened = { url: a.url, start: a.start, paused: a.paused, subtitles: a.subtitles };
        s.state = a.paused ? "paused" : "playing";
        s.current_time = a.start || 0;
        s.subtitles = a.subtitles || null;
        return status();
      }
      case "status":
        return status();
    }
    requireOpen();
    switch (a.action) {
      case "play": s.state = "playing"; break;
      case "pause": s.state = "paused"; break;
      case "toggle": s.state = s.state === "playing" ? "paused" : "playing"; break;
      case "next": s.item += 1; s.current_time = 0; break;
      case "previous": s.item = Math.max(0, s.item - 1); s.current_time = 0; break;
      case "seek": {
        const t = a.relative ? s.current_time + a.seconds : a.seconds;
        s.current_time = Math.min(DURATION, Math.max(0, t));
        break;
      }
      case "subtitles":
        if (a.lang === "list") return { ...status(), tracks: TRACKS };
        if (a.lang === "off") s.subtitles = null;
        else if (a.lang === "on") s.subtitles = "en";
        else if (TRACKS.some((t) => t.language === a.lang)) s.subtitles = a.lang;
        else throw new Error(`no subtitles for "${a.lang}"`);
        break;
      case "audio":
        if (a.lang === "list") return { ...status(), audio_tracks: AUDIO };
        if (!AUDIO.includes(a.lang)) throw new Error(`no audio track for "${a.lang}"`);
        s.audio = a.lang;
        break;
      case "volume":
        if (a.level != null) s.volume = a.level;
        if (a.muted != null) s.muted = a.muted;
        break;
      case "speed": s.playback_rate = a.rate; break;
      case "quality":
        if (!QUALITIES.includes(a.level)) throw new Error(`quality "${a.level}" not available`);
        s.quality = a.level;
        break;
      default:
        throw new Error(`${a.action} is not supported by the fake plugin`);
    }
    return status();
  }

  return { run };
})();
