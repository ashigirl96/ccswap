use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::{Reply, append_line, pyjson, read_json, shlex};

const NOT_FOUND: i32 = 44;
const DUPLICATE: i32 = 45;
const VALUE_OPTS: [&str; 13] = [
    "-a", "-s", "-w", "-X", "-l", "-D", "-j", "-c", "-C", "-G", "-T", "-r", "-k",
];

#[derive(Default)]
struct Out {
    out: Vec<String>,
    err: Vec<String>,
}

pub fn main(state: &Path, args: &[String]) -> Reply {
    let stdin = (args.first().map(String::as_str) == Some("-i")).then(|| {
        let mut bytes = Vec::new();
        std::io::stdin()
            .read_to_end(&mut bytes)
            .expect("read stdin");
        String::from_utf8_lossy(&bytes).into_owned()
    });
    let words = match &stdin {
        Some(text) => shlex::split(text).expect("shlex"),
        None => args.to_vec(),
    };
    if let Some(first) = words.first() {
        pause_if_armed(state, first);
    }
    let mut record = Map::new();
    record.insert("argv".into(), json!(args));
    record.insert("stdin".into(), json!(stdin));
    if let Some(Value::Array(paths)) = read_json(&state.join("observe.json")) {
        let observed: Map<String, Value> = paths
            .iter()
            .filter_map(Value::as_str)
            .map(|p| (p.to_string(), Value::Bool(Path::new(p).exists())))
            .collect();
        record.insert("observed".into(), Value::Object(observed));
    }
    append_line(
        &state.join("calls.jsonl"),
        &pyjson::to_string_compact(&Value::Object(record)),
    );
    if let Some(rc) = read_rc(&state.join("fail_rc")) {
        return Reply {
            exit: rc,
            stdout: String::new(),
            stderr: "security: SecKeychainCopyDefault: User interaction is not allowed.\n".into(),
        };
    }
    let mut out = Out::default();
    let rc = match &stdin {
        Some(text) => {
            let mut rc = 0;
            for line in split_lines(text) {
                if !line.trim().is_empty() {
                    rc = run(state, &shlex::split(line).expect("shlex"), &mut out);
                }
            }
            rc
        }
        None => run(state, args, &mut out),
    };
    Reply {
        exit: rc,
        stdout: out.out.concat(),
        stderr: if out.err.is_empty() {
            String::new()
        } else {
            out.err.join("\n") + "\n"
        },
    }
}

fn run(state: &Path, argv: &[String], out: &mut Out) -> i32 {
    let Some((cmd, rest)) = argv.split_first() else {
        return 1;
    };
    let (opts, flags) = parse(cmd, rest);
    let opt = |name: &str| opts.get(name).map_or("None", String::as_str);
    for fail in [
        format!("fail_{cmd}"),
        format!("fail_{cmd}@{}", opt("-s")),
        format!("fail_{cmd}@{}@{}", opt("-s"), opt("-a")),
    ] {
        if let Some(rc) = read_rc(&state.join(fail)) {
            out.err.push(
                "security: SecKeychainItemModifyContent: User interaction is not allowed.".into(),
            );
            return rc;
        }
    }
    let items_path = state.join("items.json");
    let mut items = match read_json(&items_path) {
        Some(Value::Array(items)) => items,
        Some(other) => panic!("items.json is not a list: {other}"),
        None => Vec::new(),
    };
    let service = opts.get("-s").map_or(Value::Null, |s| json!(s));
    let account = opts.get("-a").map_or(Value::Null, |a| json!(a));
    let found = items
        .iter()
        .position(|i| i["service"] == service && i["account"] == account);
    match cmd.as_str() {
        "find-generic-password" => {
            let Some(at) = found else {
                return not_found(out);
            };
            let item = &items[at];
            let field = |k: &str| item[k].as_str().unwrap_or_default().to_string();
            if flags.contains("-w") {
                out.out.push(format!("{}\n", field("password")));
            } else {
                out.out.push(format!(
                    "keychain: \"fake\"\nclass: \"genp\"\nattributes:\n    \"acct\"<blob>=\"{}\"\n    \"svce\"<blob>=\"{}\"\n",
                    field("account"),
                    field("service")
                ));
            }
            0
        }
        "add-generic-password" => {
            let password = match opts.get("-X") {
                Some(hex) => String::from_utf8(from_hex(hex)).expect("-X is UTF-8"),
                None => opts.get("-w").cloned().unwrap_or_default(),
            };
            match found {
                Some(_) if !flags.contains("-U") => {
                    out.err.push(
                        "security: SecKeychainItemCreateFromContent: The specified item already exists in the keychain."
                            .into(),
                    );
                    return DUPLICATE;
                }
                Some(at) => items[at]["password"] = json!(password),
                None => items
                    .push(json!({ "service": service, "account": account, "password": password })),
            }
            save(&items_path, &items);
            0
        }
        "delete-generic-password" => {
            let Some(at) = found else {
                return not_found(out);
            };
            items.remove(at);
            save(&items_path, &items);
            out.out
                .push("keychain: \"fake\"\npassword has been deleted.\n".into());
            0
        }
        _ => {
            out.err
                .push(format!("security: unknown command {}", py_repr(cmd)));
            1
        }
    }
}

fn parse(cmd: &str, args: &[String]) -> (BTreeMap<String, String>, BTreeSet<String>) {
    let takes_value =
        |a: &str| VALUE_OPTS.contains(&a) && (cmd == "add-generic-password" || a != "-w");
    let (mut opts, mut flags) = (BTreeMap::new(), BTreeSet::new());
    let mut i = 0;
    while i < args.len() {
        if takes_value(&args[i]) && i + 1 < args.len() {
            opts.insert(args[i].clone(), args[i + 1].clone());
            i += 2;
        } else {
            flags.insert(args[i].clone());
            i += 1;
        }
    }
    (opts, flags)
}

fn not_found(out: &mut Out) -> i32 {
    out.err.push(
        "security: SecKeychainSearchCopyNext: The specified item could not be found in the keychain."
            .into(),
    );
    NOT_FOUND
}

fn save(path: &Path, items: &[Value]) {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, pyjson::to_string_pretty(&json!(items))).expect("write items");
    std::fs::rename(&tmp, path).expect("replace items");
}

fn read_rc(path: &Path) -> Option<i32> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(text.trim().parse().expect("rc file holds an integer"))
}

/// Holds for at most 4.5 s because callers time `security` out after 5 s.
fn pause_if_armed(state: &Path, cmd: &str) {
    // Only the call that wins the rename pauses.
    if std::fs::rename(state.join(format!("pause_{cmd}")), state.join("paused")).is_err() {
        return;
    }
    let deadline = Instant::now() + Duration::from_millis(4500);
    while !state.join("release").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `bytes.fromhex`, which also skips whitespace between bytes.
fn from_hex(hex: &str) -> Vec<u8> {
    let digits: Vec<u8> = hex.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    assert!(digits.len().is_multiple_of(2), "odd-length hex");
    digits
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).expect("hex digit"))
        .collect()
}

/// `str.splitlines`.
fn split_lines(text: &str) -> Vec<&str> {
    let breaks = [
        '\n', '\r', '\u{0b}', '\u{0c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}',
        '\u{2029}',
    ];
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if !breaks.contains(&c) {
            continue;
        }
        lines.push(&text[start..at]);
        start = at + c.len_utf8();
        if c == '\r' && chars.peek().is_some_and(|&(_, n)| n == '\n') {
            chars.next();
            start += 1;
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// `repr(str)` for the ASCII command names the tests use.
fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_split_like_python_splitlines() {
        assert_eq!(split_lines("a\r\nb\rc\n\nd"), ["a", "b", "c", "", "d"]);
        assert_eq!(split_lines("a\n"), ["a"]);
    }

    #[test]
    fn hex_skips_whitespace() {
        assert_eq!(from_hex("73 33\n63"), b"s3c");
    }

    #[test]
    fn repr_quotes_like_python() {
        assert_eq!(py_repr("bogus"), "'bogus'");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("a\tb"), "'a\\tb'");
    }
}
