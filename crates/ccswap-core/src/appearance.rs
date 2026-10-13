//! Terminal background detection (upstream `appearance.py`): OSC 11 asks
//! the terminal for its background colour, DA1 marks the end of the reply,
//! and the colour's luminance picks the light or dark palette. Every
//! failure resolves to `None` (dark) without blocking for more than the
//! timeout.

use std::io;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::env::Environment;

pub const QUERY: &[u8] = b"\x1b]11;?\x07";
pub const DA1_QUERY: &[u8] = b"\x1b[c";
pub const TIMEOUT: Duration = Duration::from_secs(1);
const MAX_REPLY: usize = 256;
const READ_CHUNK: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Background {
    Light,
    Dark,
}

impl Background {
    pub fn as_str(self) -> &'static str {
        match self {
            Background::Light => "light",
            Background::Dark => "dark",
        }
    }
}

fn is_hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

fn hex_value(digits: &[u8]) -> Option<u64> {
    u64::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()
}

/// The length of a string terminator (BEL or ST) at the start of `rest`.
fn terminator(rest: &[u8]) -> Option<usize> {
    if rest.starts_with(b"\x07") {
        Some(1)
    } else if rest.starts_with(b"\x1b\\") {
        Some(2)
    } else {
        None
    }
}

fn hex_run(s: &[u8]) -> usize {
    s.iter().take_while(|b| is_hex(**b)).count()
}

/// `ESC]11;rgb:R/G/B` + terminator, any number of hex digits per channel.
fn parse_rgb(body: &[u8]) -> Option<(f64, f64, f64)> {
    let body = body.strip_prefix(b"rgb:")?;
    let mut channels = [0.0; 3];
    let mut pos = 0;
    for (i, channel) in channels.iter_mut().enumerate() {
        let n = hex_run(&body[pos..]);
        if n == 0 || n > 15 {
            return None;
        }
        let digits = &body[pos..pos + n];
        *channel = hex_value(digits)? as f64 / ((16u64.pow(n as u32)) - 1) as f64;
        pos += n;
        if i < 2 {
            if body.get(pos) != Some(&b'/') {
                return None;
            }
            pos += 1;
        }
    }
    terminator(&body[pos..])?;
    Some((channels[0], channels[1], channels[2]))
}

/// `ESC]11;#RRGGBB` + terminator.
fn parse_hash(body: &[u8]) -> Option<(f64, f64, f64)> {
    let body = body.strip_prefix(b"#")?;
    if body.len() < 6 || hex_run(&body[..6]) != 6 {
        return None;
    }
    terminator(&body[6..])?;
    let c = |i: usize| hex_value(&body[i..i + 2]).map(|v| v as f64 / 255.0);
    Some((c(0)?, c(2)?, c(4)?))
}

fn openers(reply: &[u8]) -> impl Iterator<Item = &[u8]> {
    const OPENER: &[u8] = b"\x1b]11;";
    (0..reply.len())
        .filter(move |&i| reply[i..].starts_with(OPENER))
        .map(move |i| &reply[i + OPENER.len()..])
}

/// The background as `(r, g, b)` in 0..1; the full `ESC]11;` opener and a
/// terminator are required so echoed input cannot pass for a reply.
pub fn parse_osc11(reply: &[u8]) -> Option<(f64, f64, f64)> {
    openers(reply)
        .find_map(parse_rgb)
        .or_else(|| openers(reply).find_map(parse_hash))
}

pub fn classify(reply: &[u8]) -> Option<Background> {
    let (r, g, b) = parse_osc11(reply)?;
    let luminance = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    Some(if luminance > 0.5 {
        Background::Light
    } else {
        Background::Dark
    })
}

/// `CSI ? Ps c`; the `?` keeps an echoed `CSI c` query from counting.
pub fn has_da1_reply(buf: &[u8]) -> bool {
    let starts = (0..buf.len()).filter_map(|i| {
        if buf[i..].starts_with(b"\x1b[?") {
            Some(i + 3)
        } else if buf[i..].starts_with(b"\x9b?") {
            Some(i + 2)
        } else {
            None
        }
    });
    for start in starts {
        let params = buf[start..]
            .iter()
            .take_while(|b| b.is_ascii_digit() || **b == b';' || **b == b':')
            .count();
        if buf.get(start + params) == Some(&b'c') {
            return true;
        }
    }
    false
}

/// The terminal side of the probe, so the read loop runs against a fake.
pub trait Tty {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// Up to `max` bytes once readable within `timeout`; `None` on timeout,
    /// an empty vec at end of input.
    fn read_within(&mut self, timeout: Duration, max: usize) -> io::Result<Option<Vec<u8>>>;
}

/// Sends OSC 11 + DA1 and reads until the DA1 reply, the deadline or the
/// size cap. `None` when nothing came back or the terminal failed.
pub fn query(tty: &mut dyn Tty, now: &mut dyn FnMut() -> Instant) -> Option<Vec<u8>> {
    tty.write_all(&[QUERY, DA1_QUERY].concat()).ok()?;
    let deadline = now() + TIMEOUT;
    let mut buf = Vec::new();
    while buf.len() < MAX_REPLY {
        let current = now();
        if current >= deadline {
            break;
        }
        match tty.read_within(deadline - current, READ_CHUNK) {
            Ok(Some(chunk)) if !chunk.is_empty() => buf.extend(chunk),
            Ok(_) => break,
            Err(_) => return None,
        }
        if has_da1_reply(&buf) {
            break;
        }
    }
    (!buf.is_empty()).then_some(buf)
}

/// TERM=dumb, the Linux console, tmux and screen cannot answer (or pass
/// through) the query.
pub fn probe_supported(env: &Environment) -> bool {
    if env.get("TERM").is_some_and(|t| t == "dumb" || t == "linux") {
        return false;
    }
    env.non_empty("TMUX").is_none() && env.non_empty("STY").is_none()
}

mod posix {
    use std::io;
    use std::time::Duration;

    use super::Tty;

    /// stdin in cbreak mode for the probe; restores the saved mode on drop.
    pub struct Stdio {
        saved: libc::termios,
    }

    impl Stdio {
        pub fn open() -> Option<Self> {
            unsafe {
                if libc::isatty(0) != 1 || libc::isatty(1) != 1 {
                    return None;
                }
                let mut saved: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(0, &mut saved) != 0 {
                    return None;
                }
                let mut raw = saved;
                raw.c_lflag &= !(libc::ICANON | libc::ECHO);
                raw.c_cc[libc::VMIN] = 1;
                raw.c_cc[libc::VTIME] = 0;
                // TCSANOW: draining first could block under flow control.
                if libc::tcsetattr(0, libc::TCSANOW, &raw) != 0 {
                    return None;
                }
                Some(Stdio { saved })
            }
        }
    }

    impl Drop for Stdio {
        fn drop(&mut self) {
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &self.saved);
            }
        }
    }

    impl Tty for Stdio {
        fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
            use std::io::Write;
            let mut out = io::stdout().lock();
            out.write_all(bytes)?;
            out.flush()
        }

        fn read_within(&mut self, timeout: Duration, max: usize) -> io::Result<Option<Vec<u8>>> {
            let mut fds = libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            };
            let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
            let ready = unsafe { libc::poll(&mut fds, 1, ms) };
            if ready < 0 {
                return Err(io::Error::last_os_error());
            }
            if ready == 0 {
                return Ok(None);
            }
            let mut buf = vec![0u8; max];
            let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), max) };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            buf.truncate(n as usize);
            Ok(Some(buf))
        }
    }
}

static DETECTED: OnceLock<Option<Background>> = OnceLock::new();

/// The terminal background, probed once per process. Must first run while
/// the terminal is still in cooked mode, before a TUI takes it over.
pub fn detect_terminal_background(env: &Environment) -> Option<Background> {
    *DETECTED.get_or_init(|| {
        if !probe_supported(env) {
            return None;
        }
        let mut tty = posix::Stdio::open()?;
        let reply = query(&mut tty, &mut Instant::now)?;
        classify(&reply)
    })
}

/// What an earlier [`detect_terminal_background`] found, without probing.
pub fn detected_background() -> Option<Background> {
    DETECTED.get().copied().flatten()
}

/// Discards pending terminal input, e.g. a reply that arrived late.
pub fn drain_stdin() {
    unsafe {
        if libc::isatty(0) == 1 {
            libc::tcflush(0, libc::TCIFLUSH);
        }
    }
}

/// `dark`/`light` pass through; `auto` follows `detect`, dark when unknown.
pub fn resolve_theme(setting: &str, detect: impl FnOnce() -> Option<Background>) -> Background {
    match setting {
        "light" => Background::Light,
        "dark" => Background::Dark,
        _ => detect().unwrap_or(Background::Dark),
    }
}

/// Never probe when colour is off, before `run` execs a child, or ahead of
/// `--json` output.
pub fn cli_should_probe(argv: &[String], colors_enabled: bool) -> bool {
    colors_enabled && argv.first().is_none_or(|a| a != "run") && !argv.iter().any(|a| a == "--json")
}

pub fn cli_theme(
    setting: &str,
    detect: impl FnOnce() -> Option<Background>,
    colors: bool,
) -> Background {
    if setting == "auto" && !colors {
        return Background::Dark;
    }
    resolve_theme(setting, detect)
}

#[cfg(test)]
mod tests;
