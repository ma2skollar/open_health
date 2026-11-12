// DOM regression checks: npm install --prefix /tmp/oura-dom-check jsdom@26
// NODE_PATH=/tmp/oura-dom-check/node_modules node tools/test_manual_charts.cjs
const assert = require('node:assert/strict');
const fs = require('node:fs');
const { JSDOM } = require('jsdom');
const html = fs.readFileSync('dashboard/web/index.html', 'utf8');
const source = fs.readFileSync('dashboard/web/app.js', 'utf8');
const date = offset => {
  const d = new Date(2026, 10, 12 + offset, 12);
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
};
const points = (offsets, base) => offsets.map((offset, i) => ({ date: date(offset), value: base + i * 10 }));
const mean = pts => pts.slice(6).map((p, i) => ({ date: p.date, value: pts.slice(i, i + 7).reduce((s, p) => s + p.value, 0) / 7 }));
async function mount(calories, weight, unit = 'kg') {
  const dom = new JSDOM(html, { url: 'http://localhost:8090', runScripts: 'outside-only' });
  const w = dom.window;
  w.matchMedia = () => ({ matches: false });
  const NativeDate = w.Date;
  w.Date = class extends NativeDate {
    constructor(...args) { super(...(args.length ? args : [2026, 10, 12, 12])); }
  };
  w.TextDecoder = TextDecoder;
  const timers = new Map();
  let timerId = 0;
  const nativeSet = w.setTimeout.bind(w), nativeClear = w.clearTimeout.bind(w);
  w.setTimeout = (callback, delay, ...args) => {
    if (delay !== 10000) return nativeSet(callback, delay, ...args);
    const id = `inspection-${++timerId}`;
    timers.set(id, callback);
    return id;
  };
  w.clearTimeout = id => timers.has(id) ? timers.delete(id) : nativeClear(id);
  const series = { calories, weight, calories_trend: mean(calories), weight_trend: mean(weight), unit,
    calories_summary: calories.length ? { avg_28: 2000, avg_7: 2000 } : {},
    weight_summary: weight.length ? { avg_28: 75, avg_7: 75 } : {} };
  w.fetch = async url => {
    const payload = url.includes('/api/manual/series') ? series
      : url.includes('/api/manual/entries') ? { entries: [] } : { error: 'No ring database' };
    return { ok: true, text: async () => JSON.stringify(payload), json: async () => payload };
  };
  w.eval(source);
  await new Promise(resolve => setImmediate(resolve));
  const plots = [...w.document.querySelectorAll('.manual-plot')];
  assert.equal(plots.length, 2, 'both metrics remain inspectable, even when one has no data');
  plots.forEach(p => { p.getBoundingClientRect = () => ({ left: 0, width: 600 }); });
  const readouts = () => [...w.document.querySelectorAll('.manual-chart-readout')].map(p => p.textContent);
  const key = (plot, key) => plot.dispatchEvent(new w.KeyboardEvent('keydown', { key, bubbles: true, cancelable: true }));
  return { dom, w, plots, timers, readouts, key };
}
(async () => {
  const calories = points([-40, -34, -30, -28, -26, -24, -20, -10, -2], 1900);
  const weight = points([-60, -55, -50, -45, -40, -35, -30, -25, -3], 70);
  const t = await mount(calories, weight);
  const [cal, wt] = t.plots;
  assert(t.readouts().every(text => text.startsWith(date(0)) && text.includes('No entry')));
  assert(t.plots.every(p => !p.classList.contains('is-inspecting')), 'no cursor before inspection');
  assert.equal(cal.querySelectorAll('.manual-bars rect').length, 5, 'old history is clipped to four weeks');
  assert.equal(cal.querySelectorAll('.manual-missing path').length, 23, 'every missing calendar date is marked');
  t.key(cal, 'Home');
  assert(t.readouts().every(text => text.startsWith(date(-27))), 'keyboard selection shares exact calendar day');
  assert(t.readouts()[0].includes('No entry') && t.readouts()[0].includes('Inferred trend'));
  assert(t.readouts()[1].includes('7-entry trend'), 'pre-range history seeds a complete trend');
  assert(t.plots.every(p => !p.querySelector('.manual-selected-trend').hidden), 'trend begins on first visible date');
  assert.equal(cal.style.getPropertyValue('--manual-point-x'), wt.style.getPropertyValue('--manual-point-x'));
  assert(cal.querySelector('.manual-selected-dot').hidden, 'no fake zero-valued measurement');
  t.key(wt, 'End');
  assert(t.readouts().every(text => text.startsWith(date(0)) && !text.includes('trend')), 'no trailing extrapolation');
  cal.dispatchEvent(new t.w.MouseEvent('pointermove', { clientX: 300 }));
  assert(t.readouts().every(text => text.startsWith(date(-13))), 'pointer movement synchronizes the other chart');
  const oldTimer = [...t.timers.keys()][0];
  cal.dispatchEvent(new t.w.MouseEvent('pointermove', { clientX: 300 }));
  assert(!t.timers.has(oldTimer), 'movement within one date still renews inactivity deadline');
  assert.equal(t.timers.size, 1);
  [...t.timers.values()][0]();
  assert(t.plots.every(p => !p.classList.contains('is-inspecting')), 'both cursors expire after ten seconds');
  t.key(wt, 'ArrowLeft');
  assert(t.plots.every(p => p.classList.contains('is-inspecting')), 'input restores expired indicators');
  const svgMarks = [...wt.querySelector('svg').children];
  assert(svgMarks.findIndex(p => p.classList.contains('manual-trend')) > svgMarks.findLastIndex(p => p.classList.contains('manual-dot')), 'trend paints above observations');
  t.dom.window.close();

  const sparse = await mount([], points([-2], 165), 'lb');
  sparse.key(sparse.plots[1], 'Home');
  assert(sparse.readouts()[0].includes('No entry') && !sparse.readouts()[0].includes('trend'));
  assert(sparse.readouts()[1].includes('Inferred trend: 165.00 lb'), 'single reading seeds explicitly inferred leading trend');
  assert(sparse.plots[0].querySelector('.manual-selected-dot').hidden);
  assert(sparse.plots[0].querySelector('.manual-selected-trend').hidden);
  sparse.key(sparse.plots[0], 'End');
  sparse.key(sparse.plots[0], 'ArrowRight');
  assert(sparse.readouts().every(text => text.startsWith(date(0))), 'navigation clamps to today');
  sparse.dom.window.close();

  const partial = await mount([], points([-4, -2], 70));
  partial.key(partial.plots[1], 'End');
  partial.key(partial.plots[1], 'ArrowLeft');
  partial.key(partial.plots[1], 'ArrowLeft');
  assert(partial.readouts()[1].includes('Weight: 80.00 kg') && partial.readouts()[1].includes('Inferred trend: 75.00 kg'), 'partial means include each available observation');
  partial.key(partial.plots[1], 'ArrowLeft');
  assert(partial.readouts()[1].includes('No entry') && partial.readouts()[1].includes('Inferred trend: 72.50 kg'), 'missing dates linearly interpolate partial means');
  partial.dom.window.close();
  console.log('Manual chart regression checks passed');
})().catch(error => { console.error(error); process.exitCode = 1; });
