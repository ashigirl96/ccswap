pub(crate) fn split_explicit(arg: &str) -> (&str, Option<&str>) {
    match arg.split_once('=') {
        Some((name, value)) if arg.starts_with("--") => (name, Some(value)),
        _ => (arg, None),
    }
}

pub(crate) fn lookup(name: &str, opts: &[&'static str]) -> Option<&'static str> {
    if let Some(opt) = opts.iter().find(|o| **o == name) {
        return Some(opt);
    }
    if !name.starts_with("--") || name.len() < 3 {
        return None;
    }
    let mut matches = opts.iter().filter(|o| o.starts_with(name));
    match (matches.next(), matches.next()) {
        (Some(one), None) => Some(one),
        _ => None,
    }
}

/// argparse's classification: known options first, then `-`, negative
/// numbers and anything containing a space are positionals.
pub(crate) fn looks_like_option(arg: &str, opts: &[&'static str]) -> bool {
    if !arg.starts_with('-') || arg == "-" {
        return false;
    }
    if lookup(split_explicit(arg).0, opts).is_some() {
        return true;
    }
    !(is_negative_number(arg) || arg.contains(' '))
}

fn is_negative_number(arg: &str) -> bool {
    let Some(rest) = arg.strip_prefix('-') else {
        return false;
    };
    let all_digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    match rest.split_once('.') {
        None => all_digits(rest),
        Some((int, frac)) => (int.is_empty() || all_digits(int)) && all_digits(frac),
    }
}
