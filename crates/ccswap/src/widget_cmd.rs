use std::path::{Path, PathBuf};
use std::process::Command;

use ccswap_core::env::Environment;
use ccswap_core::paths::Paths;
use ccswap_core::presentation::Style;

use crate::config_cmd::{looks_like_option, lookup, split_explicit};

const HELP: &str = include_str!("widget_help.txt");
const OPTS: &[&str] = &["-h", "--help", "--install"];
const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";
const EXTENSION: &str = "Contents/PlugIns/CcswapWidget.appex";

pub fn run(prog: &str, env: &Environment, argv: &[String]) -> i32 {
    let usage_error = |msg: &str| {
        eprintln!("usage: {prog} widget [-h] [--install]");
        eprintln!("{prog} widget: error: {msg}");
        2
    };
    let mut install = false;
    for arg in argv {
        if !looks_like_option(arg, OPTS) {
            return usage_error(&format!("unrecognized arguments: {arg}"));
        }
        let (name, explicit) = split_explicit(arg);
        match (lookup(name, OPTS), explicit) {
            (Some("--install"), None) => install = true,
            (Some("--install"), Some(v)) => {
                return usage_error(&format!(
                    "argument --install: ignored explicit argument '{v}'"
                ));
            }
            (Some(_), _) => {
                print!("{}", HELP.replace("{prog}", prog));
                return 0;
            }
            (None, _) => return usage_error(&format!("unrecognized arguments: {arg}")),
        }
    }
    if !install {
        print!("{}", HELP.replace("{prog}", prog));
        return 0;
    }
    let style = Style::detect(env, &Paths::new(env));
    match register() {
        Ok(app) => {
            println!("Widget registered from {}.", app.display());
            println!(
                "{}",
                style.dimmed(
                    "Add it from the desktop: right-click, Edit Widgets, then search for ccswap. \
                     It shows data while the menu bar runs."
                )
            );
            0
        }
        Err(message) => {
            eprintln!("{}", style.red(&format!("Error: {message}")));
            1
        }
    }
}

fn register() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| format!("Couldn't locate the running ccswap: {e}"))?;
    let app = bundle_root(&exe).ok_or_else(|| {
        format!(
            "{} isn't inside ccswap.app. The widget ships with the ccswap.app in the release \
             archive; run the ccswap inside it.",
            exe.display()
        )
    })?;
    let extension = app.join(EXTENSION);
    if !extension.is_dir() {
        return Err(format!("{} is missing.", extension.display()));
    }
    run_tool(Command::new(LSREGISTER).arg("-f").arg(&app))?;
    run_tool(Command::new("pluginkit").arg("-a").arg(&extension))?;
    // A running extension keeps the replaced binary until it exits.
    let _ = Command::new("pkill").args(["-x", "CcswapWidget"]).status();
    // The widget's buttons stay inert until linkd indexes the bundle's App
    // Intents, which happens when LaunchServices launches it.
    run_tool(
        Command::new("open")
            .args(["-g", "-j"])
            .arg(&app)
            .args(["--args", "--version"]),
    )?;
    Ok(app)
}

fn run_tool(command: &mut Command) -> Result<(), String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command
        .output()
        .map_err(|e| format!("Couldn't run {program}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// `X.app` for an executable at `X.app/Contents/MacOS/<name>`.
fn bundle_root(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    let is_bundle = macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension()? == "app";
    is_bundle.then(|| app.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_executable_in_contents_macos_belongs_to_its_app() {
        assert_eq!(
            bundle_root(Path::new(
                "/Users/u/Applications/ccswap.app/Contents/MacOS/ccswap"
            )),
            Some(PathBuf::from("/Users/u/Applications/ccswap.app"))
        );
    }

    #[test]
    fn a_loose_executable_has_no_bundle() {
        for exe in [
            "/Users/u/.local/bin/ccswap",
            "/x/Contents/MacOS/ccswap",
            "/x/ccswap.app/MacOS/ccswap",
            "ccswap",
        ] {
            assert_eq!(bundle_root(Path::new(exe)), None, "{exe}");
        }
    }
}
