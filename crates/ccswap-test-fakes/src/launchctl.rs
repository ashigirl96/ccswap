use std::path::Path;

use serde_json::json;

use crate::{Reply, append_line, pyjson, read_json};

pub fn main(state: &Path, args: &[String]) -> Reply {
    let loaded = state.join("loaded");
    let record = json!({ "argv": args, "loaded": loaded.exists() });
    append_line(
        &state.join("calls.jsonl"),
        &pyjson::to_string_compact(&record),
    );
    let sub = args.first().map(String::as_str).unwrap_or_default();
    scripted(state, sub).unwrap_or_else(|| modelled(&loaded, sub, args))
}

fn scripted(state: &Path, sub: &str) -> Option<Reply> {
    let path = state.join("script.json");
    let mut script = read_json(&path)?;
    let queue = script.get_mut(sub)?.as_array_mut()?;
    if queue.is_empty() {
        return None;
    }
    let reply = queue.remove(0);
    std::fs::write(&path, pyjson::to_string_compact(&script)).expect("write script.json");
    Some(Reply::from_json(&reply))
}

fn modelled(loaded: &Path, sub: &str, args: &[String]) -> Reply {
    let reply = |exit: i32, stderr: &str| Reply {
        exit,
        stdout: String::new(),
        stderr: stderr.into(),
    };
    let is_loaded = loaded.exists();
    match sub {
        "print" if is_loaded => Reply {
            exit: 0,
            stdout: std::fs::read_to_string(loaded).expect("read loaded"),
            stderr: String::new(),
        },
        "print" => reply(
            113,
            &format!(
                "Bad request.\nCould not find service \"{}\" in domain\n",
                args[1]
            ),
        ),
        "bootout" if is_loaded => {
            std::fs::remove_file(loaded).expect("unload");
            reply(0, "")
        }
        "bootout" => reply(3, "Boot-out failed: 3: No such process\n"),
        "kickstart" if is_loaded => reply(0, ""),
        "kickstart" => reply(113, "Could not find service in domain for user gui\n"),
        "bootstrap" if is_loaded => reply(5, "Bootstrap failed: 5: Input/output error\n"),
        "bootstrap" => {
            let plist = std::fs::read_to_string(&args[2]).expect("read plist");
            let label = plist_label(&plist).expect("plist has a Label");
            std::fs::write(
                loaded,
                format!(
                    "{}/{label} = {{\n\tstate = running\n\tpid = 4242\n}}\n",
                    args[1]
                ),
            )
            .expect("write loaded");
            reply(0, "")
        }
        _ => reply(64, &format!("Unrecognized subcommand: {sub}\n")),
    }
}

/// Reads the top-level `Label` of an XML plist; values hold no nested markup.
fn plist_label(plist: &str) -> Option<String> {
    let after_key = &plist[plist.find("<key>Label</key>")? + "<key>Label</key>".len()..];
    let value = after_key.trim_start().strip_prefix("<string>")?;
    let value = &value[..value.find("</string>")?];
    Some(
        value
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_is_read_from_the_plist_and_unescaped() {
        let plist = "<?xml version=\"1.0\"?>\n<plist version=\"1.0\">\n<dict>\n\t<key>KeepAlive</key>\n\t<true/>\n\t<key>Label</key>\n\t<string>a&amp;b.menubar</string>\n</dict>\n</plist>\n";
        assert_eq!(plist_label(plist).as_deref(), Some("a&b.menubar"));
    }

    #[test]
    fn a_plist_without_label_has_none() {
        assert_eq!(plist_label("<plist><dict></dict></plist>"), None);
    }
}
