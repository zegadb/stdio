# stdio

Terminal output formatting for zega tools: one consistent look for errors,
warnings, status lines, next steps and an ASCII banner. Two implementations of
the same idea live in this repository:

| language   | package                                                | install                     |
| ---------- | ------------------------------------------------------ | --------------------------- |
| Rust       | [`stdio`](https://crates.io/crates/stdio) on crates.io | `cargo add stdio`           |
| TypeScript | [`@zegadb/stdio`](https://www.npmjs.com/package/@zegadb/stdio) on npm | `bun add @zegadb/stdio` (or `npm install @zegadb/stdio`) |

`ascii(text)` draws arbitrary text with the same Terrace font in both (`src/terrace-font.flf`). The block-letter zega logo (`banner()`) is TypeScript only.

**They only format text and write it to the terminal.** No files are read or
written, no network calls are made, and there is no configuration. Colour (TypeScript)
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
green. The crate has no dependencies. The `logf!` and `errorf!` macros take format strings:
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

## Development

```sh
cargo test                 # the crate
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
