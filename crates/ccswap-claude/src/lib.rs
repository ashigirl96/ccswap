pub mod accounts;
pub mod actions;
pub mod anthropic_client;
pub mod autoswitch;
pub mod consume_gate;
pub mod listing;
pub mod mappings;
pub mod migrations;
pub mod probe_verdicts;
pub mod process_detection;
pub mod ranking;
pub mod secret_store;
pub mod session;
pub mod session_profile;
pub mod snapshot;
pub mod status;
pub mod store;
pub mod switcher;
pub mod transfer;
pub mod unclaimed;
pub mod usage_collector;

use ccswap_core::settings::{Registry, SettingSpec, UI_SETTINGS};

static SETTING_SECTIONS: [&[SettingSpec]; 2] = [autoswitch::AUTOSWITCH_SETTINGS, UI_SETTINGS];

/// settings.json, which upstream shares: the `autoswitch` section, then `ui`.
pub static SETTINGS: Registry = Registry::new(&SETTING_SECTIONS);
