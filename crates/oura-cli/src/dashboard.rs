//! `oura dashboard` — a local web health dashboard following
//! `notes/dashboard-v2-brainstorm.md`.
//!
//! Rust owns the DB + every non-model calculation (per-night HRV/RHR/skin-temp,
//! SpO2 % via Oura's calibration, device/data-health, baselines + deltas, the
//! digest) and the HTTP server. The torch models (sleep hypnogram, activity, CVA)
//! run through the Python runners, shelled out exactly like `oura sessions`. All
//! data stays on this machine.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use oura_manual::{ManualEntry, ManualStore, MetricKind, UpsertOutcome};
use oura_summary::{
    bedtime_overrides_path, feature_modes_path, profile_path, write_bedtime_override, ModelInputs,
    ModelOutputs, ModelRunner,
};
pub use oura_summary::{read_profile, write_profile, Demographics};

const INDEX_HTML: &str = include_str!("../../../dashboard/web/index.html");
const STYLES_CSS: &str = include_str!("../../../dashboard/web/styles.css");
const APP_JS: &str = include_str!("../../../dashboard/web/app.js");


// ── python model orchestration (shared with `oura sessions` via `pyrunner`) ────
fn repo_root() -> Option<PathBuf> {
    crate::pyrunner::repo_root(Path::new("tools/run_activity_model.py"))
}

fn python_bin(root: &Path) -> PathBuf {
    crate::pyrunner::venv_python(root)
}

/// Bound model subprocesses and drain stdout concurrently (large results must not
/// fill the pipe while the parent waits). A stalled runner is killed and reaped.
fn model_output(
    mut command: Command,
    input: Option<&[u8]>,
    timeout: std::time::Duration,
) -> Option<Value> {
    use std::io::{Read, Write};
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(32 * 1024 * 1024)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    if let Some(input) = input {
        if child
            .stdin
            .take()
            .and_then(|mut pipe| pipe.write_all(input).ok())
            .is_none()
        {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return None;
        }
    }
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            _ => {
                tracing::warn!("model runner timed out or could not be monitored");
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let bytes = reader.join().ok()?.ok()?;
    if !status?.success() {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn run_py_json(root: &Path, py: &Path, script: &str, args: &[String]) -> Option<Value> {
    run_py_json_stdin_optional(root, py, script, args, None)
}

fn run_py_json_stdin(
    root: &Path,
    py: &Path,
    script: &str,
    args: &[String],
    input: &[u8],
) -> Option<Value> {
    run_py_json_stdin_optional(root, py, script, args, Some(input))
}

fn run_py_json_stdin_optional(
    root: &Path,
    py: &Path,
    script: &str,
    args: &[String],
    input: Option<&[u8]>,
) -> Option<Value> {
    let mut command = Command::new(py);
    command.current_dir(root).arg(root.join(script)).args(args);
    model_output(command, input, std::time::Duration::from_secs(180))
}

/// The web dashboard's [`ModelRunner`]: shells out to the Python torch runners,
/// exactly as before, and falls back to Rust signal-based sleep staging when the
/// Python environment or private `.pt` model file is unavailable on a user's PC.
struct PythonRunner;
impl ModelRunner for PythonRunner {
    fn run(&self, input: ModelInputs) -> ModelOutputs {
        let ModelInputs {
            db,
            tz,
            demo,
            sleep_ranges,
        } = input;
        let root = repo_root();
        let py = root.as_deref().map(python_bin);
        let sleep_stdin = serde_json::to_vec(sleep_ranges).unwrap_or_default();
        let sleep_args = vec![
            db.display().to_string(),
            tz.to_string(),
            "--json".into(),
            "--batch".into(),
        ];
        let cva_args = vec![
            db.display().to_string(),
            "--json".into(),
            "--sex".into(),
            demo.sex.to_string(),
            "--age".into(),
            demo.age.to_string(),
            "--height".into(),
            demo.height_m.to_string(),
            "--weight".into(),
            demo.weight_kg.to_string(),
            "--ring".into(),
            demo.ring_size.to_string(),
        ];
        let act_args = vec![
            db.display().to_string(),
            "--tz".into(),
            tz.to_string(),
            "--json".into(),
        ];
        let illness_args = vec![db.display().to_string(), tz.to_string(), "--json".into()];

        let (mut sleep_batch, cva, activity, illness) = match (root.as_deref(), py.as_deref()) {
            (Some(r), Some(p)) => std::thread::scope(|s| {
                let sh = s.spawn(|| {
                    run_py_json_stdin(r, p, "tools/run_sleep_model.py", &sleep_args, &sleep_stdin)
                });
                let ch = s.spawn(|| run_py_json(r, p, "tools/run_cva_model.py", &cva_args));
                let ah = s.spawn(|| run_py_json(r, p, "tools/run_activity_model.py", &act_args));
                let ih = s.spawn(|| run_py_json(r, p, "tools/run_illness_model.py", &illness_args));
                (
                    sh.join().ok().flatten(),
                    ch.join().ok().flatten(),
                    ah.join().ok().flatten(),
                    ih.join().ok().flatten(),
                )
            }),
            _ => (None, None, None, None),
        };
        if sleep_batch_needs_fallback(sleep_batch.as_ref(), sleep_ranges.len())
            && !sleep_ranges.is_empty()
        {
            let signals = oura_summary::stage_nights_from_signals(db, sleep_ranges);
            sleep_batch = merge_sleep_fallback(sleep_batch, signals);
        }
        ModelOutputs {
            sleep_batch,
            cva,
            activity,
            illness,
        }
    }
}

/// Whether the Python sleep batch must be topped up from the signal estimator.
///
/// `run_sleep_model.py --batch` returns one entry per sleep range, in order: a staged
/// dict, or `null` for a window it couldn't stage (no torch/models, or NSSA inputs
/// missing), still exiting 0. A top-up is needed when the batch is missing, the wrong
/// length (the per-range contract was broken), or carries any null night. Kept beside
/// [`merge_sleep_fallback`] so the "when to fall back" and the "what to replace" stay
/// in agreement — this predicate is the line whose all-or-nothing form was the bug.
fn sleep_batch_needs_fallback(batch: Option<&Value>, range_count: usize) -> bool {
    match batch.and_then(|v| v.as_array()) {
        None => true,
        Some(a) => a.len() != range_count || a.iter().any(Value::is_null),
    }
}

/// Fill the Python sleep batch's missing per-night entries with the signal-derived
/// fallback. `signals` is authoritative on night count and order (exactly one entry
/// per sleep range), so the merged result has one entry per range: the Python staging
/// where present, otherwise the signal estimate.
///
/// A Python `null` (window the model couldn't stage) is replaced by its signal
/// estimate — otherwise the night keeps its physiology but shows no sleep at all.
/// Nights Python staged are preserved. A Python entry with no signal counterpart
/// (length mismatch — not expected from the current producer) is dropped, since
/// without a range it cannot be mapped onto a night anyway; a missing Python entry is
/// simply the signal estimate. If signals are unavailable, the Python batch is
/// returned untouched.
fn merge_sleep_fallback(python: Option<Value>, signals: Option<Value>) -> Option<Value> {
    let Some(Value::Array(sig)) = signals else {
        return python;
    };
    let py = match python {
        Some(Value::Array(py)) => py,
        _ => Vec::new(),
    };
    let merged = sig
        .into_iter()
        .enumerate()
        .map(|(i, s)| match py.get(i) {
            Some(p) if !p.is_null() => p.clone(),
            _ => s,
        })
        .collect();
    Some(Value::Array(merged))
}

/// Web demographics use the manual panel's latest 28 observations in its default
/// 180-calendar-day window. Values stay in canonical kg, without display rounding.
struct EffectiveProfile {
    demo: Demographics,
    weight_sample_count: usize,
}

impl EffectiveProfile {
    fn to_json(&self) -> Value {
        let mut profile = self.demo.to_json();
        profile["weight_source"] = json!(if self.weight_sample_count > 0 {
            "manual_average"
        } else {
            "profile"
        });
        profile["weight_sample_count"] = json!(self.weight_sample_count);
        profile
    }
}

fn resolve_profile(
    mut demo: Demographics,
    manual: Option<&Mutex<ManualStore>>,
    today: i64,
) -> Result<EffectiveProfile> {
    let mut weight_sample_count = 0;
    if let Some(manual) = manual {
        let from = days_to_ymd(today - 179);
        let to = days_to_ymd(today);
        let entries = lock_manual(manual)
            .list_entries(Some(&from), Some(&to), Some(MetricKind::WeightKg))
            .context("reading manual weights for profile")?;
        let latest = &entries[entries.len().saturating_sub(28)..];
        weight_sample_count = latest.len();
        if !latest.is_empty() {
            demo.weight_kg = latest.iter().map(|entry| entry.value).sum::<f64>() / latest.len() as f64;
        }
    }
    Ok(EffectiveProfile {
        demo,
        weight_sample_count,
    })
}

fn effective_profile(
    db: &Path,
    manual: Option<&Mutex<ManualStore>>,
    today: i64,
) -> Result<EffectiveProfile> {
    resolve_profile(read_profile(db), manual, today)
}

fn save_effective_profile(
    db: &Path,
    manual: Option<&Mutex<ManualStore>>,
    today: i64,
    value: &Value,
) -> Result<EffectiveProfile> {
    let saved = read_profile(db);
    let mut effective = resolve_profile(saved, manual, today)?;
    let mut updated = Demographics::from_json(value);
    if value.get("ring_size").is_none() {
        updated.ring_size = saved.ring_size;
    }
    if effective.weight_sample_count > 0 {
        // The submitted weight is the displayed mean, not a new fallback value.
        updated.weight_kg = saved.weight_kg;
    }
    let updated = write_profile(db, &updated.to_json())?;
    let effective_weight = effective.demo.weight_kg;
    effective.demo = updated;
    if effective.weight_sample_count > 0 {
        effective.demo.weight_kg = effective_weight;
    }
    Ok(effective)
}

/// `build_summary` for the web dashboard — runs the models via Python.
fn build_summary(db: &Path, tz: f64, demo: &Demographics) -> Result<Value> {
    oura_summary::build_summary(db, tz, &PythonRunner, demo)
}

// ── summary cache ─────────────────────────────────────────────────────────────
// build_summary spawns torch subprocesses (~seconds); without a cache every page
// load re-pays that. We memoise the last result and reuse it until the inputs
// change — ring data, saved profile, effective manual weight, or other summary
// inputs. Profile edits and logged-weight changes both invalidate the cache.
struct SummaryCache {
    db: PathBuf,
    tz: f64,
    token: CacheToken,
    value: Arc<Value>,
}

type CacheToken = (
    Option<SystemTime>,
    Option<SystemTime>,
    Option<SystemTime>,
    Option<SystemTime>,
    i64,
    u64,
);

/// Every input's mtime, the store decoder version, and the effective weight — any
/// change rebuilds it. Covers a sync, profile/feature/bedtime edits, historical
/// decode migrations, and logged-weight changes (including window expiry).
fn summary_token(db: &Path, demo: &Demographics) -> CacheToken {
    (
        mtime(db),
        mtime(&profile_path(db)),
        mtime(&feature_modes_path(db)),
        mtime(&bedtime_overrides_path(db)),
        oura_store::storage::DECODER_VERSION,
        demo.weight_kg.to_bits(),
    )
}

fn summary_cache() -> &'static Mutex<Option<SummaryCache>> {
    static C: OnceLock<Mutex<Option<SummaryCache>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// Cached `build_summary`: recompute when persisted inputs or effective weight change.
fn cached_summary(
    db: &Path,
    tz: f64,
    manual: Option<&Mutex<ManualStore>>,
) -> Result<Arc<Value>> {
    let profile = effective_profile(db, manual, local_today_days(tz))?;
    let token = summary_token(db, &profile.demo);
    if let Some(c) = summary_cache().lock().unwrap().as_ref() {
        if c.db == db && c.tz == tz && c.token == token {
            return Ok(c.value.clone());
        }
    }
    // build outside the lock so concurrent first-loads don't serialise on it
    let value = Arc::new(build_summary(db, tz, &profile.demo)?);
    // re-stat the inputs: if any changed while we were building (e.g. a sync landed),
    // the result may predate that change — don't cache it, so the next request
    // rebuilds against the new state instead of serving stale data.
    let profile_after = effective_profile(db, manual, local_today_days(tz))?;
    let token_after = summary_token(db, &profile_after.demo);
    if token_after == token {
        let mut guard = summary_cache().lock().unwrap();
        // don't clobber a fresher entry a concurrent build already stored (compare
        // the DB mtime, which a sync advances)
        let newer_exists = guard
            .as_ref()
            .is_some_and(|c| c.db == db && c.tz == tz && c.token.0 > token.0);
        if !newer_exists {
            *guard = Some(SummaryCache {
                db: db.to_path_buf(),
                tz,
                token,
                value: value.clone(),
            });
        }
    }
    Ok(value)
}

// ── ingest wiring (hub DB) ───────────────────────────────────────────────────

/// The hub's SQLite path: `OURA_HUB_DB`, else the dashboard's own DB path. The
/// hub may run on a different host; a missing file is a state, never an error.
pub fn hub_db(db: &Path) -> PathBuf {
    std::env::var_os("OURA_HUB_DB")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| db.to_path_buf())
}

/// Flat hub `ingest::status` wrapped under `ingest`, with the dashboard's own
/// `state` and `models` beside it. Never touches the summary or its cache and
/// never 500s: `ingest::status` already reports a missing/inaccessible DB as a
/// state.
fn status_payload(hub_db: &Path) -> Value {
    let ingest = oura_hub::ingest::status(hub_db);
    // `summary.stale`: the last-good summary predates the newest decoded data.
    // No summary at all counts as stale, so the UI can say so honestly.
    let summary_ms = ingest["summary_unix_ms"].as_i64();
    let completed = ingest["processing"]["completed_revision"].as_i64();
    let stale = match (summary_ms, completed) {
        (None, _) => true,
        (Some(_), None) => true,
        (Some(_), Some(c)) => ingest["decoded_revision"]
            .as_i64()
            .is_some_and(|decoded| decoded > c),
    };
    json!({
        // `state` here is the dashboard service's own reachability marker; the
        // ingest vocabulary lives at `ingest.state` and drives the UI copy.
        "state": "ok",
        "summary": { "stale": stale },
        "models": models_status(),
        "ingest": ingest,
    })
}

/// Which torch model runners exist and whether their `.pt` files are present.
/// Availability is existence only — no accuracy or version claim.
fn models_status() -> Value {
    let dir = models_dir();
    let mut models = serde_json::Map::new();
    for (name, script) in [
        ("sleep", "tools/run_sleep_model.py"),
        ("cva", "tools/run_cva_model.py"),
        ("activity", "tools/run_activity_model.py"),
        ("illness", "tools/run_illness_model.py"),
    ] {
        // A runner script absent from the checkout means that metric has no
        // path at all; report only runners that exist.
        if !repo_root().is_some_and(|r| r.join(script).is_file()) {
            continue;
        }
        let available = MODEL_FILES
            .iter()
            .filter(|(runner, _)| *runner == name)
            .all(|(_, file)| dir.join(file).is_file());
        models.insert(name.into(), json!({ "available": available }));
    }
    Value::Object(models)
}

/// One `.pt` per runner, as shipped by the runners themselves (`MODEL_NAME`).
const MODEL_FILES: &[(&str, &str)] = &[
    ("sleep", "sleepstaging_2_6_0.pt"),
    ("cva", "cva_2_1_0.pt"),
    ("activity", "automatic_activity_detection_3_1_11.pt"),
    ("illness", "illness_detection_0_5_1.pt"),
];

/// Background analysis worker: drain pending ingest jobs through the real
/// Python runners every ~30s. `process_pending` owns claim/backoff/failure
/// recording; a build error returns `Err` and is logged, never swallowed and
/// never fatal to the server. Model-file mtime changes also bust the summary
/// cache, so a swapped model is picked up on the next request.

/// The models directory: `OURA_MODELS_DIR`, else `notes/models` under the
/// repo root. No root and no override means no models — availability false.
fn models_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("OURA_MODELS_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    repo_root().map(|r| r.join("notes/models")).unwrap_or_default()
}

async fn analysis_worker(db: PathBuf, tz: f64) {
    let hub_db = hub_db(&db);
    let mut last_models: Option<Vec<Option<SystemTime>>> = None;
    loop {
        let job_db = db.clone();
        let job_hub = hub_db.clone();
        let ran = tokio::task::spawn_blocking(move || {
            let demo = read_profile(&job_db);
            oura_hub::ingest::process_pending(&job_hub, |db| {
                build_summary(db, tz, &demo)
            })
        })
        .await;
        match ran {
            Ok(Ok(true)) => {
                // a durable job was attempted; the next page load must see it
                summary_cache().lock().unwrap().take();
            }
            Ok(Ok(false)) => {}
            Ok(Err(e)) => tracing::warn!("ingest analysis failed: {e}"),
            Err(e) => tracing::warn!("ingest analysis worker panicked: {e}"),
        }
        // model-file changes invalidate the summary cache (B2: installing or
        // changing a model must invalidate the relevant cache)
        let token: Vec<Option<SystemTime>> = MODEL_FILES
            .iter()
            .map(|(_, file)| mtime(&models_dir().join(file)))
            .collect();
        if last_models.replace(token.clone()).is_some_and(|old| old != token) {
            summary_cache().lock().unwrap().take();
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

pub async fn serve(
    port: u16,
    bind: String,
    db: PathBuf,
    tz: f64,
    seed: Demographics,
    manual_db: PathBuf,
    weight_unit: WeightUnit,
    public_origin: String,
) -> Result<()> {
    // Seed profile.json from the CLI demographics on first run; afterwards it
    // stores editable demographics and the fallback weight for empty manual history.
    if !profile_path(&db).exists() {
        let _ = write_profile(&db, &seed.to_json());
    }
    // Manual metrics live in their own SQLite file, opened once here and shared.
    // A failure must never stop the dashboard: log a warning and keep serving, so
    // every ring route (and the UI) still works without the manual store.
    let manual = match ManualStore::open(&manual_db) {
        Ok(store) => Some(Arc::new(Mutex::new(store))),
        Err(e) => {
            eprintln!(
                "warning: manual metric store unavailable at {} ({e}); manual endpoints are disabled",
                manual_db.display()
            );
            None
        }
    };
    // `--bind ${OURA_BIND}` expands to an empty argument when the environment file
    // was written before OURA_BIND existed. That is the loopback default, not a
    // configuration error.
    let bind = if bind.trim().is_empty() {
        IpAddr::V4(std::net::Ipv4Addr::LOCALHOST).to_string()
    } else {
        bind
    };
    let public = PublicOrigin::parse(&public_origin)?.map(Arc::new);
    let listener = TcpListener::bind((bind.as_str(), port))
        .await
        .with_context(|| format!("binding {bind}:{port}"))?;
    // A wildcard bind has no address to print; print where the data lives instead
    // of a literal the operator did not ask for.
    let shown = match bind.parse::<IpAddr>() {
        Ok(ip) if ip.is_unspecified() => "<this-host>".to_string(),
        _ => bind.clone(),
    };
    println!("open_oura dashboard running — open http://{shown}:{port} in your browser");
    // Background analysis worker: drains pending ingest jobs (~30s cadence)
    // through the real Python runners. Started after the listener binds so a
    // busy model run never delays the first page.
    tokio::spawn(analysis_worker(db.clone(), tz));
    loop {
        let (sock, _) = listener.accept().await?;
        let db = db.clone();
        let manual = manual.clone();
        let public = public.clone();
        tokio::spawn(async move {
            let _ = handle(sock, port, db, tz, manual, weight_unit, public).await;
        });
    }
}

/// Serve a web asset: prefer the on-disk file under `dashboard/web/` (so the UI
/// can be edited and refreshed without recompiling) and fall back to the copy
/// embedded at build time.
fn asset(name: &str, embedded: &'static str) -> Vec<u8> {
    if let Some(root) = repo_root() {
        if let Ok(bytes) = std::fs::read(root.join("dashboard/web").join(name)) {
            return bytes;
        }
    }
    embedded.as_bytes().to_vec()
}

/// Look up a `key` in a `a=b&c=d` query string, percent-decoding the value.
fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

/// Minimal application/x-www-form-urlencoded decode: `%XX` escapes and `+` → space.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = |c: u8| (c as char).to_digit(16);
                match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn header<'a>(req: &'a str, name: &str) -> Option<&'a str> {
    req.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// Split a raw request into its header block and body. A request without the
/// CRLFCRLF terminator has no body.
fn split_request(req: &str) -> (&str, &str) {
    match req.split_once("\r\n\r\n") {
        Some((head, body)) => (head, body),
        None => (req, ""),
    }
}

/// Largest request head (request line + headers) accepted before the body.
const MAX_HEAD_BYTES: usize = 16 * 1024;
/// Largest decoded request body. Dashboard payloads are small JSON objects.
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Longest chunk-size or trailer line accepted in a chunked body.
const MAX_CHUNK_LINE: usize = 1024;
/// Wall-clock budget to receive one complete request; bounds trickling clients.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a request was refused before routing. Each maps to one status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestError {
    Malformed,
    HeadTooLarge,
    TooLarge,
    Unsupported,
    Timeout,
}

impl RequestError {
    fn status(self) -> &'static str {
        match self {
            Self::Malformed => "400 Bad Request",
            Self::HeadTooLarge => "431 Request Header Fields Too Large",
            Self::TooLarge => "413 Payload Too Large",
            Self::Unsupported => "501 Not Implemented",
            Self::Timeout => "408 Request Timeout",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::Malformed => "malformed request",
            Self::HeadTooLarge => "request headers too large",
            Self::TooLarge => "request body too large",
            Self::Unsupported => "unsupported transfer coding; use Content-Length or chunked",
            Self::Timeout => "request not received in time",
        }
    }
}

/// How the body is framed. Exactly one of these applies to every request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    Empty,
    Length(usize),
    Chunked,
}

/// Every value of a header, in order. Repeats stay visible so the framing check
/// can refuse disagreeing `Content-Length` lines instead of picking one.
fn header_values<'a>(head: &'a str, name: &str) -> Vec<&'a str> {
    head.lines()
        .skip(1)
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
        })
        .collect()
}

/// Body framing from the header block. `Transfer-Encoding` must be exactly
/// `chunked`, and it may not coexist with `Content-Length` (request smuggling).
fn framing_of(head: &str) -> std::result::Result<Framing, RequestError> {
    let te = header_values(head, "transfer-encoding");
    let cl = header_values(head, "content-length");
    if !te.is_empty() && !cl.is_empty() {
        return Err(RequestError::Malformed);
    }
    if !te.is_empty() {
        return match te.as_slice() {
            [coding] if coding.eq_ignore_ascii_case("chunked") => Ok(Framing::Chunked),
            _ => Err(RequestError::Unsupported),
        };
    }
    match cl.as_slice() {
        [] => Ok(Framing::Empty),
        [len] if !len.is_empty() && len.bytes().all(|b| b.is_ascii_digit()) => {
            let len: usize = len.parse().map_err(|_| RequestError::TooLarge)?;
            if len > MAX_BODY_BYTES {
                Err(RequestError::TooLarge)
            } else {
                Ok(Framing::Length(len))
            }
        }
        _ => Err(RequestError::Malformed),
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn expects_continue(head: &str) -> bool {
    header(head, "expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue"))
}

/// Decode a complete chunked body from `data`. `Ok(None)` means the terminal
/// chunk has not arrived yet. Any framing violation is an error, never a guess.
fn decode_chunked(data: &[u8]) -> std::result::Result<Option<Vec<u8>>, RequestError> {
    if data.len() > MAX_BODY_BYTES * 4 {
        return Err(RequestError::TooLarge);
    }
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        let Some(eol) = find_subslice(&data[pos..], b"\r\n") else {
            return if data.len() - pos > MAX_CHUNK_LINE {
                Err(RequestError::Malformed)
            } else {
                Ok(None)
            };
        };
        let size_line = &data[pos..pos + eol];
        pos += eol + 2;
        // Chunk extensions (`;name=value`) are allowed and ignored.
        let size_bytes = size_line.split(|&b| b == b';').next().unwrap_or_default();
        let size_hex = std::str::from_utf8(size_bytes)
            .map_err(|_| RequestError::Malformed)?
            .trim_matches(|c| c == ' ' || c == '\t');
        if size_hex.is_empty() || size_hex.len() > 8 || !size_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(RequestError::Malformed);
        }
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| RequestError::Malformed)?;
        if size == 0 {
            // Trailer section: header-like lines, ended by an empty line. Ignored.
            loop {
                let Some(eol) = find_subslice(&data[pos..], b"\r\n") else {
                    return if data.len() - pos > MAX_CHUNK_LINE {
                        Err(RequestError::Malformed)
                    } else {
                        Ok(None)
                    };
                };
                pos += eol + 2;
                if eol == 0 {
                    return Ok(Some(out));
                }
            }
        }
        if out.len() + size > MAX_BODY_BYTES {
            return Err(RequestError::TooLarge);
        }
        if data.len() < pos + size + 2 {
            return Ok(None);
        }
        if data[pos + size..pos + size + 2] != *b"\r\n" {
            return Err(RequestError::Malformed);
        }
        out.extend_from_slice(&data[pos..pos + size]);
        pos += size + 2;
    }
}

async fn read_more(sock: &mut TcpStream, chunk: &mut [u8]) -> std::result::Result<usize, RequestError> {
    match sock.read(chunk).await {
        Ok(0) | Err(_) => Err(RequestError::Malformed),
        Ok(n) => Ok(n),
    }
}

/// Read one complete request and return it as header block, CRLFCRLF, then the
/// decoded body, so `split_request` works on the result unchanged.
///
/// Reads accumulate until the framing says the request is complete: TCP and
/// reverse proxies may split the head or body at any byte. Limits: head
/// [`MAX_HEAD_BYTES`], body [`MAX_BODY_BYTES`] decoded, whole request within
/// [`REQUEST_TIMEOUT`]. Only `Content-Length` and `Transfer-Encoding: chunked`
/// are accepted; other codings get 501, and conflicting framing gets 400.
async fn read_request(sock: &mut TcpStream) -> std::result::Result<String, RequestError> {
    tokio::time::timeout(REQUEST_TIMEOUT, read_request_framed(sock))
        .await
        .unwrap_or(Err(RequestError::Timeout))
}

async fn read_request_framed(sock: &mut TcpStream) -> std::result::Result<String, RequestError> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err(RequestError::HeadTooLarge);
        }
        let n = read_more(sock, &mut chunk).await?;
        buf.extend_from_slice(&chunk[..n]);
    };
    if head_end > MAX_HEAD_BYTES {
        return Err(RequestError::HeadTooLarge);
    }
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut pending = buf.split_off(head_end + 4);
    let framing = framing_of(&head)?;
    if framing != Framing::Empty && pending.is_empty() && expects_continue(&head) {
        sock.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .await
            .map_err(|_| RequestError::Malformed)?;
    }
    let body = match framing {
        Framing::Empty => Vec::new(),
        Framing::Length(len) => {
            while pending.len() < len {
                let n = read_more(sock, &mut chunk).await?;
                pending.extend_from_slice(&chunk[..n]);
            }
            pending.truncate(len);
            pending
        }
        Framing::Chunked => loop {
            if let Some(body) = decode_chunked(&pending)? {
                break body;
            }
            let n = read_more(sock, &mut chunk).await?;
            pending.extend_from_slice(&chunk[..n]);
        },
    };
    let mut req = head;
    req.push_str("\r\n\r\n");
    req.push_str(&String::from_utf8_lossy(&body));
    Ok(req)
}


/// Loopback-only guard (DNS-rebind protection), applied before any routing.
///
/// The dashboard answers only when the request's `Host` is the address the client
/// actually connected to — loopback unless `--bind` moved the listener — or the
/// literal `localhost`. Comparing against the connection's own local address
/// rather than against a fixed string is what lets a LAN bind work without
/// enumerating the host's interfaces, and it still refuses a rebound name: a page
/// served from `evil.example` sends `Host: evil.example`, which is not an IP
/// literal and matches nothing.
fn host_header_ok(host: Option<&str>, local_ip: Option<IpAddr>, port: u16) -> bool {
    let Some(host) = host else {
        return false;
    };
    // rsplit_once keeps an IPv6 literal whole: `[::1]:8090` -> ("[::1]", "8090").
    let Some((addr, got_port)) = host.rsplit_once(':') else {
        return false;
    };
    if got_port.parse::<u16>() != Ok(port) {
        return false;
    }
    if addr.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let addr = addr
        .strip_prefix('[')
        .and_then(|a| a.strip_suffix(']'))
        .unwrap_or(addr);
    let Ok(ip) = addr.parse::<IpAddr>() else {
        return false;
    };
    match local_ip {
        // 127.0.0.1 and ::1 are the same socket as far as the browser is concerned.
        Some(local) => ip == local || (ip.is_loopback() && local.is_loopback()),
        None => ip.is_loopback(),
    }
}

/// `host_header_ok`, or the `Host` of the configured public origin. A reverse proxy
/// connects from loopback and forwards the public name, so only that exact name is
/// added. `X-Forwarded-*` headers are never read.
fn host_allowed(
    host: Option<&str>,
    local_ip: Option<IpAddr>,
    port: u16,
    public: Option<&PublicOrigin>,
) -> bool {
    host_header_ok(host, local_ip, port)
        || host.is_some_and(|h| public.is_some_and(|p| p.matches_host(h)))
}

/// The browser-facing origin of a reverse proxy, from `--public-origin`.
///
/// Only `https://host[:port]` is accepted (no path, query, or userinfo), so the
/// guard compares exact strings. A default `:443` is elided, because a proxy's
/// `Host` header and a browser's `Origin` both omit it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicOrigin {
    origin: String,
    authority: String,
}

impl PublicOrigin {
    /// Parse the `--public-origin` value. Empty means no public origin.
    pub fn parse(raw: &str) -> Result<Option<Self>> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(None);
        }
        let rest = raw
            .strip_prefix("https://")
            .ok_or_else(|| anyhow!("--public-origin must start with https:// (got {raw:?})"))?;
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if rest
            .chars()
            .any(|c| matches!(c, '/' | '?' | '#' | '@') || c.is_whitespace() || c.is_control())
        {
            return Err(anyhow!(
                "--public-origin must be https://host[:port] with no path, query, or userinfo (got {raw:?})"
            ));
        }
        let (host, port_text) = if rest.starts_with('[') {
            let end = rest.find(']').ok_or_else(|| anyhow!("--public-origin has an unterminated IPv6 literal"))?;
            rest[1..end].parse::<std::net::Ipv6Addr>()
                .map_err(|_| anyhow!("--public-origin has an invalid IPv6 literal"))?;
            let suffix = &rest[end + 1..];
            let port = if suffix.is_empty() { None } else {
                Some(suffix.strip_prefix(':').ok_or_else(|| anyhow!("--public-origin has an invalid authority"))?)
            };
            (&rest[..=end], port)
        } else {
            match rest.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (rest, None),
            }
        };
        let port = port_text.map(|text| text.parse::<u16>()
            .map_err(|_| anyhow!("--public-origin has an invalid port (got {raw:?})"))).transpose()?;
        if host.is_empty() {
            return Err(anyhow!("--public-origin has no host (got {raw:?})"));
        }
        let host = host.to_ascii_lowercase();
        let authority = match port {
            None | Some(443) => host,
            Some(port) => format!("{host}:{port}"),
        };
        let origin = format!("https://{authority}");
        Ok(Some(Self { origin, authority }))
    }

    /// Whether a `Host` header names this public origin.
    fn matches_host(&self, host: &str) -> bool {
        host.eq_ignore_ascii_case(&self.authority)
    }

    /// Whether an `Origin` header is this public origin.
    fn matches_origin(&self, origin: &str) -> bool {
        origin.eq_ignore_ascii_case(&self.origin)
    }
}

/// Same-origin guard for mutating/action endpoints.
///
/// Evaluated on the header block **only**: a request body line such as
/// `X-Oura-Dash: 1` must never satisfy the guard. When the browser sends
/// `Origin`, it must match the (already loopback-validated) `Host`, or the configured public origin.
fn same_origin_write_ok(req: &str, public: Option<&PublicOrigin>) -> bool {
    let (head, _) = split_request(req);
    let origin_ok = match header(head, "origin") {
        None => true,
        Some(origin) => {
            header(head, "host").is_some_and(|host| origin == format!("http://{host}"))
                || public.is_some_and(|p| p.matches_origin(origin))
        }
    };
    origin_ok && header(head, "x-oura-dash").is_some()
}

// ── manual metrics (calories eaten + body weight, no ring required) ───────────
//
// Records live in their own `manual.db` (see `crates/oura-manual`), opened once at
// startup and shared behind one mutex. Storage is canonical (kcal, kg); the
// [`WeightUnit`] only changes what is rendered, and the UI converts back before it
// POSTs. A missing value is not zero: absent rows simply produce no chart point.

/// Pounds per kilogram (exact). Display conversion only — storage stays kg.
const LB_PER_KG: f64 = 2.204_622_621_848_775_7;

/// Body-weight display unit for the manual panels. Canonical storage is always kg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeightUnit {
    Kg,
    Lb,
}

impl WeightUnit {
    /// Parse a CLI/env spelling (case-insensitive; `lbs` is an accepted alias).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "kg" => Some(WeightUnit::Kg),
            "lb" | "lbs" => Some(WeightUnit::Lb),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            WeightUnit::Kg => "kg",
            WeightUnit::Lb => "lb",
        }
    }

    /// A stored kg value expressed in this display unit (unrounded).
    fn display_value(self, kg: f64) -> f64 {
        match self {
            WeightUnit::Kg => kg,
            WeightUnit::Lb => kg * LB_PER_KG,
        }
    }
}

/// Round to one decimal place — display only; storage keeps the full value.
fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Map a wire `kind` onto [`MetricKind`]. The canonical spelling and the short
/// UI alias are both accepted so the chart keys and the storage spelling can differ.
fn parse_kind(s: &str) -> Option<MetricKind> {
    match s {
        "calories_eaten" | "calories" => Some(MetricKind::CaloriesEaten),
        "weight_kg" | "weight" => Some(MetricKind::WeightKg),
        "steps" => Some(MetricKind::Steps),
        _ => None,
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Days since the Unix epoch at `tz_offset` hours, i.e. the local calendar day.
fn local_today_days(tz_offset: f64) -> i64 {
    (unix_now() + (tz_offset * 3600.0).round() as i64).div_euclid(86_400)
}

/// Civil date `YYYY-MM-DD` from days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_to_ymd(days: i64) -> String {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn entry_to_json(e: &ManualEntry) -> Value {
    json!({
        "id": e.id,
        "local_date": e.local_date,
        "kind": e.kind.as_str(),
        "value": e.value,
        "note": e.note,
        "source": e.source,
        "batch_id": e.batch_id,
        "created_at": e.created_at,
        "updated_at": e.updated_at,
    })
}

/// A 7-entry rolling mean over consecutive stored points. Emitted only where a
/// full window exists: fewer stored values are not a mean, and nothing is
/// invented to reach seven. Dates come from the window's last point.
fn rolling_mean<'a>(pts: &'a [(&'a str, f64)], window: usize) -> Vec<(&'a str, f64)> {
    if pts.len() < window {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(pts.len() - window + 1);
    for i in window - 1..pts.len() {
        let mean = pts[i + 1 - window..=i].iter().map(|(_, v)| v).sum::<f64>() / window as f64;
        out.push((pts[i].0, mean));
    }
    out
}

/// Four-week comparison block: the trailing 28-day average, the trailing 7-day
/// average, the four weekly averages oldest-to-newest, and the change from the
/// first week to the last. Empty when there are no stored points.
fn four_week_summary(pts: &[(&str, f64)], unit: Option<WeightUnit>) -> Value {
    if pts.is_empty() {
        return json!({});
    }
    let n = pts.len();
    let avg = |slice: &[(&str, f64)]| -> f64 {
        slice.iter().map(|(_, v)| v).sum::<f64>() / slice.len() as f64
    };
    let last28 = &pts[n.saturating_sub(28)..];
    let last7 = &pts[n.saturating_sub(7)..];
    let avg28 = avg(last28);
    let avg7 = avg(last7);
    let mut weekly = Vec::new();
    for w in 0..4 {
        let end = n.saturating_sub((3 - w) * 7);
        let start = end.saturating_sub(7);
        if start < end {
            let mean = avg(&pts[start..end]);
            let value = match unit {
                Some(u) => round1(u.display_value(mean)),
                None => round1(mean),
            };
            weekly.push(json!({ "week": w, "value": value }));
        }
    }
    let change = if weekly.len() >= 2 {
        let first = weekly[0]["value"].as_f64().unwrap_or(0.0);
        let last = weekly[weekly.len() - 1]["value"].as_f64().unwrap_or(0.0);
        Some(last - first)
    } else {
        None
    };
    let avg28_shown = match unit {
        Some(u) => round1(u.display_value(avg28)),
        None => round1(avg28),
    };
    let avg7_shown = match unit {
        Some(u) => round1(u.display_value(avg7)),
        None => round1(avg7),
    };
    json!({
        "avg_28": avg28_shown,
        "avg_7": avg7_shown,
        "weekly": weekly,
        "change": change.map(round1),
    })
}

/// The chart payload: canonical calories, weight in the display unit, imported
/// steps in their stored unit, the trailing 7-entry mean of the stored values,
/// and a four-week comparison summary for calories and weight (nothing
/// interpolated or filled).
fn manual_series_json(entries: &[ManualEntry], unit: WeightUnit) -> Value {
    let mut calories: Vec<(&str, f64)> = Vec::new();
    let mut steps = Vec::new();
    let mut weight_pts: Vec<(&str, f64)> = Vec::new();
    for e in entries {
        match e.kind {
            MetricKind::CaloriesEaten => calories.push((&e.local_date, e.value)),
            // Imported history only. The ring's own step figure is estimated from
            // MET bands and is served with the ring summary, not here.
            MetricKind::Steps => steps.push(json!({ "date": e.local_date, "value": e.value })),
            MetricKind::WeightKg => weight_pts.push((&e.local_date, e.value)),
        }
    }
    let calories_json = calories
        .iter()
        .map(|(d, v)| json!({ "date": d, "value": round1(*v) }))
        .collect::<Vec<_>>();
    let weight = weight_pts
        .iter()
        .map(|(d, kg)| json!({ "date": d, "value": round1(unit.display_value(*kg)) }))
        .collect::<Vec<_>>();
    const WINDOW: usize = 7;
    let calories_trend = rolling_mean(&calories, WINDOW)
        .into_iter()
        .map(|(d, v)| json!({ "date": d, "value": round1(v) }))
        .collect::<Vec<_>>();
    let weight_trend = rolling_mean(&weight_pts, WINDOW)
        .into_iter()
        .map(|(d, kg)| json!({ "date": d, "value": round1(unit.display_value(kg)) }))
        .collect::<Vec<_>>();
    json!({
        "calories": calories_json,
        "calories_trend": calories_trend,
        "calories_summary": four_week_summary(&calories, None),
        "steps": steps,
        "weight": weight,
        "weight_trend": weight_trend,
        "weight_summary": four_week_summary(&weight_pts, Some(unit)),
        "unit": unit.as_str(),
    })
}

/// One CSV field, RFC 4180 style: quoted only when it has to be.
fn csv_field(s: &str) -> String {
    if s.bytes().any(|b| matches!(b, b',' | b'"' | b'\n' | b'\r')) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn manual_csv(entries: &[ManualEntry]) -> String {
    let mut out = String::from("local_date,kind,value,note,source,created_at,updated_at\r\n");
    for e in entries {
        out.push_str(&format!(
            "{},{},{},{},{},{},{}\r\n",
            csv_field(&e.local_date),
            csv_field(e.kind.as_str()),
            csv_field(&e.value.to_string()),
            csv_field(e.note.as_deref().unwrap_or("")),
            csv_field(&e.source),
            e.created_at,
            e.updated_at,
        ));
    }
    out
}

/// Build a stored entry from a `POST /api/manual/entries` body. Rejection here is
/// only about shape; range/date rules live in the store's validator.
fn manual_entry_from_body(body: &str, now: i64) -> std::result::Result<ManualEntry, String> {
    let req: Value = serde_json::from_str(body.trim_end_matches('\0'))
        .map_err(|_| "request body must be a JSON object".to_string())?;
    let local_date = req["local_date"]
        .as_str()
        .ok_or("local_date is required (YYYY-MM-DD)")?
        .to_string();
    let kind = req["kind"]
        .as_str()
        .and_then(parse_kind)
        .ok_or("kind must be calories_eaten or weight_kg")?;
    let value = req["value"]
        .as_f64()
        .ok_or("value is required and must be a number")?;
    Ok(ManualEntry {
        id: 0,
        local_date,
        kind,
        value,
        note: req["note"].as_str().map(str::to_string),
        source: "manual".to_string(),
        batch_id: None,
        created_at: now,
        updated_at: now,
    })
}

/// Lock the shared store, tolerating a poisoned mutex (the data is still fine).
fn lock_manual(store: &Mutex<ManualStore>) -> std::sync::MutexGuard<'_, ManualStore> {
    store.lock().unwrap_or_else(|e| e.into_inner())
}

/// A JSON response with an explicit status (`json_resp` is always 200).
async fn json_status(sock: &mut TcpStream, status: &str, v: &Value) -> Result<()> {
    write_resp(sock, status, "application/json", serde_json::to_vec(v)?.as_slice()).await
}

/// The manual store could not be opened at startup; say so instead of failing.
async fn manual_unavailable(sock: &mut TcpStream) -> Result<()> {
    json_status(
        sock,
        "503 Service Unavailable",
        &json!({ "ok": false, "error": "manual store unavailable; the dashboard started without it" }),
    )
    .await
}

/// Upsert one entry. `400` for a shape or validation failure, `500` for storage.
fn manual_post(store: &ManualStore, body: &str) -> (&'static str, Value) {
    let entry = match manual_entry_from_body(body, unix_now()) {
        Ok(entry) => entry,
        Err(msg) => return ("400 Bad Request", json!({ "ok": false, "error": msg })),
    };
    match store.upsert_entry(&entry) {
        Ok(outcome) => {
            let outcome = match outcome {
                UpsertOutcome::Inserted => "inserted",
                UpsertOutcome::Updated => "updated",
            };
            // Read back so the response carries the stored row and its provenance.
            let stored = store
                .get_entry(&entry.local_date, entry.kind)
                .ok()
                .flatten()
                .map(|e| entry_to_json(&e))
                .unwrap_or(Value::Null);
            ("200 OK", json!({ "ok": true, "outcome": outcome, "entry": stored }))
        }
        Err(oura_manual::Error::Validation(e)) => {
            ("400 Bad Request", json!({ "ok": false, "error": e.to_string() }))
        }
        Err(e) => ("500 Internal Server Error", json!({ "ok": false, "error": e.to_string() })),
    }
}

/// List entries, inclusive on both bounds. An empty store is `{"entries":[]}`.
fn manual_list(
    store: &ManualStore,
    from: Option<&str>,
    to: Option<&str>,
    kind: Option<MetricKind>,
) -> (&'static str, Value) {
    match store.list_entries(from, to, kind) {
        Ok(entries) => {
            let rows: Vec<Value> = entries.iter().map(entry_to_json).collect();
            ("200 OK", json!({ "entries": rows }))
        }
        Err(e) => ("500 Internal Server Error", json!({ "ok": false, "error": e.to_string() })),
    }
}

fn manual_delete(
    store: &ManualStore,
    date: Option<&str>,
    kind: Option<MetricKind>,
) -> (&'static str, Value) {
    let (Some(date), Some(kind)) = (date, kind) else {
        return (
            "400 Bad Request",
            json!({ "ok": false, "error": "date (YYYY-MM-DD) and kind are required" }),
        );
    };
    match store.delete_entry(date, kind) {
        Ok(deleted) => ("200 OK", json!({ "ok": true, "deleted": deleted })),
        Err(e) => ("500 Internal Server Error", json!({ "ok": false, "error": e.to_string() })),
    }
}

#[cfg(test)]
mod request_guard_tests {
    use super::*;

    #[test]
    fn a_body_line_cannot_satisfy_the_header_guard() {
        // The regression this guards: `header()` used to scan the whole request,
        // so a body line named like the CSRF header passed the check.
        let req = "POST /api/sync HTTP/1.1\r\nHost: localhost:8090\r\n\r\nX-Oura-Dash: 1\r\n";
        assert!(
            !same_origin_write_ok(req, None),
            "a request body must not be able to satisfy the guard"
        );
    }

    #[test]
    fn a_real_header_satisfies_the_guard() {
        let req = "POST /api/sync HTTP/1.1\r\nHost: localhost:8090\r\nX-Oura-Dash: 1\r\n\r\n";
        assert!(same_origin_write_ok(req, None));
    }

    #[test]
    fn a_missing_header_is_refused() {
        let req = "GET /api/ring-key HTTP/1.1\r\nHost: localhost:8090\r\n\r\n";
        assert!(!same_origin_write_ok(req, None));
    }

    #[test]
    fn header_names_are_case_insensitive() {
        let req = "POST /api/sync HTTP/1.1\r\nHOST: localhost:8090\r\nx-oura-dash: 1\r\n\r\n";
        assert!(same_origin_write_ok(req, None));
    }

    #[test]
    fn a_matching_origin_is_accepted() {
        let req = "POST /api/sync HTTP/1.1\r\nHost: localhost:8090\r\nOrigin: http://localhost:8090\r\nX-Oura-Dash: 1\r\n\r\n";
        assert!(same_origin_write_ok(req, None));
    }

    #[test]
    fn a_foreign_origin_is_refused() {
        let req = "POST /api/sync HTTP/1.1\r\nHost: localhost:8090\r\nOrigin: http://evil.example\r\nX-Oura-Dash: 1\r\n\r\n";
        assert!(!same_origin_write_ok(req, None));
    }

    #[test]
    fn a_foreign_origin_in_the_body_is_ignored() {
        let req = "POST /api/sync HTTP/1.1\r\nHost: localhost:8090\r\nX-Oura-Dash: 1\r\n\r\nOrigin: http://evil.example\r\n";
        assert!(same_origin_write_ok(req, None), "only the header block counts");
    }

    #[test]
    fn a_request_without_a_terminator_has_no_body() {
        let (head, body) = split_request("GET / HTTP/1.1\r\nHost: localhost:8090\r\n");
        assert!(head.starts_with("GET / HTTP/1.1"));
        assert_eq!(body, "");
    }

    // ── host guard ────────────────────────────────────────────────────────────
    //
    // `Host` must be the address the client actually connected to, which is
    // loopback unless the operator moved the listener with --bind.

    const LOOPBACK: Option<IpAddr> =
        Some(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    const LAN: Option<IpAddr> = Some(IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 50)));

    #[test]
    fn a_loopback_bind_accepts_its_own_address_and_localhost() {
        for host in ["127.0.0.1:8090", "localhost:8090", "LOCALHOST:8090", "[::1]:8090"] {
            assert!(host_header_ok(Some(host), LOOPBACK, 8090), "{host} is local");
        }
    }

    #[test]
    fn a_rebound_name_is_refused() {
        // A page served from evil.example sends its own name, not an address.
        assert!(!host_header_ok(Some("evil.example:8090"), LOOPBACK, 8090));
        assert!(
            !host_header_ok(Some("evil.example:8090"), LAN, 8090),
            "the guard does not depend on the bind address"
        );
    }

    #[test]
    fn a_lan_bind_accepts_the_address_the_client_used() {
        assert!(host_header_ok(Some("192.168.1.50:8090"), LAN, 8090));
        assert!(
            !host_header_ok(Some("192.168.1.51:8090"), LAN, 8090),
            "another address on the same port is not this socket"
        );
        assert!(
            !host_header_ok(Some("127.0.0.1:8090"), LAN, 8090),
            "loopback is only accepted when the connection arrived on loopback"
        );
    }

    #[test]
    fn the_port_must_match() {
        assert!(host_header_ok(Some("localhost:8090"), LOOPBACK, 8090));
        assert!(!host_header_ok(Some("localhost:8091"), LOOPBACK, 8090));
    }

    #[test]
    fn a_missing_or_unparseable_host_is_refused() {
        assert!(!host_header_ok(None, LOOPBACK, 8090));
        assert!(!host_header_ok(Some("localhost"), LOOPBACK, 8090));
        assert!(!host_header_ok(Some("localhost:http"), LOOPBACK, 8090));
        assert!(!host_header_ok(Some(""), LOOPBACK, 8090));
    }

    // ── framing and public origin ─────────────────────────────────────────────

    #[test]
    fn chunked_body_is_reassembled_across_chunks() {
        let wire = b"4\r\nWiki\r\n5;ext=1\r\npedia\r\n0\r\nX-Trailer: 1\r\n\r\n";
        assert_eq!(decode_chunked(wire).unwrap().as_deref(), Some(&b"Wikipedia"[..]));
    }

    #[test]
    fn an_incomplete_chunked_body_asks_for_more() {
        assert_eq!(decode_chunked(b"4\r\nWi").unwrap(), None);
        assert_eq!(decode_chunked(b"4\r\nWiki\r\n").unwrap(), None);
        assert_eq!(decode_chunked(b"4\r\nWiki\r\n0\r\n").unwrap(), None);
    }

    #[test]
    fn malformed_chunk_framing_is_refused() {
        assert_eq!(decode_chunked(b"zz\r\nab\r\n"), Err(RequestError::Malformed));
        assert_eq!(decode_chunked(b"2\r\nabXY0\r\n\r\n"), Err(RequestError::Malformed));
    }

    #[test]
    fn conflicting_or_unknown_framing_is_refused() {
        let both = "POST /x HTTP/1.1\r\nContent-Length: 4\r\nTransfer-Encoding: chunked";
        assert_eq!(framing_of(both), Err(RequestError::Malformed));
        let twice = "POST /x HTTP/1.1\r\nContent-Length: 4\r\nContent-Length: 9";
        assert_eq!(framing_of(twice), Err(RequestError::Malformed));
        let gzip = "POST /x HTTP/1.1\r\nTransfer-Encoding: gzip, chunked";
        assert_eq!(framing_of(gzip), Err(RequestError::Unsupported));
        let big = format!("POST /x HTTP/1.1\r\nContent-Length: {}", MAX_BODY_BYTES + 1);
        assert_eq!(framing_of(&big), Err(RequestError::TooLarge));
        assert_eq!(
            framing_of("POST /x HTTP/1.1\r\nContent-Length: 12"),
            Ok(Framing::Length(12))
        );
    }

    #[tokio::test]
    async fn a_body_split_across_writes_is_read_whole() {
        // Regression: the request used to be read with one `read`, so a body that
        // arrived in a later packet was missing and the handler answered 400.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = r#"{"value":1.5}"#;
        let head = format!(
            "POST /api/manual/entries HTTP/1.1\r\nHost: localhost:8090\r\nX-Oura-Dash: 1\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let (first, rest) = body.split_at(5);
        let client = async {
            let mut c = TcpStream::connect(addr).await.unwrap();
            c.write_all(format!("{head}{first}").as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(150)).await;
            c.write_all(rest.as_bytes()).await.unwrap();
        };
        let server = async {
            let (mut sock, _) = listener.accept().await.unwrap();
            read_request(&mut sock).await
        };
        let ((), req) = tokio::join!(client, server);
        let req = req.unwrap();
        assert_eq!(split_request(&req).1, body);
    }

    #[test]
    fn public_origin_supports_bracketed_ipv6() {
        let p = PublicOrigin::parse("https://[::1]").unwrap().unwrap();
        assert!(p.matches_host("[::1]"));
        assert!(p.matches_origin("https://[::1]"));
        let p = PublicOrigin::parse("https://[::1]:8443").unwrap().unwrap();
        assert!(p.matches_host("[::1]:8443"));
        assert!(PublicOrigin::parse("https://[::1").is_err());
        assert!(PublicOrigin::parse("https://[not-an-ip]").is_err());
    }

    #[test]
    fn public_origin_accepts_only_the_https_origin_form() {
        let p = PublicOrigin::parse("https://Health.Example.com:443/").unwrap().unwrap();
        assert!(host_allowed(Some("health.example.com"), LOOPBACK, 8090, Some(&p)));
        assert!(same_origin_write_ok(
            "POST /api/sync HTTP/1.1\r\nHost: health.example.com\r\nOrigin: https://health.example.com\r\nX-Oura-Dash: 1\r\n\r\n",
            Some(&p)
        ));
        assert!(!host_allowed(Some("evil.example"), LOOPBACK, 8090, Some(&p)));
        assert!(!same_origin_write_ok(
            "POST /api/sync HTTP/1.1\r\nHost: health.example.com\r\nOrigin: https://evil.example\r\nX-Oura-Dash: 1\r\n\r\n",
            Some(&p)
        ));
        assert_eq!(PublicOrigin::parse("").unwrap(), None);
        assert!(PublicOrigin::parse("http://health.example.com").is_err());
        assert!(PublicOrigin::parse("https://health.example.com/dash").is_err());
        assert!(PublicOrigin::parse("https://health.example.com?x=1").is_err());
    }
}

async fn write_resp(sock: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) -> Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body).await?;
    Ok(())
}

async fn json_resp(sock: &mut TcpStream, v: &Value) -> Result<()> {
    write_resp(
        sock,
        "200 OK",
        "application/json",
        serde_json::to_vec(v)?.as_slice(),
    )
    .await
}

async fn handle(
    mut sock: TcpStream,
    port: u16,
    db: PathBuf,
    tz: f64,
    manual: Option<Arc<Mutex<ManualStore>>>,
    weight_unit: WeightUnit,
    public: Option<Arc<PublicOrigin>>,
) -> Result<()> {
    // Header block and body, read to the framing's end (see `read_request`).
    let req = match read_request(&mut sock).await {
        Ok(req) => req,
        Err(e) => {
            let body = json!({ "ok": false, "error": e.message() });
            let _ = json_status(&mut sock, e.status(), &body).await;
            return Ok(());
        }
    };
    let method = req.split_whitespace().next().unwrap_or("GET");
    let target = req.split_whitespace().nth(1).unwrap_or("/");
    // route on the path only — a query string (e.g. cache-buster) must not 404 the API
    let path = target.split('?').next().unwrap_or(target);
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    // Header block and body, split once. A request body must never be able to
    // satisfy a header check — see `same_origin_write_ok`.
    let (head, body) = split_request(&req);

    // Loopback-only unless --bind says otherwise (DNS-rebind guard). The
    // connection's own local address is what the client put in `Host`, so that is
    // what it is compared against.
    let local_ip = sock.local_addr().ok().map(|a| a.ip());
    if !host_allowed(header(head, "host"), local_ip, port, public.as_deref()) {
        return write_resp(&mut sock, "403 Forbidden", "text/plain", b"forbidden").await;
    }
    // vendored SVG icons (Phosphor). Embedded at build time, so the packaged
    // binary serves them with no checkout on disk; the on-disk copy still wins
    // when `repo_root()` finds the repository, keeping the UI editable.
    if path.starts_with("/icons/") && path.ends_with(".svg") && !path.contains("..") {
        let name = path.trim_start_matches("/icons/");
        if let Some(root) = repo_root() {
            if let Ok(bytes) = std::fs::read(root.join("dashboard/web/icons").join(name)) {
                return write_resp(&mut sock, "200 OK", "image/svg+xml", &bytes).await;
            }
        }
        if let Some(bytes) = crate::icons::get(name) {
            return write_resp(&mut sock, "200 OK", "image/svg+xml", bytes).await;
        }
        return write_resp(&mut sock, "404 Not Found", "text/plain", b"not found").await;
    }

    // CSRF guard for mutating/action endpoints: a same-origin fetch can set this
    // header; an <img>/<form> or cross-origin fetch cannot (CORS preflight we never approve).
    let csrf_ok = same_origin_write_ok(&req, public.as_deref());
    let forbid = matches!(
        (method, path),
        ("POST", "/api/profile")
            | ("POST", "/api/research-marker")
            | ("POST", "/api/bedtime")
            | ("POST", "/api/manual/entries")
            | ("DELETE", "/api/manual/entries")
    ) && !csrf_ok;
    if forbid {
        return write_resp(&mut sock, "403 Forbidden", "text/plain", b"forbidden").await;
    }

    let effective_tz = if tz == 0.0 {
        query_param(query, "tz")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|h| h.is_finite() && (-24.0..=24.0).contains(h))
            .unwrap_or(tz)
    } else {
        tz
    };
    match (method, path) {
        (_, "/") | (_, "/index.html") => {
            write_resp(
                &mut sock,
                "200 OK",
                "text/html; charset=utf-8",
                &asset("index.html", INDEX_HTML),
            )
            .await
        }
        (_, "/styles.css") => {
            write_resp(
                &mut sock,
                "200 OK",
                "text/css; charset=utf-8",
                &asset("styles.css", STYLES_CSS),
            )
            .await
        }
        (_, "/app.js") => {
            write_resp(
                &mut sock,
                "200 OK",
                "text/javascript; charset=utf-8",
                &asset("app.js", APP_JS),
            )
            .await
        }
        ("GET", "/api/status") => {
            // Independent of summary success: flat hub status wrapped under
            // `ingest`, plus the dashboard's own state/models. Read-only; a
            // missing or unreadable hub DB is a state inside the payload.
            let body = status_payload(&hub_db(&db));
            json_resp(&mut sock, &body).await
        }
        ("GET", "/api/summary") => {
            // building the summary shells out to torch models → off the async
            // executor; cached so only the first load (or post-sync/edit) pays for it.
            let body = tokio::task::spawn_blocking(move || {
                cached_summary(&db, effective_tz, manual.as_deref())
            })
            .await
            .map_err(|e| anyhow!(e))?;
            match body {
                Ok(v) => json_resp(&mut sock, &v).await,
                Err(e) => {
                    // The summary's own error strings ("opening DB", "no decoded
                    // events") describe a database that is absent or empty. Those
                    // are states, not faults: say so plainly instead of leaking the
                    // upstream context() text into the headline.
                    let msg = e.to_string();
                    let clear = if msg.contains("opening DB") || msg.contains("no decoded events") {
                        "No ring data yet. Connect a ring and run `oura sync`."
                    } else {
                        msg.as_str()
                    };
                    json_resp(&mut sock, &json!({ "error": clear })).await
                }
            }
        }
        ("GET", "/api/hourly-hr") => {
            // HR bars per `minutes` slot (default hourly, the iOS HeartRate screen's
            // data); `days` = 0 → all of them, the day page picks out the day it shows
            let days = query_param(query, "days")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let minutes = query_param(query, "minutes")
                .and_then(|v| v.parse().ok())
                .unwrap_or(60);
            let body = tokio::task::spawn_blocking(move || {
                oura_summary::hourly_hr::hr_bins(&db, effective_tz, days, minutes)
            })
            .await
            .map_err(|e| anyhow!(e))?;
            match body {
                Ok(v) => json_resp(&mut sock, &v).await,
                Err(e) => {
                    let msg = e.to_string();
                    let clear = if msg.contains("opening DB") || msg.contains("no decoded events") {
                        "No ring data yet. Connect a ring and run `oura sync`."
                    } else {
                        msg.as_str()
                    };
                    json_resp(&mut sock, &json!({ "error": clear })).await
                }
            }
        }
        ("GET", "/api/research-export") => {
            // ZIP of raw per-beat / per-window CSVs for [start, end) — the same
            // `research_export` the iOS app and `oura research-export` use.
            let num = |k: &str| query_param(query, k).and_then(|v| v.parse::<f64>().ok());
            let (Some(start), Some(end)) = (num("start"), num("end")) else {
                return write_resp(&mut sock, "400 Bad Request", "text/plain", b"start and end (unix seconds) are required").await;
            };
            let streams: Vec<_> = query_param(query, "streams")
                .unwrap_or_default()
                .split(',')
                .filter_map(oura_summary::research_export::Stream::from_id)
                .collect();
            let tz = num("tz").unwrap_or(effective_tz);
            let built = tokio::task::spawn_blocking(move || {
                use oura_summary::research_export as rx;
                let export = rx::build(&db, &rx::Request { start_unix: start, end_unix: end, streams, tz_offset_hours: tz })?;
                let name = rx::bundle_name(start, tz);
                Ok::<_, anyhow::Error>((rx::to_zip(&export, &name)?, name))
            })
            .await
            .map_err(|e| anyhow!(e))?;
            match built {
                Ok((zip, name)) => {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Disposition: attachment; filename=\"{name}.zip\"\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
                        zip.len()
                    );
                    sock.write_all(head.as_bytes()).await?;
                    sock.write_all(&zip).await?;
                    Ok(())
                }
                Err(e) => {
                    let msg = e.to_string();
                    let clear = if msg.contains("opening DB") || msg.contains("no decoded events") {
                        "No ring data yet. Connect a ring and run `oura sync`."
                    } else {
                        msg.as_str()
                    };
                    write_resp(&mut sock, "400 Bad Request", "text/plain", clear.as_bytes()).await
                }
            }
        }
        ("POST", "/api/research-marker") => {
            let label = serde_json::from_str::<Value>(body)
                .ok()
                .and_then(|v| v["label"].as_str().map(str::to_owned))
                .unwrap_or_default();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as f64 / 1000.0)
                .unwrap_or(0.0);
            match oura_summary::research_export::add_marker(&db, now, &label) {
                Ok(()) => json_resp(&mut sock, &json!({ "ok": true, "unix": now })).await,
                Err(e) => json_resp(&mut sock, &json!({ "error": e.to_string() })).await,
            }
        }
        ("GET", "/api/profile") => {
            match effective_profile(&db, manual.as_deref(), local_today_days(effective_tz)) {
                Ok(profile) => json_resp(&mut sock, &profile.to_json()).await,
                Err(e) => json_status(
                    &mut sock,
                    "500 Internal Server Error",
                    &json!({ "error": e.to_string() }),
                ).await,
            }
        }
        ("POST", "/api/profile") => {
            match serde_json::from_str::<Value>(body.trim_end_matches('\0')) {
                Ok(v) => match save_effective_profile(
                    &db,
                    manual.as_deref(),
                    local_today_days(effective_tz),
                    &v,
                ) {
                    Ok(profile) => json_resp(&mut sock, &profile.to_json()).await,
                    Err(e) => json_status(
                        &mut sock,
                        "500 Internal Server Error",
                        &json!({ "error": e.to_string() }),
                    ).await,
                },
                Err(e) => json_resp(&mut sock, &json!({ "error": e.to_string() })).await,
            }
        }
        ("POST", "/api/bedtime") => {
            let req =
                serde_json::from_str::<Value>(body.trim_end_matches('\0')).unwrap_or(Value::Null);
            let raw_start_ds = req["raw_start_ds"].as_i64();
            let captured_unix = req["captured_unix"].as_i64();
            let reset = req["reset"].as_bool().unwrap_or(false);
            let (start_ds, end_ds) = if reset {
                (None, None)
            } else {
                match (req["start_ds"].as_i64(), req["end_ds"].as_i64()) {
                    (Some(s), Some(e)) => (Some(s), Some(e)),
                    _ => {
                        return json_resp(
                            &mut sock,
                            &json!({ "ok": false, "error": "start_ds and end_ds are required" }),
                        )
                        .await;
                    }
                }
            };
            let Some(raw_start_ds) = raw_start_ds else {
                return json_resp(
                    &mut sock,
                    &json!({ "ok": false, "error": "raw_start_ds is required" }),
                )
                .await;
            };
            match write_bedtime_override(&db, raw_start_ds, captured_unix, start_ds, end_ds) {
                Ok(v) => json_resp(&mut sock, &v).await,
                Err(e) => {
                    json_resp(&mut sock, &json!({ "ok": false, "error": e.to_string() })).await
                }
            }
        }
        ("GET", "/api/manual/entries") => {
            let Some(store) = manual.as_ref() else {
                return manual_unavailable(&mut sock).await;
            };
            let kind = match query_param(query, "kind").as_deref() {
                None => None,
                Some(k) => match parse_kind(k) {
                    Some(k) => Some(k),
                    None => {
                        return json_status(
                            &mut sock,
                            "400 Bad Request",
                            &json!({ "ok": false, "error": "kind must be calories_eaten or weight_kg" }),
                        )
                        .await
                    }
                },
            };
            let from = query_param(query, "from");
            let to = query_param(query, "to");
            let (status, body) = {
                let store = lock_manual(store);
                manual_list(&store, from.as_deref(), to.as_deref(), kind)
            };
            json_status(&mut sock, status, &body).await
        }
        ("POST", "/api/manual/entries") => {
            let Some(store) = manual.as_ref() else {
                return manual_unavailable(&mut sock).await;
            };
            let (status, body) = {
                let store = lock_manual(store);
                manual_post(&store, body)
            };
            json_status(&mut sock, status, &body).await
        }
        ("DELETE", "/api/manual/entries") => {
            let Some(store) = manual.as_ref() else {
                return manual_unavailable(&mut sock).await;
            };
            let date = query_param(query, "date");
            let kind = match query_param(query, "kind").as_deref() {
                None => None,
                Some(k) => match parse_kind(k) {
                    Some(k) => Some(k),
                    None => {
                        return json_status(
                            &mut sock,
                            "400 Bad Request",
                            &json!({ "ok": false, "error": "kind must be calories_eaten or weight_kg" }),
                        )
                        .await
                    }
                },
            };
            let (status, body) = {
                let store = lock_manual(store);
                manual_delete(&store, date.as_deref(), kind)
            };
            json_status(&mut sock, status, &body).await
        }
        ("GET", "/api/manual/series") => {
            let Some(store) = manual.as_ref() else {
                return manual_unavailable(&mut sock).await;
            };
            // `days` is the window that ends today (server-local, from the resolved
            // tz offset); clamped so a stray query value cannot ask for the world.
            let days = query_param(query, "days")
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(180)
                .clamp(1, 3650);
            let today = local_today_days(effective_tz);
            let from = days_to_ymd(today - (days - 1));
            let to = days_to_ymd(today);
            let rows = {
                let store = lock_manual(store);
                store.list_entries(Some(&from), Some(&to), None)
            };
            match rows {
                Ok(entries) => json_resp(&mut sock, &manual_series_json(&entries, weight_unit)).await,
                Err(e) => {
                    json_status(
                        &mut sock,
                        "500 Internal Server Error",
                        &json!({ "ok": false, "error": e.to_string() }),
                    )
                    .await
                }
            }
        }
        ("GET", "/api/manual/export") => {
            let Some(store) = manual.as_ref() else {
                return manual_unavailable(&mut sock).await;
            };
            let rows = {
                let store = lock_manual(store);
                store.export_all()
            };
            match rows {
                Ok(entries) => {
                    let csv = manual_csv(&entries);
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/csv; charset=utf-8\r\nContent-Disposition: attachment; filename=\"manual-entries.csv\"\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
                        csv.len()
                    );
                    sock.write_all(head.as_bytes()).await?;
                    sock.write_all(csv.as_bytes()).await?;
                    Ok(())
                }
                Err(e) => {
                    json_status(
                        &mut sock,
                        "500 Internal Server Error",
                        &json!({ "ok": false, "error": e.to_string() }),
                    )
                    .await
                }
            }
        }
        _ => write_resp(&mut sock, "404 Not Found", "text/plain", b"not found").await,
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_fills_only_null_nights_and_keeps_python_stages() {
        // Python staged nights 0 and 2 but emitted null for night 1 (NSSA could not
        // stage it, or no torch). The fallback must fill exactly night 1 from signals
        // and leave the Python results untouched.
        let python = json!([{"source": "nssa", "stages": [1, 2]}, null, {"source": "nssa"}]);
        let signals = json!([{"source": "signals"}, {"source": "signals", "stages": [2, 3]}, {"source": "signals"}]);
        let out = merge_sleep_fallback(Some(python), Some(signals)).unwrap();
        assert_eq!(
            out,
            json!([
                {"source": "nssa", "stages": [1, 2]},
                {"source": "signals", "stages": [2, 3]},
                {"source": "nssa"},
            ])
        );
    }

    #[test]
    fn fallback_takes_whole_signal_array_when_python_absent_or_empty() {
        let signals = json!([{"source": "signals"}]);
        // Python crashed (no torch) → None.
        assert_eq!(
            merge_sleep_fallback(None, Some(signals.clone())),
            Some(signals.clone())
        );
        // Python returned an empty batch.
        assert_eq!(
            merge_sleep_fallback(Some(json!([])), Some(signals.clone())),
            Some(signals)
        );
    }

    #[test]
    fn fallback_length_follows_signals_one_per_range() {
        // Signals are authoritative on night count/order (one per range). A Python
        // batch shorter than signals must not drop the uncovered nights, and a Python
        // batch longer must not append orphans that map to no range.
        let signals = json!([{"n": 0}, {"n": 1}]);
        // Python shorter (only night 0 staged): night 1 comes from signals.
        assert_eq!(
            merge_sleep_fallback(Some(json!([{"py": 0}])), Some(signals.clone())),
            Some(json!([{"py": 0}, {"n": 1}]))
        );
        // Python longer: the orphan third entry is dropped.
        assert_eq!(
            merge_sleep_fallback(
                Some(json!([null, {"py": 1}, {"orphan": true}])),
                Some(signals)
            ),
            Some(json!([{"n": 0}, {"py": 1}]))
        );
    }

    #[test]
    fn fallback_is_a_noop_when_signals_unavailable() {
        // No events to stage from: keep whatever Python returned rather than blanking.
        let python = json!([null]);
        assert_eq!(
            merge_sleep_fallback(Some(python.clone()), None),
            Some(python)
        );
    }

    #[test]
    fn needs_fallback_fires_on_null_missing_or_wrong_length_only() {
        // The gate that was the bug: a fully-staged batch of the right length needs
        // nothing; a null night, a missing batch, or a per-range count mismatch do.
        assert!(!sleep_batch_needs_fallback(Some(&json!([{"s": 1}, {"s": 2}])), 2));
        assert!(sleep_batch_needs_fallback(Some(&json!([{"s": 1}, null])), 2));
        assert!(sleep_batch_needs_fallback(None, 2));
        assert!(sleep_batch_needs_fallback(Some(&json!([{"s": 1}])), 2)); // too short
        assert!(sleep_batch_needs_fallback(Some(&json!([])), 1)); // empty vs one range
        assert!(!sleep_batch_needs_fallback(Some(&json!([])), 0)); // no ranges, no work
    }



    #[test]
    fn stalled_runner_is_killed_and_reaped() {
        let mut command = Command::new("python3");
        command.args(["-c", "import time; time.sleep(30)"]);
        assert!(model_output(command, None, std::time::Duration::from_millis(60)).is_none());
    }
    #[test]
    fn large_runner_output_does_not_deadlock_and_input_reaches_eof() {
        let mut command = Command::new("python3");
        command.args([
            "-c",
            "import json,sys; print(json.dumps({'input':sys.stdin.read(),'data':'x'*200000}))",
        ]);
        let value =
            model_output(command, Some(b"window"), std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(value["input"], "window");
        assert_eq!(value["data"].as_str().unwrap().len(), 200000);
    }

    #[test]
    fn status_payload_reports_missing_hub_db_as_state_not_error() {
        // No OURA_HUB_DB and a nonexistent path: a state, never a 500.
        let dir = std::env::temp_dir().join(format!("oura-status-{}", std::process::id()));
        let missing = dir.join("nope.db");
        let v = status_payload(&missing);
        assert_eq!(v["ingest"]["state"], "db_missing");
        assert_eq!(v["state"], "ok");
        assert!(v["summary"]["stale"].as_bool().unwrap());
        assert!(v["models"].is_object());
    }

    #[test]
    fn process_pending_on_missing_db_is_a_noop_without_running_build() {
        let dir = std::env::temp_dir().join(format!("oura-worker-{}", std::process::id()));
        let missing = dir.join("nope.db");
        // The build closure must never run when there is no DB to claim from.
        let ran = oura_hub::ingest::process_pending(&missing, |_| panic!("build must not run"));
        assert_eq!(ran.unwrap(), false);
    }
}

#[cfg(test)]
mod removed_route_tests {
    use super::*;

    /// The removed local-BLE routes (live sync, feature mutation, ring-key
    /// export) must 404 on a live server, not be resurrected by a same-origin
    /// header. A real TCP round-trip proves the catch-all answers them.
    #[tokio::test]
    async fn removed_local_ble_routes_404_even_same_origin() {
        let listener = tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let db = std::env::temp_dir().join(format!("oura-removed-route-{}.db", std::process::id()));
        let manual_db = db.with_file_name("manual-removed.db");
        let server = tokio::spawn(async move {
            if let Ok((sock, _)) = listener.accept().await {
                let _ = handle(sock, addr.port(), db, 0.0, None, WeightUnit::Kg, None).await;
            }
        });
        for (method, path) in [
            ("POST", "/api/sync"),
            ("POST", "/api/live-hr"),
            ("POST", "/api/feature"),
            ("GET", "/api/ring-key"),
            ("POST", "/api/ring-key"),
        ] {
            let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
            let req = format!(
                "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://{addr}\r\n\r\n"
            );
            client.write_all(req.as_bytes()).await.unwrap();
            let mut buf: Vec<u8> = Vec::new();
            client.read_to_end(&mut buf).await.unwrap_or(0);
            let text = String::from_utf8_lossy(&buf);
            assert!(
                text.starts_with("HTTP/1.1 404"),
                "{method} {path} must 404, got: {}",
                text.lines().next().unwrap_or_default()
            );
        }
        let _ = manual_db;
        server.abort();
    }
}

#[cfg(test)]
mod manual_api_tests {
    use super::*;

    fn entry(date: &str, kind: MetricKind, value: f64) -> ManualEntry {
        ManualEntry {
            id: 0,
            local_date: date.to_string(),
            kind,
            value,
            note: None,
            source: "manual".to_string(),
            batch_id: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn civil_dates_convert_from_days_since_the_epoch() {
        assert_eq!(days_to_ymd(0), "1970-01-01");
        assert_eq!(days_to_ymd(19_723), "2024-01-01");
        assert_eq!(days_to_ymd(19_782), "2024-02-29"); // leap day, month-aware
    }

    #[test]
    fn an_empty_store_serialises_to_empty_arrays() {
        let v = manual_series_json(&[], WeightUnit::Kg);
        assert_eq!(v["calories"], json!([]));
        assert_eq!(v["steps"], json!([]));
        assert_eq!(v["weight"], json!([]));
        assert_eq!(v["weight_trend"], json!([]));
        assert_eq!(v["unit"], "kg");
    }

    #[test]
    fn trend_requires_a_full_seven_entry_window() {
        let six: Vec<_> = (1..=6)
            .map(|d| entry(&format!("2026-01-{d:02}"), MetricKind::WeightKg, 70.0))
            .collect();
        assert_eq!(manual_series_json(&six, WeightUnit::Kg)["weight_trend"], json!([]));

        let seven: Vec<_> = (1..=7)
            .map(|d| entry(&format!("2026-01-{d:02}"), MetricKind::WeightKg, d as f64))
            .collect();
        let trend = manual_series_json(&seven, WeightUnit::Kg)["weight_trend"].clone();
        assert_eq!(trend.as_array().unwrap().len(), 1);
        assert_eq!(trend[0]["date"], "2026-01-07");
        assert_eq!(trend[0]["value"], 4.0); // mean of 1..=7
    }

    #[test]
    fn calories_and_weight_are_independent_series() {
        let entries = vec![
            entry("2026-01-01", MetricKind::CaloriesEaten, 2100.0),
            entry("2026-01-01", MetricKind::WeightKg, 75.5),
        ];
        let v = manual_series_json(&entries, WeightUnit::Kg);
        assert_eq!(v["calories"][0]["value"], 2100.0);
        assert_eq!(v["calories"][0]["date"], "2026-01-01");
        assert_eq!(v["weight"][0]["value"], 75.5);
    }

    #[test]
    fn imported_steps_are_served_unconverted_and_stay_out_of_the_other_series() {
        let entries = vec![
            entry("2026-01-01", MetricKind::Steps, 8500.0),
            entry("2026-01-01", MetricKind::CaloriesEaten, 2100.0),
            entry("2026-01-01", MetricKind::WeightKg, 75.5),
        ];
        let v = manual_series_json(&entries, WeightUnit::Lb);
        assert_eq!(v["steps"].as_array().unwrap().len(), 1);
        assert_eq!(v["steps"][0]["date"], "2026-01-01");
        assert_eq!(
            v["steps"][0]["value"], 8500.0,
            "a step count is a count, never converted to pounds"
        );
        assert_eq!(v["calories"].as_array().unwrap().len(), 1);
        assert_eq!(
            v["weight"].as_array().unwrap().len(),
            1,
            "the step entry must not appear as a weight"
        );
    }

    #[test]
    fn pounds_are_only_a_display_conversion() {
        let entries = vec![entry("2026-01-01", MetricKind::WeightKg, 75.5)];
        let v = manual_series_json(&entries, WeightUnit::Lb);
        assert_eq!(v["unit"], "lb");
        assert_eq!(v["weight"][0]["value"], 166.4); // 75.5 kg = 166.449… lb, shown rounded
    }

    #[test]
    fn csv_keeps_the_header_and_quotes_only_when_needed() {
        let mut e = entry("2026-01-01", MetricKind::CaloriesEaten, 2100.0);
        e.note = Some("a, \"b\"".to_string());
        let csv = manual_csv(&[e]);
        let mut lines = csv.split("\r\n");
        assert_eq!(
            lines.next().unwrap(),
            "local_date,kind,value,note,source,created_at,updated_at"
        );
        assert_eq!(
            lines.next().unwrap(),
            "2026-01-01,calories_eaten,2100,\"a, \"\"b\"\"\",manual,0,0"
        );
    }

    #[test]
    fn kind_accepts_the_canonical_and_short_spellings_only() {
        assert_eq!(parse_kind("calories_eaten"), Some(MetricKind::CaloriesEaten));
        assert_eq!(parse_kind("calories"), Some(MetricKind::CaloriesEaten));
        assert_eq!(parse_kind("weight_kg"), Some(MetricKind::WeightKg));
        assert_eq!(parse_kind("weight"), Some(MetricKind::WeightKg));
        assert_eq!(parse_kind("steps"), Some(MetricKind::Steps));
        assert_eq!(parse_kind("bogus"), None);
    }

    #[test]
    fn post_body_is_stamped_and_rejects_missing_fields() {
        assert!(manual_entry_from_body("{}", 1).is_err());
        assert!(manual_entry_from_body("{\"local_date\":\"2026-01-01\"}", 1).is_err());
        assert!(manual_entry_from_body("not json", 1).is_err());
        let ok = manual_entry_from_body(
            "{\"local_date\":\"2026-01-01\",\"kind\":\"calories\",\"value\":2000}",
            7,
        )
        .unwrap();
        assert_eq!(ok.kind, MetricKind::CaloriesEaten);
        assert_eq!(ok.value, 2000.0);
        assert_eq!(ok.source, "manual");
        assert_eq!((ok.created_at, ok.updated_at), (7, 7));
    }

    #[test]
    fn weight_unit_parsing_and_display_rounding() {
        assert_eq!(WeightUnit::parse("KG"), Some(WeightUnit::Kg));
        assert_eq!(WeightUnit::parse("lbs"), Some(WeightUnit::Lb));
        assert_eq!(WeightUnit::parse("stone"), None);
        assert_eq!(round1(2.25), 2.3);
        assert_eq!(round1(75.5), 75.5);
    }
}

#[cfg(test)]
mod manual_route_tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_manual_path() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("oura-manual-test-{unique}.db"));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn post_creates_an_entry_and_returns_it_with_provenance() {
        let path = temp_manual_path();
        let store = ManualStore::open(&path).unwrap();
        let body = r#"{"local_date":"2026-01-01","kind":"calories","value":2000}"#;
        let (status, response) = manual_post(&store, body);
        assert_eq!(status, "200 OK");
        assert_eq!(response["ok"], true);
        assert_eq!(response["outcome"], "inserted");
        let entry = &response["entry"];
        assert_eq!(entry["local_date"], "2026-01-01");
        assert_eq!(entry["kind"], "calories_eaten");
        assert_eq!(entry["value"], 2000.0);
        assert_eq!(entry["source"], "manual");
        assert!(entry["id"].is_number());
    }

    #[test]
    fn post_rejects_missing_date() {
        let path = temp_manual_path();
        let store = ManualStore::open(&path).unwrap();
        let (status, response) = manual_post(&store, r#"{"kind":"calories","value":2000}"#);
        assert_eq!(status, "400 Bad Request");
        assert_eq!(response["ok"], false);
        assert!(response["error"].as_str().unwrap().contains("local_date"));
    }

    #[test]
    fn post_rejects_invalid_kind() {
        let path = temp_manual_path();
        let store = ManualStore::open(&path).unwrap();
        let (status, response) = manual_post(
            &store,
            r#"{"local_date":"2026-01-01","kind":"bogus","value":1}"#,
        );
        assert_eq!(status, "400 Bad Request");
        assert!(response["error"].as_str().unwrap().contains("kind"));
    }

    #[test]
    fn post_rejects_non_numeric_value() {
        let path = temp_manual_path();
        let store = ManualStore::open(&path).unwrap();
        let (status, response) = manual_post(
            &store,
            r#"{"local_date":"2026-01-01","kind":"calories","value":"not-a-number"}"#,
        );
        assert_eq!(status, "400 Bad Request");
        assert!(response["error"].as_str().unwrap().contains("value"));
    }

    #[test]
    fn post_rejects_malformed_json() {
        let path = temp_manual_path();
        let store = ManualStore::open(&path).unwrap();
        let (status, response) = manual_post(&store, "not json");
        assert_eq!(status, "400 Bad Request");
        assert!(response["error"].as_str().unwrap().contains("JSON"));
    }

    #[test]
    fn list_returns_entries_and_filters_by_date_and_kind() {
        let path = temp_manual_path();
        let store = ManualStore::open(&path).unwrap();
        manual_post(&store, r#"{"local_date":"2026-01-01","kind":"calories","value":2000}"#);
        manual_post(&store, r#"{"local_date":"2026-01-02","kind":"weight","value":80}"#);
        manual_post(&store, r#"{"local_date":"2026-01-03","kind":"calories","value":2100}"#);

        let (status, response) = manual_list(&store, None, None, None);
        assert_eq!(status, "200 OK");
        assert_eq!(response["entries"].as_array().unwrap().len(), 3);

        let (status, filtered) = manual_list(
            &store,
            Some("2026-01-02"),
            Some("2026-01-02"),
            Some(MetricKind::WeightKg),
        );
        assert_eq!(status, "200 OK");
        let entries = filtered["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["local_date"], "2026-01-02");
        assert_eq!(entries[0]["kind"], "weight_kg");
    }

    #[test]
    fn list_on_empty_store_returns_empty_array() {
        let path = temp_manual_path();
        let store = ManualStore::open(&path).unwrap();
        let (status, response) = manual_list(&store, None, None, None);
        assert_eq!(status, "200 OK");
        assert!(response["entries"].as_array().unwrap().is_empty());
    }

    #[test]
    fn delete_removes_the_entry() {
        let path = temp_manual_path();
        let store = ManualStore::open(&path).unwrap();
        manual_post(&store, r#"{"local_date":"2026-01-01","kind":"calories","value":2000}"#);
        let (status, response) = manual_delete(
            &store,
            Some("2026-01-01"),
            Some(MetricKind::CaloriesEaten),
        );
        assert_eq!(status, "200 OK");
        assert_eq!(response["deleted"], true);
        let (_, after) = manual_list(&store, None, None, None);
        assert!(after["entries"].as_array().unwrap().is_empty());
    }

    #[test]
    fn delete_requires_date_and_kind() {
        let path = temp_manual_path();
        let store = ManualStore::open(&path).unwrap();
        let (status, response) = manual_delete(&store, None, Some(MetricKind::CaloriesEaten));
        assert_eq!(status, "400 Bad Request");
        assert!(response["error"].as_str().unwrap().contains("required"));

        let (status, response) = manual_delete(&store, Some("2026-01-01"), None);
        assert_eq!(status, "400 Bad Request");
        assert!(response["error"].as_str().unwrap().contains("required"));
    }

    #[test]
    fn delete_of_missing_entry_reports_not_deleted() {
        let path = temp_manual_path();
        let store = ManualStore::open(&path).unwrap();
        let (status, response) = manual_delete(
            &store,
            Some("2026-01-01"),
            Some(MetricKind::CaloriesEaten),
        );
        assert_eq!(status, "200 OK");
        assert_eq!(response["deleted"], false);
    }

    #[test]
    fn series_payload_separates_calories_weight_and_imported_steps() {
        // The regression this guards: imported steps must not leak into the
        // calorie or weight series, and weight must be rounded for display only.
        let entries = vec![
            ManualEntry {
                id: 1,
                local_date: "2026-01-01".into(),
                kind: MetricKind::CaloriesEaten,
                value: 2000.0,
                note: None,
                source: "manual".into(),
                batch_id: None,
                created_at: 0,
                updated_at: 0,
            },
            ManualEntry {
                id: 2,
                local_date: "2026-01-01".into(),
                kind: MetricKind::WeightKg,
                value: 80.26,
                note: None,
                source: "manual".into(),
                batch_id: None,
                created_at: 0,
                updated_at: 0,
            },
            ManualEntry {
                id: 3,
                local_date: "2026-01-01".into(),
                kind: MetricKind::Steps,
                value: 9000.0,
                note: None,
                source: "pulshealth-export".into(),
                batch_id: Some(1),
                created_at: 0,
                updated_at: 0,
            },
        ];
        let payload = manual_series_json(&entries, WeightUnit::Kg);
        assert_eq!(payload["calories"].as_array().unwrap().len(), 1);
        assert_eq!(payload["calories"][0]["value"], 2000.0);
        assert_eq!(payload["weight"].as_array().unwrap().len(), 1);
        assert_eq!(payload["weight"][0]["value"], 80.3);
        assert_eq!(payload["steps"].as_array().unwrap().len(), 1);
        assert_eq!(payload["steps"][0]["value"], 9000.0);
        // Fewer than seven stored weights means no trend is invented.
        assert!(payload["weight_trend"].as_array().unwrap().is_empty());
    }
}

#[cfg(test)]
mod profile_tests {
    use super::*;

    const TODAY: i64 = 21_000;

    struct Fixture {
        db: PathBuf,
        manual: Mutex<ManualStore>,
    }

    impl Fixture {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "oura-profile-test-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let db = dir.join("oura.db");
            write_profile(
                &db,
                &json!({
                    "sex": "F", "age": 25.0, "height_m": 1.87,
                    "weight_kg": 111.0, "ring_size": 12.0,
                }),
            ).unwrap();
            Self {
                db,
                manual: Mutex::new(ManualStore::open_in_memory().unwrap()),
            }
        }

        fn add(&self, day: i64, kind: MetricKind, value: f64) {
            let body = json!({
                "local_date": days_to_ymd(day),
                "kind": kind.as_str(),
                "value": value,
            }).to_string();
            let (status, response) = manual_post(&lock_manual(&self.manual), &body);
            assert_eq!(status, "200 OK", "{response}");
        }

        fn profile(&self) -> EffectiveProfile {
            effective_profile(&self.db, Some(&self.manual), TODAY).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.db.parent().unwrap());
        }
    }

    #[test]
    fn profile_readback_without_ring_uses_saved_fallback_for_missing_or_empty_manual_store() {
        let fixture = Fixture::new();
        assert!(!fixture.db.exists(), "profile reads must not require a ring DB");
        for manual in [None, Some(&fixture.manual)] {
            let profile = effective_profile(&fixture.db, manual, TODAY).unwrap().to_json();
            assert_eq!(profile["sex"], "F");
            assert_eq!(profile["age"], 25.0);
            assert_eq!(profile["height_m"], 1.87);
            assert_eq!(profile["weight_kg"], 111.0);
            assert_eq!(profile["ring_size"], 12.0);
            assert_eq!(profile["weight_source"], "profile");
            assert_eq!(profile["weight_sample_count"], 0);
        }
        fixture.add(TODAY - 180, MetricKind::WeightKg, 80.0);
        fixture.add(TODAY + 1, MetricKind::WeightKg, 90.0);
        let profile = fixture.profile().to_json();
        assert_eq!(profile["weight_kg"], 111.0);
        assert_eq!(profile["weight_source"], "profile");
        assert_eq!(profile["weight_sample_count"], 0);
    }

    #[test]
    fn sparse_weights_average_observations_in_inclusive_window_through_today() {
        let fixture = Fixture::new();
        fixture.add(TODAY - 180, MetricKind::WeightKg, 300.0);
        fixture.add(TODAY - 179, MetricKind::WeightKg, 70.125);
        fixture.add(TODAY - 40, MetricKind::WeightKg, 80.25);
        fixture.add(TODAY, MetricKind::WeightKg, 90.375);
        fixture.add(TODAY + 1, MetricKind::WeightKg, 400.0);
        fixture.add(TODAY, MetricKind::CaloriesEaten, 2500.0);

        let profile = fixture.profile().to_json();
        assert_eq!(profile["weight_kg"], 80.25);
        assert_eq!(profile["weight_source"], "manual_average");
        assert_eq!(profile["weight_sample_count"], 3);
        assert_eq!(read_profile(&fixture.db).weight_kg, 111.0);
    }

    #[test]
    fn weight_mean_uses_only_latest_twenty_eight_stored_observations() {
        let fixture = Fixture::new();
        for i in 0..35 {
            fixture.add(TODAY - 2 * (34 - i), MetricKind::WeightKg, 60.0 + i as f64);
        }
        let profile = fixture.profile();
        assert_eq!(profile.weight_sample_count, 28);
        assert_eq!(profile.demo.weight_kg, 80.5);
    }

    #[test]
    fn effective_weight_keeps_canonical_precision_independent_of_display_units() {
        let fixture = Fixture::new();
        fixture.add(TODAY - 10, MetricKind::WeightKg, 70.123);
        fixture.add(TODAY, MetricKind::WeightKg, 80.987);
        let expected = (70.123 + 80.987) / 2.0;
        let entries = lock_manual(&fixture.manual).export_all().unwrap();
        for unit in [WeightUnit::Kg, WeightUnit::Lb] {
            let shown = manual_series_json(&entries, unit);
            assert_eq!(shown["weight_summary"]["avg_28"], round1(unit.display_value(expected)));
            assert_eq!(fixture.profile().demo.weight_kg, expected);
        }
        assert_ne!(fixture.profile().demo.weight_kg, round1(expected));
    }

    #[test]
    fn profile_post_keeps_saved_fallback_while_mean_active_and_preserves_ring_size() {
        let fixture = Fixture::new();
        fixture.add(TODAY, MetricKind::WeightKg, 80.125);
        let profile = save_effective_profile(
            &fixture.db,
            Some(&fixture.manual),
            TODAY,
            &json!({ "sex": "M", "age": 45.0, "height_m": 1.9, "weight_kg": 80.1 }),
        ).unwrap().to_json();
        assert_eq!(profile["weight_kg"], 80.125);
        assert_eq!(profile["weight_source"], "manual_average");
        assert_eq!(profile["weight_sample_count"], 1);
        assert_eq!(profile["sex"], "M");
        assert_eq!(profile["age"], 45.0);
        assert_eq!(profile["height_m"], 1.9);
        assert_eq!(profile["ring_size"], 12.0);
        assert_eq!(read_profile(&fixture.db).weight_kg, 111.0);

        lock_manual(&fixture.manual)
            .delete_entry(&days_to_ymd(TODAY), MetricKind::WeightKg)
            .unwrap();
        assert_eq!(fixture.profile().demo.weight_kg, 111.0);
        let profile = save_effective_profile(
            &fixture.db,
            Some(&fixture.manual),
            TODAY,
            &json!({ "sex": "M", "age": 45.0, "height_m": 1.9, "weight_kg": 95.5 }),
        ).unwrap().to_json();
        assert_eq!(profile["weight_kg"], 95.5);
        assert_eq!(profile["weight_source"], "profile");
        assert_eq!(profile["ring_size"], 12.0);
        assert_eq!(read_profile(&fixture.db).weight_kg, 95.5);
    }

    #[test]
    fn summary_cache_key_tracks_weight_edits_deletes_and_window_expiry() {
        let fixture = Fixture::new();
        let fallback = summary_token(&fixture.db, &fixture.profile().demo);
        fixture.add(TODAY - 179, MetricKind::WeightKg, 80.0);
        let first = summary_token(&fixture.db, &fixture.profile().demo);
        assert_ne!(first, fallback);
        fixture.add(TODAY - 179, MetricKind::WeightKg, 80.001);
        let edited = summary_token(&fixture.db, &fixture.profile().demo);
        assert_ne!(edited, first);
        let expired = effective_profile(&fixture.db, Some(&fixture.manual), TODAY + 1).unwrap();
        assert_eq!(summary_token(&fixture.db, &expired.demo), fallback);
        lock_manual(&fixture.manual)
            .delete_entry(&days_to_ymd(TODAY - 179), MetricKind::WeightKg)
            .unwrap();
        assert_eq!(summary_token(&fixture.db, &fixture.profile().demo), fallback);
    }
}
