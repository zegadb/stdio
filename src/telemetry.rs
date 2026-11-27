//! The optional telemetry client: metrics, logs, errors and events to the
//! Addis telemetry intake (`telemetry.addis.gg`), in the wire format of
//! `docs/telemetry-wire.md`. Compiled only with the `telemetry` feature; the
//! default build of this crate has no dependencies and no network code.
//!
//! ```no_run
//! use stdio::telemetry::{Config, Event, Kind, Log, Level, Metric, Telemetry};
//! use std::time::Duration;
//!
//! let mut config = Config::new("zega-cli", "0123456789abcdef0123456789abcdef", Kind::Service, "0.6.0");
//! config.spool = Some(stdio::telemetry::SpoolConfig::new("/var/lib/zega/telemetry.ndjson"));
//! let telemetry = Telemetry::start(config).expect("a registered client id and key");
//! telemetry.metric(Metric::counter("sync.runs", 1.0).unit("{run}"));
//! telemetry.log(Log::new(Level::Info, "sync", "started"));
//! telemetry.event(Event::new("sync.done").attr("files", 12));
//! telemetry.shutdown(Duration::from_secs(2));
//! ```
//!
//! - **Never in the way.** Capturing builds and scrubs the record, then takes
//!   a lock long enough to queue it (and append one line to the spool). A
//!   background thread does the sending. Nothing here panics into the app.
//! - **A bounded queue.** Past `max_queue` records (or the spool's byte cap)
//!   the oldest queued record is dropped and counted; the counts are sent as
//!   `telemetry.dropped`.
//! - **Identity only with consent**, through [`ConsentApi::grant`], which
//!   returns the [`ConsentHandle`] that revokes it. Records have no user field,
//!   and attribute keys naming a person are scrubbed.

use std::collections::VecDeque;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

/// Where batches go unless the config says otherwise.
pub const DEFAULT_ENDPOINT: &str = "https://telemetry.addis.gg/v1/ingest";
/// `resource.sdk.name` for this client.
pub const SDK_NAME: &str = "stdio-rs";
/// `resource.sdk.version`: this crate's version.
pub const SDK_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Wire §3.
const MAX_BODY: usize = 524_288;
const MAX_BATCH_RECORDS: usize = 500;
const MAX_RECORD: usize = 32_768;
/// Wire §9.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const BACKOFF_MAX_S: f64 = 300.0;

// ── Public types ────────────────────────────────────────────

/// What the client is registered as (wire §1); decides what is scrubbed (§7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Our own Workers, hosts and tools.
    Service,
    /// An app a customer runs: anonymous metrics only (APS 25 §7).
    CustomerApp,
}

/// `resource.platform` (wire §4.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Browser,
    Worker,
    Node,
    Bun,
    Deno,
    ReactNative,
    /// A compiled app or binary; the default here.
    Native,
}

impl Platform {
    fn as_str(self) -> &'static str {
        match self {
            Platform::Browser => "browser",
            Platform::Worker => "worker",
            Platform::Node => "node",
            Platform::Bun => "bun",
            Platform::Deno => "deno",
            Platform::ReactNative => "react-native",
            Platform::Native => "native",
        }
    }
}

/// A log record's level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "debug",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Error => "error",
        }
    }
}

/// Why a record was not sent (wire §10, `telemetry.dropped`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropReason {
    QueueFull,
    SpoolFull,
    TooLarge,
    Invalid,
    Refused,
}

impl DropReason {
    const ALL: [DropReason; 5] = [
        DropReason::QueueFull,
        DropReason::SpoolFull,
        DropReason::TooLarge,
        DropReason::Invalid,
        DropReason::Refused,
    ];

    fn as_str(self) -> &'static str {
        match self {
            DropReason::QueueFull => "queue_full",
            DropReason::SpoolFull => "spool_full",
            DropReason::TooLarge => "too_large",
            DropReason::Invalid => "invalid",
            DropReason::Refused => "refused",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// W3C trace context ids: 32 and 16 hex, neither all zeros.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceContext {
    pub trace_id: String,
    pub span_id: String,
}

/// An attribute value: a string, a finite number or a boolean.
#[derive(Clone, Debug, PartialEq)]
pub enum AttrValue {
    Str(String),
    Num(f64),
    Bool(bool),
}

impl From<&str> for AttrValue {
    fn from(v: &str) -> Self {
        AttrValue::Str(v.to_string())
    }
}
impl From<String> for AttrValue {
    fn from(v: String) -> Self {
        AttrValue::Str(v)
    }
}
impl From<f64> for AttrValue {
    fn from(v: f64) -> Self {
        AttrValue::Num(v)
    }
}
impl From<i64> for AttrValue {
    fn from(v: i64) -> Self {
        AttrValue::Num(v as f64)
    }
}
impl From<i32> for AttrValue {
    fn from(v: i32) -> Self {
        AttrValue::Num(f64::from(v))
    }
}
impl From<u32> for AttrValue {
    fn from(v: u32) -> Self {
        AttrValue::Num(f64::from(v))
    }
}
impl From<bool> for AttrValue {
    fn from(v: bool) -> Self {
        AttrValue::Bool(v)
    }
}

type Attrs = Vec<(String, AttrValue)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MetricKind {
    Counter,
    Gauge,
    Histogram,
}

/// A metric record: a counter (an increase, ≥ 0), a gauge (the value now) or
/// a histogram (one observation).
#[derive(Clone, Debug)]
pub struct Metric {
    kind: MetricKind,
    name: String,
    value: f64,
    unit: Option<String>,
    attrs: Attrs,
    trace: Option<TraceContext>,
}

impl Metric {
    fn new(kind: MetricKind, name: &str, value: f64) -> Self {
        Metric {
            kind,
            name: name.to_string(),
            value,
            unit: None,
            attrs: Vec::new(),
            trace: None,
        }
    }
    /// The increase since this install last reported it.
    pub fn counter(name: &str, value: f64) -> Self {
        Self::new(MetricKind::Counter, name, value)
    }
    pub fn gauge(name: &str, value: f64) -> Self {
        Self::new(MetricKind::Gauge, name, value)
    }
    pub fn histogram(name: &str, value: f64) -> Self {
        Self::new(MetricKind::Histogram, name, value)
    }
    /// UCUM-style: `ms`, `By`, `{request}`.
    pub fn unit(mut self, unit: &str) -> Self {
        self.unit = Some(unit.to_string());
        self
    }
    pub fn attr(mut self, key: &str, value: impl Into<AttrValue>) -> Self {
        self.attrs.push((key.to_string(), value.into()));
        self
    }
    pub fn trace(mut self, trace: TraceContext) -> Self {
        self.trace = Some(trace);
        self
    }
}

/// A log record.
#[derive(Clone, Debug)]
pub struct Log {
    level: Level,
    scope: String,
    message: String,
    trace: Option<TraceContext>,
}

impl Log {
    pub fn new(level: Level, scope: &str, message: &str) -> Self {
        Log {
            level,
            scope: scope.to_string(),
            message: message.to_string(),
            trace: None,
        }
    }
    pub fn trace(mut self, trace: TraceContext) -> Self {
        self.trace = Some(trace);
        self
    }
}

/// An error record.
#[derive(Clone, Debug)]
pub struct ErrorRecord {
    error_type: String,
    message: Option<String>,
    stack: Option<String>,
    fingerprint: Option<String>,
    code: Option<String>,
    trace: Option<TraceContext>,
}

impl ErrorRecord {
    pub fn new(error_type: &str) -> Self {
        ErrorRecord {
            error_type: error_type.to_string(),
            message: None,
            stack: None,
            fingerprint: None,
            code: None,
            trace: None,
        }
    }
    /// From any error: its type name and its message.
    pub fn from_error<E: std::error::Error>(error: &E) -> Self {
        Self::new(std::any::type_name::<E>()).message(&error.to_string())
    }
    pub fn message(mut self, message: &str) -> Self {
        self.message = Some(message.to_string());
        self
    }
    pub fn stack(mut self, stack: &str) -> Self {
        self.stack = Some(stack.to_string());
        self
    }
    /// A grouping hint: records with the same hint are one error.
    pub fn fingerprint(mut self, fingerprint: &str) -> Self {
        self.fingerprint = Some(fingerprint.to_string());
        self
    }
    /// The app's own error code; a customer app's errors group by it.
    pub fn code(mut self, code: &str) -> Self {
        self.code = Some(code.to_string());
        self
    }
    pub fn trace(mut self, trace: TraceContext) -> Self {
        self.trace = Some(trace);
        self
    }
}

/// An event record.
#[derive(Clone, Debug)]
pub struct Event {
    name: String,
    attrs: Attrs,
    trace: Option<TraceContext>,
}

impl Event {
    pub fn new(name: &str) -> Self {
        Event {
            name: name.to_string(),
            attrs: Vec::new(),
            trace: None,
        }
    }
    pub fn attr(mut self, key: &str, value: impl Into<AttrValue>) -> Self {
        self.attrs.push((key.to_string(), value.into()));
        self
    }
    pub fn trace(mut self, trace: TraceContext) -> Self {
        self.trace = Some(trace);
        self
    }
}

/// `resource.os`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Os {
    pub name: Option<String>,
    pub version: Option<String>,
}

impl Os {
    /// The OS this was compiled for, by name (`macOS`, `Linux`, `Windows`); its version is the app's to give.
    pub fn current() -> Self {
        let name = match std::env::consts::OS {
            "macos" => "macOS",
            "linux" => "Linux",
            "windows" => "Windows",
            "ios" => "iOS",
            "android" => "Android",
            "freebsd" => "FreeBSD",
            other => other,
        };
        Os {
            name: Some(name.to_string()),
            version: None,
        }
    }
}

/// The probability each record type is kept (wire §10), each 0–1; default 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sampling {
    pub metric: f64,
    pub log: f64,
    pub error: f64,
    pub event: f64,
}

impl Default for Sampling {
    fn default() -> Self {
        Sampling {
            metric: 1.0,
            log: 1.0,
            error: 1.0,
            event: 1.0,
        }
    }
}

/// A spool: queued records in a capped NDJSON file (the same format as the
/// TypeScript client's `fileSpool`), so a crash or an offline period loses
/// nothing.
#[derive(Clone, Debug)]
pub struct SpoolConfig {
    pub path: PathBuf,
    /// Live records at most this many bytes; past it, the oldest are dropped. Default 4 MiB.
    pub max_bytes: usize,
}

impl SpoolConfig {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        SpoolConfig {
            path: path.into(),
            max_bytes: 4 << 20,
        }
    }
}

/// The clock, in milliseconds since the Unix epoch (tests fix it).
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;
/// 16 random bytes (tests fix them).
pub type RandomSource = Arc<dyn Fn() -> [u8; 16] + Send + Sync>;

/// Everything the client needs, explicitly: nothing is read from the environment.
#[derive(Clone)]
pub struct Config {
    /// The registered client id (the collector's projects.json `service`), e.g. `zega-cli`.
    pub client: String,
    /// The project's key, 32 hex, from `bun run project`.
    pub key: String,
    pub kind: Kind,
    /// The app's version.
    pub release: String,
    pub platform: Platform,
    pub os: Option<Os>,
    /// A model class (`Mac`, `iPhone`), never a model number or a name.
    pub device_class: Option<String>,
    pub environment: Option<String>,
    /// `https://`, or `http://` only to loopback.
    pub endpoint: String,
    /// Records per batch, default 100, at most 500.
    pub max_batch: usize,
    /// Time between scheduled sends, default 10 s. Zero: only on a full batch or a flush.
    pub interval: Duration,
    /// Queued records, default 1,000.
    pub max_queue: usize,
    pub sampling: Sampling,
    pub spool: Option<SpoolConfig>,
    /// Where the install id is kept. Without it, the id lasts one process.
    pub install_id_path: Option<PathBuf>,
    /// How long dropping the client waits for its last flush, default 2 s.
    pub shutdown_timeout: Duration,
    #[doc(hidden)]
    pub clock: Option<Clock>,
    #[doc(hidden)]
    pub random: Option<RandomSource>,
    #[doc(hidden)]
    pub request_timeout: Duration,
}

impl Config {
    pub fn new(client: &str, key: &str, kind: Kind, release: &str) -> Self {
        Config {
            client: client.to_string(),
            key: key.to_string(),
            kind,
            release: release.to_string(),
            platform: Platform::Native,
            os: Some(Os::current()),
            device_class: None,
            environment: None,
            endpoint: DEFAULT_ENDPOINT.to_string(),
            max_batch: 100,
            interval: Duration::from_secs(10),
            max_queue: 1000,
            sampling: Sampling::default(),
            spool: None,
            install_id_path: None,
            shutdown_timeout: Duration::from_secs(2),
            clock: None,
            random: None,
            request_timeout: REQUEST_TIMEOUT,
        }
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The key is a credential: never printed.
        f.debug_struct("Config")
            .field("client", &self.client)
            .field("kind", &self.kind)
            .field("release", &self.release)
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

/// A config that can never send, or a consent that is not well formed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError(String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "telemetry: {}", self.0)
    }
}

impl std::error::Error for ConfigError {}

/// The account whose identity the user approved sending.
#[derive(Clone, Debug)]
pub struct Identity {
    /// The Addis (or Ava) account's `sub`.
    pub sub: String,
    pub device_name: Option<String>,
    pub email: Option<String>,
}

/// What the user approved: the consent text's version, when, and for which account.
#[derive(Clone, Debug)]
pub struct Consent {
    pub version: String,
    /// When the user approved it; `None` is now. Pass the stored time when re-presenting an approval after a restart.
    pub granted_at: Option<SystemTime>,
    pub identity: Identity,
}

/// Counts for the app (and tests).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dropped {
    pub queue_full: u64,
    pub spool_full: u64,
    pub too_large: u64,
    pub invalid: u64,
    pub refused: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Records queued or in a batch being sent.
    pub queued: usize,
    pub sent: u64,
    pub dropped: Dropped,
    pub last_status: Option<u16>,
    pub last_error: Option<String>,
}

// ── Validation and formatting (wire §3–§5) ──────────────────

fn utf8_prefix(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn text(s: Option<&str>, max: usize) -> Option<String> {
    match s {
        Some(v) if !v.is_empty() => Some(utf8_prefix(v, max).to_string()),
        _ => None,
    }
}

fn all_bytes(s: &str, min: usize, max: usize, ok: impl Fn(u8) -> bool) -> bool {
    s.len() >= min && s.len() <= max && s.bytes().all(ok)
}

/// `^[a-z][a-z0-9_]*(\.[a-z0-9_]+)*$`
fn valid_name(s: &str, max: usize) -> bool {
    if s.is_empty() || s.len() > max || !s.as_bytes()[0].is_ascii_lowercase() {
        return false;
    }
    s.split('.').all(|seg| {
        !seg.is_empty()
            && seg
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    })
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `^[A-Za-z0-9][A-Za-z0-9._+-]{0,63}$`
fn valid_release(s: &str) -> bool {
    !s.is_empty()
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && all_bytes(s, 1, 64, |b| {
            b.is_ascii_alphanumeric() || b"._+-".contains(&b)
        })
}

/// `^[a-z0-9][a-z0-9._-]{0,63}$`
fn valid_client(s: &str) -> bool {
    !s.is_empty()
        && (s.as_bytes()[0].is_ascii_lowercase() || s.as_bytes()[0].is_ascii_digit())
        && all_bytes(s, 1, 64, |b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)
        })
}

fn valid_unit(s: &str) -> bool {
    all_bytes(s, 1, 32, |b| {
        b.is_ascii_alphanumeric() || b"%/_.{}-".contains(&b)
    })
}

fn valid_scope(s: &str) -> bool {
    all_bytes(s, 1, 64, |b| {
        b.is_ascii_alphanumeric() || b"._:/-".contains(&b)
    })
}

fn valid_code(s: &str) -> bool {
    all_bytes(s, 1, 64, |b| {
        b.is_ascii_alphanumeric() || b"_.:-".contains(&b)
    })
}

fn valid_environment(s: &str) -> bool {
    all_bytes(s, 1, 32, |b| {
        b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b)
    })
}

fn valid_consent_version(s: &str) -> bool {
    all_bytes(s, 1, 64, |b| {
        b.is_ascii_alphanumeric() || b"._-".contains(&b)
    })
}

fn is_loopback_http(url: &str) -> bool {
    ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|p| {
            url.strip_prefix(p).is_some_and(|rest| {
                rest.is_empty() || rest.starts_with(':') || rest.starts_with('/')
            })
        })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A JSON number: an integer when the value is one (as JavaScript prints it), else a float.
fn number(v: f64) -> Option<Value> {
    if !v.is_finite() {
        return None;
    }
    if v.fract() == 0.0 && v.abs() < 9_007_199_254_740_992.0 {
        return Some(Value::from(v as i64));
    }
    serde_json::Number::from_f64(v).map(Value::Number)
}

/// `YYYY-MM-DDTHH:MM:SS.sssZ` from milliseconds since the epoch.
fn timestamp(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        ms % 1000
    )
}

fn system_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Days since 1970-01-01 for a civil date (Hinnant's days_from_civil).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// An IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) as milliseconds since the epoch.
fn parse_http_date(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 6 || parts[5] != "GMT" {
        return None;
    }
    let day: i64 = parts[1].parse().ok()?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| *m == parts[2])? as i64
        + 1;
    let year: i64 = parts[3].parse().ok()?;
    let hms: Vec<i64> = parts[4]
        .split(':')
        .map(|x| x.parse().ok())
        .collect::<Option<Vec<_>>>()?;
    if hms.len() != 3 {
        return None;
    }
    let secs = days_from_civil(year, month, day) * 86_400 + hms[0] * 3600 + hms[1] * 60 + hms[2];
    u64::try_from(secs).ok().map(|s| s * 1000)
}

/// Retry-After as a wait, clamped to 1–3,600 s (wire §9); `None` when absent or unreadable.
pub fn retry_after(value: Option<&str>, now_ms: u64) -> Option<Duration> {
    let v = value?.trim();
    let secs = if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) {
        v.parse::<f64>().ok()?
    } else {
        (parse_http_date(v)? as f64 - now_ms as f64) / 1000.0
    };
    Some(Duration::from_secs_f64(secs.clamp(1.0, 3600.0)))
}

/// The wait after the n-th consecutive failure: d/2 + random(0, d/2), d = min(300 s, 2^(n−1) s) (wire §9).
/// `unit` is a random number in [0, 1].
pub fn backoff(n: u32, unit: f64) -> Duration {
    let d = 2f64
        .powi(n.saturating_sub(1).min(30) as i32)
        .min(BACKOFF_MAX_S);
    Duration::from_secs_f64(d / 2.0 + unit.clamp(0.0, 1.0) * d / 2.0)
}

fn trace_value(t: &Option<TraceContext>) -> Option<Value> {
    let t = t.as_ref()?;
    let trace_id = t.trace_id.to_ascii_lowercase();
    let span_id = t.span_id.to_ascii_lowercase();
    if !is_hex(&trace_id, 32)
        || !is_hex(&span_id, 16)
        || trace_id.bytes().all(|b| b == b'0')
        || span_id.bytes().all(|b| b == b'0')
    {
        return None;
    }
    Some(json!({ "trace_id": trace_id, "span_id": span_id }))
}

/// Attributes as the wire allows them: at most 32, valid keys, strings cut to 256 bytes, finite numbers.
fn clean_attrs(attrs: &Attrs) -> Option<Map<String, Value>> {
    let mut out = Map::new();
    for (k, v) in attrs {
        if out.len() >= 32 {
            break;
        }
        if !valid_name(k, 64) || out.contains_key(k) {
            continue;
        }
        let value = match v {
            AttrValue::Str(s) => Value::from(utf8_prefix(s, 256)),
            AttrValue::Num(n) => match number(*n) {
                Some(n) => n,
                None => continue,
            },
            AttrValue::Bool(b) => Value::from(*b),
        };
        out.insert(k.clone(), value);
    }
    (!out.is_empty()).then_some(out)
}

// ── Scrubbing (wire §7; the collector's src/scrub.ts) ───────

/// The collector's DROP_KEY, written out: matches the key case-insensitively.
pub fn drop_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    let anywhere = [
        "cookie",
        "authorization",
        "password",
        "passwd",
        "secret",
        "session",
        "csrf",
        "email",
        "phone",
        "ip_address",
        "query_params",
    ];
    if anywhere.iter().any(|p| k.contains(p)) {
        return true;
    }
    // auth[_-]?token, api[_-]?key, access[_-]?token, refresh[_-]?token
    for (a, b) in [
        ("auth", "token"),
        ("api", "key"),
        ("access", "token"),
        ("refresh", "token"),
    ] {
        for sep in ["", "_", "-"] {
            if k.contains(&format!("{a}{sep}{b}")) {
                return true;
            }
        }
    }
    let exact = [
        "user",
        "ip",
        "vars",
        "extra",
        "body",
        "data",
        "query_string",
        "query",
        "cookies",
        "env",
        "dsn",
    ];
    if exact.contains(&k.as_str()) || k.starts_with("enduser") {
        return true;
    }
    // ^user[._-]?(id|name|email|ip|segment)
    if let Some(rest) = k.strip_prefix("user") {
        let rest = rest.strip_prefix(['.', '_', '-']).unwrap_or(rest);
        if ["id", "name", "email", "ip", "segment"]
            .iter()
            .any(|w| rest.starts_with(w))
        {
            return true;
        }
    }
    false
}

/// The collector's DROP_ATTR.
pub fn drop_attr(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    [
        "http.request.header.",
        "http.response.header.",
        "http.request.body",
        "http.response.body",
        "url.query",
        "enduser.",
        "user.",
        "client.address",
        "net.peer.ip",
        "net.sock.peer.addr",
        "server.address",
    ]
    .iter()
    .any(|p| k.starts_with(p))
}

fn url_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    [
        "url",
        "uri",
        "href",
        "target",
        "full_url",
        "http.url",
        "url.full",
        "http.target",
        "url.original",
        "referer",
        "referrer",
        "location",
        "redirect",
        "redirect_url",
    ]
    .contains(&k.as_str())
}

const CUSTOMER_ATTR_KEYS: [&str; 7] = [
    "http.request.method",
    "http.response.status_code",
    "http.status_code",
    "rpc.method",
    "rpc.service",
    "error.type",
    "network.protocol.name",
];
const TELEMETRY_ATTR_KEYS: [&str; 2] = ["reason", "consent.version"];

fn scrub_attrs(
    attrs: Map<String, Value>,
    kind: Kind,
    reserved: bool,
) -> Option<Map<String, Value>> {
    let mut out = Map::new();
    for (k, v) in attrs {
        if drop_key(&k) || drop_attr(&k) {
            continue;
        }
        if kind == Kind::CustomerApp
            && !CUSTOMER_ATTR_KEYS.contains(&k.as_str())
            && !(reserved && TELEMETRY_ATTR_KEYS.contains(&k.as_str()))
        {
            continue;
        }
        let v = match v {
            Value::String(s) if url_key(&k) => {
                Value::String(s.split(['?', '#']).next().unwrap_or("").to_string())
            }
            other => other,
        };
        out.insert(k, v);
    }
    (!out.is_empty()).then_some(out)
}

/// The record as it may be sent for `kind`, or `None` when it may not be sent (a customer app's logs).
fn scrub_record(mut r: Map<String, Value>, kind: Kind) -> Option<Map<String, Value>> {
    match r.get("type").and_then(Value::as_str) {
        Some("log") if kind == Kind::CustomerApp => None,
        Some("error") if kind == Kind::CustomerApp => {
            r.remove("message");
            r.remove("stack");
            Some(r)
        }
        Some("metric") | Some("event") => {
            let reserved = r
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| n.starts_with("telemetry."));
            if let Some(Value::Object(attrs)) = r.remove("attrs") {
                if let Some(a) = scrub_attrs(attrs, kind, reserved) {
                    r.insert("attrs".into(), Value::Object(a));
                }
            }
            Some(r)
        }
        _ => Some(r),
    }
}

// ── The client ──────────────────────────────────────────────

struct Item {
    r: Value,
    res: Arc<Value>,
    res_key: Arc<String>,
    cv: Option<String>,
    bytes: usize,
}

struct Formed {
    id: String,
    items: Vec<Item>,
    extra: Vec<Value>,
    res: Arc<Value>,
    cv: Option<String>,
}

struct Grant {
    version: String,
    identity: Value,
    token: u64,
}

struct State {
    resource: Arc<Value>,
    resource_key: Arc<String>,
    install_id: String,
    queue: VecDeque<Item>,
    queued_bytes: usize,
    formed: VecDeque<Formed>,
    pending_drops: [u64; 5],
    grant: Option<Grant>,
    next_token: u64,
    stats: Stats,
    failures: u32,
    retry_at: u64,
    next_tick: u64,
    flush_wanted: u64,
    flush_served: u64,
    stopping: bool,
    closed: bool,
    spool_dirty: bool,
    worker_done: bool,
}

struct Settings {
    client: String,
    key: String,
    kind: Kind,
    url: String,
    max_batch: usize,
    max_queue: usize,
    interval: Duration,
    sampling: Sampling,
    spool: Option<SpoolConfig>,
    install_id_path: Option<PathBuf>,
    shutdown_timeout: Duration,
    request_timeout: Duration,
    release: String,
    platform: Platform,
    os: Option<Os>,
    device_class: Option<String>,
    environment: Option<String>,
    clock: Clock,
    random: RandomSource,
}

struct Shared {
    s: Settings,
    state: Mutex<State>,
    /// The worker waits on this.
    wake: Condvar,
    /// Flushes wait on this.
    done: Condvar,
    rng: AtomicU64,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic elsewhere while holding the lock must not take telemetry, or the app, down with it.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now(&self) -> u64 {
        (self.s.clock)()
    }

    /// A random number in [0, 1) for sampling and backoff (xorshift64*, seeded from the OS).
    fn unit(&self) -> f64 {
        let mut x = self.rng.load(Ordering::Relaxed);
        loop {
            let mut y = x;
            y ^= y >> 12;
            y ^= y << 25;
            y ^= y >> 27;
            match self
                .rng
                .compare_exchange_weak(x, y, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => {
                    return (y.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64
                        / (1u64 << 53) as f64
                }
                Err(actual) => x = actual,
            }
        }
    }

    fn resource(&self, install_id: &str) -> Value {
        let s = &self.s;
        let mut r = Map::new();
        r.insert("release".into(), Value::from(s.release.clone()));
        r.insert("platform".into(), Value::from(s.platform.as_str()));
        if let Some(os) = &s.os {
            let mut o = Map::new();
            if let Some(n) = text(os.name.as_deref(), 32) {
                o.insert("name".into(), Value::from(n));
            }
            if let Some(v) = text(os.version.as_deref(), 32) {
                o.insert("version".into(), Value::from(v));
            }
            if !o.is_empty() {
                r.insert("os".into(), Value::Object(o));
            }
        }
        if let Some(d) = text(s.device_class.as_deref(), 32) {
            r.insert("device_class".into(), Value::from(d));
        }
        r.insert("install_id".into(), Value::from(install_id));
        r.insert(
            "sdk".into(),
            json!({ "name": SDK_NAME, "version": SDK_VERSION }),
        );
        if let Some(e) = &s.environment {
            r.insert("environment".into(), Value::from(e.clone()));
        }
        Value::Object(r)
    }

    fn drop_records(&self, st: &mut State, reason: DropReason, n: u64) {
        let d = &mut st.stats.dropped;
        match reason {
            DropReason::QueueFull => d.queue_full += n,
            DropReason::SpoolFull => d.spool_full += n,
            DropReason::TooLarge => d.too_large += n,
            DropReason::Invalid => d.invalid += n,
            DropReason::Refused => d.refused += n,
        }
        st.pending_drops[reason.index()] += n;
    }

    fn spool_line(item: &Item) -> String {
        let line = json!({ "r": item.r, "res": *item.res, "cv": item.cv });
        format!("{line}\n")
    }

    fn spool_append(&self, st: &mut State, item: &Item) {
        let Some(spool) = &self.s.spool else { return };
        let line = Self::spool_line(item);
        let result = ensure_parent(&spool.path)
            .and_then(|_| open_private(&spool.path, true))
            .and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(e) = result {
            st.stats.last_error = Some(format!("spool: {e}"));
        }
    }

    /// Rewrites the spool with every live record.
    fn spool_replace(&self, st: &mut State) {
        st.spool_dirty = false;
        let Some(spool) = &self.s.spool else { return };
        let mut body = String::new();
        for f in &st.formed {
            for it in &f.items {
                body.push_str(&Self::spool_line(it));
            }
        }
        for it in &st.queue {
            body.push_str(&Self::spool_line(it));
        }
        if let Err(e) = write_atomic(&spool.path, body.as_bytes()) {
            st.stats.last_error = Some(format!("spool: {e}"));
        }
    }

    fn item(r: Value, res: Arc<Value>, res_key: Arc<String>, cv: Option<String>) -> Item {
        let mut it = Item {
            r,
            res,
            res_key,
            cv,
            bytes: 0,
        };
        it.bytes = Self::spool_line(&it).len();
        it
    }

    /// Adds to the back; past the bounds, the oldest queued record (not one in a batch) is dropped.
    fn push(&self, st: &mut State, item: Item) {
        st.queued_bytes += item.bytes;
        st.queue.push_back(item);
        let formed_count: usize = st.formed.iter().map(|f| f.items.len()).sum();
        let formed_bytes: usize = st
            .formed
            .iter()
            .flat_map(|f| f.items.iter())
            .map(|i| i.bytes)
            .sum();
        let max_bytes = self.s.spool.as_ref().map_or(usize::MAX, |s| s.max_bytes);
        while !st.queue.is_empty() {
            let over_count = st.queue.len() + formed_count > self.s.max_queue;
            let over_bytes = st.queued_bytes + formed_bytes > max_bytes;
            if !over_count && !over_bytes {
                break;
            }
            if let Some(old) = st.queue.pop_front() {
                st.queued_bytes -= old.bytes;
            }
            self.drop_records(
                st,
                if over_count {
                    DropReason::QueueFull
                } else {
                    DropReason::SpoolFull
                },
                1,
            );
            st.spool_dirty = true;
        }
    }

    /// Samples, builds, scrubs and queues one record. Never panics into the app.
    fn capture(&self, rate: f64, build: impl FnOnce(String) -> Option<Map<String, Value>>) {
        let rate = if (0.0..=1.0).contains(&rate) {
            rate
        } else {
            1.0
        };
        if rate < 1.0 && self.unit() >= rate {
            return;
        }
        let ts = timestamp(self.now());
        let built = build(ts);
        let mut st = self.lock();
        if st.closed {
            return;
        }
        let Some(mut record) = built else {
            self.drop_records(&mut st, DropReason::Invalid, 1);
            return;
        };
        if rate < 1.0 {
            if let Some(n) = number(rate) {
                record.insert("sample_rate".into(), n);
            }
        }
        let Some(record) = scrub_record(record, self.s.kind) else {
            return;
        };
        let record = Value::Object(record);
        if record.to_string().len() > MAX_RECORD {
            self.drop_records(&mut st, DropReason::TooLarge, 1);
            return;
        }
        let cv = st.grant.as_ref().map(|g| g.version.clone());
        let item = Self::item(
            record,
            Arc::clone(&st.resource),
            Arc::clone(&st.resource_key),
            cv,
        );
        self.spool_append(&mut st, &item);
        self.push(&mut st, item);
        if st.queue.len() >= self.s.max_batch {
            self.wake.notify_all();
        }
    }

    fn effective(st: &State, cv: &Option<String>) -> Option<String> {
        match (cv, &st.grant) {
            (Some(v), Some(g)) if *v == g.version => Some(v.clone()),
            _ => None,
        }
    }

    fn has_work(st: &State) -> bool {
        !st.formed.is_empty() || !st.queue.is_empty() || st.pending_drops.iter().any(|n| *n > 0)
    }

    /// The next batch from the front of the queue: one resource, one consent state, within the byte budget.
    fn form(&self, st: &mut State) -> bool {
        let (res, res_key, cv) = match st.queue.front() {
            Some(first) => (
                Arc::clone(&first.res),
                Arc::clone(&first.res_key),
                Self::effective(st, &first.cv),
            ),
            None => (Arc::clone(&st.resource), Arc::clone(&st.resource_key), None),
        };
        let mut take = 0;
        let mut bytes = 2048;
        for it in &st.queue {
            if take >= self.s.max_batch
                || Self::effective(st, &it.cv) != cv
                || *it.res_key != *res_key
            {
                break;
            }
            let size = it.r.to_string().len() + 1;
            if bytes + size > MAX_BODY && take > 0 {
                break;
            }
            take += 1;
            bytes += size;
        }
        let mut extra = Vec::new();
        let pending = st.pending_drops.iter().filter(|n| **n > 0).count();
        if take + pending <= self.s.max_batch {
            for reason in DropReason::ALL {
                let n = st.pending_drops[reason.index()];
                if n > 0 {
                    extra.push(json!({ "type": "metric", "ts": timestamp(self.now()), "kind": "counter", "name": "telemetry.dropped", "value": n, "unit": "{record}", "attrs": { "reason": reason.as_str() } }));
                }
            }
            st.pending_drops = [0; 5];
        }
        if take == 0 && extra.is_empty() {
            return false;
        }
        let items: Vec<Item> = st.queue.drain(..take).collect();
        for it in &items {
            st.queued_bytes -= it.bytes;
        }
        let id = hex(&(self.s.random)());
        st.formed.push_back(Formed {
            id,
            items,
            extra,
            res,
            cv,
        });
        true
    }

    fn serialize(&self, st: &State, f: &Formed) -> String {
        let mut b = Map::new();
        b.insert("v".into(), Value::from(1));
        b.insert("client".into(), Value::from(self.s.client.clone()));
        b.insert("batch".into(), Value::from(f.id.clone()));
        b.insert("sent_at".into(), Value::from(timestamp(self.now())));
        b.insert("resource".into(), (*f.res).clone());
        if Self::effective(st, &f.cv).is_some() {
            if let Some(g) = &st.grant {
                b.insert("identity".into(), g.identity.clone());
            }
        }
        let records: Vec<Value> = f
            .items
            .iter()
            .map(|i| i.r.clone())
            .chain(f.extra.iter().cloned())
            .collect();
        b.insert("records".into(), Value::Array(records));
        Value::Object(b).to_string()
    }

    /// Nothing queued, in a batch or spooled is sent with identity any more.
    fn forget_consent(&self, st: &mut State) {
        for it in st.queue.iter_mut() {
            it.cv = None;
        }
        for f in st.formed.iter_mut() {
            f.cv = None;
            for it in f.items.iter_mut() {
                it.cv = None;
            }
        }
        self.spool_replace(st);
    }

    fn consent_event(&self, name: &str, version: &str) {
        self.capture(1.0, |ts| {
            let mut m = Map::new();
            m.insert("type".into(), Value::from("event"));
            m.insert("ts".into(), Value::from(ts));
            m.insert("name".into(), Value::from(name));
            m.insert("attrs".into(), json!({ "consent.version": version }));
            Some(m)
        });
    }
}

fn ensure_parent(path: &Path) -> std::io::Result<()> {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => fs::create_dir_all(p),
        _ => Ok(()),
    }
}

fn open_private(path: &Path, append: bool) -> std::io::Result<fs::File> {
    let mut o = OpenOptions::new();
    o.create(true);
    if append {
        o.append(true);
    } else {
        o.write(true).truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    ensure_parent(path)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    open_private(&tmp, false)?.write_all(bytes)?;
    fs::rename(&tmp, path)
}

fn os_random() -> [u8; 16] {
    let mut b = [0u8; 16];
    if getrandom::fill(&mut b).is_err() {
        // No OS randomness: still unique per process, never derived from hardware or the user.
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        b = (n ^ (u128::from(std::process::id()) << 64)).to_le_bytes();
    }
    b
}

/// A running client. Dropping it flushes for at most `shutdown_timeout`.
pub struct Telemetry {
    shared: Arc<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

/// `telemetry.consent()`: the only way to attach identity.
pub struct ConsentApi<'a> {
    shared: &'a Arc<Shared>,
}

/// Returned by [`ConsentApi::grant`]: the only thing that can stop the identity it attached.
pub struct ConsentHandle {
    shared: Arc<Shared>,
    token: u64,
    version: String,
    granted_at: String,
}

impl Telemetry {
    /// Starts the client and its background thread. Errors only for a config that can never send.
    pub fn start(config: Config) -> Result<Telemetry, ConfigError> {
        let c = config;
        if !valid_client(&c.client) {
            return Err(ConfigError(
                "client must be a registered client id (^[a-z0-9][a-z0-9._-]{0,63}$)".into(),
            ));
        }
        if !is_hex(&c.key, 32) {
            return Err(ConfigError(
                "key must be the 32-hex key from `bun run project`".into(),
            ));
        }
        if !valid_release(&c.release) {
            return Err(ConfigError(
                "release must match ^[A-Za-z0-9][A-Za-z0-9._+-]{0,63}$".into(),
            ));
        }
        if c.environment
            .as_deref()
            .is_some_and(|e| !valid_environment(e))
        {
            return Err(ConfigError(
                "environment must match ^[a-z0-9_-]{1,32}$".into(),
            ));
        }
        if !c.endpoint.starts_with("https://") && !is_loopback_http(&c.endpoint) {
            return Err(ConfigError(
                "endpoint must be https (http only to loopback)".into(),
            ));
        }
        let clock: Clock = c
            .clock
            .clone()
            .unwrap_or_else(|| Arc::new(|| system_ms(SystemTime::now())));
        let random: RandomSource = c.random.clone().unwrap_or_else(|| Arc::new(os_random));
        let seed = u64::from_le_bytes(os_random()[..8].try_into().unwrap_or([7; 8])) | 1;
        let settings = Settings {
            client: c.client,
            key: c.key,
            kind: c.kind,
            url: c.endpoint,
            max_batch: c.max_batch.clamp(1, MAX_BATCH_RECORDS),
            max_queue: c.max_queue.max(1),
            interval: c.interval,
            sampling: c.sampling,
            spool: c.spool,
            install_id_path: c.install_id_path,
            shutdown_timeout: c.shutdown_timeout,
            request_timeout: c.request_timeout,
            release: c.release,
            platform: c.platform,
            os: c.os,
            device_class: c.device_class,
            environment: c.environment,
            clock,
            random,
        };

        let mut last_error = None;
        let stored = settings
            .install_id_path
            .as_ref()
            .and_then(|p| fs::read_to_string(p).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| is_hex(s, 32));
        let install_id = match stored {
            Some(id) => id,
            None => {
                let id = hex(&(settings.random)());
                if let Some(p) = &settings.install_id_path {
                    if let Err(e) = write_atomic(p, id.as_bytes()) {
                        last_error = Some(format!("install id: {e}"));
                    }
                }
                id
            }
        };

        let shared = Arc::new(Shared {
            s: settings,
            state: Mutex::new(State {
                resource: Arc::new(Value::Null),
                resource_key: Arc::new(String::new()),
                install_id: install_id.clone(),
                queue: VecDeque::new(),
                queued_bytes: 0,
                formed: VecDeque::new(),
                pending_drops: [0; 5],
                grant: None,
                next_token: 1,
                stats: Stats {
                    last_error,
                    ..Stats::default()
                },
                failures: 0,
                retry_at: 0,
                next_tick: 0,
                flush_wanted: 0,
                flush_served: 0,
                stopping: false,
                closed: false,
                spool_dirty: false,
                worker_done: false,
            }),
            wake: Condvar::new(),
            done: Condvar::new(),
            rng: AtomicU64::new(seed),
        });

        {
            let mut st = shared.lock();
            let res = shared.resource(&install_id);
            st.resource_key = Arc::new(res.to_string());
            st.resource = Arc::new(res);
            st.next_tick = shared.now() + st_interval_ms(&shared.s);
            // What the spool kept from before: a crash or an offline period.
            if let Some(spool) = &shared.s.spool {
                if let Ok(raw) = fs::read_to_string(&spool.path) {
                    let mut bad = 0usize;
                    for line in raw.lines().filter(|l| !l.is_empty()) {
                        let Ok(Value::Object(mut v)) = serde_json::from_str::<Value>(line) else {
                            bad += 1;
                            continue;
                        };
                        let (Some(r @ Value::Object(_)), Some(res @ Value::Object(_))) =
                            (v.remove("r"), v.remove("res"))
                        else {
                            bad += 1;
                            continue;
                        };
                        if !res
                            .get("install_id")
                            .and_then(Value::as_str)
                            .is_some_and(|i| is_hex(i, 32))
                        {
                            bad += 1;
                            continue;
                        }
                        let cv = v.remove("cv").and_then(|c| c.as_str().map(String::from));
                        let key = Arc::new(res.to_string());
                        let item = Shared::item(r, Arc::new(res), key, cv);
                        shared.push(&mut st, item);
                    }
                    if bad > 0 || st.spool_dirty {
                        shared.spool_replace(&mut st);
                    }
                }
            }
        }

        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("stdio-telemetry".into())
            .spawn(move || run(worker_shared))
            .map_err(|e| ConfigError(format!("could not start the telemetry thread: {e}")))?;
        Ok(Telemetry {
            shared,
            worker: Mutex::new(Some(worker)),
        })
    }

    pub fn metric(&self, m: Metric) {
        let s = &self.shared;
        s.capture(s.s.sampling.metric, |ts| {
            if !valid_name(&m.name, 128)
                || m.name.starts_with("telemetry.")
                || (m.kind == MetricKind::Counter && m.value < 0.0)
            {
                return None;
            }
            let value = number(m.value)?;
            if m.unit.as_deref().is_some_and(|u| !valid_unit(u)) {
                return None;
            }
            let mut r = Map::new();
            r.insert("type".into(), Value::from("metric"));
            r.insert("ts".into(), Value::from(ts));
            if let Some(t) = trace_value(&m.trace) {
                r.insert("trace".into(), t);
            }
            r.insert(
                "kind".into(),
                Value::from(match m.kind {
                    MetricKind::Counter => "counter",
                    MetricKind::Gauge => "gauge",
                    MetricKind::Histogram => "histogram",
                }),
            );
            r.insert("name".into(), Value::from(m.name.clone()));
            r.insert("value".into(), value);
            if let Some(u) = &m.unit {
                r.insert("unit".into(), Value::from(u.clone()));
            }
            if let Some(a) = clean_attrs(&m.attrs) {
                r.insert("attrs".into(), Value::Object(a));
            }
            Some(r)
        });
    }

    pub fn log(&self, l: Log) {
        let s = &self.shared;
        s.capture(s.s.sampling.log, |ts| {
            if !valid_scope(&l.scope) {
                return None;
            }
            let message = text(Some(&l.message), 2000)?;
            let mut r = Map::new();
            r.insert("type".into(), Value::from("log"));
            r.insert("ts".into(), Value::from(ts));
            if let Some(t) = trace_value(&l.trace) {
                r.insert("trace".into(), t);
            }
            r.insert("level".into(), Value::from(l.level.as_str()));
            r.insert("scope".into(), Value::from(l.scope.clone()));
            r.insert("message".into(), Value::from(message));
            Some(r)
        });
    }

    pub fn error(&self, e: ErrorRecord) {
        let s = &self.shared;
        s.capture(s.s.sampling.error, |ts| {
            let error_type = text(Some(&e.error_type), 128)?;
            let mut r = Map::new();
            r.insert("type".into(), Value::from("error"));
            r.insert("ts".into(), Value::from(ts));
            if let Some(t) = trace_value(&e.trace) {
                r.insert("trace".into(), t);
            }
            r.insert("error_type".into(), Value::from(error_type));
            if let Some(m) = text(e.message.as_deref(), 2000) {
                r.insert("message".into(), Value::from(m));
            }
            if let Some(st) = text(e.stack.as_deref(), 16_384) {
                r.insert("stack".into(), Value::from(st));
            }
            if let Some(f) = text(e.fingerprint.as_deref(), 128) {
                r.insert("fingerprint".into(), Value::from(f));
            }
            if let Some(c) = e.code.as_deref().filter(|c| valid_code(c)) {
                r.insert("code".into(), Value::from(c));
            }
            Some(r)
        });
    }

    pub fn event(&self, ev: Event) {
        let s = &self.shared;
        s.capture(s.s.sampling.event, |ts| {
            if !valid_name(&ev.name, 128) || ev.name.starts_with("telemetry.") {
                return None;
            }
            let mut r = Map::new();
            r.insert("type".into(), Value::from("event"));
            r.insert("ts".into(), Value::from(ts));
            if let Some(t) = trace_value(&ev.trace) {
                r.insert("trace".into(), t);
            }
            r.insert("name".into(), Value::from(ev.name.clone()));
            if let Some(a) = clean_attrs(&ev.attrs) {
                r.insert("attrs".into(), Value::Object(a));
            }
            Some(r)
        });
    }

    /// Consent: the only way to attach identity.
    pub fn consent(&self) -> ConsentApi<'_> {
        ConsentApi {
            shared: &self.shared,
        }
    }

    /// Sends everything queued now. Returns once the queue is empty (true), an attempt failed, or `timeout` passed (false).
    pub fn flush(&self, timeout: Duration) -> bool {
        let s = &self.shared;
        let deadline = std::time::Instant::now() + timeout;
        let mut st = s.lock();
        if st.worker_done {
            return !Shared::has_work(&st);
        }
        st.flush_wanted += 1;
        let ticket = st.flush_wanted;
        s.wake.notify_all();
        while st.flush_served < ticket && !st.worker_done {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            st = s
                .done
                .wait_timeout(st, left)
                .map(|(g, _)| g)
                .unwrap_or_else(|e| e.into_inner().0);
        }
        !Shared::has_work(&st)
    }

    /// Flushes for at most `timeout`, then stops the thread. Records captured after are ignored.
    pub fn shutdown(&self, timeout: Duration) {
        let flushed = self.flush(timeout);
        {
            let mut st = self.shared.lock();
            st.closed = true;
            st.stopping = true;
            self.shared.spool_replace(&mut st);
            self.shared.wake.notify_all();
        }
        let handle = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take();
        // Join only when the thread is not stuck in a request; otherwise it ends on its own after it.
        if let Some(h) = handle {
            if flushed {
                let _ = h.join();
            }
        }
    }

    /// A new random install id; records captured from now on carry it, and it is kept at `install_id_path`.
    pub fn reset_install_id(&self) {
        let s = &self.shared;
        let id = hex(&(s.s.random)());
        let mut st = s.lock();
        if let Some(p) = &s.s.install_id_path {
            if let Err(e) = write_atomic(p, id.as_bytes()) {
                st.stats.last_error = Some(format!("install id: {e}"));
            }
        }
        let res = s.resource(&id);
        st.install_id = id;
        st.resource_key = Arc::new(res.to_string());
        st.resource = Arc::new(res);
    }

    /// The install id records are captured under now.
    pub fn install_id(&self) -> String {
        self.shared.lock().install_id.clone()
    }

    pub fn stats(&self) -> Stats {
        let st = self.shared.lock();
        let mut out = st.stats.clone();
        out.queued = st.queue.len() + st.formed.iter().map(|f| f.items.len()).sum::<usize>();
        out
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        let worker_running = self.worker.lock().map(|w| w.is_some()).unwrap_or(false);
        if worker_running {
            self.shutdown(self.shared.s.shutdown_timeout);
        }
    }
}

impl ConsentApi<'_> {
    /// Attach `consent.identity` from now on. Errors for a malformed version or an empty `sub`.
    pub fn grant(&self, consent: Consent) -> Result<ConsentHandle, ConfigError> {
        let s = self.shared;
        if !valid_consent_version(&consent.version) {
            return Err(ConfigError(
                "consent version must match ^[A-Za-z0-9._-]{1,64}$".into(),
            ));
        }
        let sub = text(Some(&consent.identity.sub), 255)
            .ok_or_else(|| ConfigError("consent needs identity.sub".into()))?;
        let granted_at = timestamp(consent.granted_at.map_or_else(|| s.now(), system_ms));
        let mut identity = Map::new();
        identity.insert("sub".into(), Value::from(sub));
        if let Some(d) = text(consent.identity.device_name.as_deref(), 64) {
            identity.insert("device_name".into(), Value::from(d));
        }
        if let Some(e) = consent
            .identity
            .email
            .as_deref()
            .filter(|e| e.contains('@') && e.len() >= 3 && e.len() <= 254)
        {
            identity.insert("email".into(), Value::from(e));
        }
        identity.insert(
            "consent".into(),
            json!({ "version": consent.version, "granted_at": granted_at }),
        );
        let token = {
            let mut st = s.lock();
            let token = st.next_token;
            st.next_token += 1;
            let previous = st.grant.replace(Grant {
                version: consent.version.clone(),
                identity: Value::Object(identity),
                token,
            });
            if previous.is_some_and(|p| p.version != consent.version) {
                s.forget_consent(&mut st);
            }
            token
        };
        s.consent_event("telemetry.consent.granted", &consent.version);
        Ok(ConsentHandle {
            shared: Arc::clone(s),
            token,
            version: consent.version,
            granted_at,
        })
    }

    /// The grant in force (its version and time), without its identity.
    pub fn current(&self) -> Option<(String, String)> {
        let st = self.shared.lock();
        st.grant.as_ref().map(|g| {
            (
                g.version.clone(),
                g.identity["consent"]["granted_at"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
            )
        })
    }
}

impl ConsentHandle {
    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn granted_at(&self) -> &str {
        &self.granted_at
    }

    /// True while this grant is the one in force.
    pub fn is_active(&self) -> bool {
        self.shared
            .lock()
            .grant
            .as_ref()
            .is_some_and(|g| g.token == self.token)
    }

    /// Stops sending identity at once and sends `telemetry.consent.revoked`. False when this grant was no longer in force.
    pub fn revoke(&self) -> bool {
        let s = &self.shared;
        {
            let mut st = s.lock();
            if !st.grant.as_ref().is_some_and(|g| g.token == self.token) {
                return false;
            }
            st.grant = None;
            s.forget_consent(&mut st);
        }
        s.consent_event("telemetry.consent.revoked", &self.version);
        true
    }
}

fn st_interval_ms(s: &Settings) -> u64 {
    s.interval.as_millis() as u64
}

// ── The background thread ───────────────────────────────────

struct Attempt {
    id: String,
    body: String,
    flushing: bool,
}

fn next_attempt(s: &Shared) -> Option<Attempt> {
    let mut st = s.lock();
    loop {
        let now = s.now();
        let flushing = st.flush_wanted > st.flush_served;
        if st.stopping && !flushing {
            st.worker_done = true;
            s.done.notify_all();
            return None;
        }
        if st.spool_dirty && (flushing || now >= st.next_tick) {
            s.spool_replace(&mut st);
        }
        let work = Shared::has_work(&st);
        if flushing && !work {
            st.flush_served = st.flush_wanted;
            s.done.notify_all();
            continue;
        }
        let interval = st_interval_ms(&s.s);
        let tick_due = interval > 0 && now >= st.next_tick;
        if tick_due {
            st.next_tick = now + interval;
        }
        // A batch already formed (a retry, or the halves of a 413) goes as soon as any wait is over.
        let due = work
            && (flushing
                || (now >= st.retry_at
                    && (tick_due || st.queue.len() >= s.s.max_batch || !st.formed.is_empty())));
        if due && (!st.formed.is_empty() || s.form(&mut st)) {
            let f = st.formed.front()?;
            let body = s.serialize(&st, f);
            return Some(Attempt {
                id: f.id.clone(),
                body,
                flushing,
            });
        }
        let mut wait = if interval > 0 {
            st.next_tick.saturating_sub(now)
        } else {
            60_000
        };
        if work && st.retry_at > now {
            wait = wait.min(st.retry_at - now);
        }
        st = s
            .wake
            .wait_timeout(st, Duration::from_millis(wait.clamp(5, 60_000)))
            .map(|(g, _)| g)
            .unwrap_or_else(|e| e.into_inner().0);
    }
}

struct Outcome {
    status: u16,
    retry_after: Option<String>,
    error: Option<String>,
}

fn send(agent: &ureq::Agent, s: &Settings, body: &str) -> Outcome {
    let mut req = agent
        .post(&s.url)
        .header("authorization", &format!("Bearer {}", s.key))
        .header("content-type", "application/json");
    let mut bytes = body.as_bytes().to_vec();
    if bytes.len() > 1024 {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        if let Ok(z) = enc.write_all(&bytes).and_then(|_| enc.finish()) {
            bytes = z;
            req = req.header("content-encoding", "gzip");
        }
    }
    match req.send(&bytes[..]) {
        Ok(mut res) => {
            let status = res.status().as_u16();
            let retry_after = res
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            let _ = res.body_mut().read_to_vec();
            Outcome {
                status,
                retry_after,
                error: None,
            }
        }
        Err(e) => Outcome {
            status: 0,
            retry_after: None,
            error: Some(e.to_string()),
        },
    }
}

fn run(shared: Arc<Shared>) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(shared.s.request_timeout))
        .http_status_as_error(false)
        .build()
        .into();
    while let Some(attempt) = next_attempt(&shared) {
        let outcome = send(&agent, &shared.s, &attempt.body);
        let s = &*shared;
        let mut st = s.lock();
        if outcome.status > 0 {
            st.stats.last_status = Some(outcome.status);
        }
        if let Some(e) = outcome.error {
            st.stats.last_error = Some(e);
        }
        let Some(pos) = st.formed.iter().position(|f| f.id == attempt.id) else {
            continue;
        };
        let status = outcome.status;
        let failed = if (200..300).contains(&status) {
            if let Some(f) = st.formed.remove(pos) {
                st.stats.sent += (f.items.len() + f.extra.len()) as u64;
            }
            st.failures = 0;
            st.retry_at = 0;
            s.spool_replace(&mut st);
            false
        } else if status == 413 {
            if let Some(f) = st.formed.remove(pos) {
                let n = f.items.len() + f.extra.len();
                if n > 1 {
                    let half = n.div_ceil(2);
                    let mut items = f.items;
                    let mut extra = f.extra;
                    let (first_items, first_extra) = if half >= items.len() {
                        let e2 = extra.split_off(half - items.len());
                        (
                            std::mem::take(&mut items),
                            std::mem::replace(&mut extra, e2),
                        )
                    } else {
                        let rest = items.split_off(half);
                        (std::mem::replace(&mut items, rest), Vec::new())
                    };
                    let second = Formed {
                        id: hex(&(s.s.random)()),
                        items,
                        extra,
                        res: Arc::clone(&f.res),
                        cv: f.cv.clone(),
                    };
                    let first = Formed {
                        id: hex(&(s.s.random)()),
                        items: first_items,
                        extra: first_extra,
                        res: f.res,
                        cv: f.cv,
                    };
                    st.formed.insert(pos, second);
                    st.formed.insert(pos, first);
                } else {
                    let k = f.items.len() as u64;
                    if k > 0 {
                        s.drop_records(&mut st, DropReason::TooLarge, k);
                        s.spool_replace(&mut st);
                    }
                }
            }
            false
        } else if status == 0 || status == 429 || status >= 500 {
            st.failures += 1;
            let now = s.now();
            let wait = if status == 429 || status == 503 {
                retry_after(outcome.retry_after.as_deref(), now)
            } else {
                None
            }
            .unwrap_or_else(|| backoff(st.failures, s.unit()));
            st.retry_at = now + wait.as_millis() as u64;
            true
        } else {
            if let Some(f) = st.formed.remove(pos) {
                let k = f.items.len() as u64;
                if k > 0 {
                    s.drop_records(
                        &mut st,
                        if status == 400 {
                            DropReason::Invalid
                        } else {
                            DropReason::Refused
                        },
                        k,
                    );
                    s.spool_replace(&mut st);
                }
            }
            false
        };
        if failed && attempt.flushing {
            // A flush makes one attempt per batch and reports the failure; the retry waits as scheduled.
            st.flush_served = st.flush_wanted;
            s.done.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_the_wire_form() {
        assert_eq!(timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(timestamp(1_795_802_641_123), "2026-11-27T18:04:01.123Z");
        assert_eq!(timestamp(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn http_dates_and_retry_after() {
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777_000)
        );
        assert_eq!(retry_after(Some("60"), 0), Some(Duration::from_secs(60)));
        assert_eq!(retry_after(Some("0"), 0), Some(Duration::from_secs(1)));
        assert_eq!(
            retry_after(Some("99999"), 0),
            Some(Duration::from_secs(3600))
        );
        assert_eq!(
            retry_after(Some("Thu, 01 Jan 1970 00:00:30 GMT"), 0),
            Some(Duration::from_secs(30))
        );
        assert_eq!(retry_after(Some("soon"), 0), None);
        assert_eq!(retry_after(None, 0), None);
    }

    #[test]
    fn backoff_follows_the_spec() {
        assert_eq!(backoff(1, 0.0), Duration::from_millis(500));
        assert_eq!(backoff(1, 1.0), Duration::from_secs(1));
        assert_eq!(backoff(4, 0.0), Duration::from_secs(4));
        assert_eq!(backoff(20, 1.0), Duration::from_secs(300));
    }

    #[test]
    fn utf8_truncation_never_splits_a_character() {
        let s = "é".repeat(1500);
        assert_eq!(utf8_prefix(&s, 2000).len(), 2000);
        assert_eq!(utf8_prefix(&s, 2001).len(), 2000);
        assert_eq!(utf8_prefix("abc", 10), "abc");
    }

    #[test]
    fn names_match_the_wire_pattern() {
        for ok in ["a", "sync.runs", "a_b.c1.2x", "http.request.method"] {
            assert!(valid_name(ok, 128), "{ok}");
        }
        for bad in ["", "A", "1a", "a..b", "a.", ".a", "a-b", "a b"] {
            assert!(!valid_name(bad, 128), "{bad}");
        }
    }

    #[test]
    fn integral_numbers_print_as_integers() {
        assert_eq!(number(1.0).unwrap().to_string(), "1");
        assert_eq!(number(0.5).unwrap().to_string(), "0.5");
        assert!(number(f64::NAN).is_none());
    }
}
