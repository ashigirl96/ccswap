//! `add-token`: register a setup-token or managed API key without a login.
//! No network calls; the account is personal (`organizationUuid ""`).

use serde_json::{Value, json};

use super::{Accounts, Console, accounts_mut, display_tag, record_text, sequence_mut};
use crate::error::{Error, ErrorKind, Result};
use crate::identifier;
use crate::locks::{STORE_LOCK_TIMEOUT, StoreLock};
use crate::pyjson;
use crate::secret_store::looks_like_api_key;
use crate::store::{self, JsonObject};

const SETUP_TOKEN_SCOPES: [&str; 1] = ["user:inference"];

#[derive(Debug, Clone)]
pub enum TokenInput {
    Value(String),
    /// One line of standard input (`-`).
    Stdin,
    /// A hidden `Token: ` prompt.
    Prompt,
}

#[derive(Debug, Clone, Default)]
pub struct AddTokenOptions {
    pub email: Option<String>,
    pub slot: Option<i64>,
    /// Skip the overwrite prompt.
    pub assume_yes: bool,
}

impl Accounts<'_> {
    pub fn add_token(
        &self,
        input: TokenInput,
        options: &AddTokenOptions,
        console: &mut dyn Console,
    ) -> Result<()> {
        self.refuse_session_shell()?;
        let raw = match input {
            TokenInput::Value(v) if !v.is_empty() => v,
            TokenInput::Stdin => console.prompt("").unwrap_or_default(),
            TokenInput::Value(_) | TokenInput::Prompt => {
                console.secret("Token: ").unwrap_or_default()
            }
        };
        let token = raw.trim();
        if token.is_empty() {
            return Err(validation("Token cannot be empty"));
        }
        let is_api_key = looks_like_api_key(token);
        let email = options.email.clone().filter(|e| !e.is_empty());
        if let Some(email) = &email
            && !identifier::is_valid_email(email)
        {
            return Err(validation(format!("Invalid email format: {email}")));
        }

        store::setup_directories(self.paths)?;
        store::init_sequence(self.paths)?;
        store::migrate_org_fields(self.paths)?;

        let mut slot = options.slot;
        let email = match email {
            Some(email) => email,
            None => {
                let n = match slot {
                    Some(n) => n,
                    None => store::next_account_number(Some(&self.roster()?)) as i64,
                };
                slot = Some(n);
                let label = if is_api_key { "api-key" } else { "setup-token" };
                format!("{label}-{n}@token.local")
            }
        };
        let data = self.roster()?;
        reject_cross_kind_collision(&data, &email, is_api_key)?;

        let credentials = if is_api_key {
            token.to_string()
        } else {
            pyjson::to_string_compact(&json!({
                "claudeAiOauth": {"accessToken": token, "scopes": SETUP_TOKEN_SCOPES}
            }))
        };
        let config = pyjson::to_string_compact(&json!({
            "oauthAccount": {
                "emailAddress": email,
                "accountUuid": "",
                "organizationUuid": null,
                "organizationName": null,
            }
        }));
        let kind_label = if is_api_key { "API key" } else { "token" };
        let style = self.style;

        if slot.is_none()
            && let Some(num) = store::find_account_slot(&data, &email, "")
        {
            let _lock = self.store_lock()?;
            self.write_backup(&num, &email, &credentials)?;
            store::write_account_config(self.paths, &num, &email, &config)?;
            self.clear_dead_token(&num, &email, "")?;
            let mut data = self.roster()?;
            data.insert("lastUpdated".into(), store::timestamp_now().into());
            store::write_sequence(self.paths, &data)?;
            crate::log::info(format!("Updated {kind_label} for account {num}: {email}"));
            console.line(&format!(
                "{} for Account {num} ({email} {}).",
                style.accent(&format!("Updated {kind_label}")),
                style.muted("[personal]")
            ));
            return Ok(());
        }

        let mut displace = None;
        let mut migrate_from = None;
        let num = match slot {
            Some(slot) => {
                if slot < 1 {
                    return Err(Error::config("Slot number must be >= 1"));
                }
                let num = slot.to_string();
                migrate_from = store::find_account_slot(&data, &email, "").filter(|o| *o != num);
                if let Some(existing) = store::accounts(&data).and_then(|a| a.get(&num)) {
                    let text = |key: &str, default: &str| {
                        existing
                            .get(key)
                            .and_then(Value::as_str)
                            .unwrap_or(default)
                            .to_string()
                    };
                    let existing_email = text("email", "unknown");
                    let existing_org = text("organizationUuid", "");
                    if !(existing_email == email && existing_org.is_empty()) {
                        console.line(&style.yellowed(&format!("Slot {slot} already occupied")));
                        console.line(&format!(
                            "{existing_email} {}",
                            style.muted(&format!(
                                "[{}]",
                                display_tag(&text("organizationName", ""))
                            ))
                        ));
                        if !options.assume_yes {
                            match console.prompt(&format!("Overwrite slot {slot}? [y/N] ")) {
                                None => {
                                    console.line(&format!("\n{}", style.dimmed("Cancelled")));
                                    return Ok(());
                                }
                                Some(answer)
                                    if matches!(
                                        answer.trim().to_lowercase().as_str(),
                                        "y" | "yes"
                                    ) => {}
                                Some(_) => {
                                    console.line(&style.dimmed("Cancelled"));
                                    return Ok(());
                                }
                            }
                        }
                        displace = Some((num.clone(), existing_email, existing_org));
                    }
                }
                num
            }
            None => store::next_account_number(Some(&data)).to_string(),
        };

        let _lock = self.store_lock()?;
        if let Some((d_num, d_email, d_org)) = &displace {
            self.delete_account_files(d_num, d_email)?;
            self.drop_record(d_num)?;
            self.prune_mappings(d_email, d_org, console);
        }
        if let Some(old) = &migrate_from {
            let old_email = record_text(&self.roster()?, old, "email");
            self.delete_account_files(old, &old_email)?;
            self.drop_record(old)?;
        }

        self.write_backup(&num, &email, &credentials)?;
        store::write_account_config(self.paths, &num, &email, &config)?;
        self.clear_dead_token(&num, &email, "")?;

        let mut data = self.roster()?;
        let mut record = json!({
            "email": email,
            "uuid": "",
            "organizationUuid": "",
            "organizationName": "",
            "added": store::timestamp_now(),
        });
        if is_api_key {
            record["kind"] = "api_key".into();
        }
        accounts_mut(&mut data).insert(num.clone(), record);
        let num_int: i64 = num.parse().unwrap_or_default();
        let sequence = sequence_mut(&mut data);
        if !sequence.iter().any(|v| v.as_i64() == Some(num_int)) {
            sequence.push(num_int.into());
            sequence.sort_by_key(|v| v.as_i64().unwrap_or_default());
        }
        data.insert("lastUpdated".into(), store::timestamp_now().into());
        store::write_sequence(self.paths, &data)?;
        crate::log::info(format!("Added account {num} from {kind_label}: {email}"));

        if let Some(old) = &migrate_from {
            console.line(&style.dimmed(&format!(
                "Moved from slot {old} → {}",
                slot.unwrap_or_default()
            )));
        }
        console.line(&format!(
            "{} Account {num}: {email} {} {}",
            style.accent("Added"),
            style.muted("[personal]"),
            style.muted(&format!("(from {kind_label})"))
        ));
        Ok(())
    }

    fn store_lock(&self) -> Result<StoreLock> {
        StoreLock::acquire(&self.paths.store_lock_file(), STORE_LOCK_TIMEOUT)
    }
}

/// Python's `getpass.getpass`: read from the controlling terminal with echo
/// off, else from standard input with a warning. `None` on end of input.
pub fn getpass(prompt: &str) -> Option<String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open("/dev/tty");
    if let Ok(mut tty) = tty {
        let fd = tty.as_raw_fd();
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut saved) } == 0 {
            let mut quiet = saved;
            quiet.c_lflag &= !libc::ECHO;
            unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &quiet) };
            let _ = tty.write_all(prompt.as_bytes());
            let _ = tty.flush();
            let mut line = String::new();
            let read = BufReader::new(&tty).read_line(&mut line);
            unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &saved) };
            let _ = tty.write_all(b"\n");
            return match read {
                Ok(0) | Err(_) => None,
                Ok(_) => Some(line.trim_end_matches('\n').to_string()),
            };
        }
    }
    let mut err = std::io::stderr().lock();
    let _ = write!(err, "Warning: Password input may be echoed.\n{prompt}");
    let _ = err.flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim_end_matches('\n').to_string()),
    }
}

/// An account and a token account sharing `(email, "")` could not be told
/// apart at switch time.
fn reject_cross_kind_collision(data: &JsonObject, email: &str, is_api_key: bool) -> Result<()> {
    let Some(slot) = store::find_account_slot(data, email, "") else {
        return Ok(());
    };
    let existing_is_api_key = store::accounts(data)
        .and_then(|a| a.get(&slot))
        .and_then(|r| r.get("kind"))
        .and_then(Value::as_str)
        == Some("api_key");
    if existing_is_api_key == is_api_key {
        return Ok(());
    }
    let label = |api: bool| if api { "API-key" } else { "OAuth" };
    Err(validation(format!(
        "'{email}' already exists as an {} account (slot {slot}); cannot add it as an {} account. Pass a distinct --email.",
        label(existing_is_api_key),
        label(is_api_key)
    )))
}

fn validation(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::ValidationError, message)
}
