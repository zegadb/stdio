# Telemetry wire format, version 1

How an app sends metrics, logs, errors and events to the Addis telemetry
intake, `telemetry.addis.gg`. A client in any language can be written from this
page alone. The TypeScript (`@addiscorp/stdio/telemetry`) and Rust (`stdio`,
feature `telemetry`) clients in this repository implement it, and the
collector's `POST /v1/ingest` implements the receiving side exactly as written
here.

The key words MUST, MUST NOT, SHOULD and MAY are used as in RFC 2119.
Sizes are in **bytes of UTF-8**, never characters. "Hex" means lowercase
`[0-9a-f]`.

## 1. Registration

A client can send only once it is registered with the collector. Registration
is an entry in the collector's `projects.json` (addiscorp/infra,
`services/telemetry`), made with `bun run project -- <project id> <client id>
<kind>`:

| field | meaning |
|---|---|
| project id | digits, the collector's own number for the project |
| **client id** | what is sending, e.g. `addis-website`, `ava-desktop`, `ava-iphone`. It is the entry's `service` field: `^[a-z0-9][a-z0-9._-]{0,63}$` |
| kind | `service` (our own Workers, hosts and tools) or `customer-app` (an app a customer runs). Decides what may be kept (§7) |
| key | 32 hex characters, printed once by `bun run project`; `projects.json` holds only its SHA-256 |

The app is configured with its client id, its key and its kind, explicitly in
code. No client reads them from the environment.

## 2. The request

```
POST https://telemetry.addis.gg/v1/ingest
Authorization: Bearer <key>
Content-Type: application/json
Content-Encoding: gzip            (optional)
```

- **Key.** `Authorization: Bearer <key>`. A browser's `navigator.sendBeacon`
  cannot set headers, so a request MAY instead carry the key as the query
  parameter `?key=<key>` and nothing else in the query. A request with both,
  or with neither, is refused (401).
- **Content-Type.** `application/json`. A beacon request MAY use
  `text/plain;charset=UTF-8` (the type `sendBeacon` can send without a CORS
  preflight); the body is the same JSON. Anything else: 415.
- **Content-Encoding.** `gzip` or absent (`identity`). A client SHOULD gzip a
  body over 1024 bytes when it can. Anything else: 415.
- **CORS.** The collector answers `OPTIONS /v1/ingest` with 204 and
  `Access-Control-Allow-Origin: *`, `Access-Control-Allow-Methods: POST`,
  `Access-Control-Allow-Headers: authorization, content-type, content-encoding`,
  `Access-Control-Max-Age: 86400`. Every `POST /v1/ingest` response carries
  `Access-Control-Allow-Origin: *` and
  `Access-Control-Expose-Headers: retry-after`.
- HTTPS only. A client MUST NOT send to an `http://` URL except a loopback
  address (`127.0.0.1`, `[::1]`, `localhost`), which exists for tests.

## 3. Size limits

| what | limit | past it |
|---|---|---|
| the body as sent (compressed or not) | 524,288 bytes (512 KiB) | 413, the whole batch |
| the body after gunzip | 524,288 bytes | 413, the whole batch |
| records in one batch | 1 to 500 | 0 records: 400; over 500: 413 |
| one record, as compact JSON | 32,768 bytes | that record rejected (`too_large`) |
| a beacon body (browser limit) | 61,440 bytes (60 KiB) | the client sends a smaller batch |

Field limits are in §5 and §6. A client truncates a string that is over its
limit to the longest prefix that fits **without splitting a UTF-8 sequence**,
with no marker added, before it sends. The collector does not truncate: a field
over its limit makes the record invalid.

## 4. The batch

The body is one JSON object (UTF-8, no BOM):

```json
{
  "v": 1,
  "client": "ava-desktop",
  "batch": "6f1c0d2a9b3e4f5061728394a5b6c7d8",
  "sent_at": "2026-11-27T18:04:05.123Z",
  "resource": {
    "release": "1.4.2",
    "platform": "native",
    "os": { "name": "macOS", "version": "26.1" },
    "device_class": "Mac",
    "install_id": "0a1b2c3d4e5f60718293a4b5c6d7e8f9",
    "sdk": { "name": "stdio-rs", "version": "0.6.0" },
    "environment": "production"
  },
  "records": [
    { "type": "metric", "ts": "2026-11-27T18:04:01.000Z", "kind": "counter", "name": "sync.runs", "value": 1, "unit": "{run}" }
  ]
}
```

| field | type | rule |
|---|---|---|
| `v` | integer | `1` |
| `client` | string | the client id (§1). MUST be the registered client id of the key's project, else 403 |
| `batch` | string | 32 hex, random per batch. **The same batch, retried, keeps its id** (§9) |
| `sent_at` | timestamp | when this attempt was sent |
| `resource` | object | who is sending; applies to every record (§4.1) |
| `identity` | object | **only** with the user's consent (§8). Absent otherwise |
| `records` | array | 1–500 records (§5) |

**Timestamps** are UTC in exactly this form: `YYYY-MM-DDTHH:MM:SS.sssZ`
(24 characters, milliseconds, `Z`). The collector rejects any other form.

**Strictness.** A top-level, `resource` or `identity` object with a field not
listed here, a missing required field, or a field of the wrong type is a 400
for the whole batch. A record with an unknown field is rejected on its own
(`unknown_field`). New fields come with a new `v`.

### 4.1 `resource`

Every record in a batch shares one resource. A client MUST NOT put records
captured under different resources (another release, another install id after a
reset) in one batch: it records the resource with each record when it is
captured and starts a new batch where it changes.

| field | required | rule |
|---|---|---|
| `release` | yes | the app's version, `^[A-Za-z0-9][A-Za-z0-9._+-]{0,63}$` |
| `platform` | yes | one of `browser`, `worker` (Cloudflare Workers and similar), `node`, `bun`, `deno`, `react-native`, `native` (a compiled app or binary: Rust, Swift, Kotlin) |
| `os` | no | `{ "name": string 1–32, "version": string 1–32 }`, both optional inside; e.g. `macOS` / `26.1`, `iOS` / `18.2`, `Android` / `15`, `Windows`, `Linux` |
| `device_class` | no | 1–32 bytes. The model **class**: `iPhone`, `iPad`, `Mac`, `Pixel`, `Samsung Galaxy`, never a model number, a serial or a name |
| `install_id` | yes | 32 hex (§4.2) |
| `sdk` | yes | `{ "name": ^[a-z0-9@/._-]{1,64}$, "version": ^[A-Za-z0-9][A-Za-z0-9._+-]{0,63}$ }` |
| `environment` | no | `^[a-z0-9_-]{1,32}$`, e.g. `production`, `staging`, `development` |

### 4.2 The install id

A per-install anonymous id, so counts can be told apart without knowing who
anyone is.

- 16 bytes from a cryptographically secure random source, as 32 hex.
- Made the first time the app runs and kept by the app (a file, the keychain,
  `localStorage`, the app's own storage). Without storage it lives for one
  process.
- **Never derived from hardware or from anything about the user**: not a MAC
  address, serial, IDFA/IDFV, Android ID, hostname, user name or account id.
- **Resettable**: the app offers "reset", which makes a new random id. Records
  captured after a reset carry the new id; records captured before keep the old
  one (§4.1), so a reset never links the two.

## 5. Records

Every record is an object with these common fields, then the fields of its
type.

| field | required | rule |
|---|---|---|
| `type` | yes | `metric`, `log`, `error` or `event` |
| `ts` | yes | timestamp (§4): when it happened, not when it was sent |
| `trace` | no | `{ "trace_id": 32 hex, "span_id": 16 hex }`, neither all zeros (W3C trace context ids) |
| `sample_rate` | no | number, `0 < r < 1`: the record was kept with this probability (§10). Absent means 1 |

**Names** (`metric.name`, `event.name`) match
`^[a-z][a-z0-9_]*(\.[a-z0-9_]+)*$` and are at most 128 bytes. Names starting
`telemetry.` are reserved for §10 and §8; a client never lets an app send one.

**Attributes** (`attrs`, optional on `metric` and `event`): an object of at
most 32 entries. Keys match `^[a-z][a-z0-9_]*(\.[a-z0-9_]+)*$`, at most 64
bytes. Values are a string of at most 256 bytes, a finite number, or a
boolean; never `null`, an array or an object. Scrubbing (§7) may remove
entries; an `attrs` left empty is omitted.

### `metric`

| field | required | rule |
|---|---|---|
| `kind` | yes | `counter`, `gauge` or `histogram` |
| `name` | yes | a name |
| `value` | yes | a finite JSON number. **counter**: the increase since this install last reported this counter with the same attrs, `≥ 0` (a delta, never a running total). **gauge**: the value now. **histogram**: one observation; the collector builds the distribution |
| `unit` | no | `^[A-Za-z0-9%/_.{}-]{1,32}$`, UCUM-style as in OpenTelemetry: `ms`, `s`, `By`, `1`, `{request}` |
| `attrs` | no | attributes |

### `log`

| field | required | rule |
|---|---|---|
| `level` | yes | `debug`, `info`, `warn` or `error` |
| `scope` | yes | what logged it, `^[A-Za-z0-9._:/-]{1,64}$` (the `[action]` of a stdio line) |
| `message` | yes | 1–2,000 bytes |

### `error`

| field | required | rule |
|---|---|---|
| `error_type` | yes | the error's class or kind, 1–128 bytes, e.g. `TypeError`, `io::ErrorKind::NotFound` |
| `message` | no | 1–2,000 bytes |
| `stack` | no | 1–16,384 bytes, the stack as the runtime prints it |
| `fingerprint` | no | 1–128 bytes, a grouping hint: records with the same hint are one error |
| `code` | no | `^[A-Za-z0-9_.:-]{1,64}$`, the app's own error code; a `customer-app`'s errors group by it |

### `event`

| field | required | rule |
|---|---|---|
| `name` | yes | a name |
| `attrs` | no | attributes |

## 6. `identity` and consent

Present **only** when the user has explicitly approved sending it (§8):

```json
"identity": {
  "sub": "acc_01J9Z5V7X2",
  "device_name": "Maya's MacBook",
  "email": "maya@example.com",
  "consent": { "version": "2026-11-27", "granted_at": "2026-11-27T17:59:00.000Z" }
}
```

| field | required | rule |
|---|---|---|
| `sub` | yes | the Addis (or Ava) account's subject, 1–255 bytes |
| `device_name` | no | 1–64 bytes, only if the app chose to include it |
| `email` | no | 3–254 bytes containing `@`, only if the app chose to include it |
| `consent.version` | yes | `^[A-Za-z0-9._-]{1,64}$`, the version of the consent text the user approved |
| `consent.granted_at` | yes | timestamp: when the user approved it |

`identity` applies to every record in the batch, and every record in the batch
was captured while that consent was in force. Records have no identity field
of their own, and attribute keys that name a person are removed (§7), so
identity cannot ride along anywhere else.

## 7. Scrubbing

A client applies these rules before a record is queued; the collector applies
them again with the kind it has registered for the key, never trusting the
client's. They are the collector's `src/scrub.ts` rules.

**Every kind.** An attribute is removed when its key matches, case-insensitively,

```
DROP_KEY = cookie|authorization|auth[_-]?token|password|passwd|secret|api[_-]?key|access[_-]?token|refresh[_-]?token|session|csrf|^user$|^user[._-]?(id|name|email|ip|segment)|^enduser|email|phone|ip_address|^ip$|^vars$|^extra$|^body$|^data$|^query_string$|^query$|query_params|^cookies$|^env$|^dsn$
DROP_ATTR = ^(http\.request\.header\.|http\.response\.header\.|http\.request\.body|http\.response\.body|url\.query|enduser\.|user\.|client\.address|net\.peer\.ip|net\.sock\.peer\.addr|server\.address)
```

(unanchored alternatives match anywhere in the key). A string attribute whose
key matches `^(url|uri|href|target|full_url|http\.url|url\.full|http\.target|url\.original|referer|referrer|location|redirect|redirect_url)$`
loses everything from its first `?` or `#`.

**`customer-app`** (APS 25 §7: app version, OS version, device model class,
error codes with counts), additionally:

- `log` records are not sent.
- `error` keeps only `error_type`, `code` and `fingerprint` (and the common
  fields); `message` and `stack` are removed.
- `metric` and `event` attributes keep only the keys `http.request.method`,
  `http.response.status_code`, `http.status_code`, `rpc.method`,
  `rpc.service`, `error.type`, `network.protocol.name`; on a `telemetry.*`
  record, also `reason` and `consent.version`.
- `resource` is kept as it is: every field in §4.1 is on the allowlist or is
  the anonymous install id.

## 8. Consent

- **By default nothing identifying is sent**: no `identity`, and nothing in a
  record names a person (§7).
- An app attaches `identity` only after the user approves, and only through the
  client's consent call (`consent.grant(...)`), which takes the consent text's
  version and the time it was approved and returns a handle. There is no other
  way to set it.
- Records captured while consent is in force are sent with `identity`; records
  captured before it are sent without, in batches of their own.
- **Revoking** (the handle's `revoke()`) takes effect at once: no batch sent
  after it carries `identity`, including records captured while consent was in
  force and retries of a batch first sent with it (a retry is rebuilt, §9).
  The client then sends the event `telemetry.consent.revoked` with
  `attrs: { "consent.version": <version> }`, without `identity`.
- Granting sends `telemetry.consent.granted` with the same attribute, with
  `identity`.
- A client MUST NOT write `identity` to disk. After a restart, `identity` is
  sent again only once the app grants again (re-presenting the user's stored
  approval, with its original `granted_at`).

## 9. Responses, retries and order

| status | meaning | the client |
|---|---|---|
| **202** | accepted. Body: `{"accepted": <n>, "rejected": [{"index": <i>, "reason": <r>}], "duplicate": <bool>}` | done; the batch is removed. Rejected records are not retried |
| 400 | the batch is not valid (§4) | discards the batch (`invalid`) |
| 401 | missing, malformed or unknown key | discards the batch (`refused`) |
| 403 | the key is valid but `client` is not its project's client id | discards the batch (`refused`) |
| 404, 405 | wrong path or method | discards the batch (`refused`) |
| 413 | too large (§3) | more than one record: splits it into two halves, each a new batch with a new id, the older half first. One record: discards it (`too_large`) |
| 415 | content type or encoding | discards the batch (`refused`) |
| **429** | rate limited | retries the same batch after `Retry-After` |
| **503** | busy | retries the same batch after `Retry-After` |
| other 5xx, a network error, or no response within 10 s | | retries the same batch with backoff |
| any other status | | discards the batch (`refused`) |

`rejected[].reason` is one of `invalid`, `unknown_field`, `too_large`. Every
`202` carries all three keys; `rejected` is `[]` when none were.

- **Retry-After** is delta-seconds (`60`) or an HTTP-date. Clamp it to
  1–3,600 s. Without one, back off.
- **Backoff**: after the n-th consecutive failure, wait `d/2 + random(0, d/2)`
  with `d = min(300 s, 2^(n−1) s)`. A success resets n.
- Retries never end on their own; the queue's bound (§10) is what limits them.
- **A retry keeps the batch's id and its records**, and is re-serialized with
  a new `sent_at`, and with `identity` removed if consent was revoked since.
- **Idempotency**: the collector remembers the batch ids it accepted for 24
  hours. A batch id seen again gets `202` with `accepted: 0`, `rejected: []`
  and `duplicate: true`, and nothing is stored twice.
- **One batch in flight** per client: the next is sent only once the previous
  has an outcome. Batches go oldest first.

## 10. Client behaviour

Not visible on the wire except where noted, but every client works this way.

- **Batching**: a batch is sent when the queue holds `max_batch` records
  (default 100) or every `interval` (default 10 s), and on flush/shutdown. A
  batch never crosses a change of resource or of consent (§4.1, §8).
- **A bounded queue**: at most `max_queue` records (default 1,000), and at
  most the spool's byte cap when there is a spool, counting records in a batch
  being sent. When full, **the oldest record not already in a batch is
  dropped** and counted.
- **Drops are sent** as the counter `telemetry.dropped`, unit `{record}`, one
  record per reason with `attrs: { "reason": <r> }`, `r` one of `queue_full`,
  `spool_full`, `too_large`, `invalid`, `refused`; its value is the number
  dropped since the last report.
- **Sampling** is set per record type (`metric`, `log`, `error`, `event`),
  default 1. It is decided when a record is captured; a kept record with a
  rate below 1 carries `sample_rate`. `telemetry.*` records are never sampled.
- **Never in the host's way**: capturing a record does not wait for the
  network or the disk beyond one append, never throws, and the client never
  crashes the app; a failed or slow intake only makes the queue wait.
- **Spool** (optional, where there is a disk): queued records are also written
  to a capped local file, so a crash or an offline period loses nothing; on
  start, the client sends what the spool holds. The spool holds no `identity`.

## 11. A complete example

`test/fixtures/telemetry-golden.json` in this repository is a batch both
clients must produce, byte for byte after parsing, from the same calls with a
fixed clock and fixed ids; the test suites of both check it.
