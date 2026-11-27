# stdio

Terminal output formatting for zega tools: one consistent look for errors,
warnings, status lines, next steps and an ASCII banner. Two implementations of
the same idea live in this repository:

| language   | package                                                | install                     |
| ---------- | ------------------------------------------------------ | --------------------------- |
| Rust       | [`stdio`](https://crates.io/crates/stdio) on crates.io | `cargo add stdio`           |
| TypeScript | [`@zegadb/stdio`](https://www.npmjs.com/package/@zegadb/stdio) on npm | `bun add @zegadb/stdio` (or `npm install @zegadb/stdio`) |

`ascii(text)` draws arbitrary text with the same Terrace font in both (`src/terrace-font.flf`). The block-letter zega logo (`banner()`) is TypeScript only.

**By default they only format text and write it to the terminal.** No files are read or
written, no network calls are made, and there is no configuration. That stays
true unless you opt in to the separate telemetry client (below): it is a
separate entry point in TypeScript and a cargo feature, off by default, in Rust. Colour (TypeScript)
is on only when the terminal supports it; it is off for output that is not a
terminal and when [`NO_COLOR`](https://no-color.org) is set, which is the one
environment signal the TypeScript package looks at.

## Rust

```rust
use stdio::{ascii, error, log, next_step, raw, success, warn};

fn main() {
    raw(&ascii("zega"));
    log("build", "compiling...");
    warn("cache", "stale entries detected");
    error("build", "compilation failed");
    success("build complete");
    next_step("start the server", "npm run dev");
}
```

```text
[build] compiling...
● [cache] stale entries detected
[build] compilation failed
✓ build complete
  → start the server: npm run dev
```

Since 0.5 the crate prints exactly what the TypeScript package prints: the same
glyphs and colours (cyan brackets, red errors, yellow warnings, a green tick,
grey hints), colour only when the stream is a terminal and never with
`NO_COLOR`, and the same streams: problems (`error`, `warn`, `warn_simple`,
`fail`, `diagnostic`) on stderr, everything else on stdout. `ascii` is zega
green, and `logo()`/`banner()` draw the zega logo as the TypeScript package
does. `start_banner(&StartBanner { .. })` prints the same Vite-style start-up
banner as `startBanner`; its `mark` is the zega logo, an `ascii` title, or a
tool's own art (`Mark::Art`). The crate has no dependencies. The `logf!` and `errorf!` macros take format strings:
`stdio::errorf!("build", "{} errors", 3)`.

## TypeScript

```ts
import { ascii, banner, error, log, nextStep, success, warn } from '@zegadb/stdio'

banner() // the zega logo
log('build', 'compiling...')
warn('cache', 'stale entries detected')
error('build', 'compilation failed')
success('build complete')
nextStep('start the server', 'npx zega dev')

console.log(ascii('my app')) // any text, in the Terrace font (zega green)
```

```text
[build] compiling...
● [cache] stale entries detected
[build] compilation failed
✓ build complete
  → start the server: npx zega dev
```

As in the Rust crate, anything a script would treat as a problem goes to
**stderr**: `error`, `warn`, `fail`, `diagnostic` and `fatal` (which prints the
error and exits with status 1). Everything else (`log`, `info`, `success`,
`status`, `hint`, `detail`, `nextStep`, `nextSteps`, `header`, `banner`,
`startBanner`) goes to **stdout**, so a command that prints JSON on stdout can
still report errors without corrupting it. Colour is decided per stream. `blank`
prints an empty line to stdout. Everything is also available as `out.*` and as
the default export.

### `banner()` and `logo()`

`banner()` prints the zega logo; `logo()` returns it as a string. It is five
rows of block characters in two tones (a lighter body and a darker edge on the
left of each stroke), fixed art for the four letters z, e, g, a, so it cannot
draw other text (use `ascii(text)` for that). Colours: body `#55FF55`, edge
`#00AA00` (the 16-colour VGA bright green and green). True colour where the
terminal supports it, otherwise ANSI bright green (92) and green (32), and
plain blocks for `NO_COLOR` or output that is not a terminal:

```text
▐██████▐██████▐██████▐██████
   ▐██ ▐██    ▐██    ▐██ ▐██
  ▐██  ▐████  ▐██ ▐██▐██████
 ▐██   ▐██    ▐██  ▐█▐██ ▐██
▐██████▐██████▐██████▐██ ▐██
```

### `startBanner()`

The start-up screen for a dev server, in the style of Vite's: logo, tagline,
what is running where, and a hint.

```ts
import { startBanner } from '@zegadb/stdio'

startBanner({
  tagline: 'The graph database',
  urls: [
    { label: 'Local', url: 'http://localhost:8506/' },
    { label: 'Network', url: 'http://192.168.1.4:8506/', dim: true },
    { label: 'Explorer', status: 'stopped', hint: 'zega start --explorer' },
  ],
  hint: { key: 'h', action: 'show help' },
})
```

```text

▐██████▐██████▐██████▐██████
   ▐██ ▐██    ▐██    ▐██ ▐██
  ▐██  ▐████  ▐██ ▐██▐██████
 ▐██   ▐██    ▐██  ▐█▐██ ▐██
▐██████▐██████▐██████▐██ ▐██

  The graph database

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  ➜  Local:    http://localhost:8506/
  ➜  Network:  http://192.168.1.4:8506/
  ➜  Explorer: (stopped)
     zega start --explorer

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

  press h to show help

```

Leave out `title` for the zega logo, or pass `title: 'my app'` to draw that text
in the Terrace font instead.

Each entry in `urls` is `{ label, url?, status?, hint?, dim? }`: the label is
bold and aligned with the others, the address is cyan, `status` appears in
parentheses (alone when there is no address), `hint` is a dimmed line under the
entry, and `dim` (or having no address) shows the entry dimmed. `tagline` and
`hint` are optional. The screen is not cleared unless you pass `clear: true`,
and then only when stdout is a terminal.

## Telemetry (optional)

A client for sending metrics, logs, errors and events from an Addis app or
service to the Addis telemetry intake, `telemetry.addis.gg`. **It is never on
by accident**:

- TypeScript: only through its own entry point, `@addiscorp/stdio/telemetry`
  (and `@addiscorp/stdio/telemetry/node` for the disk spool). The main entry
  imports none of it; a test bundles the main entry and checks that.
- Rust: only with the cargo feature `telemetry` (`stdio = { version = "…",
  features = ["telemetry"] }`). Without it the crate has no dependencies (CI
  checks `cargo tree`); with it, `ureq` (rustls), `flate2`, `serde_json` and
  `getrandom`.
- Nothing is sent until the app creates a client with its **registered**
  client id and key, in code. Neither is read from the environment.

The wire format is [`docs/telemetry-wire.md`](docs/telemetry-wire.md): JSON
over HTTPS to `POST /v1/ingest`, short enough to write a Swift or Kotlin
client from.

### What is sent, and when

Every batch carries the client id and key, and a `resource`: the app's
release, its platform and OS name and version, a device **class** (`Mac`,
`iPhone`, never a model number or name), the SDK's name and version, and an
**install id**: 16 random bytes made on first run, kept by the app, resettable,
never derived from the hardware or the user. Then the records the app chose
to capture:

| record | fields |
|---|---|
| metric | counter, gauge or histogram; name, value, unit, attributes |
| log | level, scope, message |
| error | type, message, stack, fingerprint hint, error code |
| event | name, attributes |

Each has a timestamp and may carry a trace/span id. Records are batched and
sent every 10 s, at 100 records, and on `flush()`/shutdown. Attributes whose
key names a credential, a session or a person (`user.id`, `email`,
`session_id`, `authorization`, …) are removed before a record is queued, and
URLs lose their query strings: the same rules the collector applies. A
`customer-app` client (APS 25 §7) sends less: no logs, errors as type, code
and fingerprint only, and attributes from a short allowlist.

**Nothing identifying is sent by default.** Identity (the account's `sub`,
and a device name or email only if the app adds them) is attached only after
the user approves, and only through `consent.grant(...)`, which returns the
handle that revokes it. It goes only on records captured while consent is in
force, with the consent's version and time. `revoke()` takes effect at once,
even for records already queued, and sends `telemetry.consent.revoked`.
Identity is never written to disk. Records have no user field to put it in.

**It never gets in the app's way.** Capturing a record never waits for the
network; a down or slow intake only makes the queue wait. The queue is
bounded (1,000 records): the oldest are dropped and the count is sent as
`telemetry.dropped`. Sampling can be set per record type. Retries follow
`Retry-After` and back off.

### TypeScript

```ts
import { createTelemetry, installPagehide, localStorageStore } from '@addiscorp/stdio/telemetry'

const telemetry = createTelemetry({
  client: 'addis-website',          // registered in the collector's projects.json
  key: TELEMETRY_KEY,               // from `bun run project`, passed in explicitly
  kind: 'service',                  // or 'customer-app'
  release: '1.4.2',
  store: localStorageStore(),       // keeps the install id
})
installPagehide(telemetry)          // browsers: sendBeacon when the page is hidden

telemetry.counter('signup.started', 1, { attrs: { 'http.request.method': 'POST' } })
telemetry.log('info', 'billing', 'invoice sent')
telemetry.error(err, { code: 'E_SYNC' })
telemetry.event('screen.view', { screen: 'settings' })

// Only after the user says yes, with the version of the text they approved:
const consent = telemetry.consent.grant({ version: '2026-11-27', identity: { sub: account.sub } })
consent.revoke()                    // when they say no again
```

| runtime | how |
|---|---|
| browser | `fetch` with `keepalive`, gzip via `CompressionStream`, `installPagehide` for `sendBeacon`; in memory, `localStorageStore()` for the install id |
| Cloudflare Workers | `interval: 0`, then `ctx.waitUntil(telemetry.flush())` at the end of a request; no filesystem |
| Node, Bun | `spool: fileSpool(path)` and `store: fileStore(path)` from `@addiscorp/stdio/telemetry/node`: a capped NDJSON file (4 MiB by default) appended before each capture returns, so a crash or an offline period loses nothing; `flushOnExit(telemetry)` |
| React Native | in memory, or `spool: kvSpool(AsyncStorage)` and a store of your own; needs `crypto.getRandomValues` (a polyfill such as `react-native-get-random-values`) |

### Rust

```rust
use std::time::Duration;
use stdio::telemetry::{Config, Consent, Event, Identity, Kind, Metric, SpoolConfig, Telemetry};

let mut config = Config::new("zega-cli", key, Kind::Service, env!("CARGO_PKG_VERSION"));
config.spool = Some(SpoolConfig::new(data_dir.join("telemetry.ndjson")));
config.install_id_path = Some(data_dir.join("install-id"));
let telemetry = Telemetry::start(config)?;

telemetry.metric(Metric::counter("sync.runs", 1.0).unit("{run}"));
telemetry.event(Event::new("sync.done").attr("files", 12));
let consent = telemetry.consent().grant(Consent { version: "2026-11-27".into(), granted_at: None, identity: Identity { sub, device_name: None, email: None } })?;
consent.revoke();
telemetry.shutdown(Duration::from_secs(2));
```

A background thread sends; capturing takes a lock long enough to queue the
record and append one spool line. Dropping the client flushes for at most
`shutdown_timeout` (2 s) and never waits out a hung request.

## Development

```sh
cargo test                 # the crate
cargo test --features telemetry   # and the telemetry client
bun install
bun test                   # the TypeScript package
bun run build              # tsc -> dist/
```

`src/font.ts` is generated from `src/terrace-font.flf` (`bun run embed-font`),
so the TypeScript package carries the font inside its code and also works in a
bundle or a compiled executable. `bun run build` and `bun test` regenerate it,
and CI fails if the committed copy is out of date.

## License

MIT
