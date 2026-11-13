const { test } = require('node:test');
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const vm = require('node:vm');
const script = readFileSync(`${__dirname}/../src/cursor.js`, 'utf8');

function page(initialFullscreen = false) {
  let now = 0;
  let id = 0;
  const timers = new Map();
  const listeners = {};
  const styles = new Set();
  const target = (prefix) => ({
    addEventListener(type, fn) {
      (listeners[`${prefix}:${type}`] ??= []).push(fn);
    },
  });
  const media = { ...target('media'), matches: initialFullscreen };
  const document = {
    ...target('document'),
    fullscreenElement: null,
    documentElement: { appendChild: (style) => styles.add(style) },
    createElement: () => ({ remove() { styles.delete(this); } }),
  };
  const context = vm.createContext({
    window: target('window'), document, matchMedia: () => media,
    setTimeout(fn, delay) { timers.set(++id, { fn, at: now + delay }); return id; },
    clearTimeout: (id) => timers.delete(id),
  });
  const emit = (type, e = {}) => (listeners[type] ?? []).forEach(fn => fn(e));
  vm.runInContext(script, context);
  return {
    hidden: () => styles.size > 0,
    tick(ms) {
      now += ms;
      for (const [id, timer] of timers) {
        if (timer.at <= now) { timers.delete(id); timer.fn(); }
      }
    },
    fullscreen(on) { media.matches = on; emit('media:change'); },
    domFullscreen(on) {
      document.fullscreenElement = on ? {} : null;
      emit('document:fullscreenchange');
    },
    move(x, y) { emit('window:mousemove', { screenX: x, screenY: y }); },
    reinstall() { vm.runInContext(script, context); },
    timerCount: () => timers.size,
  };
}

test('windowed cursor never hides', () => {
  const p = page();
  p.move(1, 1);
  p.tick(60000);
  assert.equal(p.hidden(), false);
  assert.equal(p.timerCount(), 0);
});

test('fullscreen hides at ten seconds; movement restores and restarts the wait', () => {
  const p = page(true);
  p.tick(9999);
  assert.equal(p.hidden(), false);
  p.tick(1);
  assert.equal(p.hidden(), true);
  p.move(1, 1);
  assert.equal(p.hidden(), false);
  p.tick(9999);
  assert.equal(p.hidden(), false);
  p.tick(1);
  assert.equal(p.hidden(), true);
});

test('stationary synthetic movement does not postpone hiding', () => {
  const p = page(true);
  p.move(10, 20);
  p.tick(9000);
  p.move(10, 20);
  p.tick(1000);
  assert.equal(p.hidden(), true);
  p.move(11, 20);
  assert.equal(p.hidden(), false);
});

test('leaving fullscreen restores cursor and cancels pending hiding', () => {
  const p = page(true);
  p.tick(10000);
  p.fullscreen(false);
  assert.equal(p.hidden(), false);
  p.fullscreen(true);
  p.tick(9000);
  p.fullscreen(false);
  p.tick(10000);
  assert.equal(p.hidden(), false);
  p.fullscreen(true);
  p.tick(9999);
  assert.equal(p.hidden(), false);
  p.tick(1);
  assert.equal(p.hidden(), true);
});

test('DOM fullscreen also works', () => {
  const p = page();
  p.domFullscreen(true);
  p.tick(10000);
  assert.equal(p.hidden(), true);
  p.domFullscreen(false);
  assert.equal(p.hidden(), false);
});

test('repeated injection does not duplicate timers or listeners', () => {
  const p = page(true);
  p.reinstall();
  assert.equal(p.timerCount(), 1);
  p.move(1, 2);
  assert.equal(p.timerCount(), 1);
});
