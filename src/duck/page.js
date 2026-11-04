// Ducking hook, injected into every frame of every player page.
//
// Lowers all media elements by a gain without the page noticing: `volume`
// still reads and writes the page's own value; the element plays at
// `volume * gain`. tvctl calls `window.__tvctlDuck.set(gain, fadeMs)` in the
// main frame, which forwards it to child frames.
if (!window.__tvctlDuck) window.__tvctlDuck = (() => {
  const proto = HTMLMediaElement.prototype;
  const native = Object.getOwnPropertyDescriptor(proto, "volume");
  const KEY = "__tvctlDuck";
  const own = new WeakMap(); // element -> volume the page set
  const tracked = new Set(); // WeakRefs of elements seen
  let gain = 1;
  let ramp = null;

  const track = (el) => {
    if (own.has(el)) return;
    own.set(el, native.get.call(el));
    tracked.add(new WeakRef(el));
  };
  const elements = () => {
    for (const el of document.querySelectorAll("video, audio")) track(el);
    const out = [];
    for (const ref of tracked) {
      const el = ref.deref();
      if (el) out.push(el);
      else tracked.delete(ref);
    }
    return out;
  };
  const apply = (el) => native.set.call(el, Math.min(1, Math.max(0, own.get(el) * gain)));

  Object.defineProperty(proto, "volume", {
    configurable: true,
    enumerable: native.enumerable,
    get() {
      return own.has(this) ? own.get(this) : native.get.call(this);
    },
    set(v) {
      const n = Number(v);
      if (!Number.isFinite(n)) throw new TypeError("The provided double value is non-finite.");
      if (n < 0 || n > 1) {
        throw new DOMException(`The volume provided (${n}) is outside the range [0, 1].`, "IndexSizeError");
      }
      track(this);
      own.set(this, n);
      apply(this);
    },
  });
  const play = proto.play;
  proto.play = function (...args) {
    track(this);
    if (gain !== 1) apply(this);
    return play.apply(this, args);
  };

  const setGain = (g) => {
    gain = g;
    for (const el of elements()) apply(el);
  };
  const forward = (target, fadeMs) => {
    for (let i = 0; i < window.frames.length; i++) {
      try {
        window.frames[i].postMessage({ [KEY]: { target, fadeMs } }, "*");
      } catch (_) {}
    }
  };
  // Fades by elapsed time, so throttled timers still end at the target.
  const set = (target, fadeMs = 0) => {
    target = Math.min(1, Math.max(0, Number(target)));
    forward(target, fadeMs);
    if (ramp && ramp.target === target) return target;
    if (ramp) clearInterval(ramp.timer);
    ramp = null;
    if (!(fadeMs > 0) || gain === target) {
      setGain(target);
      return gain;
    }
    const from = gain;
    const start = Date.now();
    ramp = {
      target,
      timer: setInterval(() => {
        const t = Math.min(1, (Date.now() - start) / fadeMs);
        setGain(t >= 1 ? target : from + (target - from) * t);
        if (t >= 1) {
          clearInterval(ramp.timer);
          ramp = null;
        }
      }, 20),
    };
    return target;
  };
  window.addEventListener("message", (e) => {
    const m = e.data && e.data[KEY];
    if (m && e.source === window.parent && window.parent !== window) set(m.target, m.fadeMs);
  });

  return {
    set,
    get gain() {
      return gain;
    },
    /// The volume an element actually plays at (for tests).
    actual: (el) => native.get.call(el),
  };
})();
