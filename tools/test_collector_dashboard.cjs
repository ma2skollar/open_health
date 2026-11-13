// DOM regression checks: npm install --prefix /tmp/oura-dom-check jsdom@26
// NODE_PATH=/tmp/oura-dom-check/node_modules node tools/test_collector_dashboard.cjs
const assert = require('node:assert/strict');
const fs = require('node:fs');
const { JSDOM } = require('jsdom');
const html = fs.readFileSync('dashboard/web/index.html', 'utf8');
const source = fs.readFileSync('dashboard/web/app.js', 'utf8');

const hubStatus = state => ({
  state,
  models: {},
  ingest: {
    state, revision: 1, raw_events: 2, decoded_events: 1,
    processing: { state: 'idle', stage: 'complete', last_error: null, completed_revision: 1 },
    collectors: [],
  },
});

async function mount(status, summary) {
  const dom = new JSDOM(html, { url: 'http://localhost:8090', runScripts: 'outside-only' });
  const w = dom.window;
  // jsdom's document is "hidden" by default, which skips the status poll; keep it visible.
  Object.defineProperty(w.document, 'hidden', { configurable: true, get: () => false });
  w.matchMedia = () => ({ matches: false });
  w.TextDecoder = TextDecoder;
  w.fetch = async url => {
    const payload = url.includes('/api/status') ? status
      : url.includes('/api/manual/series') ? { calories: [], weight: [] }
      : url.includes('/api/manual/entries') ? { entries: [] }
      : summary;
    return { ok: true, text: async () => JSON.stringify(payload), json: async () => payload };
  };
  w.eval(source);
  // The status panel fills from the first /api/status poll; wait for it rather
  // than racing one setImmediate against a live fetch.
  const settle = Date.now() + 1500;
  while (Date.now() < settle && !w.document.querySelector('.at-state'))
    await new Promise(resolve => setTimeout(resolve, 25));
  await new Promise(resolve => setTimeout(resolve, 50));
  return [dom, w];
}

const text = (w, sel) => (w.document.querySelector(sel)?.textContent ?? '').replace(/\s+/g, ' ').trim();

(async () => {
  // ── A2: one distinct message per ingest state; no sync instruction anywhere ──
  const messages = {};
  for (const state of ['db_missing', 'db_inaccessible', 'no_upload', 'collector_ready',
                       'undecoded', 'raw_pending', 'analysis_failed', 'ready']) {
    const [dom, w] = await mount(
      hubStatus(state),
      { error: 'No ring data yet', tz: 0, nights: [], activity_daily: {}, activity_profile: {}, activity: [] },
    );
    const msg = text(w, '.at-state');
    assert(msg.includes(state === 'db_missing' ? 'missing'
      : state === 'db_inaccessible' ? 'cannot open'
      : state === 'no_upload' ? 'no collector batch'
      : state === 'collector_ready' ? 'no raw events'
      : state === 'undecoded' ? 'none are decoded'
      : state === 'raw_pending' ? 'queued'
      : state === 'analysis_failed' ? 'failed'
      : 'current'), `${state} message must say something distinct`);
    // No local-BLE control or instruction may survive on the LXC dashboard
    // (`clock_synchronized` is a heartbeat field, not a sync command).
    assert(!/oura sync|sync ring|sync-btn|run.*sync/i.test(w.document.body.textContent), `${state}: no local sync instruction`);
    messages[state] = msg;
    dom.window.close();
  }
  assert.equal(new Set(Object.values(messages)).size, 8, 'every state gets its own sentence');

  // ── A2: status panel renders even when the summary fetch fails hard ──
  {
    const [dom, w] = await mount(hubStatus('undecoded'), { error: 'No ring database' });
    assert(text(w, '#activity-tracking').includes('No dated activity coverage'), 'activity panel truthfully empty without a summary');
    assert(w.document.querySelector('.at-state'), 'status panel rendered without a summary');
    dom.window.close();
  }

  // ── A2/B6: collectors row: device, heartbeat age, build ──
  {
    const status = hubStatus('ready');
    status.ingest.collectors = [{ device_id: 'pi-ring', received_unix_ms: Date.now() - 60000,
      collector_build: 'collector 0.3', heartbeat: { ble_attempt_unix_ms: Date.now() - 60000, ble_success_unix_ms: null,
      upload_receipt_unix_ms: null, queue_count: 4, queue_bytes: 1048576, queue_oldest_unix_ms: null,
      disk_free_bytes: 2 * 1048576, clock_synchronized: true, error_codes: ['ble_timeout'] } }];
    const [dom, w] = await mount(status,
      { error: 'No ring data yet', tz: 0, nights: [], activity_daily: {}, activity_profile: {}, activity: [] });
    assert(text(w, '.collector-health .subhead').includes('pi-ring'), 'collector device_id shown');
    assert(text(w, '.collector-health').includes('1 min ago'), 'heartbeat age shown');
    assert(text(w, '.collector-health').includes('collector 0.3'), 'collector build shown');
    assert(text(w, '.collector-health').includes('ble_timeout'), 'bounded collector error code shown');
    assert(text(w, '#collector-status').includes('Processing'), 'processing block state shown');
    assert(text(w, '#collector-status').includes('complete'), 'processing stage shown');
    dom.window.close();
  }

  // ── A3/B6: provenance labels, null steps, no-zero-when-missing ──
  {
    const day = {
      date: '2026-11-12', steps: null, estimated_steps: 1200, steps_source: 'met_estimate',
      observed_minutes: 60, active_minutes: 10, sedentary_minutes: 20, unknown_minutes: 30,
      hours: [{ start_unix: 0, end_unix: 3600, label: '00:00', steps: null, estimated_steps: 100,
        steps_source: 'met_estimate', observed_minutes: 60, active_minutes: 5, sedentary_minutes: 5,
        unknown_minutes: 50 },
        { start_unix: 3600, end_unix: 7200, label: '01:00', steps: null, estimated_steps: 0,
        steps_source: 'blocked', observed_minutes: 10, active_minutes: 0, sedentary_minutes: null,
        unknown_minutes: 10 }],
    };
    const summary = { tz: 0, digest: 'ok', nights: [], activity_daily: {}, activity_profile: {}, activity: [],
      activity_tracking: { days: [day], source: 'summary',
        steps_blocker: '0x51/0x52 counts undecoded upstream; real_steps 0x0b feature off by default and unverified on the target ring' } };
    const [dom, w] = await mount(hubStatus('ready'), summary);
    const at = text(w, '#activity-tracking');
    assert(at.includes('2026-11-12'), 'day date shown');
    assert(at.includes('steps estimated from MET'), 'estimate provenance label shown');
    assert(at.includes('Estimated steps (MET)'), 'estimated steps labelled as MET-derived');
    assert(at.includes('1,200'), 'estimated step count rendered');
    assert(at.includes('—'), 'null steps rendered as em-dash, never zero');
    assert(at.includes('unobserved, not zero'), 'unknown minutes labelled unknown, not zero');
    assert(at.includes('00:00') && at.includes('01:00'), 'hourly distribution rows shown');
    // The shared steps_blocker note belongs to the blocked day below.
    assert(!at.includes('Steps for this day are blocked'), 'a met_estimate day gets no blocker note');
    assert(!/\b0 steps\b/.test(at), 'no fake zero steps');
    // steps_source blocked day → explicit blocker note
    const blockedSummary = { ...summary, activity_tracking: { ...summary.activity_tracking,
      days: [{ ...day, steps_source: 'blocked', sedentary_minutes: null, estimated_steps: 0 }] } };
    const [dom2, w2] = await mount(hubStatus('ready'), blockedSummary);
    const at2 = text(w2, '#activity-tracking');
    assert(at2.includes('no wear evidence'), 'null sedentary minutes mean no wear evidence, not zero');
    assert(at2.includes('Steps for this day are blocked'), 'blocked day gets explicit blocker note');
    assert(at2.includes('counts undecoded upstream'), 'steps_blocker text surfaces on the blocked day');
    // Both fixtures' poll timers must die with their windows, or the run hangs.
    dom.window.close();
    dom2.window.close();
  }

  // ── footer stays truthful: LAN-only processing, no third-party egress ──
  assert(html.includes('Analysis stays on that server; this dashboard uses no third-party services'),
    'footer privacy text unchanged');

  console.log('Collector dashboard DOM regression checks passed');
  // The app's 5 s status poll keeps scheduling timers past window.close(),
  // so end the run explicitly instead of draining an event loop that never ends.
  process.exit(0);
})().catch(error => { console.error(error); process.exit(1); });