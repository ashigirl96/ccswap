//! The XML property list subset the LaunchAgent needs, serialized the way
//! Python's `plistlib.dumps` does (sorted keys, tab indent).

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plist {
    String(String),
    Bool(bool),
    Array(Vec<Plist>),
    Dict(BTreeMap<String, Plist>),
}

const HEADER: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n";

impl Plist {
    pub fn string(value: impl AsRef<str>) -> Self {
        Plist::String(value.as_ref().to_string())
    }

    pub fn dict<const N: usize>(entries: [(&str, Plist); N]) -> Self {
        Plist::Dict(entries.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    pub fn to_xml(&self) -> String {
        let mut out = String::from(HEADER);
        self.write(&mut out, 0);
        out.push_str("</plist>\n");
        out
    }

    fn write(&self, out: &mut String, depth: usize) {
        let indent = "\t".repeat(depth);
        match self {
            Plist::String(s) => {
                out.push_str(&format!("{indent}<string>{}</string>\n", escape(s)));
            }
            Plist::Bool(true) => out.push_str(&format!("{indent}<true/>\n")),
            Plist::Bool(false) => out.push_str(&format!("{indent}<false/>\n")),
            Plist::Array(items) if items.is_empty() => {
                out.push_str(&format!("{indent}<array/>\n"));
            }
            Plist::Array(items) => {
                out.push_str(&format!("{indent}<array>\n"));
                for item in items {
                    item.write(out, depth + 1);
                }
                out.push_str(&format!("{indent}</array>\n"));
            }
            Plist::Dict(map) if map.is_empty() => {
                out.push_str(&format!("{indent}<dict/>\n"));
            }
            Plist::Dict(map) => {
                out.push_str(&format!("{indent}<dict>\n"));
                for (key, value) in map {
                    out.push_str(&format!("{indent}\t<key>{}</key>\n", escape(key)));
                    value.write(out, depth + 1);
                }
                out.push_str(&format!("{indent}</dict>\n"));
            }
        }
    }
}

fn escape(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
