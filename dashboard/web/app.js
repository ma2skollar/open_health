// open_oura dashboard — fetch /api/summary (Rust computes it) and render.
//
// SIBLING CLIENT: the native iOS app (apps/ios/OuraApp/OuraApp.swift) renders the SAME
// summary JSON. A user-facing change here usually belongs there too — see the feature
// map in docs/clients-web-and-ios.md. New computed fields go in crates/oura-summary.
"use strict";

// ── theme ──────────────────────────────────────────────────────────────────
// Light and dark themes live on the <html> element's data-theme attribute. The
// default follows the operating system; a saved choice overrides it. Runs before
// the rest of the dashboard paints so there is no flash of the wrong theme.
(function themeToggle() {
  const btn = document.getElementById("theme-btn");
  if (!btn) return;
  const saved = localStorage.getItem("theme");
  const current = saved || (window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
  document.documentElement.dataset.theme = current;
  const updateIcon = () => {
    const name = document.documentElement.dataset.theme === "dark" ? "sun" : "moon";
    btn.querySelector(".ic").style.setProperty("--i", `url(/icons/${name}.svg)`);
  };
  updateIcon();
  btn.addEventListener("click", () => {
    const next = document.documentElement.dataset.theme === "dark" ? "light" : "dark";
    document.documentElement.dataset.theme = next;
    localStorage.setItem("theme", next);
    updateIcon();
  });
})();

const $ = (id) => document.getElementById(id);
const el = (tag, cls, html) => {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (html != null) n.innerHTML = html;
  return n;
};
const num = (v, d = "—") => (v == null || Number.isNaN(v) ? d : v);
const icon = (name, cls = "") => `<span class="ic ${cls}" style="--i:url(/icons/${name}.svg)"></span>`;
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
const cap = (s) => esc(s).replace(/^./, (c) => c.toUpperCase());
const kfmt = (n) => (n >= 1000 ? (n / 1000).toFixed(n >= 10000 ? 0 : 1) + "k" : String(Math.round(n)));

let CURRENT_PROFILE = null;
let CURRENT_TZ = 0; // hours from UTC, as the summary reports it
const browserTzOffset = () => {
  const off = -new Date().getTimezoneOffset() / 60;
  return Number.isFinite(off) ? off : 0;
};

// ── local dashboard fetch helpers ──────────────────────────────────────────
// Every mutating endpoint is gated by the X-Oura-Dash header; these centralize it
// (and the JSON POST envelope) so the call sites can't drift. They return the raw
// Response, so each caller keeps its own r.ok / body / error handling.
const DASH_HEADERS = { "X-Oura-Dash": "1" };
function postDash(url, body) {
  const opts = { method: "POST", headers: { ...DASH_HEADERS } };
  if (body !== undefined) {
    opts.headers["Content-Type"] = "application/json";
    opts.body = JSON.stringify(body);
  }
  return fetch(url, opts);
}
function getDash(url) {
  return fetch(url, { headers: { ...DASH_HEADERS } });
}
function deleteDash(url) {
  return fetch(url, { method: "DELETE", headers: { ...DASH_HEADERS } });
}

// Parse a typed decimal. `comma: true` also accepts "75,5" (iPhone decimal
// keypad). Exactly one separator, no grouping, signs, exponents, or non-finite
// input; anything else returns null so callers can reject it.
function parseDecimal(raw, { comma = false } = {}) {
  const s = String(raw ?? "").trim();
  const re = comma ? /^(\d+([.,]\d*)?|[.,]\d+)$/ : /^(\d+(\.\d*)?|\.\d+)$/;
  if (!re.test(s)) return null;
  const n = Number(s.replace(",", "."));
  return Number.isFinite(n) ? n : null;
}

// Read a dashboard response as JSON. A non-JSON body (proxy page, crash text)
// becomes a readable { error } instead of a raw JSON parse error. Real JSON
// errors from the backend pass through unchanged.
async function readJsonResponse(r) {
  const text = await r.text();
  let j = null;
  try { j = JSON.parse(text); } catch { j = null; }
  if (!j || typeof j !== "object") {
    return { ok: false, error: `The dashboard server replied HTTP ${r.status} without a JSON answer. Check that oura dashboard is running.` };
  }
  if (!r.ok) {
    j.ok = false;
    if (!j.error) j.error = `The dashboard server replied HTTP ${r.status}.`;
  }
  return j;
}

// Smooth curve THROUGH the points using a monotone cubic Hermite spline
// (Fritsch–Carlson). Monotone = the curve never overshoots past a data point, so it
// won't invent peaks/valleys the data doesn't have — the right call for real metrics.
function smoothPath(pts) {
  const n = pts.length;
  if (n < 2) return "";
  const x = pts.map((p) => p[0]), y = pts.map((p) => p[1]);
  if (n === 2) return `M${x[0].toFixed(1)} ${y[0].toFixed(1)} L${x[1].toFixed(1)} ${y[1].toFixed(1)}`;
  const dx = [], dy = [], dd = []; // secant slopes
  for (let i = 0; i < n - 1; i++) { dx[i] = x[i + 1] - x[i]; dy[i] = y[i + 1] - y[i]; dd[i] = dy[i] / dx[i]; }
  const m = new Array(n);
  m[0] = dd[0];
  m[n - 1] = dd[n - 2];
  for (let i = 1; i < n - 1; i++) m[i] = dd[i - 1] * dd[i] <= 0 ? 0 : (dd[i - 1] + dd[i]) / 2;
  for (let i = 0; i < n - 1; i++) {
    if (dd[i] === 0) { m[i] = 0; m[i + 1] = 0; continue; }
    const a = m[i] / dd[i], b = m[i + 1] / dd[i], s2 = a * a + b * b;
    if (s2 > 9) { const t = 3 / Math.sqrt(s2); m[i] = t * a * dd[i]; m[i + 1] = t * b * dd[i]; }
  }
  let d = `M${x[0].toFixed(1)} ${y[0].toFixed(1)}`;
  for (let i = 0; i < n - 1; i++) {
    const h = dx[i] / 3;
    d += ` C${(x[i] + h).toFixed(1)} ${(y[i] + m[i] * h).toFixed(1)} ` +
         `${(x[i + 1] - h).toFixed(1)} ${(y[i + 1] - m[i + 1] * h).toFixed(1)} ` +
         `${x[i + 1].toFixed(1)} ${y[i + 1].toFixed(1)}`;
  }
  return d;
}

// monochrome, thin, with a faint area fill — subtle and elegant
function sparkline(series) {
  const s = (series || []).filter((x) => x != null);
  if (s.length < 2) return "";
  const w = 100, h = 26, min = Math.min(...s), max = Math.max(...s);
  const rng = max - min || 1;
  const pts = s.map((v, i) => [(i / (s.length - 1)) * w, h - ((v - min) / rng) * (h - 5) - 3]);
  const d = smoothPath(pts);
  const area = `${d} L${w.toFixed(1)} ${h} L0 ${h} Z`;
  const last = pts[pts.length - 1];
  // the SVG is stretched non-uniformly (preserveAspectRatio=none), which would
  // squash an in-SVG <circle> into an ellipse — so the end dot is a separate,
  // unstretched element positioned at the last point (vertical axis is 1:1 with px).
  return `<svg viewBox="0 0 ${w} ${h}" preserveAspectRatio="none">
    <path d="${area}" fill="var(--spark-fill)" stroke="none"/>
    <path d="${d}" fill="none" stroke="var(--spark)" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" vector-effect="non-scaling-stroke"/>
  </svg><i class="spark-dot" style="top:${last[1].toFixed(1)}px"></i>`;
}

// a status pill (colored dot + label); kind ∈ ok | warn | neutral
function pill(label, kind) {
  const p = el("span", "pill " + kind);
  p.append(el("i"), document.createTextNode(label));
  return p;
}

// Normal / attention status from a delta% and which direction is healthy.
function statusFor(deltaPct, good) {
  if (deltaPct == null) return null;
  if (Math.abs(deltaPct) <= 5) return { label: "Normal", kind: "ok" };
  const improving = good === "up" ? deltaPct > 0 : deltaPct < 0;
  if (improving) return { label: good === "up" ? "High" : "Low", kind: "ok" };
  return { label: good === "up" ? "Low" : "Elevated", kind: "warn" };
}

// baseline comparison bar: current value as a fill, the reference (personal baseline
// or target) as a marker, on a shared 0…(max×1.3) scale — the "your result vs
// reference" pattern from clinical dashboards.
function cmpBar(value, ref, unit, refLabel = "baseline") {
  const hi = Math.max(value, ref) * 1.3 || 1;
  const wrap = el("div", "cmp");
  const track = el("div", "cmp-track");
  const fill = el("i", "cmp-fill");
  fill.style.width = Math.max(3, Math.min(100, (value / hi) * 100)) + "%";
  const mark = el("span", "cmp-mark");
  mark.style.left = Math.min(100, (ref / hi) * 100) + "%";
  track.append(fill, mark);
  const dRaw = Math.round((value - ref) * 10) / 10;
  const cap = el("div", "cmp-cap");
  cap.innerHTML = `${refLabel} <b>${num(Math.round(ref * 10) / 10)}</b>${unit} · ${dRaw >= 0 ? "+" : ""}${dRaw}${unit}`;
  wrap.append(track, cap);
  return wrap;
}

function metricCard(label, value, unit, opts = {}) {
  const { deltaPct, ref, refLabel, good = "up", status, sub } = opts;
  const t = el("article", "tile");
  const head = el("div", "tile-head");
  head.append(el("div", "label", label));
  const st = status !== undefined ? status : statusFor(deltaPct, good);
  if (st) head.append(pill(st.label, st.kind));
  t.append(head);
  t.append(el("div", "value", `${num(value)}<span class="unit">${unit || ""}</span>`));
  if (typeof value === "number" && ref != null) t.append(cmpBar(value, ref, unit || "", refLabel));
  else if (sub) t.append(el("div", "sub", sub));
  return t;
}

const relAge = (diff) => {
  const a = Math.abs(Math.round(diff * 10) / 10);
  if (diff < -0.05) return { short: `${a} yr younger`, long: `${a} ${a === 1 ? "year" : "years"} younger than` };
  if (diff > 0.05) return { short: `${a} yr older`, long: `${a} ${a === 1 ? "year" : "years"} older than` };
  return { short: "in line", long: "in line with" };
};

function renderTiles(d) {
  const box = $("tiles");
  box.innerHTML = "";
  box.classList.add("reveal");
  const hv = d.vitals?.hrv || {}, rh = d.vitals?.rhr || {};
  const n0 = (d.nights || [])[0] || {};
  box.append(metricCard("HRV (RMSSD)", hv.latest, " ms", { deltaPct: hv.delta_pct, ref: hv.baseline, good: "up" }));
  box.append(metricCard("Resting HR", rh.latest, " bpm", { deltaPct: rh.delta_pct, ref: rh.baseline, good: "down" }));
  const eff = n0.efficiency;
  box.append(metricCard("Sleep efficiency", eff, "%", {
    ref: 85, refLabel: "target", good: "up",
    status: eff == null ? null : eff >= 85 ? { label: "Normal", kind: "ok" } : eff >= 75 ? { label: "Fair", kind: "neutral" } : { label: "Low", kind: "warn" },
  }));
  const cv = d.cardio;
  if (cv && cv.vascular_age != null) {
    const diff = cv.vascular_age - cv.chronological_age;
    box.append(metricCard("Vascular age", cv.vascular_age, " yr", {
      ref: cv.chronological_age, refLabel: "your age", good: "down",
      status: diff < -0.5 ? { label: "Younger", kind: "ok" } : diff > 0.5 ? { label: "Older", kind: "warn" } : { label: "In line", kind: "neutral" },
    }));
  } else {
    box.append(metricCard("Vascular age", "—", "", { sub: cvaMissing(d).short }));
  }
}

function hypnogram(stages) {
  const wrap = el("div", "hyp");
  (stages || []).forEach((s) => wrap.append(el("i", "s" + s)));
  return wrap;
}

// ── the unified "day" (night + activity of the same date) ──────────────────
// A day is keyed by YYYY-MM-DD. The most recent one is the hero card; the rest
// live behind "Show all N days". This mirrors the iOS app's home + AllDaysView.

// The calendar date you WOKE from a night. Nights are labelled by onset date (the
// evening you went to bed), so an overnight sleep that crosses midnight belongs to
// the next day's "morning". Pairing a day with the sleep you woke from — not the
// sleep you started that evening — is what makes "night + activity of the day" read
// as one coherent day. Kept identical to the iOS Summary.wakeYmd.
function wakeYmd(n) {
  if (!n || !n.ymd) return null;
  if (n.wake_ymd) return n.wake_ymd;
  if (n.start && n.end && n.end < n.start) {
    const [y, m, dd] = n.ymd.split("-").map(Number);
    const t = new Date(y, m - 1, dd + 1);
    return `${t.getFullYear()}-${String(t.getMonth() + 1).padStart(2, "0")}-${String(t.getDate()).padStart(2, "0")}`;
  }
  return n.ymd;
}

// every date that has a night (by wake date), a movement profile, daily totals, or a
// session — newest first
function dayKeys(d) {
  const set = new Set();
  (d.nights || []).forEach((n) => { const w = wakeYmd(n); if (w) set.add(w); });
  Object.keys(d.activity_profile || {}).forEach((k) => set.add(k));
  Object.keys(d.activity_daily || {}).forEach((k) => set.add(k));
  (d.activity || []).forEach((s) => { const y = (s.start || "").split(" ")[0]; if (y) set.add(y); });
  return [...set].filter(Boolean).sort().reverse();
}

// the primary sleep you woke from on the morning of `ymd` — the longest in-bed night
// wins over same-morning naps. Falls back to a MM-DD match for older data lacking ymd.
function nightForDay(d, ymd) {
  const cands = (d.nights || []).filter((n) => wakeYmd(n) === ymd);
  if (cands.length) return cands.reduce((a, b) => ((b.in_bed_h || 0) > (a.in_bed_h || 0) ? b : a));
  return (d.nights || []).find((n) => n.ymd == null && (n.date || "").endsWith(ymd.slice(5))) || null;
}

// this day's sessions, start rewritten to minutes-past-midnight so openActDetail()
// (which formats s.start with hhmm()) renders them correctly.
function sessionsForDay(d, ymd) {
  return (d.activity || [])
    .filter((s) => (s.start || "").startsWith(ymd))
    .map((s) => {
      const [h, m] = ((s.start || "").split(" ")[1] || "0:0").split(":").map(Number);
      return { ...s, start: (h || 0) * 60 + (m || 0) };
    })
    .sort((a, b) => a.start - b.start);
}

const dayTitle = (ymd) => {
  const p = (ymd || "").split("-");
  if (p.length !== 3) return ymd;
  const dt = new Date(+p[0], +p[1] - 1, +p[2]);
  return `${WD[dt.getDay()]} · ${fmtDay(ymd)}`;
};

// continuous movement ridge (96 × 15-min MET buckets) as a filled SVG area
const RIDGE_H = 30;
function ridgeSvg(profile) {
  const prof = (profile || []).map((v) => v || 0);
  if (prof.length < 2) return "";
  const peak = Math.max(0.5, ...prof);
  const pts = prof.map((v, i) => [(i / (prof.length - 1)) * 100, RIDGE_H - Math.min(1, v / peak) * RIDGE_H]);
  const d = `${smoothPath(pts)} L100 ${RIDGE_H} L0 ${RIDGE_H} Z`;
  return `<svg class="day-ridge" viewBox="0 0 100 ${RIDGE_H}" preserveAspectRatio="none"><path d="${d}"/></svg>`;
}

// the combined day card: a clickable Sleep region (→ sleep detail) above a clickable
// Activity region (→ activity detail). Used as the hero on the home panel.
function dayCard(d, ymd) {
  const card = el("div", "day-card");
  card.append(el("div", "day-date", dayTitle(ymd)));

  const n = nightForDay(d, ymd);
  if (n) {
    const sp = el("button", "day-part");
    sp.type = "button";
    sp.append(el("div", "dp-head",
      `<span class="dp-tag">Sleep</span><span class="dp-meta">${esc(n.start || "—")}–${esc(n.end || "—")} · ${num(n.in_bed_h)}h</span><span class="dp-chev"></span>`));
    if (n.stages && n.stages.length) sp.append(hypnogram(n.stages));
    const comp = el("div", "breakdown");
    const seg = (l, v) => `<span>${l} <b>${num(v)}%</b></span>`;
    comp.innerHTML = seg("Deep", n.deep_pct) + seg("Light", n.light_pct) + seg("REM", n.rem_pct) + seg("Awake", n.wake_pct);
    sp.append(comp);
    sp.addEventListener("click", () => openDayPage(d, ymd, "sleep"));
    card.append(sp);
  }

  const ap = el("button", "day-part");
  ap.type = "button";
  const ds = (d.activity_daily || {})[ymd];
  const stat = ds ? `${ds.steps == null ? "—" : kfmt(ds.steps)} MET-estimated steps · ${ds.active_kcal == null ? "—" : Math.round(ds.active_kcal)} kcal` : "no activity totals";
  ap.append(el("div", "dp-head",
    `<span class="dp-tag">Activity</span><span class="dp-meta">${stat}</span><span class="dp-chev"></span>`));
  const prof = (d.activity_profile || {})[ymd];
  if (prof && prof.length > 1) ap.insertAdjacentHTML("beforeend", ridgeSvg(prof));
  const sessions = sessionsForDay(d, ymd);
  if (sessions.length) {
    const chips = el("div", "day-sessions");
    sessions.forEach((s) => {
      const chip = el("span", "day-chip" + (s.is_workout >= 0.5 ? " workout" : ""));
      const ico = el("span", "ic");
      ico.style.setProperty("--i", `url(/icons/${actIcon(s.label)}.svg)`);
      const nm = el("span", "day-chip-name");
      nm.textContent = s.label || "activity";
      chip.append(ico, nm);
      chips.append(chip);
    });
    ap.append(chips);
  }
  ap.addEventListener("click", () => openDayPage(d, ymd, "activity"));
  card.append(ap);

  // heart rate: the tab's 15-minute bars as a strip on the same 24 h as the ridge above
  const hp = el("button", "day-part");
  hp.type = "button";
  hp.append(el("div", "dp-head",
    `<span class="dp-tag">Heart rate</span><span class="dp-meta">loading…</span><span class="dp-chev"></span>`));
  const strip = el("div", "day-hr");
  hp.append(strip);
  hp.addEventListener("click", () => openDayPage(d, ymd, "heart"));
  card.append(hp);
  fillDayHr(ymd, hp.querySelector(".dp-meta"), strip);
  return card;
}

// The summary doesn't carry per-slot heart rate, so the "Your day" strip fetches it
// on its own and fills in when it arrives.
let HR_QUARTER_LOAD = null;
function fillDayHr(ymd, meta, strip) {
  const request = HR_QUARTER_LOAD ||= fetch(`/api/hourly-hr?minutes=15&tz=${browserTzOffset()}`).then((r) => r.json());
  request
    .then((j) => {
      if (j.error) throw new Error(j.error);
      const { bySlot, rows } = hrDaySlots(j, ymd);
      if (!rows.length) { meta.textContent = "no readings yet"; return; }
      const r0 = Math.round;
      const low = Math.min(...rows.map((r) => r.low)), high = Math.max(...rows.map((r) => r.high));
      const latest = hrLatestOn(j, ymd);
      meta.textContent = (latest != null ? `latest ${r0(latest)} bpm · ` : "") + `${r0(low)}–${r0(high)} bpm`;
      strip.innerHTML =
        `<div class="day-hr-plot">${hrBinsSvg(bySlot, Math.max(0, low - 5), high + 5, 1000, 44, false)}</div>` +
        `<div class="met-axis">${[0, 6, 12, 18, 24].map((h) => `<span style="left:${(h / 24 * 100).toFixed(1)}%">${String(h).padStart(2, "0")}</span>`).join("")}</div>`;
    })
    .catch(() => { meta.textContent = "couldn't load"; });
}

function renderDay(d) {
  const box = $("day");
  box.innerHTML = "";
  const days = dayKeys(d);
  // Nights the shared brain withheld because the ring clock could not be resolved
  // unambiguously (missing anchor, accelerated counter, or multi-reboot stall).
  const clockWarnings = d.clock?.warnings || [];
  const unanchored = (d.clock?.epochs || []).some((e) => !e.anchors);
  const canSyncFix = unanchored || (d.clock?.undated_nights || []).some((n) => n.recoverable);
  const clockNote = () => {
    if (!clockWarnings.length && !unanchored) return null;
    const note = el("div", "clock-note");
    note.append(el("div", "clock-note-title", "Ring clock"));
    for (const w of clockWarnings) note.append(el("p", null, esc(w)));
    if (unanchored && !clockWarnings.length) {
      note.append(el("p", null, "Part of the ring's history has no time anchor, so it cannot be placed on the calendar yet."));
      note.append(el("p", "clock-note-fix", "Waiting for a usable clock anchor from the collector. Ambiguous history stays undated."));
    }
    return note;
  };
  if (!days.length) {
    box.append(el("div", "error", canSyncFix
      ? "No dated days yet: the ring's clock hasn't been anchored for this data."
      : clockWarnings.length
        ? "No dated days yet: recorded nights could not be dated unambiguously."
        : "No dated records yet. Uploaded history needs usable clock anchors before it can appear here."));
    const note = clockNote();
    if (note) box.append(note);
    $("sleep-legend").hidden = true;
    return;
  }
  const top = days[0];
  $("sleep-legend").hidden = !(nightForDay(d, top) || {}).stages;
  box.append(dayCard(d, top));
  if (days.length > 1) {
    const btn = el("button", "more-toggle");
    btn.textContent = `Show all ${days.length} days`;
    btn.addEventListener("click", () => openDaysBrowser(d, days));
    box.append(btn);
  }
  const note = clockNote();
  if (note) box.append(note);
}

const debtDuration = (minutes) => {
  const m = Math.max(0, Math.round(minutes || 0));
  return m >= 60 ? `${Math.floor(m / 60)}h ${m % 60}m` : `${m}m`;
};
const debtStateCopy = (state) => ({
  high: "Your sleep debt is high right now. Prioritize several consistent nights with enough sleep.",
  moderate: "You’ve built up a moderate amount of sleep debt. A few longer nights can help you recover.",
  low: "You’re mostly meeting your sleep need, with a small amount left to recover.",
  none: "You’ve met your sleep need consistently over the past two weeks.",
}[state] || "You’ve met your sleep need consistently over the past two weeks.");

function sleepDebtSvg(sd, mode) {
  const values = (sd.days || []).map((x) => mode === "debt" ? x.cumulative_debt_min : x.total_sleep_min);
  const w = 900, h = 190, max = mode === "debt" ? 600 : Math.max(720, ...values.filter((x) => x != null));
  const x = (i) => i / Math.max(1, values.length - 1) * w;
  const y = (v) => h - Math.min(1, Math.max(0, v / max)) * h;
  let path = "", started = false;
  values.forEach((v, i) => {
    if (v == null) { started = false; return; }
    path += `${started ? " L" : "M"}${x(i).toFixed(1)} ${y(v).toFixed(1)}`; started = true;
  });
  const grid = [0.25, 0.5, 0.75].map((f) => `<line x1="0" y1="${h * (1-f)}" x2="${w}" y2="${h * (1-f)}"/>`).join("");
  const need = mode === "sleep" ? `<line class="sd-need" x1="0" y1="${y(sd.need_h * 60)}" x2="${w}" y2="${y(sd.need_h * 60)}"/>` : "";
  return `<svg class="sd-chart" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none"><g class="sd-grid">${grid}</g>${need}<path class="sd-line" d="${path}"/></svg>`;
}

function openSleepDebt(sd) {
  let dlg = $("sleep-debt-dialog");
  if (!dlg) {
    dlg = el("dialog", "dialog sleep-debt-dialog"); dlg.id = "sleep-debt-dialog";
    document.body.append(dlg);
    dlg.addEventListener("click", (e) => { if (e.target === dlg) dlg.close(); });
  }
  const valid = sd.valid;
  dlg.innerHTML = `<form method="dialog">
    <div class="sd-detail-head"><div><h3>Sleep debt</h3><div class="dialog-sub">Past ${sd.window_days || 14} days</div></div><button class="dd-close" aria-label="Close">×</button></div>
    ${valid ? `<div class="sd-value">${debtDuration(sd.debt_min)} <span>${esc(sd.state)}</span></div><p class="sd-copy">${debtStateCopy(sd.state)}</p>`
      : `<div class="sd-value small">Not enough data yet</div><p class="sd-copy">${sd.valid_days || 0} of 5 sleep days available within the past 2 weeks.</p>`}
    <div class="sd-tabs"><button type="button" data-mode="debt" class="active">Cumulative debt</button><button type="button" data-mode="sleep">Total sleep</button></div>
    <div class="sd-graph">${sleepDebtSvg(sd, "debt")}</div>
    <div class="sd-axis"><span>${esc((sd.days?.[0]?.date || "").slice(5))}</span><span>${esc((sd.days?.at(-1)?.date || "").slice(5))}</span></div>
    <p class="subhead">How it works</p><p class="sd-copy">Sleep debt estimates missed sleep over the past 14 days. Total sleep combines main sleep and naps, recent days carry more weight, and your sleep need (${debtDuration(sd.need_h * 60)}) is personalized from your typical sleep over the past 3 months, ignoring unusually short or long days.</p>
  </form>`;
  dlg.querySelectorAll(".sd-tabs button").forEach((button) => button.addEventListener("click", () => {
    dlg.querySelectorAll(".sd-tabs button").forEach((b) => b.classList.toggle("active", b === button));
    dlg.querySelector(".sd-graph").innerHTML = sleepDebtSvg(sd, button.dataset.mode);
  }));
  dlg.showModal();
}

function renderSleepDebt(d) {
  const box = $("sleep-debt"), sd = d.sleep_debt;
  box.innerHTML = "";
  if (!sd) { box.append(el("div", "error", "No sleep data yet.")); return; }
  const button = el("button", "sd-card"); button.type = "button";
  if (sd.valid) button.innerHTML = `<div class="sd-card-value">${debtDuration(sd.debt_min)} <span>${esc(sd.state)}</span></div><p>${debtStateCopy(sd.state)}</p><span class="sd-period">Past ${sd.window_days || 14} days · view details →</span>`;
  else button.innerHTML = `<div class="sd-card-value pending">${sd.valid_days || 0} of 5 days available</div><p>5 days of sleep data are needed within the past 2 weeks.</p><span class="sd-period">View details →</span>`;
  button.addEventListener("click", () => openSleepDebt(sd)); box.append(button);
}

// Symptom Radar: Oura's on-device illness-detection model. Traffic-light state from the
// calibrated decision + the biomarkers (breath / lowest HR / HRV / temp) that deviate
// from your personal baseline. See docs/algorithms/illness-detection.md.
const ILLNESS_COPY = {
  NO_SIGNS: "No signs of illness. Your biometrics are within your normal range.",
  MINOR_SIGNS: "Minor signs. A few biometrics are outside your usual range — worth an easy day.",
  MAJOR_SIGNS: "Major signs. Several biometrics are elevated — your body may be fighting something.",
};
const ILLNESS_LIGHT = { NO_SIGNS: "ok", MINOR_SIGNS: "warn", MAJOR_SIGNS: "alert" };
const BIOMARKER_LABEL = {
  AverageBreath: "Breathing rate", LowestHeartRate: "Lowest heart rate",
  AverageHrv: "HRV", TemperatureDeviation: "Body temperature",
};
const BIOMARKER_UNIT = {
  AverageBreath: " br/min", LowestHeartRate: " bpm", AverageHrv: " ms", TemperatureDeviation: "°C",
};

const cvaMissing = (d) => {
  const gated = (d.device?.insights || []).find((i) => i.name === "Cardiovascular age")?.status === "gated";
  return gated
    ? { short: "needs cva_ppg data", long: "Cardiovascular age needs recorded cva_ppg data. The server cannot change ring modes." }
    : { short: "needs the CVA model", long: "Cardiovascular age needs the CVA model runner (Python + tools/run_cva_model.py); the ring's PPG data is already there." };
};

function renderIllness(d) {
  const box = $("illness");
  box.innerHTML = "";
  // Oura's model when the Python runner produced it, otherwise the shared core's
  // model-free radar (same choice as the iOS app's `symptomRadar`).
  const fromModel = !!d.illness?.available;
  const ill = fromModel ? d.illness : (d.symptoms || d.illness);
  if (!ill) { box.append(el("div", "error", "Symptom radar needs a few nights of sleep data.")); return; }
  // The runner emits snake_case, the core camelCase.
  const trafficLight = ill.traffic_light ?? ill.trafficLight;
  const daysWithData = ill.days_with_data ?? ill.daysWithData;
  if (!ill.available) {
    const why = ill.status === "MISSING_LAST_NIGHT_SLEEP" ? "Last night's sleep is missing from uploaded history."
      : ill.status === "MISSING_SLEEP_DATA" ? (fromModel || d.illness
        ? "Too many recent nights are missing (needs ≥ 7 of the last 14)."
        : "Needs at least 7 nights of sleep to learn your baseline.")
      : "Not enough history yet.";
    box.append(el("div", "error", why));
    return;
  }
  const light = ILLNESS_LIGHT[trafficLight] || "ok";
  const head = el("div", `il-status il-${light}`);
  const label = { NO_SIGNS: "No signs", MINOR_SIGNS: "Minor signs", MAJOR_SIGNS: "Major signs" }[trafficLight] || "No result yet";
  head.innerHTML = `<span class="il-dot"></span><span class="il-label">${label}</span>`;
  box.append(head);
  const flagged = (ill.biomarkers || []).filter((b) => b.indicatesSymptoms);
  // The decision is the model's overall score, not a count of flags: a night can have
  // biometrics out of range and still not add up to strain.
  const copy = ill.status === "NO_SIGNS" && flagged.length
    ? `No signs of illness. ${flagged.length === 1 ? "One biometric is" : `${flagged.length} biometrics are`} outside your usual range, but together they don't match the pattern of your body fighting something.`
    : ILLNESS_COPY[ill.status] || "";
  box.append(el("p", "il-copy", copy));

  if (flagged.length) {
    const list = el("div", "il-biomarkers");
    for (const b of flagged) {
      const dir = b.reason === "ELEVATED" ? "↑ elevated" : "↓ decreased";
      const unit = BIOMARKER_UNIT[b.type] || "";
      list.append(el("div", `il-bm il-${b.reason === "ELEVATED" ? "up" : "down"}`,
        `<span class="il-bm-name">${BIOMARKER_LABEL[b.type] || b.type}</span>` +
        `<span class="il-bm-val">${b.value}${unit} <em>${dir}</em></span>`));
    }
    box.append(list);
  }
  const basis = fromModel ? `On-device illness model · ${daysWithData} of 30 days` : `Compared with your own baseline · ${daysWithData} nights`;
  box.append(el("div", "il-foot", `${basis} · ${esc(ill.date)}`));
}

function renderCardio(d) {
  const box = $("cardio");
  const cv = d.cardio;
  const vo2 = d.fitness?.vo2max;
  box.innerHTML = "";
  // VO₂max is model-free (from demographics), so it shows even without the CVA model.
  const vo2Kv = vo2 != null ? el("div", "kv", `<div class="k">VO₂max estimate</div><div class="v">${vo2} ml/kg/min</div>`) : null;
  if (!cv || cv.vascular_age == null) {
    box.append(el("div", "error", cvaMissing(d).long));
    if (vo2Kv) { const kvs = el("div", "kvs"); kvs.append(vo2Kv); box.append(kvs); }
    return;
  }
  box.append(el("div", "big-metric", `<span class="n">${cv.vascular_age}</span><span class="u">years vascular age</span>`));
  box.append(el("div", "sub", `${relAge(cv.vascular_age - cv.chronological_age).long} your age (${cv.chronological_age})`));
  const kvs = el("div", "kvs");
  kvs.append(el("div", "kv", `<div class="k">Pulse-wave velocity</div><div class="v">${cv.pwv_ms != null ? cv.pwv_ms + " m/s" : "—"}</div>`));
  kvs.append(el("div", "kv", `<div class="k">Segments analysed</div><div class="v">${num(cv.segments)}</div>`));
  if (vo2Kv) kvs.append(vo2Kv);
  box.append(kvs);
}

function renderSpo2(d) {
  const box = $("spo2");
  box.innerHTML = "";
  const n0 = (d.nights || []).find((n) => n.spo2_mean != null);
  if (!n0) {
    box.append(el("div", "error", "Blood oxygen needs the spo2 feature on overnight."));
    return;
  }
  // SpO2 gauge scale: clamp the reading into [SPO2_MIN, 100] and map to 0–100% fill.
  const SPO2_MIN = 85, SPO2_HEALTHY = 95;
  box.append(el("div", "big-metric", `<span class="n">${n0.spo2_mean}</span><span class="u">% avg, last night</span>`));
  box.append(el("div", "sub", "Calibrated from the ring's R-ratio (Oura's own curve)."));
  const pct = Math.max(0, Math.min(100, ((n0.spo2_mean - SPO2_MIN) / (100 - SPO2_MIN)) * 100));
  const g = el("div", "gauge");
  const fill = el("i");
  fill.style.width = pct + "%";
  g.append(fill);
  box.append(g);
  box.append(el("div", "scale", `<span>${SPO2_MIN}</span><span>healthy ≥ ${SPO2_HEALTHY}</span><span>100</span>`));
}

const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const fmtDay = (ymd) => {
  const p = (ymd || "").split("-");
  return p.length === 3 ? `${MONTHS[+p[1] - 1]} ${+p[2]}` : ymd;
};

const WD = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const hhmm = (min) => `${String((min / 60) | 0).padStart(2, "0")}:${String(min % 60).padStart(2, "0")}`;

// activity type → vendored glyph (keyword-matched so the ~40 AAD labels resolve to one
// of the clean vendored icons; unknowns fall back to a generic activity mark).
const ACT_ICON = [
  [/run/, "act-running"],
  [/walk|hik|nordic/, "act-walking"],
  [/cycl|bik/, "act-cycling"],
  [/swim|dive/, "act-swimming"],
  [/yoga|pilates|stretch|meditat/, "act-yoga"],
  [/strength|core|cross ?train|hiit|interval|fitness|elliptical|row|box|martial|climb/, "act-strength"],
];
const actIcon = (label) => {
  const l = (label || "").toLowerCase();
  for (const [re, icon] of ACT_ICON) if (re.test(l)) return icon;
  return "act-default";
};

// session detail popover (opened from a day's session row)
function openActDetail(s) {
  let dlg = $("act-dialog");
  if (!dlg) {
    dlg = el("dialog", "dialog act-dialog");
    dlg.id = "act-dialog";
    document.body.append(dlg);
    dlg.addEventListener("click", (e) => { if (e.target === dlg) dlg.close(); });
  }
  const work = s.is_workout >= 0.5;
  const conf = s.label_confidence != null ? Math.round(s.label_confidence * 100) + "%" : "—";
  const kv = (k, v) => `<div class="kv"><div class="k">${k}</div><div class="v">${v}</div></div>`;
  const top3 = (s.top3 || [])
    .map(([n, p]) => `<div class="t3"><span class="t3n">${cap(n)}</span><span class="t3bar"><i style="width:${Math.round(p * 100)}%"></i></span><span class="t3p">${Math.round(p * 100)}%</span></div>`)
    .join("");
  dlg.innerHTML =
    `<form method="dialog">
      <div class="ad-head">
        <span class="ic" style="--i:url(/icons/${actIcon(s.label)}.svg)"></span>
        <h3>${cap(s.label || "activity")}</h3>
        ${work ? '<span class="ad-tag">workout</span>' : ""}
      </div>
      <div class="ad-grid">
        ${kv("Time", `${hhmm(s.start)}–${s.end}`)}
        ${kv("Duration", `${s.duration_min} min`)}
        ${s.active_kcal != null ? kv("Active calories", `${Math.round(s.active_kcal).toLocaleString()} kcal`) : ""}
        ${kv("Confidence", conf)}
        ${kv("Workout", work ? "yes" : "no")}
      </div>
      <p class="subhead">Model's guesses</p>
      <div class="t3list">${top3 || '<div class="ad-muted">no alternates</div>'}</div>
      <p class="ad-foot">Oura automatic_activity_detection — best guess from MET / motion / HR / temp.</p>
      <div class="dialog-actions"><button class="btn-primary">Close</button></div>
    </form>`;
  dlg.showModal();
}

// ── full-page sleep & activity reports ─────────────────────────────────────
// Detail opens as a full page (not a modal): a scientific report with a stacked
// polysomnograph (hypnogram + aligned signal lanes sharing one night-time axis and a
// hover crosshair), clinical metrics, and interpretation. Activity gets its own page.

const STAGE = {
  1: { name: "Deep", cls: "deep", lvl: 3 },
  2: { name: "Light", cls: "light", lvl: 2 },
  3: { name: "REM", cls: "rem", lvl: 1 },
  4: { name: "Awake", cls: "wake", lvl: 0 },
};
const parseHM = (s) => { const [h, m] = (s || "0:0").split(":").map(Number); return (h || 0) * 60 + (m || 0); };
// the night's clock window unwrapped across midnight (end can exceed 1440)
function nightWin(n) {
  let a = parseHM(n.start), b = parseHM(n.end);
  if (b <= a) b += 1440;
  return { a, b, span: Math.max(1, b - a) };
}
const clockAt = (win, f) => {
  const t = Math.round(win.a + f * win.span) % 1440;
  return `${String(Math.floor(t / 60)).padStart(2, "0")}:${String(t % 60).padStart(2, "0")}`;
};

// full-page overlay control
function showPage(node) {
  const p = $("page");
  p.replaceChildren(node);
  p.hidden = false;
  p.scrollTop = 0;
  document.body.classList.add("page-open");
}
function closePage() {
  const p = $("page");
  p.hidden = true;
  p.replaceChildren();
  document.body.classList.remove("page-open");
  DAY_NAV = null;
}
let DAY_NAV = null; // the open day page's ← / → targets
document.addEventListener("keydown", (e) => {
  if ($("page").hidden) return;
  if (e.key === "Escape") closePage();
  else if (DAY_NAV && !e.altKey && !e.ctrlKey && !e.metaKey && !e.shiftKey) {
    const go = e.key === "ArrowLeft" ? DAY_NAV.older : e.key === "ArrowRight" ? DAY_NAV.newer : null;
    if (go) { e.preventDefault(); go(); }
  }
});

// ── day page: one scrolling page — Sleep, Activity, Heart rate — on one time axis ─
// Every chart plots against the same axis (dayAxis) and shares one cursor (dayCursor),
// so a vertical line marks the same moment in all of them. `focus` ("sleep" |
// "activity" | "heart") scrolls to that section on open.
function openDayPage(d, ymd, focus = null, keepScroll = false) {
  const wrap = el("div", "rpt");
  const head = el("div", "rpt-head");
  const back = el("button", "rpt-back", "‹ Back");
  back.type = "button";
  back.addEventListener("click", closePage);
  const axis = dayAxis(d, ymd);
  const exp = el("button", "rpt-back rpt-export", "Export JSON");
  exp.type = "button";
  exp.title = "Download this day's sleep, activity and heart-rate data as JSON";
  exp.addEventListener("click", () => exportDayJson(d, ymd, axis));
  // ‹ › step through the days that have data (dayKeys, newest first) at the same
  // scroll position — so one chart can be compared across days
  const days = dayKeys(d);
  const older = days.find((k) => k < ymd) || null;
  const newer = days.slice().reverse().find((k) => k > ymd) || null;
  const go = (k) => openDayPage(d, k, null, true);
  const step = (k, label, name, key) => {
    const b = el("button", "rpt-day", label);
    b.type = "button";
    b.title = k ? `${name} day: ${dayTitle(k)} (${key})` : `No ${name === "Previous" ? "earlier" : "later"} day`;
    b.setAttribute("aria-label", b.title);
    if (k) b.addEventListener("click", () => go(k));
    else b.disabled = true;
    return b;
  };
  const when = el("div", "rpt-when");
  when.append(step(older, "‹", "Previous", "←"), el("div", "rpt-title", dayTitle(ymd)), step(newer, "›", "Next", "→"));
  head.append(back, when, exp);
  const cursor = dayCursor(axis);
  const body = el("div", "rpt-body");
  body.append(sleepSection(d, ymd, axis, cursor), activitySection(d, ymd, axis, cursor), heartSection(ymd, axis, cursor));
  wrap.append(head, body);
  const top = $("page").scrollTop;
  showPage(wrap);
  if (keepScroll) $("page").scrollTop = top;
  else if (focus) wrap.querySelector(`#sec-${focus}`)?.scrollIntoView({ block: "start" });
  DAY_NAV = { older: older && (() => go(older)), newer: newer && (() => go(newer)) };
}

// The day page's shared time axis, in unix seconds: from the start of this day's night
// (floored to the hour) when it began the evening before, else local midnight, to the
// midnight that ends the day. `frac` maps a time onto it and `at` maps back.
function dayAxis(d, ymd) {
  const tz = d.tz || 0;
  const [y, mo, dd] = ymd.split("-").map(Number);
  const dayStart = Date.UTC(y, mo - 1, dd) / 1000 - tz * 3600;
  const n = nightForDay(d, ymd);
  const t0 = n && n.start_unix != null && n.start_unix < dayStart ? Math.floor(n.start_unix / 3600) * 3600 : dayStart;
  const t1 = dayStart + 86400;
  const pad = (v) => String(v).padStart(2, "0");
  return {
    t0, t1, dayStart,
    frac: (t) => (t - t0) / (t1 - t0),
    at: (f) => t0 + f * (t1 - t0),
    clock: (t) => {
      const s = (((Math.floor(t) + tz * 3600) % 86400) + 86400) % 86400;
      return `${pad(Math.floor(s / 3600))}:${pad(Math.floor((s % 3600) / 60))}`;
    },
    // 6-hourly ticks on the local clock, plus the axis start when it isn't one
    ticks() {
      const out = [];
      for (let t = Math.ceil(t0 / 3600) * 3600; t <= t1; t += 3600) {
        const h = ((((t + tz * 3600) / 3600) % 24) + 24) % 24;
        if (h % 6 === 0 || t === t0) out.push({ f: (t - t0) / (t1 - t0), label: t === t1 ? "24" : pad(h) });
      }
      return out;
    },
  };
}

// One cursor for the whole day page: hovering any timeline box draws the line at the
// same moment through every box, shows the clock in each box's pill, and swaps each
// row's summary for its value at that time. Boxes register as they render (heart rate
// arrives later and re-renders on the 15 min / 1 h switch); `onMove` hooks extra
// readouts, one per key.
function dayCursor(axis) {
  let boxes = [];
  const hooks = new Map();
  const show = (f) => {
    boxes = boxes.filter((b) => b.el.isConnected);
    const t = axis.at(f);
    for (const b of boxes) {
      b.line.hidden = b.pill.hidden = false;
      b.line.style.setProperty("--f", f);
      b.pill.style.setProperty("--f", f);
      b.pill.textContent = axis.clock(t);
      for (const r of b.rows) r.val.textContent = r.valueAt(t);
    }
    hooks.forEach((fn) => fn(t));
  };
  const hide = () => {
    for (const b of boxes) {
      b.line.hidden = b.pill.hidden = true;
      for (const r of b.rows) r.val.textContent = r.summary;
    }
    hooks.forEach((fn) => fn(null));
  };
  return {
    add(b) {
      boxes.push(b);
      const move = (e) => {
        const r = b.el.querySelector(".tl-plot").getBoundingClientRect();
        show(Math.max(0, Math.min(1, (e.clientX - r.left) / r.width)));
      };
      b.plots.addEventListener("pointermove", move);
      b.plots.addEventListener("pointerdown", move);
      b.plots.addEventListener("pointerleave", hide);
    },
    onMove(key, fn) { hooks.set(key, fn); },
  };
}

// A timeline box: labelled rows over the shared axis, faint 6-hour lines, an hour axis,
// and this box's share of the page cursor. Each row: { label, summary, html, height,
// valueAt(t), rticks } — `rticks` is HTML for the right-hand scale column.
function tlBox(axis, cursor, rows) {
  const box = el("div", "tl");
  const plots = el("div", "tl-plots");
  const ticks = axis.ticks();
  plots.insertAdjacentHTML("beforeend", ticks.map((k) => `<i class="tl-vline" style="--f:${k.f.toFixed(4)}"></i>`).join(""));
  const live = [];
  for (const R of rows) {
    const row = el("div", "tl-row");
    if (R.height) row.style.height = R.height + "px";
    const gut = el("div", "tl-gut");
    const val = el("div", "tl-val");
    val.textContent = R.summary || "";
    gut.append(el("div", "tl-label", esc(R.label)), val);
    const plot = el("div", "tl-plot", R.html);
    row.append(gut, plot, el("div", "tl-rgut", R.rticks || ""));
    plots.append(row);
    live.push({ val, summary: R.summary || "", valueAt: R.valueAt });
  }
  const line = el("div", "tl-cursor");
  const pill = el("div", "tl-pill");
  line.hidden = pill.hidden = true;
  plots.append(line, pill);
  const axisRow = el("div", "tl-axis", ticks.map((k) => `<span style="--f:${k.f.toFixed(4)}">${k.label}</span>`).join(""));
  box.append(plots, axisRow);
  cursor.add({ el: box, plots, line, pill, rows: live });
  return box;
}

const secHead = (title, extra) => {
  const h = el("div", "rpt-sec-head");
  h.append(el("h2", "", title));
  if (extra) h.append(extra);
  return h;
};
const statTile = (k, v) => `<div class="ss"><div class="ss-v">${v}</div><div class="ss-k">${k}</div></div>`;

// The whole day as a JSON download: header + the sleep, activity and heart-rate
// sections (iOS `DayExport` exports one tab's section in the same shapes).
function exportDayJson(d, ymd, axis) {
  const n = nightForDay(d, ymd);
  const debt = ((d.sleep_debt || {}).days || []).find((x) => x.date === ymd) || null;
  const hr = HOURLY_HR || {};
  const tzOff = d.tz ?? browserTzOffset();
  const tzName = (typeof Intl !== "undefined" && Intl.DateTimeFormat && Intl.DateTimeFormat().resolvedOptions().timeZone)
    || `UTC${tzOff >= 0 ? "+" : ""}${tzOff}`;
  const payload = {
    day: ymd, kind: "day", generated_at: new Date().toISOString(),
    timezone: tzName,
    tz_offset: tzOff,
    app_version: "web", profile: d.profile || null,
    sleep: n ? { night: n, sleep_debt: debt } : null,
    activity: {
      daily: (d.activity_daily || {})[ymd] || null,
      profile_met: (d.activity_profile || {})[ymd] || [],
      workouts: (Array.isArray(d.activity) ? d.activity : []).filter((w) => String(w.start || "").startsWith(ymd)),
    },
    heart_rate: { minutes: hr.minutes || 60, bins: (hr.bins || []).filter((b) => b.unix >= axis.t0 && b.unix < axis.t1) },
  };
  const blob = new Blob([JSON.stringify(payload, null, 2)], { type: "application/json" });
  const a = document.createElement("a");
  a.href = URL.createObjectURL(blob);
  a.download = `oura-${ymd}.json`;
  document.body.append(a); a.click(); a.remove();
  setTimeout(() => URL.revokeObjectURL(a.href), 1000);
}

const stageLegend = () => el("div", "legend rpt-legend",
  `<span><i class="sw deep"></i>Deep</span><span><i class="sw light"></i>Light</span>` +
  `<span><i class="sw rem"></i>REM</span><span><i class="sw wake"></i>Awake</span>`);

// stepped clinical hypnogram: y = stage level (Awake top → Deep bottom), colored runs
function hypnoSvg(stages, w, h, span = [0, 1]) {
  const n = stages.length;
  const padT = 8, plotH = h - 16;
  const yOf = (lvl) => padT + (lvl / 3) * plotH;
  const xOf = (i) => (span[0] + (i / Math.max(1, n)) * (span[1] - span[0])) * w;
  let grid = "";
  for (let l = 0; l < 4; l++) grid += `<line x1="${xOf(0).toFixed(1)}" y1="${yOf(l).toFixed(1)}" x2="${xOf(n).toFixed(1)}" y2="${yOf(l).toFixed(1)}" stroke="var(--line-soft)" stroke-width="0.5"/>`;
  let runs = "", conn = "", prevLvl = null, i = 0;
  while (i < n) {
    const code = stages[i]; let j = i;
    while (j < n && stages[j] === code) j++;
    if (!STAGE[code]) { prevLvl = null; i = j; continue; }
    const st = STAGE[code];
    const x1 = xOf(i), x2 = xOf(j), y = yOf(st.lvl);
    runs += `<line x1="${x1.toFixed(1)}" y1="${y.toFixed(1)}" x2="${x2.toFixed(1)}" y2="${y.toFixed(1)}" stroke="var(--${st.cls})" stroke-width="2.5" vector-effect="non-scaling-stroke"/>`;
    if (prevLvl !== null) conn += `<line x1="${x1.toFixed(1)}" y1="${yOf(prevLvl).toFixed(1)}" x2="${x1.toFixed(1)}" y2="${y.toFixed(1)}" stroke="var(--faint)" stroke-width="0.8" opacity="0.5" vector-effect="non-scaling-stroke"/>`;
    prevLvl = st.lvl; i = j;
  }
  return `<svg class="lane-svg" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none">${grid}${conn}${runs}</svg>`;
}

// smooth auto-scaled line + faint area + dashed mean, for one signal lane
function laneSvg(v, w, h, color, span = [0, 1], times = null) {
  if (times && times.length === v.length && v.length) {
    const min = Math.min(0, ...v), max = Math.max(min + 1, ...v);
    const mean = v.reduce((a, b) => a + b, 0) / v.length;
    const bars = v.map((value, i) => {
      const x = times[i] * w, y = 5 + (1 - (value - min) / (max - min)) * (h - 10);
      return `<line x1="${x}" x2="${x}" y1="${h - 5}" y2="${y}" stroke="${color}" stroke-width="1.5"/>`;
    }).join("");
    return { mean, min, max, svg: `<svg class="lane-svg" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none">${bars}</svg>` };
  }
  if (v.length < 2) return null;
  const min = Math.min(...v), max = Math.max(...v), rng = (max - min) || 1;
  const mean = v.reduce((a, b) => a + b, 0) / v.length;
  const pad = 5, y = (val) => pad + (1 - (val - min) / rng) * (h - 2 * pad);
  const x0 = span[0] * w, x1 = span[1] * w;
  const pts = v.map((val, i) => [x0 + (i / (v.length - 1)) * (x1 - x0), y(val)]);
  const line = smoothPath(pts), my = y(mean).toFixed(1);
  return {
    mean, min, max,
    svg: `<svg class="lane-svg" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none">` +
      `<path d="${line} L${x1} ${h} L${x0} ${h} Z" fill="${color}" opacity="0.09"/>` +
      `<line x1="${x0}" y1="${my}" x2="${x1}" y2="${my}" stroke="${color}" stroke-width="0.6" stroke-dasharray="3 3" opacity="0.45"/>` +
      `<path d="${line}" fill="none" stroke="${color}" stroke-width="1.4" vector-effect="non-scaling-stroke"/></svg>`,
  };
}

// A signal lane from time-true [unix, value] points (nights[].series_t) on the day axis.
// The line breaks wherever the ring recorded nothing for more than LANE_GAP_S, instead
// of being stretched over the gap.
const LANE_GAP_S = 15 * 60;
function timedLaneSvg(pts, axis, w, h, color) {
  if (pts.length < 2) return null;
  const vals = pts.map((p) => p[1]);
  const min = Math.min(...vals), max = Math.max(...vals), rng = (max - min) || 1;
  const mean = vals.reduce((a, b) => a + b, 0) / vals.length;
  const pad = 5, x = (t) => axis.frac(t) * w, y = (v) => pad + (1 - (v - min) / rng) * (h - 2 * pad);
  const segs = [[]];
  pts.forEach((p, i) => {
    if (i && p[0] - pts[i - 1][0] > LANE_GAP_S) segs.push([]);
    segs[segs.length - 1].push([x(p[0]), y(p[1])]);
  });
  let area = "", line = "";
  for (const s of segs) {
    if (s.length === 1) s.push([s[0][0] + 1.5, s[0][1]]); // a lone point still shows
    const d = smoothPath(s);
    line += d + " ";
    area += `${d} L${s[s.length - 1][0].toFixed(1)} ${h} L${s[0][0].toFixed(1)} ${h} Z `;
  }
  const my = y(mean).toFixed(1);
  return {
    mean,
    svg: `<svg class="lane-svg" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none">` +
      `<path d="${area}" fill="${color}" opacity="0.09"/>` +
      `<line x1="${x(pts[0][0]).toFixed(1)}" y1="${my}" x2="${x(pts[pts.length - 1][0]).toFixed(1)}" y2="${my}" stroke="${color}" stroke-width="0.6" stroke-dasharray="3 3" opacity="0.45"/>` +
      `<path d="${line}" fill="none" stroke="${color}" stroke-width="1.4" vector-effect="non-scaling-stroke"/></svg>`,
  };
}
// the point nearest `t` within `tol` seconds, else null
function nearestPoint(pts, t, tol) {
  let best = null;
  for (const p of pts) if (Math.abs(p[0] - t) <= tol && (!best || Math.abs(p[0] - t) < Math.abs(best[0] - t))) best = p;
  return best;
}

// horizontal stage-proportion bar (Deep/Light/REM/Awake)
function stageBar(n) {
  const bar = el("div", "stagebar");
  const parts = [["deep", n.deep_pct], ["light", n.light_pct], ["rem", n.rem_pct], ["wake", n.wake_pct]];
  bar.innerHTML = parts.map(([c, v]) => `<i class="sw-${c}" style="width:${Math.max(0, v || 0)}%" title="${Math.round(v || 0)}%"></i>`).join("");
  return bar;
}

// science-based read of the night → a few plain-language sentences + a sleep-debt note
function sleepInterpretation(d, n, m) {
  if (n.staging_complete === false) return el("p", "error", "Sleep analysis is incomplete; gaps cannot establish awake time or sleep onset.");
  const wrap = el("div", "interp");
  const out = [];
  if (n.efficiency != null)
    out.push(n.efficiency >= 85 ? `Sleep efficiency of ${n.efficiency}% is solid — little time awake once down.`
      : n.efficiency >= 75 ? `Efficiency ${n.efficiency}% is fair; some fragmentation kept you from deeper rest.`
      : `Efficiency ${n.efficiency}% is low — a lot of the night in bed wasn't spent asleep.`);
  if (n.deep_pct != null)
    out.push(n.deep_pct < 10 ? `Deep sleep was scarce (${n.deep_pct}%) — the physically-restorative stage; low deep often follows late meals, alcohol, or stress.`
      : `Deep sleep ${n.deep_pct}% (target ~13–23%), the physically-restorative stage.`);
  if (n.rem_pct != null && m.rem_latency_min != null)
    out.push(`REM was ${n.rem_pct}% with first REM ${Math.round(m.rem_latency_min)} min after onset (a short REM latency can signal REM pressure or sleep debt).`);
  if (m.waso_min != null && m.awakenings != null)
    out.push(`You spent ${Math.round(m.waso_min)} min awake across ${m.awakenings} awakening${m.awakenings === 1 ? "" : "s"} after first falling asleep.`);
  out.forEach((t) => wrap.append(el("p", "interp-p", t)));
  const sd = d.sleep_debt;
  if (sd && sd.valid && sd.debt_min != null) {
    const h = Math.floor(sd.debt_min / 60), mm = Math.round(sd.debt_min % 60);
    const note = el("div", "interp-debt");
    note.innerHTML = `<div class="id-v">${h}h ${mm}m</div><div class="id-k">accumulated sleep debt vs an ${sd.need_h} h nightly need` +
      `${sd.recent_shortfall_min > 0 ? ` · last night ${Math.round(sd.recent_shortfall_min)} min short` : ""}</div>`;
    wrap.append(note);
  }
  return wrap;
}

// Sleep: stats, then the overnight lanes on the night's slice of the day axis — shown
// with or without a hypnogram (stages need Oura's SleepNet model; the signals don't) —
// then the stage-derived architecture and interpretation when stages exist.
function sleepSection(d, ymd, axis, cursor) {
  const sec = el("section", "rpt-sec");
  sec.id = "sec-sleep";
  const n = nightForDay(d, ymd);
  const canEditBedtime = !!(n && n.start_ds != null && n.end_ds != null);
  const editToggle = canEditBedtime ? el("button", "bedtime-edit-btn", n.bedtime_manual ? "Bedtime · edited" : "Adjust bedtime") : null;
  if (editToggle) {
    editToggle.type = "button";
    editToggle.title = "Adjust bedtime and wake-up window for this night";
  }
  sec.append(secHead("Sleep", editToggle));
  if (!n) {
    sec.append(el("div", "ad-muted", "No sleep ended on this day in the ring's data."));
    return sec;
  }
  const m = n.metrics || {}, s = n.series || {};
  const staged = !!(n.stages_full && n.stages_full.length);
  const asleepH = m.asleep_min != null ? m.asleep_min / 60 : null;
  const strip = el("div", "stat-strip");
  strip.innerHTML =
    statTile("Time in bed", num(n.in_bed_h) + " h") +
    statTile("Asleep", asleepH != null ? asleepH.toFixed(1) + " h" : "—") +
    statTile("Efficiency", n.efficiency != null ? n.efficiency + "%" : "—") +
    statTile(n.bedtime_manual ? "Bedtime · edited" : "Bedtime", `${n.start}–${n.end}`);
  sec.append(strip);
  if (canEditBedtime) {
    const editor = el("form", "bedtime-editor");
    editor.hidden = true;
    const rawRange = n.raw_start && n.raw_end ? `${n.raw_start}–${n.raw_end}` : null;
    editor.innerHTML =
      `<label class="be-field"><span>Bedtime</span><input type="time" name="start" value="${esc(n.start || "23:00")}" required></label>` +
      `<span class="be-sep">→</span>` +
      `<label class="be-field"><span>Wake-up</span><input type="time" name="end" value="${esc(n.end || "07:00")}" required></label>` +
      `<div class="be-actions">` +
      `<button type="submit" class="be-save">Apply</button>` +
      (n.bedtime_manual ? `<button type="button" class="be-reset" title="Restore ring-detected window${rawRange ? ` (${rawRange})` : ""}">Reset</button>` : "") +
      `<button type="button" class="be-cancel">Cancel</button>` +
      `</div>`;
    const toggleEditor = () => {
      editor.hidden = !editor.hidden;
      editToggle.classList.toggle("active", !editor.hidden);
    };
    editToggle.addEventListener("click", toggleEditor);
    const bedTile = strip.lastElementChild;
    if (bedTile) {
      bedTile.classList.add("ss-interactive");
      bedTile.title = "Click to adjust bedtime and wake-up";
      bedTile.addEventListener("click", toggleEditor);
    }
    editor.querySelector(".be-cancel")?.addEventListener("click", () => {
      editor.hidden = true;
      editToggle.classList.remove("active");
    });
    const submitOverride = async (payload, okMsg) => {
      try {
        const res = await (await postDash("/api/bedtime", payload)).json();
        if (!res.ok) {
          toast(res.error || "Could not update bedtime.", "error");
          return;
        }
        toast(okMsg, "ok");
        const fresh = await (await fetch(`/api/summary?tz=${browserTzOffset()}`)).json();
        if (!fresh.error) {
          CURRENT_TZ = fresh.tz || 0;
          await load();
          openDayPage(fresh, ymd, null, true);
        }
      } catch (_) {
        toast("Couldn't reach the local server.", "error");
      }
    };
    editor.querySelector(".be-reset")?.addEventListener("click", () => {
      submitOverride(
        { raw_start_ds: n.raw_start_ds ?? n.start_ds, captured_unix: n.captured_unix ?? 0, reset: true },
        "Restored ring-detected sleep window."
      );
    });
    editor.addEventListener("submit", (e) => {
      e.preventDefault();
      const fd = new FormData(editor);
      const startHM = String(fd.get("start") || n.start);
      const endHM = String(fd.get("end") || n.end);
      let dStart = parseHM(startHM) - parseHM(n.start);
      if (dStart > 720) dStart -= 1440;
      if (dStart < -720) dStart += 1440;
      let dEnd = parseHM(endHM) - parseHM(n.end);
      if (dEnd > 720) dEnd -= 1440;
      if (dEnd < -720) dEnd += 1440;
      const newStartDs = Math.round(n.start_ds + dStart * 600);
      let newEndDs = Math.round(n.end_ds + dEnd * 600);
      if (newEndDs <= newStartDs) newEndDs += 1440 * 600;
      if (newEndDs - newStartDs < 30 * 600 || newEndDs - newStartDs > 20 * 36000) {
        toast("Sleep window must be between 30 minutes and 20 hours.", "error");
        return;
      }
      submitOverride(
        { raw_start_ds: n.raw_start_ds ?? n.start_ds, captured_unix: n.captured_unix ?? 0, start_ds: newStartDs, end_ds: newEndDs },
        `Updated sleep window (${startHM}–${endHM}).`
      );
    });
    sec.append(editor);
  }
  if (staged && n.staging_complete === false)
    sec.append(el("p", "error", "Incomplete sleep analysis. Gaps mean no data, not awake time; sleep metrics are withheld."));
  if (staged && n.staging_source === "signals") {
    const note = el("p", "hr-note",
      "Hypnogram from the local signal estimate (heart rate, HRV and motion), not Oura's sleep-staging model. " +
      "To use Oura's validated model instead, install Python + torch and place the model files under notes/models/ " +
      "(or set OURA_MODELS_DIR); see dashboard/README.md.");
    sec.append(note);
  }
  if (staged) sec.append(stageLegend());
  else sec.append(el("p", "hr-note", "Sleep stages are unavailable for this night. The lanes below are what the ring recorded overnight."));

  const W = 1000, a = axis.frac(n.start_unix), b = axis.frac(n.end_unix);
  const rows = [];
  if (staged && n.stages_5min && n.stages_5min.length) {
    // What the official Oura app draws: the same stages in 5-minute blocks, laid
    // from the first NSSA epoch (floor of bedtime start to the 30 s grid).
    const st5 = n.stages_5min, t0 = n.stages_t0_unix || n.start_unix;
    const a5 = axis.frac(t0), b5 = axis.frac(t0 + st5.length * 300);
    rows.push({
      label: "Stages · 5 min (Oura app)", summary: "", height: 96, html: hypnoSvg(st5, W, 92, [a5, b5]),
      valueAt: (t) => {
        const f = axis.frac(t);
        return f < a5 || f >= b5 ? "—" : (STAGE[st5[Math.floor(((f - a5) / Math.max(1e-9, b5 - a5)) * st5.length)]] || {}).name || "";
      },
    });
  }
  if (staged) {
    const st = n.stages_full;
    rows.push({
      label: "Stages", summary: "", height: 96, html: hypnoSvg(st, W, 92, [a, b]),
      valueAt: (t) => {
        const f = axis.frac(t);
        return f < a || f > b ? "—" : (STAGE[st[Math.round(((f - a) / Math.max(1e-9, b - a)) * (st.length - 1))]] || {}).name || "";
      },
    });
  }
  const timed = n.series_t; // real times; the flat `series` is only spread evenly
  const addLane = (key, label, unit, color, dp = 0, span = [0, 1]) => {
    if (timed) {
      const pts = timed[key] || [];
      const L = key === "motion"
        ? laneSvg(pts.map(p => p[1]), W, 50, color, [0, 1], pts.map(p => axis.frac(p[0])))
        : timedLaneSvg(pts, axis, W, 50, color);
      if (!L) return;
      const fmt = (x) => (dp ? x.toFixed(dp) : Math.round(x));
      rows.push({
        label, summary: `${fmt(L.mean)} ${unit}`, html: L.svg,
        valueAt: (t) => { const p = nearestPoint(pts, t, key === "motion" ? 30 : LANE_GAP_S / 2); return p ? `${fmt(p[1])} ${unit}` : "—"; },
      });
      return;
    }
    if (key === "motion") {
      if (Array.isArray(s.motion_time) && s.motion_time.length === (s.motion || []).length) {
        const L = laneSvg(s.motion, W, 50, color, [0, 1], s.motion_time.map(f => a + f * (b - a)));
        if (L) rows.push({label, summary: `${Math.round(L.mean)} ${unit}`, html: L.svg,
          valueAt: t => { const pts = s.motion_time.map((f,i) => [n.start_unix + f * (n.end_unix-n.start_unix), s.motion[i]]);
            const p = nearestPoint(pts,t,30); return p ? `${Math.round(p[1])} ${unit}` : "—"; }});
      }
      return;
    }
    const v = (s[key] || []).filter((x) => x != null);
    const sa = a + span[0] * (b - a), sb = a + span[1] * (b - a);
    const L = laneSvg(v, W, 50, color, [sa, sb]);
    if (!L) return;
    const fmt = (x) => (dp ? x.toFixed(dp) : Math.round(x));
    rows.push({
      label, summary: `${fmt(L.mean)} ${unit}`, html: L.svg,
      valueAt: (t) => {
        const f = axis.frac(t);
        return f < sa || f > sb ? "—" : `${fmt(v[Math.round(((f - sa) / Math.max(1e-9, sb - sa)) * (v.length - 1))])} ${unit}`;
      },
    });
  };
  addLane("hr", "Heart rate", "bpm", "var(--warn)");
  addLane("hrv", "HRV", "ms", "var(--accent)");
  addLane("spo2", "Blood O₂", "%", "var(--rem)");
  addLane("temp", "Skin temp", "°C", "var(--light)", 1, s.temp_span || [0, 1]);
  addLane("motion", "Motion", "s", "var(--faint)");
  if (rows.length) sec.append(el("p", "subhead", "Overnight"), tlBox(axis, cursor, rows));
  if (!staged) return sec;

  // architecture + clinical metrics
  sec.append(el("p", "subhead", "Sleep architecture"), stageBar(n));
  const mg = el("div", "metric-grid");
  const mins = (x) => (x != null ? Math.round(x) + " min" : "—");
  const mc = (k, v) => `<div class="mc"><div class="mc-v">${v}</div><div class="mc-k">${k}</div></div>`;
  mg.innerHTML =
    mc("Sleep onset", mins(m.sol_min)) +
    mc("REM latency", mins(m.rem_latency_min)) +
    mc("Awake (WASO)", mins(m.waso_min)) +
    mc("Awakenings", m.awakenings != null ? m.awakenings : "—") +
    mc("Sleep cycles", m.cycles != null ? m.cycles : "—") +
    mc("Fragmentation", m.frag_index != null ? m.frag_index + " /h" : "—");
  sec.append(mg);

  // autonomic recovery resolved by sleep stage. Deep-sleep HRV is the recovery-relevant
  // number; we deliberately don't show a single overnight HRV "slope" — nocturnal HRV is
  // stage-driven (deep ↑, REM ↓), so a slope mostly tracks stage order, not recovery.
  const au = n.autonomic;
  if (au && [au.hrv_deep, au.hrv_light, au.hrv_rem, au.hr_deep, au.hr_light, au.hr_rem].some((x) => x != null)) {
    sec.append(el("p", "subhead", "Autonomic recovery by stage"));
    const at = el("div", "metric-grid");
    const val = (x, u) => (x != null ? x + u : "—");
    at.innerHTML =
      mc("HRV · Deep", val(au.hrv_deep, " ms")) +
      mc("HRV · Light", val(au.hrv_light, " ms")) +
      mc("HRV · REM", val(au.hrv_rem, " ms")) +
      mc("HR · Deep", val(au.hr_deep, " bpm")) +
      mc("HR · Light", val(au.hr_light, " bpm")) +
      mc("HR · REM", val(au.hr_rem, " bpm"));
    sec.append(at);
  }
  sec.append(el("p", "subhead", "Interpretation"), sleepInterpretation(d, n, m));
  return sec;
}

// 15-minute MET-above-rest buckets across the day's own 24 h, on the day axis
function movementSvg(prof, axis, w, h) {
  const p = (prof || []).map((x) => x || 0);
  if (p.length < 2) return "";
  const x0 = axis.frac(axis.dayStart) * w, peak = Math.max(1, ...p), pad = 6;
  const pts = p.map((v, i) => [x0 + ((i + 0.5) / p.length) * (w - x0), pad + (1 - Math.min(1, v / peak)) * (h - 2 * pad)]);
  const line = smoothPath(pts);
  const [first, last] = [pts[0][0].toFixed(1), pts[pts.length - 1][0].toFixed(1)];
  return `<svg class="lane-svg" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none">` +
    `<path d="${line} L${last} ${h} L${first} ${h} Z" fill="var(--accent)" opacity="0.14"/>` +
    `<path d="${line}" fill="none" stroke="var(--accent)" stroke-width="1.4" vector-effect="non-scaling-stroke"/></svg>`;
}

function activitySection(d, ymd, axis, cursor) {
  const sec = el("section", "rpt-sec");
  sec.id = "sec-activity";
  sec.append(secHead("Activity"));
  const ds = (d.activity_daily || {})[ymd];
  const prof = (d.activity_profile || {})[ymd] || [];
  const strip = el("div", "stat-strip");
  strip.innerHTML =
    statTile("MET-estimated steps", ds?.steps == null ? "—" : Math.round(ds.steps).toLocaleString()) +
    statTile("Active energy", ds?.active_kcal == null ? "—" : Math.round(ds.active_kcal) + " kcal") +
    statTile("Total energy", ds?.total_kcal == null ? "—" : Math.round(ds.total_kcal) + " kcal") +
    statTile("Distance · derived from estimated steps", ds?.distance_m == null ? "—" : (ds.distance_m / 1000).toFixed(1) + " km");
  sec.append(strip);

  const svg = movementSvg(prof, axis, 1000, 90);
  if (svg) {
    const p = prof.map((v) => v || 0), bucketS = 86400 / p.length;
    const at = (t) => {
      const i = Math.floor((t - axis.dayStart) / bucketS);
      return i >= 0 && i < p.length ? `${p[i].toFixed(1)} MET` : "—";
    };
    sec.append(el("p", "subhead", "Movement"),
      tlBox(axis, cursor, [{ label: "Movement", summary: `peak ${Math.max(...p).toFixed(1)}`, height: 90, html: svg, valueAt: at }]));
  }

  // intensity-derived metrics (buckets are 15-min MET-above-rest)
  const bucketMin = 24 * 60 / (prof.length || 96);
  const activeMin = prof.filter((v) => (v || 0) >= 3).length * bucketMin;
  const lightMin = prof.filter((v) => (v || 0) >= 1.5 && (v || 0) < 3).length * bucketMin;
  const peakMet = prof.length ? Math.max(...prof.map((v) => v || 0)) : 0;
  const sessions = sessionsForDay(d, ymd);
  const mg = el("div", "metric-grid");
  const mc = (k, v) => `<div class="mc"><div class="mc-v">${v}</div><div class="mc-k">${k}</div></div>`;
  mg.innerHTML =
    mc("Active", Math.round(activeMin) + " min") +
    mc("Lightly active", Math.round(lightMin) + " min") +
    mc("Peak intensity", peakMet.toFixed(1) + " MET") +
    mc("Sessions", sessions.length);
  sec.append(mg);

  sec.append(el("p", "subhead", "Sessions"));
  if (sessions.length) {
    const list = el("div", "dd-sessions");
    sessions.forEach((sess) => {
      const row = el("button", "dd-session" + (sess.is_workout >= 0.5 ? " workout" : ""));
      row.type = "button";
      const ico = el("span", "ic");
      ico.style.setProperty("--i", `url(/icons/${actIcon(sess.label)}.svg)`);
      const nm = el("span", "dd-s-name"); nm.textContent = sess.label || "activity";
      const meta = el("span", "dd-s-meta"); meta.textContent = `${sess.duration_min} min · ${hhmm(sess.start)}`;
      row.append(ico, nm, meta);
      row.addEventListener("click", () => openActDetail(sess));
      list.append(row);
    });
    sec.append(list);
  } else {
    sec.append(el("div", "ad-muted", "No sessions detected this day."));
  }
  return sec;
}

// ── heart rate across the day ────────────────────────────────
// Mirror of iOS `HeartRate.swift`, finer: one bar per slot spanning its 5th–95th
// percentile band with a tick at the median, from GET /api/hourly-hr?minutes=
// (oura-summary::hourly_hr::hr_bins). 15-minute slots by default, hourly on the
// switch. Slots built from few values are drawn lighter: while you sleep the ring
// keeps only 5-minute averages, three per quarter hour. Empty slots had no readings.
let HOURLY_HR = null; // last /api/hourly-hr response, for the day's JSON export
let HR_BIN_MIN = 15;
let HR_SEQ = 0;
const HR_FEW_VALUES = 10; // below this a band is a handful of values, not a spread

// One day's slots from an /api/hourly-hr response: `bySlot[i]` is slot i's row (or
// null), for the tab chart and the "Your day" strip alike.
function hrDaySlots(j, ymd) {
  const m = j.minutes || 60;
  const slotOf = (r) => Math.floor((r.hour * 60 + (r.minute || 0)) / m);
  const bySlot = new Array(1440 / m).fill(null);
  for (const r of j.bins || []) if (r.ymd === ymd) bySlot[slotOf(r)] = r;
  return { m, slotOf, bySlot, rows: bySlot.filter(Boolean) };
}

// The newest reading's bpm when it falls on `ymd` (local clock), else null.
function hrLatestOn(j, ymd) {
  if (!j.latest) return null;
  const at = new Date((j.latest.unix + (j.tz_offset || 0) * 3600) * 1000).toISOString().slice(0, 10);
  return at === ymd ? j.latest.bpm : null;
}

function hrBinsSvg(bySlot, lo, hi, w, h, withGrid = true) {
  const y = (v) => h - ((v - lo) / (hi - lo)) * h;
  const col = w / bySlot.length;
  const bw = col * (bySlot.length > 24 ? 0.6 : 0.5);
  let grid = "";
  for (let v = Math.ceil(lo / 20) * 20; withGrid && v <= hi; v += 20)
    grid += `<line class="hr-grid" x1="0" x2="${w}" y1="${y(v).toFixed(1)}" y2="${y(v).toFixed(1)}"/>`;
  let bars = "";
  bySlot.forEach((b, i) => {
    if (!b) return;
    const x = i * col + (col - bw) / 2, thin = b.count < HR_FEW_VALUES ? " thin" : "";
    bars += `<rect class="hr-bar${thin}" x="${x.toFixed(2)}" y="${y(b.high).toFixed(1)}" width="${bw.toFixed(2)}" ` +
      `height="${Math.max(2, y(b.low) - y(b.high)).toFixed(1)}"/>` +
      `<line class="hr-med${thin}" x1="${(x - col * 0.08).toFixed(2)}" x2="${(x + bw + col * 0.08).toFixed(2)}" ` +
      `y1="${y(b.median).toFixed(1)}" y2="${y(b.median).toFixed(1)}"/>`;
  });
  return `<svg class="hr-svg" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none">${grid}` +
    `<rect class="hr-hover" x="0" y="0" width="${col.toFixed(2)}" height="${h}" style="display:none"/>${bars}</svg>`;
}


// the slots of an /api/hourly-hr response placed by time on the day axis
function hrTimelineSvg(bins, axis, minutes, lo, hi, w, h) {
  const y = (v) => h - ((v - lo) / (hi - lo)) * h;
  const slotW = ((minutes * 60) / (axis.t1 - axis.t0)) * w, bw = slotW * (minutes < 60 ? 0.6 : 0.5);
  let grid = "";
  for (let v = Math.ceil(lo / 20) * 20; v <= hi; v += 20)
    grid += `<line class="hr-grid" x1="0" x2="${w}" y1="${y(v).toFixed(1)}" y2="${y(v).toFixed(1)}"/>`;
  let bars = "";
  for (const b of bins) {
    const x = axis.frac(b.unix) * w + (slotW - bw) / 2, thin = b.count < HR_FEW_VALUES ? " thin" : "";
    bars += `<rect class="hr-bar${thin}" x="${x.toFixed(2)}" y="${y(b.high).toFixed(1)}" width="${bw.toFixed(2)}" ` +
      `height="${Math.max(2, y(b.low) - y(b.high)).toFixed(1)}"/>` +
      `<line class="hr-med${thin}" x1="${(x - slotW * 0.08).toFixed(2)}" x2="${(x + bw + slotW * 0.08).toFixed(2)}" ` +
      `y1="${y(b.median).toFixed(1)}" y2="${y(b.median).toFixed(1)}"/>`;
  }
  return `<svg class="hr-svg" viewBox="0 0 ${w} ${h}" preserveAspectRatio="none">${grid}${bars}</svg>`;
}

function heartSection(ymd, axis, cursor) {
  const sec = el("section", "rpt-sec");
  sec.id = "sec-heart";
  const sw = el("div", "hr-bin");
  sec.append(secHead("Heart rate", sw));
  const strip = el("div", "stat-strip");
  const chart = el("div");
  const readout = el("p", "hr-readout", "&nbsp;");
  sec.append(strip, chart, readout,
    el("p", "hr-note", "Each bar spans that slot's typical range (5th–95th percentile of its values); the tick is the median. " +
      "Lighter bars are built from only a few values: while you sleep the ring stores 5-minute averages rather than beats. " +
      "Empty slots had no readings."));
  const load = () => {
    const seq = ++HR_SEQ; // a slower earlier response must not overwrite a newer one
    sw.querySelectorAll("button").forEach((b) => b.classList.toggle("on", +b.dataset.min === HR_BIN_MIN));
    chart.replaceChildren(el("div", "skeleton skeleton-block"));
    fetch(`/api/hourly-hr?minutes=${HR_BIN_MIN}&tz=${browserTzOffset()}`)
      .then((r) => r.json())
      .then((j) => {
        if (seq !== HR_SEQ) return;
        if (j.error) throw new Error(j.error);
        HOURLY_HR = j;
        renderHeart(j, ymd, axis, cursor, strip, chart, readout);
      })
      .catch((e) => {
        if (seq !== HR_SEQ) return;
        chart.replaceChildren(el("p", "hr-empty", "Couldn't load heart rate: " + esc(e.message)));
        readout.innerHTML = "&nbsp;";
      });
  };
  for (const [min, label] of [[15, "15 min"], [60, "1 h"]]) {
    const b = el("button", "", label);
    b.type = "button";
    b.dataset.min = min;
    b.addEventListener("click", () => { if (HR_BIN_MIN !== min) { HR_BIN_MIN = min; load(); } });
    sw.append(b);
  }
  load();
  return sec;
}

function renderHeart(j, ymd, axis, cursor, strip, chart, readout) {
  const span = (j.minutes || 60) * 60;
  const bins = (j.bins || []).filter((b) => b.unix >= axis.t0 && b.unix < axis.t1);
  const day = bins.filter((b) => b.ymd === ymd); // the stats cover the day itself
  if (!bins.length) {
    strip.innerHTML = "";
    chart.replaceChildren(el("p", "hr-empty", "No historical heart-rate readings for this day."));
    readout.innerHTML = "&nbsp;";
    cursor.onMove("heart", () => {});
    return;
  }
  const r0 = Math.round;
  const statRows = day.length ? day : bins;
  const low = Math.min(...statRows.map((r) => r.low)), high = Math.max(...statRows.map((r) => r.high));
  const calm = statRows.reduce((a, b) => (b.median < a.median ? b : a));
  const latest = hrLatestOn(j, ymd);
  strip.innerHTML =
    (latest != null ? statTile("Latest", `${r0(latest)} bpm`) : "") +
    statTile("Range", `${r0(low)}–${r0(high)} bpm`) +
    statTile(`Lowest · ${axis.clock(calm.unix)}`, `${r0(calm.median)} bpm`) +
    statTile("Hours measured", `${new Set(statRows.map((r) => `${r.ymd} ${r.hour}`)).size} of 24`);

  const allLow = Math.min(...bins.map((r) => r.low)), allHigh = Math.max(...bins.map((r) => r.high));
  const lo = Math.max(0, Math.floor(allLow / 10) * 10 - 10), hi = Math.ceil(allHigh / 10) * 10 + 10;
  const H = 140, rticks = [];
  for (let v = Math.ceil(lo / 20) * 20; v <= hi; v += 20)
    rticks.push(`<span style="top:${(((hi - v) / (hi - lo)) * 100).toFixed(1)}%">${v}</span>`);
  const sorted = statRows.map((r) => r.median).sort((a, b) => a - b);
  const at = (t) => bins.find((b) => t >= b.unix && t < b.unix + span);
  chart.replaceChildren(tlBox(axis, cursor, [{
    label: "Heart rate", summary: `${r0(sorted[Math.floor(sorted.length / 2)])} bpm`, height: H,
    html: hrTimelineSvg(bins, axis, span / 60, lo, hi, 1000, H), rticks: rticks.join(""),
    valueAt: (t) => { const b = at(t); return b ? `${r0(b.median)} bpm` : "—"; },
  }]));

  // the detailed slot line under the chart follows the page cursor from any chart
  const plural = (n, word) => `${n.toLocaleString()} ${word}${n === 1 ? "" : "s"}`;
  const madeOf = (b) => [b.beats ? plural(b.beats, "beat") : "", b.averages ? plural(b.averages, "five-minute average") : ""]
    .filter(Boolean).join(" + ");
  const idle = "Move over any chart to see that moment across sleep, activity and heart rate.";
  readout.textContent = idle;
  cursor.onMove("heart", (t) => {
    if (t == null) { readout.textContent = idle; return; }
    const b = at(t), s0 = b ? b.unix : Math.floor(t / span) * span;
    readout.textContent = `${axis.clock(s0)}–${axis.clock(s0 + span)} · ` + (!b ? "no readings" : b.beats
      ? `median ${r0(b.median)} bpm · typical ${r0(b.low)}–${r0(b.high)} · min ${r0(b.min)} · max ${r0(b.max)} · ${madeOf(b)}`
      : `median ${r0(b.median)} bpm · range ${r0(b.min)}–${r0(b.max)} · ${madeOf(b)}`);
  });
}

// the "previous days" page: every day as a row (date, mini-hypnogram, totals) that
// opens its full-page report. Uses its own dialog id.
function openDaysBrowser(d, days) {
  let dlg = $("days-dialog");
  if (!dlg) {
    dlg = el("dialog", "dialog day-dialog");
    dlg.id = "days-dialog";
    dlg.addEventListener("click", (e) => { if (e.target === dlg) dlg.close(); });
    document.body.append(dlg);
  }
  const form = el("form");
  form.method = "dialog";
  const head = el("div", "dd-head");
  const h = el("h3");
  h.textContent = `All ${days.length} days`;
  const close = el("button", "dd-close", "✕");
  close.type = "button";
  close.setAttribute("aria-label", "Close");
  close.addEventListener("click", () => dlg.close());
  head.append(h, close);
  form.append(head);

  const list = el("div", "daylist");
  days.forEach((ymd) => {
    const n = nightForDay(d, ymd);
    const ds = (d.activity_daily || {})[ymd];
    const row = el("button", "daylist-row");
    row.type = "button";
    const left = el("div", "dl-left");
    left.append(el("div", "dl-date", dayTitle(ymd)));
    left.append(el("div", "dl-sub", ds ? `${ds.steps == null ? "—" : kfmt(ds.steps)} MET-estimated steps · ${ds.active_kcal == null ? "—" : Math.round(ds.active_kcal)} kcal` : (n ? "sleep only" : "—")));
    row.append(left);
    if (n && n.stages && n.stages.length) {
      const hyp = hypnogram(n.stages);
      hyp.classList.add("dl-hyp");
      row.append(hyp);
    }
    row.append(el("span", "dp-chev"));
    row.addEventListener("click", () => { dlg.close(); openDayPage(d, ymd, "sleep"); });
    list.append(row);
  });
  form.append(list);
  dlg.replaceChildren(form);
  dlg.showModal();
}

// capability → glyph (mix of vendored phosphor + hugeicons)
const CAP_ICON = {
  "Daytime HR": "heartbeat", "SpO2": "wind", "Exercise HR": "person-simple-run",
  "Real steps": "act-walking", "Cardio PPG (CVA)": "heartbeat",
};
const capIcon = (name) => CAP_ICON[name] || "cpu";


// The ring's own battery readings (device.battery_history, [unix, percent]) over the
// days they span. Stretches where the level rises are it charging, which the readings
// show on their own — the event's two flag bytes aren't decoded, so nothing here
// depends on guessing what they mean.
function batteryChart(pts) {
  if (pts.length < 2) return null;
  const wrap = el("div", "batt-chart");
  const head = el("div", "batt-head");
  head.append(el("p", "subhead", "Battery"));
  const readout = el("span", "batt-readout");
  head.append(readout);
  wrap.append(head);

  const W = 100, H = 40, t0 = pts[0][0], t1 = pts[pts.length - 1][0];
  const span = Math.max(1, t1 - t0);
  // A full battery is the top of the scale, but the bottom follows the data: a ring
  // that has only been 80-100% draws as a flat line against a 0-100 axis. Both ends
  // are labelled so a zoomed axis can't read as a bigger swing than it is.
  const lo = Math.max(0, Math.floor(Math.min(...pts.map((p) => p[1])) / 10) * 10 - 5), hi = 100;
  const x = (t) => ((t - t0) / span) * W, y = (p) => H - ((p - lo) / (hi - lo)) * H;
  // one path per run of the same direction, so charging can be drawn differently
  const segs = [];
  for (let i = 1; i < pts.length; i++) {
    const up = pts[i][1] > pts[i - 1][1];
    if (!segs.length || segs[segs.length - 1].up !== up) segs.push({ up, pts: [pts[i - 1]] });
    segs[segs.length - 1].pts.push(pts[i]);
  }
  const line = (s) => `<path class="batt-line${s.up ? " up" : ""}" d="${s.pts.map((p, i) => `${i ? "L" : "M"}${x(p[0]).toFixed(2)} ${y(p[1]).toFixed(2)}`).join(" ")}"/>`;
  let grid = "";
  for (let v = Math.ceil((lo + 1) / 10) * 10; v < hi; v += 10)
    grid += `<line class="batt-grid" x1="0" x2="${W}" y1="${y(v).toFixed(2)}" y2="${y(v).toFixed(2)}"/>`;
  const plot = el("div", "batt-plot",
    `<svg viewBox="0 0 ${W} ${H}" preserveAspectRatio="none">${grid}${segs.map(line).join("")}` +
    `<line class="batt-cursor" x1="0" x2="0" y1="0" y2="${H}" style="display:none"/></svg>` +
    `<i class="batt-max">${hi}%</i><i class="batt-min">${lo}%</i>`);
  wrap.append(plot);

  const days = Math.max(1, Math.round((t1 - t0) / 86400));
  const foot = el("div", "batt-foot");
  foot.append(el("span", "", `${dayTitle(ymdOf(t0, d0TZ()))} → ${hhmmOf(t1)}`), el("span", "", `${days} day${days === 1 ? "" : "s"}`));
  wrap.append(foot);

  const cursor = plot.querySelector(".batt-cursor");
  const at = (e) => {
    const r = plot.getBoundingClientRect();
    const t = t0 + Math.max(0, Math.min(1, (e.clientX - r.left) / r.width)) * span;
    const p = pts.reduce((a, b) => (Math.abs(b[0] - t) < Math.abs(a[0] - t) ? b : a));
    cursor.setAttribute("x1", x(p[0]).toFixed(2));
    cursor.setAttribute("x2", x(p[0]).toFixed(2));
    cursor.style.display = "";
    readout.textContent = `${hhmmOf(p[0])} · ${Math.round(p[1])}%`;
  };
  plot.addEventListener("pointermove", at);
  plot.addEventListener("pointerdown", at);
  plot.addEventListener("pointerleave", () => { cursor.style.display = "none"; readout.textContent = ""; });
  return wrap;
}
// local-clock helpers for the battery chart's labels (the summary's tz offset)
const d0TZ = () => (CURRENT_TZ || 0);
const ymdOf = (unix, tz) => new Date((unix + tz * 3600) * 1000).toISOString().slice(0, 10);
const hhmmOf = (unix) => {
  const s = new Date((unix + d0TZ() * 3600) * 1000).toISOString();
  return `${s.slice(5, 10).replace("-", "/")} ${s.slice(11, 16)}`;
};

// "Recorded <date> <hm> (Xd/Xh/<1h ago)" for a battery reading, or "" if unknown.
// Shared by the device card and the toolbar battery pill so the two cannot drift.
function batteryRecordedLabel(dev) {
  if (!dev.battery_recorded) return "";
  let rec = `Recorded ${dev.battery_recorded}${dev.battery_recorded_hm ? " " + dev.battery_recorded_hm : ""}`;
  const h = dev.battery_fresh_hours;
  if (h != null) {
    if (h >= 24) rec += ` (${Math.round(h / 24)}d ago)`;
    else if (h >= 1) rec += ` (${Math.round(h)}h ago)`;
    else rec += ` (<1h ago)`;
  }
  return rec;
}

function renderDevice(d) {
  const box = $("device");
  const dev = d.device || {};
  box.innerHTML = "";

  const stats = el("div", "dh-stats");
  const stat = (k, v, u) => el("div", "dh-stat", `<div class="k">${k}</div><div class="v">${v}<span class="u">${u || ""}</span></div>`);
  const bpct = dev.battery_pct;
  const bstat = stat("Battery", bpct != null ? bpct : "—", "%");
  if (bpct != null && bpct < 20) bstat.classList.add("low");
  if (dev.battery_recorded) {
    bstat.title = `Battery: ${bpct != null ? bpct + "%" : "—"}${dev.battery_v ? " (" + dev.battery_v + " V)" : ""}\n${batteryRecordedLabel(dev)}`;
  }
  stats.append(bstat);
  const fresh = dev.fresh_hours != null ? (dev.fresh_hours < 1 ? "<1" : Math.round(dev.fresh_hours)) : "—";
  stats.append(stat("Last recorded download", fresh, " h ago"));
  stats.append(stat("History", num(dev.days_of_data), " days"));
  stats.append(stat("Events", num(dev.raw_events ?? dev.total_events).toLocaleString()));
  box.append(stats);
  const battery = batteryChart(dev.battery_history || []);
  if (battery) box.append(battery);

  // left = data streams (what the ring is recording)
  const left = el("div");
  const streams = dev.streams || [];
  if (streams.length) {
    left.append(el("p", "subhead", "Data captured"));
    const max = Math.max(...streams.map((s) => s.count), 1);
    const sc = el("div", "streams");
    streams.forEach((s) => {
      const row = el("div", "stream");
      const nm = el("span", "s-name");
      nm.textContent = s.name;
      const bar = el("span", "s-bar");
      const fill = el("i");
      fill.style.width = Math.max(3, (s.count / max) * 100) + "%";
      bar.append(fill);
      const val = el("span", "s-val");
      val.textContent = s.count.toLocaleString();
      row.append(nm, bar, val);
      sc.append(row);
    });
    left.append(sc);
  }
  box.append(left);

  // right = insights
  const right = el("div");
  right.append(el("p", "subhead", "Insights available"));
  const ins = el("div", "insights");
  (dev.insights || []).forEach((i) => {
    const row = el("div", "insight");
    row.append(el("div", null, `${i.name}${i.status === "gated" && i.why ? `<span class="why"> · ${i.why}</span>` : ""}`));
    const st = el("span", "status " + i.status);
    st.innerHTML = `<i></i>${i.status}`;
    row.append(st);
    ins.append(row);
  });
  right.append(ins);
  box.append(right);

  // ── advanced / debugging (collapsed by default) ──────────────────────────
  const adv = el("details", "dh-advanced");
  const sum = el("summary");
  sum.innerHTML = `<span class="ic" style="--i:url(/icons/cpu.svg)"></span>Advanced &amp; debugging<span class="chev"></span>`;
  adv.append(sum);
  const ab = el("div", "adv-body");

  // device identity + sync internals
  ab.append(el("p", "subhead", "Device"));
  const kv = el("div", "adv-kv");
  const kvItem = (k, v) => `<div><i>${k}</i><b>${v}</b></div>`;
  const rawTotal = dev.raw_events ?? dev.total_events;
  const decodedTotal = dev.decoded_events ?? dev.total_events;
  const historyDetail = `${num(dev.days_of_data)} days elapsed` +
    (dev.observed_days != null ? ` · ${dev.observed_days} days observed` : "") +
    (dev.observed_coverage_days != null ? ` (${num(dev.observed_coverage_days)} d active)` : "");
  kv.innerHTML =
    kvItem("Ring ID", esc(dev.serial || "—")) +
    kvItem("Firmware", esc(dev.firmware || "—")) +
    kvItem("API", esc(dev.api_version || "—")) +
    kvItem("MAC", esc(dev.mac || "—")) +
    kvItem("Hardware", esc(dev.hardware_id || "—")) +
    kvItem("Battery", dev.battery_v != null ? `${dev.battery_pct != null ? dev.battery_pct + "%" : "—"} · ${dev.battery_v} V` + (dev.battery_recorded ? `<br><span style="color:var(--faint);font-size:11px">Recorded ${dev.battery_recorded}${dev.battery_recorded_hm ? " " + dev.battery_recorded_hm : ""}</span>` : "") : (dev.battery_pct != null ? `${dev.battery_pct}%` : "—")) +
    kvItem("Last sync", `${esc(dev.synced || "—")} ${esc(dev.synced_hm || "")}`) +
    kvItem("Sync cursor", dev.next_cursor != null ? dev.next_cursor.toLocaleString() : "—") +
    kvItem("History", historyDetail) +
    kvItem("Events", `${num(rawTotal).toLocaleString()} raw · ${num(decodedTotal).toLocaleString()} decoded`);
  ab.append(kv);

  // Read-only capability metadata does not establish observed data or analysis.
  ab.append(el("p", "subhead", "Capabilities · reported modes"));
  const caps = el("div", "caps");
  (dev.measuring || []).forEach((m) => {
    const row = el("div", "cap" + (m.on ? " on" : ""));
    const ic = el("span", "ic");
    ic.style.setProperty("--i", `url(/icons/${capIcon(m.name)}.svg)`);
    const nm = el("span", "cap-name");
    nm.textContent = m.name;
    const mode = el("span", "ad-muted", m.on ? "mode enabled" : "mode disabled / unknown");
    row.append(ic, nm, mode);
    caps.append(row);
  });
  ab.append(caps);

  const ev = dev.event_counts || [];
  if (ev.length) {
    ab.append(el("p", "subhead", `Event stream · ${ev.length} types`));
    const emax = Math.max(...ev.map((e) => e.count), 1);
    const tbl = el("div", "ev-table");
    ev.forEach((e) => {
      const row = el("div", "ev-row");
      const nm = el("span", "ev-n");
      nm.textContent = e.name;
      const bar = el("span", "ev-bar");
      const fi = el("i");
      fi.style.width = Math.max(2, (e.count / emax) * 100) + "%";
      bar.append(fi);
      const c = el("span", "ev-c");
      c.textContent = e.count.toLocaleString();
      row.append(nm, bar, c);
      tbl.append(row);
    });
    ab.append(tbl);
  }
  adv.append(ab);
  box.append(adv);
}


function renderActions(d) {
  const dev = d.device || {};
  const b = $("batt");
  if (dev.battery_pct != null) {
    b.innerHTML = icon("battery-high") + `<span>${dev.battery_pct}%</span>`;
    b.classList.toggle("low", dev.battery_pct < 20);
    b.hidden = false;
    let title = `Ring battery ${dev.battery_pct}%`;
    if (dev.battery_v) title += ` · ${dev.battery_v} V`;
    const rec = batteryRecordedLabel(dev);
    if (rec) title += `\n${rec}`;
    b.title = title;
  } else {
    b.hidden = true;
  }
  $("foot-meta").textContent = `${num(dev.nights)} nights · ${num(dev.total_events).toLocaleString()} events`;
}

// ── profile dialog ──────────────────────────────────────────
async function openProfile() {
  $("profile-btn").disabled = true;
  try {
    const p = await getDash(`/api/profile?tz=${browserTzOffset()}`).then(readJsonResponse);
    if (p.error) {
      toast(p.error, "error");
      return;
    }
    CURRENT_PROFILE = p;
    $("f-sex").value = p.sex;
    $("f-age").value = p.age;
    $("f-height").value = Number(p.height_m.toFixed(2));
    $("f-weight").value = Number(p.weight_kg.toFixed(2));
    const averaged = p.weight_source === "manual_average";
    $("f-weight").readOnly = averaged;
    $("profile-weight-help").textContent = averaged
      ? `Using the average of ${p.weight_sample_count} logged weight ${p.weight_sample_count === 1 ? "entry" : "entries"}. Update weight in Daily entry.`
      : "No recent logged weights. This weight is saved as a fallback; logged weights take priority.";
    $("profile-dialog").showModal();
  } catch {
    toast("Couldn't load saved details. Check the dashboard server and try again.", "error");
  } finally {
    $("profile-btn").disabled = false;
  }
}
async function saveProfile(e) {
  e.preventDefault();
  const weight = parseDecimal($("f-weight").value, { comma: true });
  if (weight === null || weight < 20 || weight > 500) {
    toast("Enter weight in kg as a number between 20 and 500, for example 75.5 or 75,5.", "error");
    return;
  }
  const body = {
    sex: $("f-sex").value,
    age: +$("f-age").value,
    height_m: +$("f-height").value,
    weight_kg: weight,
    ring_size: (CURRENT_PROFILE && CURRENT_PROFILE.ring_size) || 10, // not on the ring; kept default
  };
  $("profile-save").disabled = true;
  try {
    // the server replies 200 with an { error } body on write failures; a non-JSON
    // HTTP error becomes a readable { error } too (readJsonResponse)
    const j = await postDash("/api/profile", body).then(readJsonResponse);
    if (j.error) {
      toast(j.error, "error");
      return;
    }
    CURRENT_PROFILE = j;
    $("profile-dialog").close();
    toast("Details saved on the dashboard server.");
    await load(); // re-runs CVA with the new demographics
  } catch {
    toast("Couldn't reach the local server.", "error");
  } finally {
    $("profile-save").disabled = false;
  }
}

// ── notifications ───────────────────────────────────────────
function toast(msg, kind = "info") {
  let t = $("toast");
  if (!t) { t = el("div", "toast"); t.id = "toast"; document.body.append(t); }
  t.className = "toast " + kind;
  // status dot + message (textContent on the span keeps the message injection-safe)
  const dot = el("span", "toast-dot");
  const text = el("span", "toast-msg");
  text.textContent = msg;
  t.replaceChildren(dot, text);
  requestAnimationFrame(() => t.classList.add("show"));
  clearTimeout(toast._t);
  toast._t = setTimeout(() => t.classList.remove("show"), kind === "error" ? 8000 : 3800);
}


// Collector health is independent of physiological summaries, including on first upload.
let COLLECTOR_STATUS = null;
let STATUS_TIMER = null;
let STATUS_REQUEST = null;
let STATUS_SIGNATURE = null;
let STATUS_CHECKED_AT = null;
let STATUS_FAILED = false;
let LAST_SUMMARY = null;

const ageLabel = (unixMs) => {
  if (unixMs == null) return "—";
  const seconds = (Date.now() - unixMs) / 1000;
  if (seconds < -60) return "clock is ahead";
  if (seconds < 60) return "<1 min ago";
  if (seconds < 3600) return `${Math.floor(seconds / 60)} min ago`;
  return `${Math.floor(seconds / 3600)} h ago`;
};
const bytesLabel = (bytes) => bytes == null ? "—" : `${(bytes / 1048576).toFixed(1)} MiB`;

// A2: one distinct human sentence per `ingest.state`. The LXC server has no
// ring, so no message here ever tells you to run a local sync.
const INGEST_STATE_COPY = {
  db_missing: "The ingest database is missing on the server. Point OURA_HUB_DB at the hub database and restart the dashboard service.",
  db_inaccessible: "The ingest database exists but the server cannot open it. Check the dashboard service's read permission on that file.",
  no_upload: "The server is up, but no collector batch has arrived yet. Start the collector on the Pi; its first signed batch will appear here.",
  collector_ready: "A collector is registered, but no raw events are stored yet. Decoding starts on the server once events arrive.",
  undecoded: "Raw events are stored but none are decoded yet. The server decodes each batch; the summary follows once decoding finishes.",
  raw_pending: "New raw events are queued; the server is decoding and re-analyzing the affected days now.",
  analysis_failed: "Analysis failed on the newest revision. The last known-good summary stays visible; the error is shown below.",
  ready: "Uploaded, decoded and analyzed — the latest summary is current.",
};

// A3/B6: the honest per-day activity panel. `steps` is null until a real count
// source exists; `estimated_steps` is MET-derived and labelled as an estimate.
// Unknown minutes are unobserved, never zero-activity; null sedentary minutes
// mean no wear evidence at all.
function renderActivityTracking(d) {
  const box = $("activity-tracking");
  box.replaceChildren();
  const at = d.activity_tracking || {};
  if (!Array.isArray(at.days) || !at.days.length) {
    box.append(el("p", "ad-muted", "No dated activity coverage yet. Minutes appear once the collector uploads and the server decodes them."));
    if (at.steps_blocker) box.append(el("p", "at-blocked", `Steps are unavailable: ${esc(at.steps_blocker)}`));
    return;
  }
  const item = (label, value) => `<div><i>${esc(label)}</i><b>${value}</b></div>`;
  at.days.forEach((day) => {
    const card = el("div", "at-day");
    card.append(el("p", "subhead", `${esc(day.date)} · <span class="at-est">steps estimated from MET</span>`));
    const kv = el("div", "adv-kv");
    kv.innerHTML = item("Steps", day.steps == null ? "—" : kfmt(day.steps)) +
      item("Estimated steps (MET)", day.estimated_steps == null ? "—" : Math.round(day.estimated_steps).toLocaleString()) +
      item("Active minutes", num(day.active_minutes)) +
      item("Sedentary minutes", day.sedentary_minutes == null ? "no wear evidence" : num(day.sedentary_minutes)) +
      item("Unknown minutes", `${num(day.unknown_minutes)} (unobserved, not zero)`) +
      item("Observed minutes", num(day.observed_minutes));
    card.append(kv);
    if (day.steps_source === "blocked")
      card.append(el("p", "at-blocked", `Steps for this day are blocked: ${esc(at.steps_blocker || "no decoded step source")}.`));
    (Array.isArray(day.hours) ? day.hours : []).forEach((h) => {
      const sed = h.sedentary_minutes == null ? "no wear evidence" : num(h.sedentary_minutes);
      card.append(el("p", "at-hour",
        `<i>${esc(h.label || "—")}</i> <b>${h.estimated_steps == null ? "—" : Math.round(h.estimated_steps).toLocaleString()}</b> est. steps (MET)` +
        ` · ${num(h.active_minutes)} active · ${sed} sedentary · ${num(h.unknown_minutes)} unknown`));
    });
    box.append(card);
  });
}

function renderCollectorStatus() {
  const box = $("collector-status");
  box.replaceChildren();
  if (!COLLECTOR_STATUS) {
    box.append(el("p", "error", STATUS_FAILED ? "Collector status unavailable. Server connection failed; no health state can be confirmed." : "Checking collector status…"));
    return;
  }
  const s = COLLECTOR_STATUS, ingest = s.ingest || {}, processing = ingest.processing || {};
  // A2: one distinct sentence per ingest state — DB trouble, first upload,
  // pending decode, failed analysis and current data never share a message.
  const state = ingest.state || s.state || "unknown";
  const note = el("p", "at-state");
  note.textContent = INGEST_STATE_COPY[state] || `Collector state: ${state}.`;
  box.append(note);
  if (STATUS_FAILED) box.append(el("p", "error", `Status is stale: last successful check ${ageLabel(STATUS_CHECKED_AT)}. Server connection failed.`));
  const kv = el("div", "adv-kv");
  const item = (label, value) => `<div><i>${esc(label)}</i><b>${esc(value)}</b></div>`;
  kv.innerHTML = item("Raw events", num(ingest.raw_events).toLocaleString()) +
    item("Decoded events", num(ingest.decoded_events).toLocaleString()) +
    item("Processing", `${processing.state || "—"}${processing.stage ? " · " + processing.stage : ""}`) + item("Input revision", num(ingest.revision)) +
    item("Analyzed revision", num(processing.completed_revision));
  box.append(kv);
  if (processing.last_error) {
    const error = el("p", "error");
    error.textContent = `Processing error: ${String(processing.last_error).slice(0, 240)}`;
    box.append(error);
  }
  const models = s.models || {};
  const missing = Object.entries(models).filter(([, model]) => !model.available).map(([name]) => name);
  box.append(el("p", "ad-muted", missing.length ? `Missing models: ${missing.map(esc).join(", ")}. Model-dependent metrics remain unavailable.` : (Object.keys(models).length ? "All configured models are available." : "No model runners are configured.")));
  if (!ingest.collectors.length) box.append(el("p", "error", "No collector heartbeat received yet."));
  ingest.collectors.forEach((collector) => {
    const h = collector.heartbeat || {}, row = el("div", "collector-health");
    const stale = Date.now() - collector.received_unix_ms > 5 * 60 * 1000;
    row.append(el("p", "subhead", `${esc(collector.device_id)} · ${stale ? "stale / offline" : "recent heartbeat"}`));
    const fields = el("div", "adv-kv");
    fields.innerHTML = item("Heartbeat", ageLabel(collector.received_unix_ms)) + item("Build", esc(collector.collector_build || "—")) +
      item("Last BLE attempt", ageLabel(h.ble_attempt_unix_ms)) + item("Last BLE success", ageLabel(h.ble_success_unix_ms)) +
      item("Last upload receipt", ageLabel(h.upload_receipt_unix_ms)) + item("Queue", `${num(h.queue_count)} events · ${bytesLabel(h.queue_bytes)}`) +
      item("Oldest queued", ageLabel(h.queue_oldest_unix_ms)) + item("Disk free", bytesLabel(h.disk_free_bytes)) +
      item("Clock", h.clock_synchronized ? "synchronized" : "not synchronized");
    row.append(fields);
    if (h.disk_free_bytes != null && h.disk_free_bytes < 256 * 1048576) row.append(el("p", "error", "Disk pressure: less than 256 MiB free."));
    if (h.error_codes.length) row.append(el("p", "error", `Collector errors: ${h.error_codes.slice(0, 8).map(esc).join(", ")}`));
    box.append(row);
  });
}

async function pollCollectorStatus() {
  if (document.hidden || STATUS_REQUEST) return;
  clearTimeout(STATUS_TIMER);
  const controller = new AbortController();
  STATUS_REQUEST = controller;
  const deadline = setTimeout(() => controller.abort(), 12000);
  try {
    const response = await fetch("/api/status", { signal: controller.signal, cache: "no-store" });
    if (!response.ok) throw new Error("Status unavailable");
    const s = await response.json();
    const signature = JSON.stringify([s.state, s.ingest.revision, s.ingest.raw_events, s.ingest.decoded_events, s.ingest.processing.state, s.ingest.processing.completed_revision, s.models]);
    const changed = STATUS_SIGNATURE != null && signature !== STATUS_SIGNATURE;
    COLLECTOR_STATUS = s;
    STATUS_SIGNATURE = signature;
    STATUS_CHECKED_AT = Date.now();
    STATUS_FAILED = false;
    renderCollectorStatus();
    if (changed) load();
  } catch (_) {
    if (!document.hidden) {
      STATUS_FAILED = true;
      renderCollectorStatus();
    }
  } finally {
    clearTimeout(deadline);
    STATUS_REQUEST = null;
    if (!document.hidden) STATUS_TIMER = setTimeout(pollCollectorStatus, 5000);
  }
}

document.addEventListener("visibilitychange", () => {
  clearTimeout(STATUS_TIMER);
  if (document.hidden) STATUS_REQUEST?.abort();
  else {
    renderCollectorStatus();
    if (!STATUS_REQUEST) pollCollectorStatus();
  }
});

// A failed summary must not hide collector health or the independent manual store.
function showLoadError(msg) {
  document.querySelectorAll(".skeleton").forEach((el) => {
    if (el.id === "digest") return; // handled below — keep it for the message
    el.remove();
  });
  const dg = $("digest");
  dg.classList.remove("skeleton", "skeleton-text");
  dg.classList.add("reveal");
  dg.textContent = msg;
}

let SUMMARY_REQUEST = null;
let SUMMARY_AGAIN = false;
function load() {
  if (SUMMARY_REQUEST) {
    SUMMARY_AGAIN = true;
    return SUMMARY_REQUEST;
  }
  SUMMARY_REQUEST = loadSummary().finally(() => {
    SUMMARY_REQUEST = null;
    if (SUMMARY_AGAIN) {
      SUMMARY_AGAIN = false;
      if (!document.hidden) load();
    }
  });
  return SUMMARY_REQUEST;
}

async function loadSummary() {
  const controller = new AbortController();
  const deadline = setTimeout(() => controller.abort(), 120000);
  let d;
  try {
    const response = await fetch(`/api/summary?tz=${browserTzOffset()}`, { signal: controller.signal });
    d = await response.json();
    if (!response.ok && !d.error) throw new Error("Summary unavailable");
  } catch (_) {
    renderActivityTracking({}); // A2: the activity panel reads truthfully without a summary
    showLoadError("Summary unavailable. Collector health is checked separately; saved manual entries remain available.");
    return;
  } finally {
    clearTimeout(deadline);
  }
  if (d.error) {
    if (!LAST_SUMMARY) {
      renderTiles({});
      renderDay({});
      renderSleepDebt({});
      renderIllness({});
      renderCardio({});
      renderSpo2({});
      renderDevice({});
      renderActivityTracking({});
    }
    showLoadError(d.status?.message || "No physiological summary available yet. Check collector and processing status.");
    return;
  }
  LAST_SUMMARY = d;
  CURRENT_TZ = d.tz || 0;
  $("digest").classList.remove("skeleton", "skeleton-text");
  $("digest").classList.add("reveal");
  $("digest").innerHTML = (d.digest || "").replace(/([+-]?\d[\d.]*\s?(?:%|bpm|ms|m\/s))/g, '<span class="metric">$1</span>');
  renderActions(d);
  renderTiles(d);
  HR_QUARTER_LOAD = null;
  renderDay(d);
  renderActivityTracking(d);
  renderSleepDebt(d);
  renderIllness(d);
  renderCardio(d);
  renderSpo2(d);
  renderDevice(d);
  document.querySelectorAll(".panel").forEach((p, i) => {
    p.classList.add("reveal");
    p.style.setProperty("--d", i * 60 + "ms");
  });
}

$("profile-btn").addEventListener("click", openProfile);
$("profile-form").addEventListener("submit", saveProfile);
$("profile-cancel").addEventListener("click", () => $("profile-dialog").close());
load();
pollCollectorStatus();

// ── research export: raw CSVs for a window, built by oura-summary::research_export ──
(function researchExport() {
  const form = document.getElementById("research-form");
  if (!form) return;
  const pad = (n) => String(n).padStart(2, "0");
  const local = (d) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
  const start = new Date(); start.setDate(start.getDate() - 1); start.setHours(20, 0, 0, 0);
  const end = new Date(); end.setHours(12, 0, 0, 0);
  form.querySelector("#rx-start").value = local(start);
  form.querySelector("#rx-end").value = local(end);
  form.addEventListener("submit", (e) => {
    e.preventDefault();
    const status = form.querySelector("#rx-status");
    const s = new Date(form.querySelector("#rx-start").value);
    const t = new Date(form.querySelector("#rx-end").value);
    const streams = [...form.querySelectorAll("#rx-streams input:checked")].map((i) => i.value);
    if (!(t > s)) { status.textContent = "The end must be after the start."; return; }
    if (!streams.length) { status.textContent = "Choose at least one kind of data."; return; }
    const q = new URLSearchParams({
      start: String(s.getTime() / 1000), end: String(t.getTime() / 1000),
      streams: streams.join(","), tz: String(-s.getTimezoneOffset() / 60),
    });
    const a = document.createElement("a");
    a.href = `/api/research-export?${q}`;
    document.body.append(a); a.click(); a.remove();
    status.textContent = "Preparing the ZIP. Your browser will save it when it is ready.";
  });
  form.querySelector("#rx-mark").addEventListener("click", async () => {
    const status = form.querySelector("#rx-status");
    const label = form.querySelector("#rx-label").value;
    try {
      const r = await fetch("/api/research-marker", {
        method: "POST", headers: { "Content-Type": "application/json", "X-Oura-Dash": "1" },
        body: JSON.stringify({ label }),
      });
      const j = await r.json();
      if (j.error) throw new Error(j.error);
      status.textContent = `Marked at ${new Date(j.unix * 1000).toLocaleTimeString()}. It will be in markers.csv.`;
    } catch (err) {
      status.textContent = `Could not save the marker: ${err.message}`;
    }
  });
})();

// ── daily manual entry: calories eaten + body weight, no ring needed ─────────
// Talks to /api/manual/*. Storage is canonical (kcal, kg); the configured display
// unit only changes what is shown, and a save converts back to kg. A missing day
// has no row — it is never charted as zero.
(function manualEntry() {
  const panel = $("manual-panel");
  if (!panel) return;

  const LB_PER_KG = 2.2046226218487757;
  let UNIT = "kg";
  let ENTRIES = [];

  const pad2 = (n) => String(n).padStart(2, "0");
  const localToday = () => {
    const d = new Date();
    return `${d.getFullYear()}-${pad2(d.getMonth() + 1)}-${pad2(d.getDate())}`;
  };
  const shiftDays = (ymd, delta) => {
    const [y, m, d] = ymd.split("-").map(Number);
    const dt = new Date(y, m - 1, d);
    dt.setDate(dt.getDate() + delta);
    return `${dt.getFullYear()}-${pad2(dt.getMonth() + 1)}-${pad2(dt.getDate())}`;
  };
  const trimNum = (v) => String(Math.round(v * 100) / 100);
  const round1 = (v) => Math.round(v * 10) / 10;
  const round2 = (v) => Math.round(v * 100) / 100;
  const dispWeight = (kg) => (UNIT === "lb" ? kg * LB_PER_KG : kg);
  const storeWeight = (shown) => (UNIT === "lb" ? shown / LB_PER_KG : shown);
  const ymdToDay = (ymd) => {
    const [y, m, d] = ymd.split("-").map(Number);
    return Math.round(Date.UTC(y, m - 1, d) / 86400000);
  };

  const dateInput = $("manual-date");
  const calInput = $("manual-calories");
  const wtInput = $("manual-weight");
  const noteInput = $("manual-note");
  const wtUnit = $("manual-weight-unit");
  const wtLegend = $("manual-weight-legend");
  const calLegend = $("manual-calories-legend");
  const calChart = $("manual-calories-chart");
  const wtChart = $("manual-weight-chart");
  const summary = $("manual-summary");
  const recent = $("manual-recent");
  const dlg = $("manual-dialog");
  const EMPTY = '<p class="manual-empty">No data yet.</p>';

  const msg = (text, isError) => {
    const p = $("manual-msg");
    if (!p) return;
    p.textContent = text || "";
    p.classList.toggle("is-error", !!isError);
  };

  // Trend gaps are linearly interpolated. Inferred leading values are display-only;
  // stored observations and the server's full seven-entry means stay unchanged.
  function manualTrendSvg(points, at) {
    if (!points.length) return "";
    if (points.length === 1) {
      const [x, y] = at(points[0]);
      return `<circle class="manual-trend-dot" cx="${x}" cy="${y}" r="2.5"></circle>`;
    }
    // A straight segment gives every missing date the same daily slope;
    // one missing date lies exactly at the midpoint of its neighbors.
    const path = points.map((p, i) => `${i ? "L" : "M"}${at(p).join(" ")}`).join(" ");
    return `<path class="manual-trend" d="${path}"></path>`;
  }

  function calendarPoints(points, trend, dates) {
    const history = (points || []).filter((p) => p && p.value != null && p.date <= dates.at(-1))
      .sort((a, b) => a.date.localeCompare(b.date));
    const values = new Map(history.map((p) => [p.date, p.value]));
    const anchors = (trend || []).filter((p) => p && p.value != null && p.date <= dates.at(-1))
      .map((p) => ({ ...p, inferred: false }));
    // Before a full window exists, use the available entries, not invented zeros.
    const firstFullDate = anchors[0]?.date;
    let sum = 0;
    for (const [index, p] of history.slice(0, 6).entries()) {
      sum += p.value;
      if (!firstFullDate || p.date < firstFullDate) {
        anchors.push({ date: p.date, value: sum / (index + 1), inferred: true });
      }
    }
    anchors.sort((a, b) => a.date.localeCompare(b.date));
    return dates.map((date) => {
      const right = anchors.findIndex((p) => p.date >= date);
      let value = null, inferred = false;
      if (right >= 0) {
        const next = anchors[right], prev = anchors[right - 1];
        if (next.date === date) {
          value = next.value;
          inferred = next.inferred;
        } else if (!prev) {
          value = next.value;
          inferred = true;
        } else {
          const fraction = (ymdToDay(date) - ymdToDay(prev.date)) / (ymdToDay(next.date) - ymdToDay(prev.date));
          value = prev.value + fraction * (next.value - prev.value);
          inferred = prev.inferred || next.inferred;
        }
      }
      return { date, value: values.get(date) ?? null, trend: value, inferred };
    });
  }

  function missingMarks(points, xOf, y) {
    return `<g class="manual-missing">${points.filter((p) => p.value == null).map((p) => {
      const x = xOf(p);
      return `<path d="M${x - 2} ${y - 2}l4 4m-4 0l4 -4"></path>`;
    }).join("")}</g>`;
  }

  // Calories as bars plus a rolling-trend line. Empty days keep their place on
  // the date axis, with a missing-entry mark rather than a zero-valued bar.
  function caloriesSvg(points) {
    const pts = points.filter((p) => p.value != null);
    const tpts = points.filter((p) => p.trend != null).map((p) => ({ date: p.date, value: p.trend }));
    const W = 600, H = 220, base = H - 10;
    const max = Math.max(1, ...pts.map((p) => p.value), ...tpts.map((p) => p.value));
    const d0 = ymdToDay(points[0].date), bw = W / points.length;
    const xOf = (p) => (ymdToDay(p.date) - d0 + 0.5) * bw;
    const yOf = (v) => base - (v / max) * (H - 25);
    const bars = pts.map((p) => {
      const bh = Math.max(1, base - yOf(p.value));
      return `<rect x="${(xOf(p) - bw * 0.32).toFixed(2)}" y="${(base - bh).toFixed(2)}" width="${(bw * 0.64).toFixed(2)}" height="${bh.toFixed(2)}" rx="1.5"></rect>`;
    }).join("");
    return {
      svg: `<svg class="manual-svg" viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" aria-hidden="true"><g class="manual-bars">${bars}</g>${missingMarks(points, xOf, H - 4)}${manualTrendSvg(tpts, (p) => [xOf(p), yOf(p.value)])}</svg>`,
      points: points.map((p) => ({ ...p, x: xOf(p), y: p.value == null ? null : yOf(p.value), trendY: p.trend == null ? null : yOf(p.trend) })),
      width: W, height: H, label: "Calories", unit: "kcal", decimals: 0,
      hasTrend: tpts.length > 0,
    };
  }

  // Weight observations stay separate dots; only the rolling trend has a line.
  // Stretch both axes to the plot bounds so narrow cards do not letterbox.
  function weightSvg(points) {
    const pts = points.filter((p) => p.value != null);
    const tpts = points.filter((p) => p.trend != null).map((p) => ({ date: p.date, value: p.trend }));
    const W = 600, H = 220, padT = 10, padB = 12;
    const vals = pts.concat(tpts).map((p) => p.value);
    const min = vals.length ? Math.min(...vals) : 0, max = vals.length ? Math.max(...vals) : 1;
    const rng = Math.max(0.5, max - min);
    const d0 = ymdToDay(points[0].date);
    const xOf = (p) => (ymdToDay(p.date) - d0 + 0.5) * W / points.length;
    const yOf = (v) => padT + (1 - (v - min + (rng - (max - min)) / 2) / rng) * (H - padT - padB);
    const at = (p) => [xOf(p), yOf(p.value)];
    const dots = pts.map((p) => {
      const [x, y] = at(p);
      return `<path class="manual-dot" d="M${x.toFixed(2)} ${y.toFixed(2)}h0"></path>`;
    }).join("");
    return {
      svg: `<svg class="manual-svg" viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" aria-hidden="true">${dots}${missingMarks(points, xOf, H - 4)}${manualTrendSvg(tpts, at)}</svg>`,
      points: points.map((p) => ({ ...p, x: xOf(p), y: p.value == null ? null : yOf(p.value), trendY: p.trend == null ? null : yOf(p.trend) })),
      width: W, height: H, label: "Weight", unit: UNIT, decimals: 2,
      hasTrend: tpts.length > 0,
    };
  }

  let chartInspectors = [];
  let inspectionTimer;
  function inspectDay(index) {
    chartInspectors.forEach((chart) => chart.show(index, true));
    clearTimeout(inspectionTimer);
    inspectionTimer = setTimeout(() => chartInspectors.forEach((chart) => chart.hide()), 10000);
  }

  function renderInteractiveChart(container, chart) {
    const readoutId = `${container.id}-readout`, hintId = `${container.id}-hint`;
    container.innerHTML = `<div class="manual-plot" tabindex="0" role="slider" aria-label="${chart.label} by date" aria-orientation="horizontal" aria-valuemin="1" aria-valuemax="${chart.points.length}" aria-describedby="${hintId} ${readoutId}">
      ${chart.svg}<i class="manual-crosshair" aria-hidden="true"></i><i class="manual-selected-dot" aria-hidden="true" hidden></i><i class="manual-selected-trend" aria-hidden="true" hidden></i>
      </div><p class="manual-chart-readout" id="${readoutId}" aria-live="polite" aria-atomic="true"></p>
      <p class="manual-chart-hint" id="${hintId}">× No entry. Hover or tap to inspect both charts. Use arrow keys, Home or End when focused. Inferred trends use available history.</p>`;
    const plot = container.querySelector(".manual-plot");
    const readout = container.querySelector(".manual-chart-readout");
    const marker = plot.querySelector(".manual-selected-dot");
    const trendMarker = plot.querySelector(".manual-selected-trend");
    let selected = chart.points.length - 1;
    const format = (value) => `${value.toLocaleString(undefined, { minimumFractionDigits: chart.decimals, maximumFractionDigits: chart.decimals })} ${chart.unit}`;
    const show = (index, active = false) => {
      selected = index;
      const p = chart.points[index];
      const text = `${p.date} · ${chart.label}: ${p.value == null ? "No entry" : format(p.value)}${p.trend != null ? ` · ${p.inferred ? "Inferred trend" : "7-entry trend"}: ${format(p.trend)}` : ""}`;
      plot.classList.toggle("is-inspecting", active);
      plot.style.setProperty("--manual-point-x", `${p.x / chart.width * 100}%`);
      marker.hidden = p.value == null;
      if (p.value != null) plot.style.setProperty("--manual-point-y", `${p.y / chart.height * 100}%`);
      trendMarker.hidden = p.trend == null;
      if (p.trend != null) trendMarker.style.top = `${p.trendY / chart.height * 100}%`;
      plot.setAttribute("aria-valuenow", String(index + 1));
      plot.setAttribute("aria-valuetext", text);
      readout.textContent = text;
    };
    const inspect = (event) => {
      const box = plot.getBoundingClientRect();
      const fraction = Math.max(0, Math.min(1, (event.clientX - box.left) / box.width));
      inspectDay(Math.min(chart.points.length - 1, Math.floor(fraction * chart.points.length)));
    };
    plot.addEventListener("pointermove", (event) => {
      if (event.pointerType !== "touch" || event.buttons) inspect(event);
    });
    plot.addEventListener("pointerdown", (event) => {
      if (!event.isPrimary || event.button !== 0) return;
      plot.focus({ preventScroll: true });
      plot.setPointerCapture(event.pointerId);
      inspect(event);
    });
    plot.addEventListener("keydown", (event) => {
      let index;
      switch (event.key) {
        case "ArrowLeft": case "ArrowDown": index = Math.max(0, selected - 1); break;
        case "ArrowRight": case "ArrowUp": index = Math.min(chart.points.length - 1, selected + 1); break;
        case "Home": index = 0; break;
        case "End": index = chart.points.length - 1; break;
        default: return;
      }
      event.preventDefault();
      inspectDay(index);
    });
    show(selected);
    return { show, hide: () => plot.classList.remove("is-inspecting") };
  }

  // "1,940" with a sign only when the value is not zero. Null means "no
  // comparison period", which reads as an em dash, not as zero.
  function formatChange(v, unit, decimals = 0) {
    if (v == null) return "—";
    const sign = v > 0 ? "+" : "";
    const rounded = Math.round(Math.abs(v) * 10 ** decimals) / 10 ** decimals;
    const formatted = rounded.toLocaleString(undefined, { minimumFractionDigits: decimals, maximumFractionDigits: decimals });
    return `${sign}${formatted} ${unit}`;
  }

  // Four-week comparison numbers. The backend sends averages already in the
  // display unit; this only formats them. The block stays hidden when there is
  // nothing to compare.
  function renderSummary(series) {
    const cal = series.calories_summary;
    const wt = series.weight_summary;
    const hasCal = cal && cal.avg_28 != null;
    const hasWt = wt && wt.avg_28 != null;
    summary.hidden = !(hasCal || hasWt);
    if (hasCal) {
      $("manual-cal-avg28").textContent = `${Math.round(cal.avg_28).toLocaleString()} kcal`;
      $("manual-cal-avg7").textContent = `${Math.round(cal.avg_7).toLocaleString()} kcal`;
      $("manual-cal-change").textContent = formatChange(cal.change, "kcal");
      $("manual-cal-weeks").innerHTML = (cal.weekly || [])
        .map((w) => `<span class="manual-week"><span class="manual-week-label">W${w.week + 1}</span><span class="manual-week-value">${Math.round(w.value).toLocaleString()}</span></span>`)
        .join("");
    }
    if (hasWt) {
      $("manual-wt-avg28").textContent = `${round2(wt.avg_28).toFixed(2)} ${UNIT}`;
      $("manual-wt-avg7").textContent = `${round2(wt.avg_7).toFixed(2)} ${UNIT}`;
      $("manual-wt-change").textContent = formatChange(wt.change, UNIT, 2);
      $("manual-wt-weeks").innerHTML = (wt.weekly || [])
        .map((w) => `<span class="manual-week"><span class="manual-week-label">W${w.week + 1}</span><span class="manual-week-value">${round2(w.value).toFixed(2)}</span></span>`)
        .join("");
    }
  }

  function renderCharts(series) {
    clearTimeout(inspectionTimer);
    const today = localToday();
    const dates = Array.from({ length: 28 }, (_, i) => shiftDays(today, i - 27));
    const cal = caloriesSvg(calendarPoints(series.calories, series.calories_trend, dates));
    const wt = weightSvg(calendarPoints(series.weight, series.weight_trend, dates));
    chartInspectors = [renderInteractiveChart(calChart, cal), renderInteractiveChart(wtChart, wt)];
    calLegend.hidden = !cal.hasTrend;
    wtLegend.hidden = !wt.hasTrend;
  }

  function renderRecent() {
    const today = localToday();
    const from = shiftDays(today, -29);
    const byDate = new Map();
    for (const e of ENTRIES) {
      if (e.local_date < from || e.local_date > today) continue;
      if (!byDate.has(e.local_date)) byDate.set(e.local_date, {});
      byDate.get(e.local_date)[e.kind] = e;
    }
    const dates = [...byDate.keys()].sort().reverse();
    if (!dates.length) {
      recent.replaceChildren(el("p", "manual-empty", "No entries yet."));
      return;
    }
    const table = el("div", "manual-table");
    table.append(el("div", "manual-thead", "<span>Date</span><span>Values</span><span></span>"));
    for (const date of dates) {
      const rec = byDate.get(date);
      const kcal = rec.calories_eaten ? `${Math.round(rec.calories_eaten.value)} kcal` : "—";
      const wt = rec.weight_kg ? `${round2(dispWeight(rec.weight_kg.value)).toFixed(2)} ${UNIT}` : "—";
      const row = el("div", "manual-trow");
      row.append(el("div", "manual-date", esc(fmtDay(date))));
      row.append(el("div", "manual-vals", `<span>${kcal}</span><span>${wt}</span>`));
      const actions = el("div", "manual-actions");
      const edit = el("button", "manual-btn", "Edit");
      edit.type = "button";
      edit.addEventListener("click", () => openModal(date));
      const del = el("button", "manual-btn manual-btn-danger", "Delete");
      del.type = "button";
      del.addEventListener("click", () => removeDate(date));
      actions.append(edit, del);
      row.append(actions);
      table.append(row);
    }
    recent.replaceChildren(table);
  }

  // Prefill from stored entries for the date currently in the form.
  function fillFromEntries() {
    const rec = ENTRIES.filter((e) => e.local_date === dateInput.value);
    const cal = rec.find((e) => e.kind === "calories_eaten");
    const wt = rec.find((e) => e.kind === "weight_kg");
    calInput.value = cal ? trimNum(cal.value) : "";
    wtInput.value = wt ? trimNum(dispWeight(wt.value)) : "";
    msg("");
  }

  // Open the dialog. With a date, the form prefills from stored entries; with
  // none, it starts on today.
  function openModal(date) {
    dateInput.value = date || localToday();
    fillFromEntries();
    msg("");
    if (typeof dlg.showModal === "function") dlg.showModal();
    else dlg.setAttribute("open", "");
  }

  // Save both fields in one submit. Empty fields are skipped; at least one
  // value must be present. Both requests run in parallel. If one fails, a
  // successful one stays stored: the table refreshes, and the dialog stays
  // open with the error and the typed values for a retry.
  async function saveModal() {
    const date = dateInput.value || localToday();
    if (!/^\d{4}-\d{2}-\d{2}$/.test(date)) {
      msg("Pick a date first.", true);
      return;
    }
    const calRaw = calInput.value.trim();
    const wtRaw = wtInput.value.trim();
    const cal = calRaw === "" ? null : parseDecimal(calRaw, { comma: false });
    const wt = wtRaw === "" ? null : parseDecimal(wtRaw, { comma: true });
    if (calRaw !== "" && cal === null) {
      msg("Enter calories as a number, for example 1940 or 1940.5. Do not use thousands separators.", true);
      return;
    }
    if (wtRaw !== "") {
      const [lo, hi] = UNIT === "lb" ? [44, 1100] : [20, 500];
      if (wt === null) {
        msg(`Enter a weight in ${UNIT} as a number, for example 75.5 or 75,5.`, true);
        return;
      }
      if (wt < lo || wt > hi) {
        msg(`Enter a weight in ${UNIT} between ${lo} and ${hi}.`, true);
        return;
      }
    }
    if (cal === null && wt === null) {
      msg("Enter calories or a weight.", true);
      return;
    }
    const note = noteInput.value.trim() || null;
    const payloads = [];
    if (cal !== null) payloads.push({ local_date: date, kind: "calories_eaten", value: cal, note });
    if (wt !== null) payloads.push({ local_date: date, kind: "weight_kg", value: storeWeight(wt), note });
    let results;
    try {
      results = await Promise.all(payloads.map((p) => postDash("/api/manual/entries", p).then(readJsonResponse)));
    } catch {
      msg("Couldn't reach the local server.", true);
      return;
    }
    const label = { calories_eaten: "Calories", weight_kg: "Weight" };
    const errors = results
      .map((j, i) => (j.ok ? null : `${label[payloads[i].kind]} not saved: ${j.error || "Could not save."}`))
      .filter(Boolean);
    if (errors.length) {
      // keep what succeeded: refresh its row; the dialog and typed values stay
      if (errors.length < results.length) await refresh();
      msg(errors.join(" "), true);
      return;
    }
    dlg.close();
    await refresh();
  }

  async function removeDate(date) {
    if (!window.confirm(`Delete the entries for ${date}?`)) return;
    const kinds = ["calories_eaten", "weight_kg"];
    let results;
    try {
      results = await Promise.all(kinds.map((kind) =>
        deleteDash(`/api/manual/entries?date=${encodeURIComponent(date)}&kind=${kind}`).then(readJsonResponse)));
    } catch {
      toast("Couldn't reach the local server.", "error");
      return;
    }
    // a failed delete must not look like success; refresh whatever was removed
    if (results.some((j) => j.ok)) await refresh();
    const failed = results.filter((j) => !j.ok);
    if (failed.length) {
      toast(failed.map((j) => j.error || "Could not delete entries.").join(" "), "error");
      return;
    }
    msg("");
  }

  async function refresh() {
    try {
      const [sr, er] = await Promise.all([
        fetch("/api/manual/series?days=180").then(readJsonResponse),
        fetch(`/api/manual/entries?from=${shiftDays(localToday(), -29)}&to=${localToday()}`).then(readJsonResponse),
      ]);
      if (sr.error || er.error) throw new Error(sr.error || er.error);
      UNIT = sr.unit || "kg";
      wtUnit.textContent = UNIT;
      ENTRIES = er.entries || [];
      renderSummary(sr);
      renderCharts(sr);
      renderRecent();
    } catch (err) {
      calChart.innerHTML = EMPTY;
      wtChart.innerHTML = EMPTY;
      wtLegend.hidden = true;
      calLegend.hidden = true;
      summary.hidden = true;
      recent.replaceChildren(el("p", "manual-empty", esc(`Could not load entries: ${err.message}`)));
    }
  }

  dateInput.value = localToday();
  $("manual-add").addEventListener("click", () => openModal());
  $("manual-form").addEventListener("submit", (e) => { e.preventDefault(); saveModal(); });
  $("manual-cancel").addEventListener("click", () => dlg.close());
  dlg.querySelector(".dd-close").addEventListener("click", () => dlg.close());
  dlg.addEventListener("click", (e) => { if (e.target === dlg) dlg.close(); });
  dateInput.addEventListener("change", fillFromEntries);
  refresh().then(fillFromEntries);
})();
