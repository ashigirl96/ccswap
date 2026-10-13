use std::path::Path;

use serde_json::{Map, Value, json};

use crate::{Reply, append_line, pyjson, read_json};

pub fn main(state: &Path, args: &[String]) -> Reply {
    let env: Map<String, Value> = std::env::vars_os()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                Value::String(v.to_string_lossy().into_owned()),
            )
        })
        .collect();
    let cwd = std::env::current_dir().expect("cwd");
    let record = json!({ "argv": args, "cwd": cwd.to_string_lossy(), "env": env });
    append_line(
        &state.join("calls.jsonl"),
        &pyjson::to_string_compact(&record),
    );
    let responses = read_json(&state.join("responses.json")).unwrap_or_else(|| json!({}));
    Reply::from_json(&responses[args.join(" ")])
}
