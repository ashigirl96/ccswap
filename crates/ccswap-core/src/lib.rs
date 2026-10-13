pub mod endpoints;
pub mod env;
pub mod error;
pub mod paths;
pub mod presentation;
pub mod pyjson;
pub mod secret_store;
pub mod status;
pub mod store;
pub mod usage;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
