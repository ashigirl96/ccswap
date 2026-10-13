//! JSON text byte-identical to Python's `json.dumps` defaults, which every
//! upstream file and `--json` payload is written with: ASCII-only output,
//! `", "` / `": "` separators, and Python's float repr.

use serde_json::{Map, Number, Value};

/// `json.dumps(value, indent=2)`.
pub fn to_string_pretty(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, Some(2), 0);
    out
}

/// `json.dumps(value)`: one line, Python separators.
pub fn to_string_compact(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, None, 0);
    out
}

fn write_value(out: &mut String, value: &Value, indent: Option<usize>, depth: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&number_repr(n)),
        Value::String(s) => write_string(out, s),
        Value::Array(items) => write_seq(out, '[', ']', items.len(), indent, depth, |out, i| {
            write_value(out, &items[i], indent, depth + 1)
        }),
        Value::Object(map) => write_object(out, map, indent, depth),
    }
}

fn write_object(out: &mut String, map: &Map<String, Value>, indent: Option<usize>, depth: usize) {
    let entries: Vec<_> = map.iter().collect();
    write_seq(out, '{', '}', entries.len(), indent, depth, |out, i| {
        let (k, v) = entries[i];
        write_string(out, k);
        out.push_str(": ");
        write_value(out, v, indent, depth + 1);
    });
}

fn write_seq(
    out: &mut String,
    open: char,
    close: char,
    len: usize,
    indent: Option<usize>,
    depth: usize,
    mut item: impl FnMut(&mut String, usize),
) {
    out.push(open);
    if len == 0 {
        out.push(close);
        return;
    }
    for i in 0..len {
        match indent {
            Some(width) => {
                out.push_str(if i == 0 { "\n" } else { ",\n" });
                out.push_str(&" ".repeat(width * (depth + 1)));
            }
            None if i > 0 => out.push_str(", "),
            None => {}
        }
        item(out, i);
    }
    if let Some(width) = indent {
        out.push('\n');
        out.push_str(&" ".repeat(width * depth));
    }
    out.push(close);
}

fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

fn number_repr(n: &Number) -> String {
    if n.is_i64() || n.is_u64() {
        return n.to_string();
    }
    float_repr(n.as_f64().unwrap_or(f64::NAN))
}

/// Python `repr(float)`: shortest round-trip digits, positional for
/// exponents in `-4..16`, otherwise `d.ddde±XX`.
pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').expect("{:e} always has an exponent");
    let exp: i32 = exp.parse().expect("integer exponent");
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", mantissa),
    };
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    if (-4..16).contains(&exp) {
        let point = exp + 1;
        let body = if point <= 0 {
            format!("0.{}{digits}", "0".repeat((-point) as usize))
        } else if point as usize >= digits.len() {
            format!("{digits}{}.0", "0".repeat(point as usize - digits.len()))
        } else {
            let (int, frac) = digits.split_at(point as usize);
            format!("{int}.{frac}")
        };
        format!("{sign}{body}")
    } else {
        let (first, rest) = digits.split_at(1);
        let mantissa = if rest.is_empty() {
            first.to_string()
        } else {
            format!("{first}.{rest}")
        };
        let exp_sign = if exp < 0 { '-' } else { '+' };
        format!("{sign}{mantissa}e{exp_sign}{:02}", exp.abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pretty_matches_python_indent_2() {
        let value = json!({"a": [1, 2], "b": {}, "c": [], "d": {"e": null}});
        assert_eq!(
            to_string_pretty(&value),
            "{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": {},\n  \"c\": [],\n  \"d\": {\n    \"e\": null\n  }\n}"
        );
    }

    #[test]
    fn compact_uses_python_separators() {
        let value = json!({"a": [1, true], "b": "x"});
        assert_eq!(to_string_compact(&value), r#"{"a": [1, true], "b": "x"}"#);
    }

    #[test]
    fn keys_keep_insertion_order() {
        let value: Value = serde_json::from_str(r#"{"z": 1, "a": 2}"#).unwrap();
        assert_eq!(to_string_compact(&value), r#"{"z": 1, "a": 2}"#);
    }

    #[test]
    fn non_ascii_and_controls_are_escaped_like_ensure_ascii() {
        let value = json!("é\u{1F600}\u{7f}\u{1}\"\\\n");
        assert_eq!(
            to_string_compact(&value),
            concat!(
                r#""\"#,
                "u00e9",
                r#"\"#,
                "ud83d",
                r#"\"#,
                "ude00",
                r#"\u007f\u0001\"\\\n""#
            )
        );
    }

    #[test]
    fn floats_follow_python_repr() {
        let cases = [
            (80.0, "80.0"),
            (0.1, "0.1"),
            (25.5, "25.5"),
            (-3.0, "-3.0"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (1e15, "1000000000000000.0"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (123456.789, "123456.789"),
            (0.0, "0.0"),
        ];
        for (f, expected) in cases {
            assert_eq!(float_repr(f), expected, "{f}");
        }
    }

    #[test]
    fn integers_stay_integers() {
        let value: Value = serde_json::from_str("[10, 10.0, -7]").unwrap();
        assert_eq!(to_string_compact(&value), "[10, 10.0, -7]");
    }
}
