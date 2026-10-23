//! Claude Code's wording for usage states, and an account's usage lines in
//! `list` and `status`.

use ccswap_core::poll_policy::SERVE_TTL_S;
use ccswap_core::presentation::Style;
use ccswap_core::usage::Sentinel;
use ccswap_core::usage_store::UsageEntry;
use ccswap_core::usage_view::{format_age, format_usage_lines, last_seen_note, unavailable_line};

/// The one-line note in human output.
pub fn sentinel_note(sentinel: Sentinel) -> &'static str {
    match sentinel {
        Sentinel::NoCredentials => "no credentials",
        Sentinel::TokenExpired => {
            "token expired — refresh deferred this pass; retries automatically"
        }
        Sentinel::ApiKey => "API key (no quota)",
        Sentinel::KeychainUnavailable => "keychain unavailable — locked or in use; try again",
        Sentinel::ReloginRequired => {
            "re-login needed — refresh token dead; log in with Claude Code, then run: ccswap add"
        }
        Sentinel::ForeignCredential => {
            "live credential belongs to another account — a switch repairs it"
        }
    }
}

/// Friendly text for fetch-error kinds that need more than their identifier.
pub fn error_note(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "store-unmirrored" => {
            "CLAUDE_SECURESTORAGE_CONFIG_DIR set — unset it or run from a normal shell"
        }
        "invalid_client" => "cswap's OAuth client was rejected — systemic, not this account",
        "consume-busy" => "another cswap surface holds the slot — retries next pass",
        "stash-unreadable" => {
            "this slot's stashed successor is unreadable — unlock the keychain or fix the file, then retry; `ccswap unclaimed` inspects it"
        }
        _ => return None,
    })
}

/// Styled usage lines (without indent) for one account.
pub fn usage_entry_lines(entry: &UsageEntry, style: &Style) -> Vec<String> {
    if let Some(sentinel) = entry.sentinel {
        let mut out = vec![style.dimmed(sentinel_note(sentinel))];
        if let Some(seen) = last_seen_note(entry)
            && sentinel != Sentinel::ApiKey
        {
            out.push(format!("{} {}", style.dimmed("└"), style.muted(&seen)));
        }
        return out;
    }
    if let Some(last_good) = &entry.last_good {
        let mut lines = format_usage_lines(last_good, entry.fetched_at);
        if let (Some(last), Some(age), Some(fetched_at)) =
            (lines.last_mut(), entry.age_s, entry.fetched_at)
            && age > SERVE_TTL_S
        {
            last.push_str(&format!(" · {}", format_age(fetched_at)));
        }
        let n = lines.len();
        return lines
            .iter()
            .enumerate()
            .map(|(j, line)| {
                let connector = if j == n - 1 { "└" } else { "├" };
                format!("{} {}", style.dimmed(connector), style.muted(line))
            })
            .collect();
    }
    vec![unavailable_line(entry, style, error_note)]
}
