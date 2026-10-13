//! The subset of Python's argparse that upstream's parsers rely on,
//! byte-compatible down to its error text and exit code 2.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arity {
    Flag,
    Value,
    /// `nargs="?"` with `const=""`.
    OptionalValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Store,
    Help,
    Version,
}

#[derive(Debug, Clone, Copy)]
pub struct Opt {
    pub name: &'static str,
    pub arity: Arity,
    pub action: Action,
    pub exclusive: bool,
    pub choices: Option<&'static [&'static str]>,
    pub int: bool,
    pub float: bool,
}

impl Opt {
    pub const fn flag(name: &'static str) -> Self {
        Opt {
            name,
            arity: Arity::Flag,
            action: Action::Store,
            exclusive: false,
            choices: None,
            int: false,
            float: false,
        }
    }

    pub const fn value(name: &'static str) -> Self {
        Opt {
            arity: Arity::Value,
            ..Self::flag(name)
        }
    }

    pub const fn optional_value(name: &'static str) -> Self {
        Opt {
            arity: Arity::OptionalValue,
            ..Self::flag(name)
        }
    }

    pub const fn exclusive(self) -> Self {
        Opt {
            exclusive: true,
            ..self
        }
    }

    pub const fn choices(self, choices: &'static [&'static str]) -> Self {
        Opt {
            choices: Some(choices),
            ..self
        }
    }

    pub const fn int(self) -> Self {
        Opt { int: true, ..self }
    }

    pub const fn float(self) -> Self {
        Opt {
            float: true,
            ..self
        }
    }

    pub const fn action(self, action: Action) -> Self {
        Opt { action, ..self }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Flag,
    Text(String),
}

#[derive(Debug, Default)]
pub struct Parsed {
    values: BTreeMap<&'static str, Value>,
    order: Vec<&'static str>,
}

impl Parsed {
    /// Which of `names` was given last (argparse's last-wins for options
    /// sharing a destination, e.g. `--x` / `--no-x`).
    pub fn last_of(&self, names: &[&str]) -> Option<&'static str> {
        self.order.iter().rev().find(|n| names.contains(n)).copied()
    }

    pub fn flag(&self, name: &str) -> bool {
        self.values.contains_key(name)
    }

    pub fn value(&self, name: &str) -> Option<&str> {
        match self.values.get(name) {
            Some(Value::Text(text)) => Some(text),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Parsed,
    Help,
    Version,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParseError(pub String);

pub struct Parser<'a> {
    pub opts: &'a [Opt],
}

impl Parser<'_> {
    pub fn parse(&self, args: &[String]) -> Result<(Outcome, Parsed), ParseError> {
        let mut parsed = Parsed::default();
        let mut extras: Vec<String> = Vec::new();
        let mut exclusive_seen: Option<&'static str> = None;
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            i += 1;
            if arg == "--" {
                extras.extend(args[i - 1..].iter().cloned());
                break;
            }
            if !looks_like_option(arg) {
                extras.push(arg.clone());
                continue;
            }
            let (spelled, explicit) = match arg.split_once('=') {
                Some((name, value)) if arg.starts_with("--") => (name, Some(value.to_string())),
                _ => (arg.as_str(), None),
            };
            let Some(opt) = self.lookup(spelled)? else {
                extras.push(arg.clone());
                continue;
            };
            let value = match opt.arity {
                Arity::Flag => {
                    if let Some(v) = explicit {
                        return Err(ParseError(format!(
                            "argument {}: ignored explicit argument '{v}'",
                            opt.name
                        )));
                    }
                    Value::Flag
                }
                Arity::Value => match explicit {
                    Some(v) => Value::Text(v),
                    None if i < args.len() && !looks_like_option(&args[i]) => {
                        i += 1;
                        Value::Text(args[i - 1].clone())
                    }
                    None => {
                        return Err(ParseError(format!(
                            "argument {}: expected one argument",
                            opt.name
                        )));
                    }
                },
                Arity::OptionalValue => match explicit {
                    Some(v) => Value::Text(v),
                    None if i < args.len() && !looks_like_option(&args[i]) => {
                        i += 1;
                        Value::Text(args[i - 1].clone())
                    }
                    None => Value::Text(String::new()),
                },
            };
            if let Value::Text(text) = &value {
                check_value(opt, text)?;
            }
            if opt.exclusive {
                match exclusive_seen {
                    Some(prev) if prev != opt.name => {
                        return Err(ParseError(format!(
                            "argument {}: not allowed with argument {prev}",
                            opt.name
                        )));
                    }
                    _ => exclusive_seen = Some(opt.name),
                }
            }
            match opt.action {
                Action::Help => return Ok((Outcome::Help, parsed)),
                Action::Version => return Ok((Outcome::Version, parsed)),
                Action::Store => {
                    parsed.values.insert(opt.name, value);
                    parsed.order.push(opt.name);
                }
            }
        }
        if !extras.is_empty() {
            return Err(ParseError(format!(
                "unrecognized arguments: {}",
                extras.join(" ")
            )));
        }
        Ok((Outcome::Parsed, parsed))
    }

    fn lookup(&self, spelled: &str) -> Result<Option<&Opt>, ParseError> {
        if let Some(opt) = self.opts.iter().find(|o| o.name == spelled) {
            return Ok(Some(opt));
        }
        if !spelled.starts_with("--") {
            return Ok(None);
        }
        let matches: Vec<&Opt> = self
            .opts
            .iter()
            .filter(|o| o.name.starts_with("--") && o.name.starts_with(spelled))
            .collect();
        match matches.as_slice() {
            [] => Ok(None),
            [one] => Ok(Some(one)),
            many => Err(ParseError(format!(
                "ambiguous option: {spelled} could match {}",
                many.iter().map(|o| o.name).collect::<Vec<_>>().join(", ")
            ))),
        }
    }
}

fn check_value(opt: &Opt, text: &str) -> Result<(), ParseError> {
    if opt.int && text.trim().parse::<i64>().is_err() {
        return Err(ParseError(format!(
            "argument {}: invalid int value: '{text}'",
            opt.name
        )));
    }
    if opt.float && ccswap_core::settings::python_float(text).is_none() {
        return Err(ParseError(format!(
            "argument {}: invalid float value: '{text}'",
            opt.name
        )));
    }
    if let Some(choices) = opt.choices
        && !choices.contains(&text)
    {
        let listed: Vec<String> = choices.iter().map(|c| format!("'{c}'")).collect();
        return Err(ParseError(format!(
            "argument {}: invalid choice: '{text}' (choose from {})",
            opt.name,
            listed.join(", ")
        )));
    }
    Ok(())
}

fn looks_like_option(arg: &str) -> bool {
    arg.starts_with('-') && arg != "-" && arg.parse::<f64>().is_err()
}
