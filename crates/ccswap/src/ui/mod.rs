pub mod launch_agent;
pub mod menubar;
pub mod service;
pub mod tui;
pub mod widget;

use crate::cli::VerbHelp;

pub const VERB_HELP: VerbHelp = &[
    (&["tui"], include_str!("help/tui.txt")),
    (&["watch"], include_str!("help/watch.txt")),
    (&["menubar"], include_str!("help/menubar.txt")),
];
