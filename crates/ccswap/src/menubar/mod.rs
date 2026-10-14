#[cfg(target_os = "macos")]
mod app;
#[cfg(target_os = "macos")]
mod appkit;
#[cfg(target_os = "macos")]
mod card_view;
pub mod format;
pub mod menu;
pub mod settings;
pub mod widget;

use std::sync::Arc;

use ccswap_core::actions::Actions;
use ccswap_core::env::Environment;

/// Blocks until the user quits; the exit code is 0 then.
#[cfg(target_os = "macos")]
pub fn run(actions: Arc<dyn Actions>, env: Environment) -> i32 {
    app::run(actions, env)
}

#[cfg(not(target_os = "macos"))]
pub fn run(_actions: Arc<dyn Actions>, _env: Environment) -> i32 {
    eprintln!("Error: The menu bar is only available on macOS.");
    1
}
