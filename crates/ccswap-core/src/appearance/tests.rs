//! Ported from upstream tests/test_appearance.py.

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use super::*;

#[test]
fn parses_16bit_8bit_and_hex_replies() {
    assert_eq!(
        parse_osc11(b"\x1b]11;rgb:ffff/ffff/ffff\x07"),
        Some((1.0, 1.0, 1.0))
    );
    assert_eq!(
        parse_osc11(b"\x1b]11;rgb:00/00/00\x1b\\"),
        Some((0.0, 0.0, 0.0))
    );
    let (r, _, b) = parse_osc11(b"\x1b]11;#ff8000\x07").unwrap();
    assert!((r - 1.0).abs() < 1e-9 && b.abs() < 1e-9);
}

#[test]
fn unframed_or_unterminated_replies_are_rejected() {
    for junk in [
        &b"garbage"[..],
        b"rgb:ffff/ffff/ffff",
        b"]11;rgb:ffff/ffff/ffff\x07",
        b"\x1b]11;rgb:ffff/ffff/ffff",
        b"#ff8000",
        b"\x1b]11;#ff8000",
    ] {
        assert_eq!(parse_osc11(junk), None, "{junk:?}");
    }
}

#[test]
fn classify_pins_the_half_luminance_cutoff() {
    assert_eq!(
        classify(b"\x1b]11;rgb:ffff/ffff/ffff\x07"),
        Some(Background::Light)
    );
    assert_eq!(
        classify(b"\x1b]11;rgb:0000/0000/0000\x07"),
        Some(Background::Dark)
    );
    assert_eq!(
        classify(b"\x1b]11;rgb:8080/8080/8080\x07"),
        Some(Background::Light)
    );
    assert_eq!(
        classify(b"\x1b]11;rgb:7f7f/7f7f/7f7f\x07"),
        Some(Background::Dark)
    );
    assert_eq!(classify(b"nope"), None);
}

struct FakeTty {
    pending: VecDeque<Vec<u8>>,
    writes: Vec<Vec<u8>>,
    reads: Vec<Vec<u8>>,
    timeouts: Vec<Duration>,
}

impl FakeTty {
    fn new(chunks: &[&[u8]]) -> Self {
        FakeTty {
            pending: chunks.iter().map(|c| c.to_vec()).collect(),
            writes: Vec::new(),
            reads: Vec::new(),
            timeouts: Vec::new(),
        }
    }
}

impl Tty for FakeTty {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.writes.push(bytes.to_vec());
        Ok(())
    }

    fn read_within(&mut self, timeout: Duration, max: usize) -> io::Result<Option<Vec<u8>>> {
        assert_eq!(max, 32);
        self.timeouts.push(timeout);
        let chunk = self.pending.pop_front();
        if let Some(c) = &chunk {
            self.reads.push(c.clone());
        }
        Ok(chunk)
    }
}

fn run(tty: &mut FakeTty) -> Option<Vec<u8>> {
    query(tty, &mut Instant::now)
}

#[test]
fn waits_for_da1_after_a_complete_osc_reply() {
    let (osc, da1) = (&b"\x1b]11;rgb:1e1d/1e1d/1e1d\x07"[..], &b"\x1b[?1;2c"[..]);
    let mut tty = FakeTty::new(&[osc, da1]);
    assert_eq!(run(&mut tty), Some([osc, da1].concat()));
    assert_eq!(tty.reads, [osc, da1]);
    assert_eq!(tty.writes, [b"\x1b]11;?\x07\x1b[c".to_vec()]);
}

#[test]
fn accepts_a_reply_delayed_beyond_150ms() {
    let reply = b"\x1b]11;rgb:ffff/ffff/ffff\x07\x1b[?1;2c";
    let start = Instant::now();
    let mut times = [start, start + Duration::from_millis(200)].into_iter();
    let mut clock = || times.next().unwrap_or(start + Duration::from_millis(200));
    let mut tty = FakeTty::new(&[reply]);
    assert_eq!(query(&mut tty, &mut clock), Some(reply.to_vec()));
    assert_eq!(tty.timeouts, [Duration::from_millis(800)]);
}

#[test]
fn da1_first_means_osc11_is_unsupported() {
    let da1 = b"\x1b[?62;1;2;6c";
    let mut tty = FakeTty::new(&[da1]);
    let reply = run(&mut tty);
    assert_eq!(tty.reads, [da1.to_vec()]);
    assert_eq!(classify(&reply.unwrap()), None);
}

#[test]
fn accepts_a_da1_reply_without_parameters() {
    let mut tty = FakeTty::new(&[b"\x1b[?c"]);
    assert_eq!(run(&mut tty), Some(b"\x1b[?c".to_vec()));
}

#[test]
fn an_echoed_da1_query_is_not_the_reply() {
    let chunks: [&[u8]; 3] = [b"\x1b[c", b"\x1b]11;rgb:0000/0000/0000\x07", b"\x1b[?1;2c"];
    let mut tty = FakeTty::new(&chunks);
    assert_eq!(run(&mut tty), Some(chunks.concat()));
    assert_eq!(tty.reads.len(), 3);
}

#[test]
fn accepts_a_fragmented_da1_reply() {
    let chunks: [&[u8]; 3] = [b"\x1b]11;rgb:0000/0000/0000\x07", b"\x1b[?", b"62;1;2;6c"];
    let mut tty = FakeTty::new(&chunks);
    assert_eq!(run(&mut tty), Some(chunks.concat()));
}

#[test]
fn a_silent_terminal_yields_none() {
    let mut tty = FakeTty::new(&[]);
    assert_eq!(run(&mut tty), None);
}

#[test]
fn resolve_theme_probes_only_for_auto() {
    let boom = || -> Option<Background> { panic!("explicit theme must not probe") };
    assert_eq!(resolve_theme("dark", boom), Background::Dark);
    assert_eq!(resolve_theme("light", boom), Background::Light);
    assert_eq!(
        resolve_theme("auto", || Some(Background::Light)),
        Background::Light
    );
    assert_eq!(resolve_theme("auto", || None), Background::Dark);
}

#[test]
fn cli_theme_skips_detection_when_colours_are_off() {
    let boom = || -> Option<Background> { panic!("must not probe") };
    assert_eq!(cli_theme("auto", boom, false), Background::Dark);
    assert_eq!(
        cli_theme("auto", || Some(Background::Light), true),
        Background::Light
    );
    assert_eq!(cli_theme("light", boom, true), Background::Light);
}

#[test]
fn cli_should_probe_gates() {
    let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(!cli_should_probe(&argv(&["run", "2"]), true));
    assert!(!cli_should_probe(&argv(&["list", "--json"]), true));
    assert!(!cli_should_probe(&argv(&["list"]), false));
    assert!(cli_should_probe(&argv(&["list"]), true));
}

#[test]
fn unsupported_terminals_are_not_probed() {
    let env = |pairs: &[(&str, &str)]| Environment::from_pairs(pairs.iter().copied());
    assert!(!probe_supported(&env(&[(
        "TMUX",
        "/tmp/tmux-1000/default,1,0"
    )])));
    assert!(!probe_supported(&env(&[("STY", "123.pts")])));
    assert!(!probe_supported(&env(&[("TERM", "dumb")])));
    assert!(!probe_supported(&env(&[("TERM", "linux")])));
    assert!(probe_supported(&env(&[("TERM", "xterm-256color")])));
}
