//! # stdio
//!
//! Terminal output formatting for zega tools, line for line the same as the
//! TypeScript package (@zegadb/stdio) in this repository: the same glyphs and
//! colours, colour only on a terminal and never with NO_COLOR, problems
//! (error, warn, fail, diagnostic) on stderr and everything else on stdout.
//!
//! ## Format
//!
//! ```text
//! [action] message
//! ```
//!
//! ## Usage
//!
//! ```rust
//! use stdio::{log, error, warn, success, fail};
//!
//! log("build", "compiling contract...");
//! success("build complete");
//! error("build", "compilation failed");
//! ```
//!

use std::collections::HashMap;
use std::io::IsTerminal;
#[cfg(target_arch = "wasm32")]
use std::sync::{Mutex, OnceLock};

mod terrace_font;

#[cfg(target_arch = "wasm32")]
static CAPTURED_OUTPUT: OnceLock<Mutex<Option<String>>> = OnceLock::new();

/// Zega green, the accent of zega.dev in dark mode (#3CCB77).
const ZEGA_GREEN: (u8, u8, u8) = (0x3c, 0xcb, 0x77);

// ============================================================
// Colour: the same rules as @zegadb/stdio. Colour is on only when the
// stream is a terminal, and off when NO_COLOR is set and not empty
// (https://no-color.org).
// ============================================================

/// Where a line goes. Anything a script would treat as a problem (error,
/// warn, fail, diagnostic) goes to stderr; everything else to stdout.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stream {
    Out,
    Err,
}

/// Paints text when colour is on, and passes it through when it is off.
#[derive(Clone, Copy, Debug)]
struct Ink(bool);

impl Ink {
    fn for_stream(stream: Stream) -> Self {
        if std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()) {
            return Ink(false);
        }
        Ink(match stream {
            Stream::Out => std::io::stdout().is_terminal(),
            Stream::Err => std::io::stderr().is_terminal(),
        })
    }

    fn paint(self, open: &str, close: &str, text: &str) -> String {
        if self.0 {
            format!("\x1b[{open}m{text}\x1b[{close}m")
        } else {
            text.to_string()
        }
    }

    fn cyan(self, text: &str) -> String {
        self.paint("36", "39", text)
    }
    fn red(self, text: &str) -> String {
        self.paint("31", "39", text)
    }
    fn green(self, text: &str) -> String {
        self.paint("32", "39", text)
    }
    fn yellow(self, text: &str) -> String {
        self.paint("33", "39", text)
    }
    fn gray(self, text: &str) -> String {
        self.paint("90", "39", text)
    }
    fn bold(self, text: &str) -> String {
        self.paint("1", "22", text)
    }
    fn rgb(self, (r, g, b): (u8, u8, u8), text: &str) -> String {
        self.paint(&format!("38;2;{r};{g};{b}"), "39", text)
    }
}

// ============================================================
// Line builders: pure formatting, unit-testable with a plain Ink.
// ============================================================

fn log_line(c: Ink, action: &str, value: &str) -> String {
    format!("{} {}", c.cyan(&format!("[{action}]")), value)
}

fn error_line(c: Ink, action: &str, message: &str) -> String {
    format!("{} {}", c.cyan(&format!("[{action}]")), c.red(message))
}

fn warn_line(c: Ink, name: Option<&str>, message: &str) -> String {
    match name {
        Some(name) => format!(
            "{} {} {}",
            c.yellow("●"),
            c.cyan(&format!("[{name}]")),
            message
        ),
        None => format!("{} {}", c.yellow("●"), message),
    }
}

fn status_line(c: Ink, name: &str, message: &str, ok: bool) -> String {
    let dot = if ok { c.green("●") } else { c.red("○") };
    let message = if ok { c.gray(message) } else { c.red(message) };
    format!("{} {} {}", dot, c.cyan(&format!("[{name}]")), message)
}

fn info_line(c: Ink, label: &str, value: &str) -> String {
    format!("  {:<10} {}", label, c.cyan(value))
}

fn next_step_line(c: Ink, description: &str, command: &str) -> String {
    format!("  {} {}: {}", c.gray("→"), description, c.cyan(command))
}

fn diagnostic_line(c: Ink, component: &str, message: &str) -> String {
    format!(
        "{} {} {}",
        c.yellow("⚠"),
        c.cyan(&format!("[{component}]")),
        c.yellow(message)
    )
}

// ============================================================
// Emit core: every call funnels through here.
// ============================================================

fn emit(stream: Stream, line: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = stream;
        if let Some(lock) = CAPTURED_OUTPUT.get() {
            if let Ok(mut guard) = lock.lock() {
                if let Some(buf) = guard.as_mut() {
                    buf.push_str(line);
                    buf.push('\n');
                    return;
                }
            }
        }
        eprintln!("{line}");
    }
    #[cfg(not(target_arch = "wasm32"))]
    match stream {
        Stream::Out => println!("{line}"),
        Stream::Err => eprintln!("{line}"),
    }
}

/// Start capturing output into an in-memory buffer.
///
/// Only available on wasm32, where there is no terminal to write to;
/// embedders use it to collect output in memory until [`end_capture`].
#[cfg(target_arch = "wasm32")]
pub fn begin_capture() {
    let lock = CAPTURED_OUTPUT.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = lock.lock() {
        *guard = Some(String::new());
    }
}

/// Stop capturing and return everything emitted since [`begin_capture`].
#[cfg(target_arch = "wasm32")]
pub fn end_capture() -> String {
    let lock = CAPTURED_OUTPUT.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = lock.lock() {
        return guard.take().unwrap_or_default();
    }
    String::new()
}

// ============================================================
// The functions, matching @zegadb/stdio line for line.
// ============================================================

/// `[action] value`, cyan brackets. stdout.
pub fn log(action: &str, value: &str) {
    emit(
        Stream::Out,
        &log_line(Ink::for_stream(Stream::Out), action, value),
    );
}

/// `[action] message`, cyan brackets, red message. stderr.
pub fn error(action: &str, message: &str) {
    emit(
        Stream::Err,
        &error_line(Ink::for_stream(Stream::Err), action, message),
    );
}

/// `● [name] message`, yellow dot. stderr.
pub fn warn(name: &str, message: &str) {
    emit(
        Stream::Err,
        &warn_line(Ink::for_stream(Stream::Err), Some(name), message),
    );
}

/// `● message`, yellow dot. stderr.
pub fn warn_simple(message: &str) {
    emit(
        Stream::Err,
        &warn_line(Ink::for_stream(Stream::Err), None, message),
    );
}

/// `● [name] message`: a green dot and grey message when ok, a red ring and
/// red message when not. stdout.
pub fn status(name: &str, message: &str, ok: bool) {
    emit(
        Stream::Out,
        &status_line(Ink::for_stream(Stream::Out), name, message, ok),
    );
}

/// A blank line, the title in bold, then a grey rule. stdout.
pub fn header(title: &str) {
    let c = Ink::for_stream(Stream::Out);
    emit(Stream::Out, "");
    emit(Stream::Out, &c.bold(title));
    emit(Stream::Out, &c.gray(&"─".repeat(40)));
}

/// A blank line. stdout.
pub fn blank() {
    emit(Stream::Out, "");
}

/// `✓ message`, green tick. stdout.
pub fn success(message: &str) {
    let c = Ink::for_stream(Stream::Out);
    emit(Stream::Out, &format!("{} {}", c.green("✓"), message));
}

/// `✗ message`, red cross. stderr.
pub fn fail(message: &str) {
    let c = Ink::for_stream(Stream::Err);
    emit(Stream::Err, &format!("{} {}", c.red("✗"), message));
}

/// `  label      value`, cyan value. stdout.
pub fn info(label: &str, value: &str) {
    emit(
        Stream::Out,
        &info_line(Ink::for_stream(Stream::Out), label, value),
    );
}

/// `  message`, grey. stdout.
pub fn hint(message: &str) {
    emit(
        Stream::Out,
        &Ink::for_stream(Stream::Out).gray(&format!("  {message}")),
    );
}

/// `    → message`, grey arrow. stdout.
pub fn detail(message: &str) {
    let c = Ink::for_stream(Stream::Out);
    emit(Stream::Out, &format!("    {} {}", c.gray("→"), message));
}

/// `  → description: command`, grey arrow, cyan command. stdout.
pub fn next_step(description: &str, command: &str) {
    emit(
        Stream::Out,
        &next_step_line(Ink::for_stream(Stream::Out), description, command),
    );
}

/// `⚠ [component] message`, yellow. stderr.
pub fn diagnostic(component: &str, message: &str) {
    emit(
        Stream::Err,
        &diagnostic_line(Ink::for_stream(Stream::Err), component, message),
    );
}

/// A line as given. stdout.
pub fn raw(message: &str) {
    emit(Stream::Out, message);
}

/// `[note] message`, cyan brackets: a nudge about a situation, not a
/// failure. stdout.
pub fn note(message: &str) {
    emit(
        Stream::Out,
        &log_line(Ink::for_stream(Stream::Out), "note", message),
    );
}

// ============================================================
// ASCII art (Terrace figlet font)
// ============================================================

/// ASCII art in the Terrace font, in zega green and bold when colour is on
/// for stdout; pass it to [`raw`] or print it yourself.
pub fn ascii(text: &str) -> String {
    ascii_with(Ink::for_stream(Stream::Out), text)
}

fn ascii_with(c: Ink, text: &str) -> String {
    let font = FigFont::parse(terrace_font::TERRACE_FONT);
    let art = font
        .render(text)
        .unwrap_or_else(|| text.to_string())
        .trim_end()
        .to_string();
    c.bold(&c.rgb(ZEGA_GREEN, &art))
}

struct FigFont {
    height: usize,
    glyphs: HashMap<char, Vec<String>>,
}

impl FigFont {
    fn parse(source: &str) -> Self {
        let mut lines = source.lines();
        let header = lines.next().unwrap_or_default();
        let mut header_parts = header.split_whitespace();
        let signature = header_parts.next().unwrap_or_default();
        let hardblank = signature.chars().last().unwrap_or('$');
        let height = header_parts
            .next()
            .and_then(|part| part.parse::<usize>().ok())
            .unwrap_or(1);
        let comment_lines = header_parts
            .nth(3)
            .and_then(|part| part.parse::<usize>().ok())
            .unwrap_or(0);

        for _ in 0..comment_lines {
            lines.next();
        }

        let mut glyphs = HashMap::new();
        let mut endmark = '@';
        for codepoint in 32u8..=126u8 {
            let mut glyph = Vec::with_capacity(height);
            for i in 0..height {
                if let Some(line) = lines.next() {
                    let mut line = line.trim_end_matches('\r').to_string();
                    if i == 0 && !line.is_empty() {
                        if let Some(last) = line.chars().last() {
                            endmark = last;
                        }
                    }
                    while line.ends_with(endmark) {
                        line.pop();
                    }
                    if hardblank != ' ' {
                        line = line.replace(hardblank, " ");
                    }
                    glyph.push(line);
                } else {
                    glyph.push(String::new());
                }
            }
            glyphs.insert(codepoint as char, glyph);
        }

        Self { height, glyphs }
    }

    fn render(&self, text: &str) -> Option<String> {
        let mut lines = vec![String::new(); self.height];
        for ch in text.chars() {
            let glyph = self.glyphs.get(&ch).or_else(|| self.glyphs.get(&'?'));
            let glyph = glyph?;
            for (idx, line) in lines.iter_mut().enumerate() {
                if let Some(part) = glyph.get(idx) {
                    line.push_str(part);
                }
            }
        }
        Some(lines.join("\n"))
    }
}

// ============================================================
// Macros for convenient formatting
// ============================================================

/// Log with format string support
///
/// # Example
/// ```
/// stdio::logf!("build", "compiled {} files in {}ms", 42, 150);
/// ```
#[macro_export]
macro_rules! logf {
    ($action:expr, $($arg:tt)*) => {
        $crate::log($action, &format!($($arg)*));
    };
}

/// Error with format string support
#[macro_export]
macro_rules! errorf {
    ($action:expr, $($arg:tt)*) => {
        $crate::error($action, &format!($($arg)*));
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: Ink = Ink(false);
    const COLOR: Ink = Ink(true);

    #[test]
    fn lines_match_the_typescript_package_without_colour() {
        assert_eq!(log_line(PLAIN, "build", "compiling"), "[build] compiling");
        assert_eq!(error_line(PLAIN, "build", "failed"), "[build] failed");
        assert_eq!(warn_line(PLAIN, Some("cache"), "stale"), "● [cache] stale");
        assert_eq!(warn_line(PLAIN, None, "stale"), "● stale");
        assert_eq!(
            status_line(PLAIN, "db", "connected", true),
            "● [db] connected"
        );
        assert_eq!(status_line(PLAIN, "db", "refused", false), "○ [db] refused");
        assert_eq!(info_line(PLAIN, "port", "8506"), "  port       8506");
        assert_eq!(
            next_step_line(PLAIN, "deploy it", "s build"),
            "  → deploy it: s build"
        );
        assert_eq!(diagnostic_line(PLAIN, "cfg", "odd"), "⚠ [cfg] odd");
    }

    #[test]
    fn colour_paints_the_same_parts_as_the_typescript_package() {
        assert_eq!(
            error_line(COLOR, "build", "failed"),
            "\x1b[36m[build]\x1b[39m \x1b[31mfailed\x1b[39m"
        );
        assert_eq!(
            next_step_line(COLOR, "see", "s help"),
            "  \x1b[90m→\x1b[39m see: \x1b[36ms help\x1b[39m"
        );
        assert_eq!(
            status_line(COLOR, "db", "up", true),
            "\x1b[32m●\x1b[39m \x1b[36m[db]\x1b[39m \x1b[90mup\x1b[39m"
        );
    }

    #[test]
    fn info_pads_the_label_to_a_fixed_column() {
        assert_eq!(
            info_line(PLAIN, "a-very-long-label", "x"),
            "  a-very-long-label x"
        );
    }

    #[test]
    fn ascii_is_plain_without_colour_and_green_with_it() {
        let plain = ascii_with(PLAIN, "zega");
        assert!(!plain.contains('\x1b'));
        assert!(plain.contains('\n'));
        let painted = ascii_with(COLOR, "zega");
        assert!(painted.starts_with("\x1b[1m\x1b[38;2;60;203;119m"));
    }

    #[test]
    fn figfont_replaces_unknown_chars_with_fallback() {
        let font = FigFont::parse(terrace_font::TERRACE_FONT);
        let rendered = font.render("a~b").expect("render");
        assert_eq!(rendered.lines().count(), font.height);
    }
}
