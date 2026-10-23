//! Codex CLI accounts, kept apart from the Claude store (ADR 0004).
//!
//! - [`CodexPaths`]: the live `auth.json` and the store under `~/.claude-swap-backup/codex/`.
//! - [`identity`]: who an `auth.json` belongs to, from its tokens and id_token claims.
//! - [`Ledger`]: `codex/accounts.json`, numbered independently of Claude slots.
//! - [`CodexStore`]: the operations, each ledger rewrite under the store `.lock`.
//! - [`usage`]: the 5-hour and weekly windows from `wham/usage`, cached like the Claude side's.
//! - [`view`]: terminal lines and `--json` envelopes.

pub mod identity;
pub mod ledger;
pub mod paths;
pub mod store;
pub mod usage;
pub mod view;

pub use identity::{CodexIdentity, read_identity};
pub use ledger::{Ledger, LedgerAccount};
pub use paths::CodexPaths;
pub use store::{AddOutcome, CodexListing, CodexStore, ListedCodexAccount};
pub use usage::CodexUsage;
