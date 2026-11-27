//! The telemetry client against a fake intake: a real HTTP server on
//! 127.0.0.1 that checks every request against docs/telemetry-wire.md,
//! strictly, and records what it was sent. `cargo test --features telemetry`.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use flate2::read::GzDecoder;
use serde_json::{Map, Value};
use stdio::telemetry::{
    backoff, drop_key, retry_after, Config, Consent, ErrorRecord, Event, Identity, Kind, Level,
    Log, Metric, Os, Platform, Sampling, SpoolConfig, Telemetry, TraceContext, SDK_NAME,
    SDK_VERSION,
};

const KEY: &str = "0123456789abcdef0123456789abcdef";
const T: u64 = 1_795_802_641_000; // 2026-11-27T18:04:01.000Z

fn golden() -> Value {
    serde_json::from_str(include_str!("../test/fixtures/telemetry-golden.json")).unwrap()
}

// ── The fake intake ─────────────────────────────────────────

#[derive(Clone, Debug)]
enum Answer {
    Status(u16, Option<&'static str>),
    Hang,
}

#[derive(Clone, Debug)]
struct Received {
    headers: Vec<(String, String)>,
    body: Value,
    problems: Vec<String>,
    at: Instant,
}

impl Received {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    fn records(&self) -> Vec<Value> {
        self.body["records"].as_array().cloned().unwrap_or_default()
    }
}

struct Intake {
    url: String,
    received: Arc<Mutex<Vec<Received>>>,
    answers: Arc<Mutex<VecDeque<Answer>>>,
    fallback: Arc<Mutex<Answer>>,
    stop: Arc<AtomicBool>,
}

impl Intake {
    fn start() -> Intake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let intake = Intake {
            url: format!("http://127.0.0.1:{port}/v1/ingest"),
            received: Arc::default(),
            answers: Arc::default(),
            fallback: Arc::new(Mutex::new(Answer::Status(202, None))),
            stop: Arc::default(),
        };
        let (received, answers, fallback, stop) = (
            intake.received.clone(),
            intake.answers.clone(),
            intake.fallback.clone(),
            intake.stop.clone(),
        );
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (received, answers, fallback, stop) = (
                            received.clone(),
                            answers.clone(),
                            fallback.clone(),
                            stop.clone(),
                        );
                        thread::spawn(move || serve(stream, received, answers, fallback, stop));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(2)),
                }
            }
        });
        intake
    }

    fn all(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }

    fn records(&self) -> Vec<Value> {
        self.all().iter().flat_map(|r| r.records()).collect()
    }

    fn names(&self) -> Vec<String> {
        self.records()
            .iter()
            .map(|r| {
                r.get("name")
                    .or_else(|| r.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            })
            .collect()
    }

    fn wait_for(&self, n: usize, within: Duration) {
        let end = Instant::now() + within;
        while self.received.lock().unwrap().len() < n {
            assert!(
                Instant::now() < end,
                "intake: {} of {n} requests",
                self.received.lock().unwrap().len()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn answer(&self, a: Answer) {
        self.answers.lock().unwrap().push_back(a);
    }
}

impl Drop for Intake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if !thread::panicking() {
            for r in self.all() {
                assert!(
                    r.problems.is_empty(),
                    "the client broke the wire format: {:?}",
                    r.problems
                );
            }
        }
    }
}

fn serve(
    stream: TcpStream,
    received: Arc<Mutex<Vec<Received>>>,
    answers: Arc<Mutex<VecDeque<Answer>>>,
    fallback: Arc<Mutex<Answer>>,
    stop: Arc<AtomicBool>,
) {
    stream.set_nonblocking(false).ok();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut out = stream;
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
            return;
        }
        let mut headers = Vec::new();
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
                if k == "content-length" {
                    length = v.parse().unwrap_or(0);
                }
                headers.push((k, v));
            }
        }
        let mut raw = vec![0; length];
        if reader.read_exact(&mut raw).is_err() {
            return;
        }
        let (problems, body) = if request_line.starts_with("POST /v1/ingest ") {
            check_batch(&headers, &raw)
        } else {
            (
                vec![format!("request: {}", request_line.trim())],
                Value::Null,
            )
        };
        let n = body["records"].as_array().map_or(0, Vec::len);
        received.lock().unwrap().push(Received {
            headers,
            body,
            problems,
            at: Instant::now(),
        });
        let answer = answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| fallback.lock().unwrap().clone());
        match answer {
            Answer::Hang => {
                while !stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(10));
                }
                return;
            }
            Answer::Status(status, retry) => {
                let payload = if status == 202 {
                    format!("{{\"accepted\":{n},\"rejected\":[],\"duplicate\":false}}")
                } else {
                    "{\"error\":\"test\"}".to_string()
                };
                let retry = retry
                    .map(|r| format!("retry-after: {r}\r\n"))
                    .unwrap_or_default();
                let resp = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{retry}\r\n{payload}", payload.len());
                if out.write_all(resp.as_bytes()).is_err() {
                    return;
                }
            }
        }
    }
}

// ── The spec, checked ───────────────────────────────────────

fn is_ts(v: &Value) -> bool {
    v.as_str().is_some_and(|s| {
        let b = s.as_bytes();
        b.len() == 24
            && b[23] == b'Z'
            && b[10] == b'T'
            && b[19] == b'.'
            && [4, 7].iter().all(|i| b[*i] == b'-')
            && [13, 16].iter().all(|i| b[*i] == b':')
            && b.iter()
                .enumerate()
                .all(|(i, c)| [4, 7, 10, 13, 16, 19, 23].contains(&i) || c.is_ascii_digit())
    })
}

fn is_name(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.as_bytes()[0].is_ascii_lowercase()
        && s.split('.').all(|seg| {
            !seg.is_empty()
                && seg
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

fn is_hex(v: &Value, n: usize) -> bool {
    v.as_str().is_some_and(|s| {
        s.len() == n
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn str_len(v: &Value, min: usize, max: usize) -> bool {
    v.as_str().is_some_and(|s| s.len() >= min && s.len() <= max)
}

fn only(o: &Map<String, Value>, allowed: &[&str], at: &str, out: &mut Vec<String>) {
    for k in o.keys() {
        if !allowed.contains(&k.as_str()) {
            out.push(format!("{at}: unknown field {k}"));
        }
    }
}

fn check_attrs(v: Option<&Value>, at: &str, out: &mut Vec<String>) {
    let Some(v) = v else { return };
    let Some(o) = v.as_object() else {
        return out.push(format!("{at}.attrs: not an object"));
    };
    if o.is_empty() || o.len() > 32 {
        out.push(format!("{at}.attrs: {} entries", o.len()));
    }
    for (k, x) in o {
        if !is_name(k, 64) {
            out.push(format!("{at}.attrs: bad key {k}"));
        }
        let ok = x.as_str().is_some_and(|s| s.len() <= 256) || x.is_number() || x.is_boolean();
        if !ok {
            out.push(format!("{at}.attrs.{k}: bad value"));
        }
    }
}

fn check_record(r: &Value, at: &str, out: &mut Vec<String>) {
    let Some(o) = r.as_object() else {
        return out.push(format!("{at}: not an object"));
    };
    if r.to_string().len() > 32_768 {
        out.push(format!("{at}: over 32 KiB"));
    }
    if !is_ts(&r["ts"]) {
        out.push(format!("{at}.ts"));
    }
    if let Some(t) = o.get("trace") {
        if !is_hex(&t["trace_id"], 32) || !is_hex(&t["span_id"], 16) {
            out.push(format!("{at}.trace"));
        }
    }
    if let Some(s) = o.get("sample_rate") {
        if !s.as_f64().is_some_and(|x| x > 0.0 && x < 1.0) {
            out.push(format!("{at}.sample_rate"));
        }
    }
    let common = ["type", "ts", "trace", "sample_rate"];
    let with = |extra: &[&'static str]| -> Vec<&'static str> {
        common
            .iter()
            .copied()
            .chain(extra.iter().copied())
            .collect()
    };
    match r["type"].as_str() {
        Some("metric") => {
            only(
                o,
                &with(&["kind", "name", "value", "unit", "attrs"]),
                at,
                out,
            );
            if !["counter", "gauge", "histogram"].contains(&r["kind"].as_str().unwrap_or("")) {
                out.push(format!("{at}.kind"));
            }
            if !r["name"].as_str().is_some_and(|n| is_name(n, 128)) {
                out.push(format!("{at}.name"));
            }
            if !r["value"].is_number()
                || (r["kind"] == "counter" && r["value"].as_f64().unwrap_or(-1.0) < 0.0)
            {
                out.push(format!("{at}.value"));
            }
            check_attrs(o.get("attrs"), at, out);
        }
        Some("log") => {
            only(o, &with(&["level", "scope", "message"]), at, out);
            if !["debug", "info", "warn", "error"].contains(&r["level"].as_str().unwrap_or(""))
                || !str_len(&r["scope"], 1, 64)
                || !str_len(&r["message"], 1, 2000)
            {
                out.push(format!("{at}: log fields"));
            }
        }
        Some("error") => {
            only(
                o,
                &with(&["error_type", "message", "stack", "fingerprint", "code"]),
                at,
                out,
            );
            if !str_len(&r["error_type"], 1, 128) {
                out.push(format!("{at}.error_type"));
            }
            for (k, max) in [
                ("message", 2000),
                ("stack", 16_384),
                ("fingerprint", 128),
                ("code", 64),
            ] {
                if let Some(v) = o.get(k) {
                    if !str_len(v, 1, max) {
                        out.push(format!("{at}.{k}"));
                    }
                }
            }
        }
        Some("event") => {
            only(o, &with(&["name", "attrs"]), at, out);
            if !r["name"].as_str().is_some_and(|n| is_name(n, 128)) {
                out.push(format!("{at}.name"));
            }
            check_attrs(o.get("attrs"), at, out);
        }
        other => out.push(format!("{at}.type: {other:?}")),
    }
}

fn check_batch(headers: &[(String, String)], raw: &[u8]) -> (Vec<String>, Value) {
    let mut out = Vec::new();
    let h = |k: &str| {
        headers
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    };
    if h("authorization") != Some(&format!("Bearer {KEY}")) {
        out.push(format!("authorization: {:?}", h("authorization")));
    }
    if h("content-type") != Some("application/json") {
        out.push(format!("content-type: {:?}", h("content-type")));
    }
    if raw.len() > 524_288 {
        out.push("body over 512 KiB".into());
    }
    let decoded = match h("content-encoding") {
        None => raw.to_vec(),
        Some("gzip") => {
            let mut d = Vec::new();
            GzDecoder::new(raw).read_to_end(&mut d).unwrap();
            d
        }
        Some(other) => {
            out.push(format!("content-encoding: {other}"));
            raw.to_vec()
        }
    };
    let Ok(body) = serde_json::from_slice::<Value>(&decoded) else {
        out.push("not JSON".into());
        return (out, Value::Null);
    };
    let Some(b) = body.as_object() else {
        out.push("not an object".into());
        return (out, body);
    };
    only(
        b,
        &[
            "v", "client", "batch", "sent_at", "resource", "identity", "records",
        ],
        "batch",
        &mut out,
    );
    if body["v"] != 1
        || !str_len(&body["client"], 1, 64)
        || !is_hex(&body["batch"], 32)
        || !is_ts(&body["sent_at"])
    {
        out.push("batch fields".into());
    }
    match body["resource"].as_object() {
        Some(res) => {
            only(
                res,
                &[
                    "release",
                    "platform",
                    "os",
                    "device_class",
                    "install_id",
                    "sdk",
                    "environment",
                ],
                "resource",
                &mut out,
            );
            if !str_len(&res["release"], 1, 64)
                || !is_hex(&res["install_id"], 32)
                || ![
                    "browser",
                    "worker",
                    "node",
                    "bun",
                    "deno",
                    "react-native",
                    "native",
                ]
                .contains(&res["platform"].as_str().unwrap_or(""))
            {
                out.push("resource fields".into());
            }
            if let Some(sdk) = res["sdk"].as_object() {
                only(sdk, &["name", "version"], "resource.sdk", &mut out);
            } else {
                out.push("resource.sdk".into());
            }
            if let Some(os) = res.get("os") {
                match os.as_object() {
                    Some(os) => only(os, &["name", "version"], "resource.os", &mut out),
                    None => out.push("resource.os".into()),
                }
            }
        }
        None => out.push("resource".into()),
    }
    if let Some(id) = b.get("identity") {
        match id.as_object() {
            Some(o) => {
                only(
                    o,
                    &["sub", "device_name", "email", "consent"],
                    "identity",
                    &mut out,
                );
                if !str_len(&id["sub"], 1, 255)
                    || !str_len(&id["consent"]["version"], 1, 64)
                    || !is_ts(&id["consent"]["granted_at"])
                {
                    out.push("identity fields".into());
                }
            }
            None => out.push("identity".into()),
        }
    }
    match body["records"].as_array() {
        Some(rs) if !rs.is_empty() && rs.len() <= 500 => {
            for (i, r) in rs.iter().enumerate() {
                check_record(r, &format!("records[{i}]"), &mut out);
            }
        }
        _ => out.push("records: 1 to 500".into()),
    }
    (out, body)
}

// ── Helpers ─────────────────────────────────────────────────

fn config(intake: &Intake) -> Config {
    let mut c = Config::new("ava-desktop", KEY, Kind::Service, "1.4.2");
    c.endpoint = intake.url.clone();
    c.interval = Duration::ZERO;
    c.request_timeout = Duration::from_secs(2);
    c
}

/// An endpoint nothing listens on.
fn down() -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    format!("http://127.0.0.1:{port}/v1/ingest")
}

/// These 32-hex values first, in order, then real randomness.
fn fixed_random(hexes: &[&str]) -> stdio::telemetry::RandomSource {
    let queue: Arc<Mutex<VecDeque<[u8; 16]>>> = Arc::new(Mutex::new(
        hexes
            .iter()
            .map(|h| {
                let mut b = [0u8; 16];
                for (i, byte) in b.iter_mut().enumerate() {
                    *byte = u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap();
                }
                b
            })
            .collect(),
    ));
    let counter = Arc::new(AtomicUsize::new(1));
    Arc::new(move || {
        queue.lock().unwrap().pop_front().unwrap_or_else(|| {
            let n = counter.fetch_add(1, Ordering::Relaxed) as u128;
            (n.wrapping_mul(0x9e37_79b9_7f4a_7c15_f39c_c060_5ced_c834)
                ^ u128::from(std::process::id()))
            .to_le_bytes()
        })
    })
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "stdio-telemetry-{}-{name}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const SEC: Duration = Duration::from_secs(5);

/// The calls the golden fixture is made from (test/telemetry.test.ts makes the same ones).
fn golden_calls(t: &Telemetry) {
    let g = golden();
    let c = &g["calls"];
    let s = |v: &Value| v.as_str().unwrap().to_string();
    let mut counter = Metric::counter(
        &s(&c["counter"]["name"]),
        c["counter"]["value"].as_f64().unwrap(),
    )
    .unit(&s(&c["counter"]["unit"]));
    for (k, v) in c["counter"]["attrs"].as_object().unwrap() {
        counter = counter.attr(k, s(v));
    }
    t.metric(counter);
    t.log(Log::new(
        Level::Info,
        &s(&c["log"]["scope"]),
        &s(&c["log"]["message"]),
    ));
    let e = &c["error"];
    t.error(
        ErrorRecord::new(&s(&e["type"]))
            .message(&s(&e["message"]))
            .stack(&s(&e["stack"]))
            .fingerprint(&s(&e["fingerprint"]))
            .code(&s(&e["code"])),
    );
    let mut event = Event::new(&s(&c["event"]["name"]));
    for (k, v) in c["event"]["attrs"].as_object().unwrap() {
        event = event.attr(k, s(v));
    }
    t.event(event);
    let gauge = &c["gauge"];
    t.metric(
        Metric::gauge(&s(&gauge["name"]), gauge["value"].as_f64().unwrap()).trace(TraceContext {
            trace_id: s(&gauge["trace"]["trace_id"]),
            span_id: s(&gauge["trace"]["span_id"]),
        }),
    );
}

fn grant() -> Consent {
    Consent {
        version: "2026-11-27".into(),
        granted_at: Some(SystemTime::UNIX_EPOCH + Duration::from_millis(1_795_802_340_000)),
        identity: Identity {
            sub: "acc_01J9Z5V7X2".into(),
            device_name: Some("Maya's MacBook".into()),
            email: None,
        },
    }
}

// ── The wire format ─────────────────────────────────────────

#[test]
fn batches_are_the_golden_fixture() {
    for (kind, key) in [
        (Kind::Service, "service"),
        (Kind::CustomerApp, "customer-app"),
    ] {
        let intake = Intake::start();
        let g = golden();
        let want = &g[key];
        let mut c = config(&intake);
        c.kind = kind;
        c.platform = Platform::Native;
        c.os = Some(Os {
            name: Some("macOS".into()),
            version: Some("26.1".into()),
        });
        c.device_class = Some("Mac".into());
        c.environment = Some("production".into());
        c.clock = Some(Arc::new(|| T));
        c.random = Some(fixed_random(&[
            want["resource"]["install_id"].as_str().unwrap(),
            want["batch"].as_str().unwrap(),
        ]));
        let t = Telemetry::start(c).unwrap();
        golden_calls(&t);
        assert!(t.flush(SEC));
        let got = intake.all();
        assert_eq!(got.len(), 1);
        let mut body = got[0].body.clone();
        let sdk = body["resource"]
            .as_object_mut()
            .unwrap()
            .remove("sdk")
            .unwrap();
        assert_eq!(
            sdk,
            serde_json::json!({ "name": SDK_NAME, "version": SDK_VERSION })
        );
        assert_eq!(&body, want, "{key}");
        assert_eq!(
            got[0].header("authorization"),
            Some(format!("Bearer {KEY}").as_str())
        );
    }
}

#[test]
fn a_body_over_1_kib_is_gzipped() {
    let intake = Intake::start();
    let t = Telemetry::start(config(&intake)).unwrap();
    for i in 0..40 {
        t.log(Log::new(
            Level::Info,
            "sync",
            &format!("line {i} ").repeat(10),
        ));
    }
    assert!(t.flush(SEC));
    assert_eq!(intake.all()[0].header("content-encoding"), Some("gzip"));
    assert_eq!(intake.records().len(), 40);
}

#[test]
fn reserved_and_malformed_records_are_counted_as_invalid() {
    let intake = Intake::start();
    let t = Telemetry::start(config(&intake)).unwrap();
    t.event(Event::new("telemetry.consent.revoked"));
    t.event(Event::new("Not A Name"));
    t.metric(Metric::counter("ok.count", -1.0));
    t.metric(Metric::gauge("ok.gauge", f64::NAN));
    t.event(Event::new("ok.event"));
    assert!(t.flush(SEC));
    assert_eq!(intake.names(), ["ok.event", "telemetry.dropped"]);
    let drop = intake
        .records()
        .into_iter()
        .find(|r| r["name"] == "telemetry.dropped")
        .unwrap();
    assert_eq!(drop["value"], 4);
    assert_eq!(drop["attrs"]["reason"], "invalid");
}

#[test]
fn drop_key_matches_the_collector() {
    let g = golden();
    for (key, want) in g["drop_key"].as_object().unwrap() {
        assert_eq!(drop_key(key), want.as_bool().unwrap(), "{key}");
    }
}

// ── Consent ─────────────────────────────────────────────────

#[test]
fn no_identity_without_a_grant() {
    let intake = Intake::start();
    let t = Telemetry::start(config(&intake)).unwrap();
    t.event(Event::new("app.open"));
    t.error(ErrorRecord::new("TypeError").message("boom"));
    assert!(t.flush(SEC));
    assert!(!intake.all().is_empty());
    for r in intake.all() {
        assert!(r.body.get("identity").is_none());
    }
}

#[test]
fn identity_after_a_grant_only_on_records_captured_under_it() {
    let intake = Intake::start();
    let t = Telemetry::start(config(&intake)).unwrap();
    t.event(Event::new("before.grant"));
    let handle = t.consent().grant(grant()).unwrap();
    t.event(Event::new("after.grant"));
    assert!(t.flush(SEC));
    let got = intake.all();
    assert_eq!(got.len(), 2);
    assert!(got[0].body.get("identity").is_none());
    assert_eq!(got[0].records().len(), 1);
    assert_eq!(
        got[1].body["identity"],
        serde_json::json!({ "sub": "acc_01J9Z5V7X2", "device_name": "Maya's MacBook", "consent": { "version": "2026-11-27", "granted_at": "2026-11-27T17:59:00.000Z" } })
    );
    let names: Vec<_> = got[1]
        .records()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["telemetry.consent.granted", "after.grant"]);
    assert!(handle.is_active());
}

#[test]
fn identity_is_gone_after_revoke() {
    let intake = Intake::start();
    let t = Telemetry::start(config(&intake)).unwrap();
    let handle = t.consent().grant(grant()).unwrap();
    t.event(Event::new("during.consent"));
    assert!(handle.revoke());
    t.event(Event::new("after.revoke"));
    assert!(t.flush(SEC));
    for r in intake.all() {
        assert!(r.body.get("identity").is_none(), "{:?}", r.body);
    }
    assert_eq!(
        intake.names(),
        [
            "telemetry.consent.granted",
            "during.consent",
            "telemetry.consent.revoked",
            "after.revoke"
        ]
    );
    assert!(!handle.is_active());
    assert!(!handle.revoke());
    assert!(t.consent().current().is_none());
}

#[test]
fn identity_is_never_spooled() {
    let dir = scratch("consent");
    let path = dir.join("spool.ndjson");
    let mut c = Config::new("ava-desktop", KEY, Kind::Service, "1.4.2");
    c.endpoint = down();
    c.interval = Duration::ZERO;
    c.spool = Some(SpoolConfig::new(&path));
    let t = Telemetry::start(c).unwrap();
    let mut g = grant();
    g.identity.email = Some("maya@example.com".into());
    let _handle = t.consent().grant(g).unwrap();
    t.event(Event::new("during.consent"));
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(on_disk.contains("during.consent"));
    for secret in ["acc_01J9Z5V7X2", "maya@example.com", "MacBook"] {
        assert!(!on_disk.contains(secret), "{secret} on disk");
    }
    std::mem::forget(t);
}

#[test]
fn attributes_cannot_carry_identity() {
    let intake = Intake::start();
    let t = Telemetry::start(config(&intake)).unwrap();
    t.event(
        Event::new("sneaky")
            .attr("user", "maya")
            .attr("user.id", "u1")
            .attr("email", "m@x.y")
            .attr("user_name", "m")
            .attr("enduser.id", "x")
            .attr("session_id", "s")
            .attr("ok", 1),
    );
    assert!(t.flush(SEC));
    assert_eq!(intake.records()[0]["attrs"], serde_json::json!({ "ok": 1 }));
}

// ── Spool ───────────────────────────────────────────────────

#[test]
fn spool_survives_a_restart() {
    let dir = scratch("spool");
    let spool = dir.join("spool.ndjson");
    let id_path = dir.join("install-id");
    let mut c = Config::new("ava-desktop", KEY, Kind::Service, "1.4.2");
    c.endpoint = down();
    c.interval = Duration::ZERO;
    c.spool = Some(SpoolConfig::new(&spool));
    c.install_id_path = Some(id_path.clone());
    let first = Telemetry::start(c).unwrap();
    first.event(Event::new("before.crash").attr("n", 1));
    first.metric(Metric::counter("jobs.done", 2.0));
    // The process dies here: no flush, no shutdown, no Drop.
    std::mem::forget(first);

    let intake = Intake::start();
    let mut c = config(&intake);
    c.release = "1.4.3".into();
    c.spool = Some(SpoolConfig::new(&spool));
    c.install_id_path = Some(id_path);
    let second = Telemetry::start(c).unwrap();
    second.event(Event::new("after.restart"));
    assert!(second.flush(SEC));
    assert_eq!(
        intake.names(),
        ["before.crash", "jobs.done", "after.restart"]
    );
    let releases: Vec<_> = intake
        .all()
        .iter()
        .map(|r| r.body["resource"]["release"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(releases, ["1.4.2", "1.4.3"]);
    let ids: std::collections::HashSet<_> = intake
        .all()
        .iter()
        .map(|r| {
            r.body["resource"]["install_id"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(ids.len(), 1);
    assert_eq!(std::fs::read_to_string(&spool).unwrap(), "");
}

#[test]
fn the_spool_is_capped_and_the_oldest_go() {
    let dir = scratch("cap");
    let spool = dir.join("spool.ndjson");
    let mut c = Config::new("ava-desktop", KEY, Kind::Service, "1.4.2");
    c.endpoint = down();
    c.interval = Duration::ZERO;
    c.spool = Some(SpoolConfig {
        path: spool.clone(),
        max_bytes: 4096,
    });
    let t = Telemetry::start(c).unwrap();
    for i in 0..200 {
        t.event(Event::new("tick").attr("i", i));
    }
    t.shutdown(SEC);
    let live: Vec<String> = std::fs::read_to_string(&spool)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    assert!(live.len() < 200);
    assert!(live.iter().map(|l| l.len() + 1).sum::<usize>() <= 4096);
    let last: Value = serde_json::from_str(live.last().unwrap()).unwrap();
    assert_eq!(last["r"]["attrs"]["i"], 199);
    assert_eq!(t.stats().dropped.spool_full, 200 - live.len() as u64);
}

// ── Never in the way ────────────────────────────────────────

#[test]
fn capture_never_blocks_while_the_intake_is_down() {
    let mut c = Config::new("ava-desktop", KEY, Kind::Service, "1.4.2");
    c.endpoint = down();
    c.interval = Duration::ZERO;
    c.max_queue = 100;
    c.max_batch = 50;
    let t = Telemetry::start(c).unwrap();
    let start = Instant::now();
    for i in 0..5000 {
        t.event(Event::new("tick").attr("i", i));
    }
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
    assert!(!t.flush(SEC));
    let s = t.stats();
    assert_eq!(s.queued, 100);
    assert_eq!(s.dropped.queue_full, 4900);
    assert!(s.last_error.is_some());
}

#[test]
fn capture_does_not_wait_on_a_request_that_hangs() {
    let intake = Intake::start();
    *intake.fallback.lock().unwrap() = Answer::Hang;
    let mut c = config(&intake);
    c.max_batch = 10;
    c.request_timeout = Duration::from_secs(30);
    c.shutdown_timeout = Duration::from_millis(100);
    let t = Telemetry::start(c).unwrap();
    for i in 0..10 {
        t.event(Event::new("first").attr("i", i));
    }
    intake.wait_for(1, SEC);
    let start = Instant::now();
    for i in 0..1000 {
        t.event(Event::new("more").attr("i", i));
    }
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(t.stats().queued, 1000);
    assert_eq!(t.stats().dropped.queue_full, 10);
    // Dropping the client does not wait out the hanging request.
    let start = Instant::now();
    drop(t);
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn drops_reach_the_intake_once_it_is_back() {
    let intake = Intake::start();
    intake.answer(Answer::Status(503, Some("1")));
    let mut c = config(&intake);
    c.max_queue = 5;
    let t = Telemetry::start(c).unwrap();
    for i in 0..8 {
        t.event(Event::new("tick").attr("i", i));
    }
    assert!(!t.flush(SEC));
    assert!(t.flush(SEC));
    let last = intake.all().last().unwrap().records();
    let ticks: Vec<_> = last
        .iter()
        .filter(|r| r["name"] == "tick")
        .map(|r| r["attrs"]["i"].as_i64().unwrap())
        .collect();
    assert_eq!(ticks, [3, 4, 5, 6, 7]);
    let drop = last
        .iter()
        .find(|r| r["name"] == "telemetry.dropped")
        .unwrap();
    assert_eq!(drop["value"], 3);
    assert_eq!(drop["attrs"]["reason"], "queue_full");
}

// ── Responses ───────────────────────────────────────────────

#[test]
fn a_scheduled_retry_honours_retry_after_with_the_same_batch() {
    let intake = Intake::start();
    intake.answer(Answer::Status(429, Some("1")));
    let mut c = config(&intake);
    c.interval = Duration::from_millis(50);
    let t = Telemetry::start(c).unwrap();
    t.event(Event::new("limited"));
    intake.wait_for(2, SEC);
    let got = intake.all();
    assert_eq!(got[0].body["batch"], got[1].body["batch"]);
    assert!(
        got[1].at - got[0].at >= Duration::from_millis(950),
        "{:?}",
        got[1].at - got[0].at
    );
}

#[test]
fn a_413_splits_the_batch_oldest_half_first() {
    let intake = Intake::start();
    intake.answer(Answer::Status(413, None));
    let t = Telemetry::start(config(&intake)).unwrap();
    for i in 0..4 {
        t.event(Event::new("part").attr("i", i));
    }
    assert!(t.flush(SEC));
    let got = intake.all();
    assert_eq!(got.len(), 3);
    let ids: std::collections::HashSet<_> = got
        .iter()
        .map(|r| r.body["batch"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 3);
    let is = |r: &Received| {
        r.records()
            .iter()
            .map(|x| x["attrs"]["i"].as_i64().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(is(&got[1]), [0, 1]);
    assert_eq!(is(&got[2]), [2, 3]);
}

#[test]
fn a_400_discards_the_batch_and_counts_it() {
    let intake = Intake::start();
    intake.answer(Answer::Status(400, None));
    let t = Telemetry::start(config(&intake)).unwrap();
    t.event(Event::new("bad"));
    assert!(t.flush(SEC));
    t.event(Event::new("next"));
    assert!(t.flush(SEC));
    assert_eq!(intake.names(), ["bad", "telemetry.dropped", "next"]);
    assert_eq!(t.stats().dropped.invalid, 1);
}

#[test]
fn retry_after_and_backoff_follow_the_spec() {
    assert_eq!(retry_after(Some("60"), 0), Some(Duration::from_secs(60)));
    assert_eq!(backoff(1, 0.0), Duration::from_millis(500));
    assert_eq!(backoff(20, 1.0), Duration::from_secs(300));
}

// ── Timers, sampling, the install id ────────────────────────

#[test]
fn the_interval_sends_without_a_flush() {
    let intake = Intake::start();
    let mut c = config(&intake);
    c.interval = Duration::from_millis(30);
    let t = Telemetry::start(c).unwrap();
    t.event(Event::new("timed"));
    intake.wait_for(1, SEC);
    assert_eq!(intake.names(), ["timed"]);
    drop(t);
}

#[test]
fn sampling_is_per_type_and_marks_what_it_kept() {
    let intake = Intake::start();
    let mut c = config(&intake);
    c.sampling = Sampling {
        log: 0.0,
        event: 0.5,
        ..Sampling::default()
    };
    let t = Telemetry::start(c).unwrap();
    for _ in 0..200 {
        t.log(Log::new(Level::Info, "sync", "never"));
        t.event(Event::new("half"));
    }
    t.error(ErrorRecord::new("Error").message("always"));
    assert!(t.flush(SEC));
    let recs = intake.records();
    assert!(!recs.iter().any(|r| r["type"] == "log"));
    let half: Vec<_> = recs.iter().filter(|r| r["name"] == "half").collect();
    assert!(half.len() > 50 && half.len() < 150, "{}", half.len());
    assert!(half.iter().all(|r| r["sample_rate"] == 0.5));
    assert!(recs
        .iter()
        .find(|r| r["type"] == "error")
        .unwrap()
        .get("sample_rate")
        .is_none());
}

#[test]
fn the_install_id_is_random_kept_and_resettable() {
    let intake = Intake::start();
    let dir = scratch("id");
    let path = dir.join("install-id");
    let mut c = config(&intake);
    c.install_id_path = Some(path.clone());
    let t = Telemetry::start(c).unwrap();
    let first = t.install_id();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
    t.event(Event::new("one"));
    t.reset_install_id();
    t.event(Event::new("two"));
    assert!(t.flush(SEC));
    let ids: Vec<_> = intake
        .all()
        .iter()
        .map(|r| {
            r.body["resource"]["install_id"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(ids, [first, t.install_id()]);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), t.install_id());
}

#[test]
fn a_config_that_can_never_send_is_refused() {
    let base = || Config::new("ava-desktop", KEY, Kind::Service, "1.0.0");
    let mut c = base();
    c.client = "Not Registered".into();
    assert!(Telemetry::start(c).is_err());
    let mut c = base();
    c.key = "secret".into();
    assert!(Telemetry::start(c).is_err());
    let mut c = base();
    c.endpoint = "http://telemetry.addis.gg/v1/ingest".into();
    assert!(Telemetry::start(c).is_err());
}
