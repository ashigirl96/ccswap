//! ChatGPT inference credentials, separate from Codex CLI's login (ADR 0005).

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ccswap_core::endpoints::Endpoints;
use ccswap_core::error::{Error, Result};
use ccswap_core::fsutil;
use ccswap_core::http_client;
use ccswap_core::locks::{STORE_LOCK_TIMEOUT, StoreLock};
use ccswap_core::paths::Paths;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const RESOURCE: &str = "https://api.openai.com/v1";
const ISSUER: &str = "https://auth.openai.com";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";

#[derive(Clone)]
pub struct InferenceLogins {
    root: PathBuf,
}

impl InferenceLogins {
    pub fn new(paths: &Paths) -> Self {
        Self {
            root: paths.backup_root(),
        }
    }

    pub fn directory(&self) -> PathBuf {
        self.root.join("openai")
    }

    pub fn login_path(&self, account_id: &str) -> PathBuf {
        // Encode the ID so even an imported ledger cannot escape the store.
        self.directory()
            .join("logins")
            .join(format!("{}.json", encode(account_id)))
    }

    fn registration_path(&self, account_id: &str) -> PathBuf {
        self.directory()
            .join("registrations")
            .join(format!("{}.json", encode(account_id)))
    }

    pub fn read(&self, account_id: &str) -> Result<Option<Value>> {
        read_json(&self.login_path(account_id))
    }

    /// Caller holds the store lock while replacing a rotating token set.
    pub fn write(&self, account_id: &str, credential: &Value) -> Result<()> {
        self.write_file(&self.login_path(account_id), &credential.to_string())
    }

    pub fn logout(&self, account_id: &str) -> Result<()> {
        let _lock = self.lock()?;
        let Some(mut credential) = self.read(account_id)? else {
            return Ok(());
        };
        let object = credential
            .as_object_mut()
            .ok_or_else(|| Error::config("Invalid inference login"))?;
        for key in [
            "access_token",
            "refresh_token",
            "id_token",
            "expires_at",
            "scope",
        ] {
            object.remove(key);
        }
        // Keep the issued client ID and verified identity for reauthorization.
        self.write_file(&self.registration_path(account_id), &credential.to_string())?;
        std::fs::remove_file(self.login_path(account_id)).map_err(io_error)
    }

    pub fn lock(&self) -> Result<StoreLock> {
        StoreLock::acquire(&self.root.join(".lock"), STORE_LOCK_TIMEOUT)
    }

    fn write_file(&self, path: &Path, contents: &str) -> Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| Error::config("Invalid OpenAI store path"))?;
        for dir in [&self.root, &self.directory(), dir] {
            fsutil::ensure_private_dir(dir).map_err(io_error)?;
        }
        fsutil::atomic_write_private(dir, path, contents.as_bytes()).map_err(io_error)
    }

    fn host_id(&self) -> Result<String> {
        let _lock = self.lock()?;
        let path = self.directory().join("host.json");
        if let Some(value) = read_json(&path)? {
            return required(&value, "ext_agent_host_id").map(str::to_string);
        }
        let mut bytes = [0u8; 16];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
            .map_err(|_| Error::config("Could not generate OpenAI host ID"))?;
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let h: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let id = format!(
            "urn:uuid:{}-{}-{}-{}-{}",
            &h[..8],
            &h[8..12],
            &h[12..16],
            &h[16..20],
            &h[20..]
        );
        self.write_file(&path, &json!({"ext_agent_host_id":id}).to_string())?;
        Ok(id)
    }

    /// The browser callback and token exchange complete before any login is replaced.
    pub fn login(&self, account_id: &str, email: &str, endpoints: &Endpoints) -> Result<()> {
        let saved = self
            .read(account_id)?
            .or(read_json(&self.registration_path(account_id))?);
        let client = saved
            .as_ref()
            .and_then(|v| v["client_id"].as_str())
            .unwrap_or("dynamic_agent_client");
        let host = self.host_id()?;
        let listener = TcpListener::bind("127.0.0.1:0").map_err(io_error)?;
        listener.set_nonblocking(true).map_err(io_error)?;
        let redirect = format!(
            "http://127.0.0.1:{}/auth/callback",
            listener.local_addr().map_err(io_error)?.port()
        );
        let state = random()?;
        let nonce = random()?;
        let verifier = random()?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut params = vec![
            ("client_id", client),
            ("ext_agent_host_id", &host),
            ("response_type", "code"),
            ("redirect_uri", &redirect),
            ("scope", SCOPES),
            ("resource", RESOURCE),
            ("state", &state),
            ("nonce", &nonce),
            ("code_challenge_method", "S256"),
            ("code_challenge", &challenge),
        ];
        if client == "dynamic_agent_client" {
            params.push(("agent_name_hint", "ccswap"));
        } else if !email.is_empty() {
            params.push(("login_hint", email));
        }
        // Do not print id_token_hint: authorization URLs are visible in terminals.
        let url = format!("{}?{}", endpoints.openai_authorize_url, form(&params));
        println!("Continue with ChatGPT:\n{url}");
        std::io::stdout().flush().map_err(io_error)?;
        if endpoints.openai_authorize_url.starts_with("https://") {
            let _ = std::process::Command::new("/usr/bin/open")
                .arg(&url)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
        let callback = callback(&listener, &state)?;
        let issued = callback
            .get("client_id")
            .map(String::as_str)
            .unwrap_or(client);
        if issued.is_empty()
            || issued == "dynamic_agent_client"
            || (client != "dynamic_agent_client" && issued != client)
        {
            return Err(Error::config(
                "OpenAI callback returned an invalid client_id",
            ));
        }
        let code = callback
            .get("code")
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::config("OpenAI callback has no authorization code"))?;
        if client == "dynamic_agent_client" {
            let _lock = self.lock()?;
            self.write_file(
                &self.registration_path(account_id),
                &json!({"client_id": issued, "ext_agent_host_id": host}).to_string(),
            )?;
        }
        let mut credential = token_request(
            endpoints,
            &[
                ("grant_type", "authorization_code"),
                ("client_id", issued),
                ("code", code),
                ("code_verifier", &verifier),
                ("redirect_uri", &redirect),
                ("resource", RESOURCE),
            ],
        )?;
        let identity = verify_id_token(
            required(&credential, "id_token")?,
            issued,
            &nonce,
            endpoints,
        )?;
        if let Some(actual) = identity.get("email").and_then(Value::as_str)
            && !actual.eq_ignore_ascii_case(email)
        {
            if saved.as_ref().is_none_or(|v| v["subject"].is_null()) {
                let _lock = self.lock()?;
                std::fs::remove_file(self.registration_path(account_id)).map_err(io_error)?;
            }
            return Err(Error::config(
                "The ChatGPT email does not match the selected Codex account",
            ));
        }
        if let Some(previous) = &saved
            && previous["subject"]
                .as_str()
                .is_some_and(|sub| identity["sub"].as_str() != Some(sub))
        {
            return Err(Error::config(
                "The ChatGPT identity does not match the saved inference login",
            ));
        }
        required(&credential, "access_token")?;
        required(&credential, "refresh_token")?;
        if !required(&credential, "token_type")?.eq_ignore_ascii_case("Bearer") {
            return Err(Error::config("OpenAI returned an unsupported token type"));
        }
        if !required(&credential, "scope")?
            .split_whitespace()
            .any(|s| s == "chatgpt.tokens.use.direct")
        {
            return Err(Error::config(
                "ChatGPT plan usage was not granted; authorize it and sign in again",
            ));
        }
        let lifetime = credential["expires_in"]
            .as_i64()
            .filter(|v| *v > 0 && *v <= 86400)
            .ok_or_else(|| Error::config("OpenAI returned an invalid token expiry"))?;
        credential["expires_at"] = (chrono::Utc::now().timestamp() + lifetime).into();
        credential["client_id"] = issued.into();
        credential["ext_agent_host_id"] = host.into();
        credential["subject"] = identity["sub"].clone();
        credential["email"] = identity["email"].clone();
        credential["issuer"] = ISSUER.into();
        let _lock = self.lock()?;
        self.write(account_id, &credential)
    }
}

fn read_json(path: &Path) -> Result<Option<Value>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| Error::config(format!("Invalid JSON in {}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error(e)),
    }
}

fn io_error(error: std::io::Error) -> Error {
    Error::config(format!("OpenAI login: {error}"))
}

fn required<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::config(format!("OpenAI response is missing {name}")))
}

fn random() -> Result<String> {
    let mut bytes = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
        .map_err(|_| Error::config("Could not generate OAuth randomness"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn form(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn decode(value: &str) -> Result<String> {
    let mut bytes = value.bytes();
    let mut out = Vec::new();
    while let Some(b) = bytes.next() {
        out.push(match b {
            b'+' => b' ',
            b'%' => {
                let hex: Vec<_> = bytes.by_ref().take(2).collect();
                std::str::from_utf8(&hex)
                    .ok()
                    .filter(|s| s.len() == 2)
                    .and_then(|s| u8::from_str_radix(s, 16).ok())
                    .ok_or_else(|| Error::config("Invalid OAuth callback encoding"))?
            }
            _ => b,
        });
    }
    String::from_utf8(out).map_err(|_| Error::config("Invalid OAuth callback encoding"))
}

fn callback(listener: &TcpListener, state: &str) -> Result<BTreeMap<String, String>> {
    let deadline = Instant::now() + Duration::from_secs(300);
    while Instant::now() < deadline {
        let (mut stream, _) = match listener.accept() {
            Ok(pair) => pair,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            Err(e) => return Err(io_error(e)),
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(io_error)?;
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(io_error)?;
        let parsed = parse_callback(&stream, state);
        let (status, body) = if parsed.is_ok() {
            (200, "Return to ccswap to finish sign-in.")
        } else {
            (400, "Invalid sign-in callback.")
        };
        let _ = write!(
            stream,
            "HTTP/1.1 {status} Callback\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        match parsed {
            Ok(fields) => {
                if let Some(error) = fields.get("error") {
                    return Err(Error::config(format!(
                        "ChatGPT sign-in was declined ({error})"
                    )));
                }
                return Ok(fields);
            }
            Err(_) => continue,
        }
    }
    Err(Error::config("Timed out waiting for ChatGPT sign-in"))
}

fn parse_callback(stream: &TcpStream, state: &str) -> Result<BTreeMap<String, String>> {
    let mut line = String::new();
    BufReader::new(stream.take(16384))
        .read_line(&mut line)
        .map_err(io_error)?;
    let parts: Vec<_> = line.split_whitespace().collect();
    let query = parts
        .get(1)
        .and_then(|target| target.strip_prefix("/auth/callback?"));
    if parts.first() != Some(&"GET") || query.is_none() {
        return Err(Error::config("Invalid OAuth callback path"));
    }
    let mut fields = BTreeMap::new();
    for item in query.unwrap().split('&') {
        let (key, value) = item.split_once('=').unwrap_or((item, ""));
        if fields.insert(decode(key)?, decode(value)?).is_some() {
            return Err(Error::config("Duplicate OAuth callback parameter"));
        }
    }
    if fields.get("state").map(String::as_str) != Some(state) {
        return Err(Error::config("OAuth state mismatch"));
    }
    Ok(fields)
}

pub fn token_request(endpoints: &Endpoints, params: &[(&str, &str)]) -> Result<Value> {
    let mut response = http_client::agent(Duration::from_secs(30))
        .post(&endpoints.openai_token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(form(params).as_bytes())
        .map_err(|_| Error::config("Could not contact the OpenAI token endpoint"))?;
    if response.status().as_u16() != 200 {
        return Err(Error::config(format!(
            "OpenAI token exchange failed (HTTP {})",
            response.status().as_u16()
        )));
    }
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|_| Error::config("Could not read the OpenAI token response"))?;
    serde_json::from_str(&body)
        .map_err(|_| Error::config("OpenAI returned an invalid token response"))
}

fn verify_id_token(token: &str, client: &str, nonce: &str, endpoints: &Endpoints) -> Result<Value> {
    let invalid = || Error::config("OpenAI ID token validation failed");
    let parts: Vec<_> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(invalid());
    }
    let header: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).map_err(|_| invalid())?)
            .map_err(|_| invalid())?;
    if header["alg"] != "RS256" {
        return Err(invalid());
    }
    let kid = required(&header, "kid")?;
    let mut response = http_client::agent(Duration::from_secs(30))
        .get(&endpoints.openai_jwks_url)
        .call()
        .map_err(|_| Error::config("Could not load OpenAI signing keys"))?;
    if response.status().as_u16() != 200 {
        return Err(Error::config("Could not load OpenAI signing keys"));
    }
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|_| invalid())?;
    let jwks: Value = serde_json::from_str(&text).map_err(|_| invalid())?;
    let key = jwks["keys"]
        .as_array()
        .and_then(|keys| {
            keys.iter().find(|k| {
                k["kid"] == kid
                    && k["kty"] == "RSA"
                    && k["use"].as_str().is_none_or(|u| u == "sig")
                    && k["alg"].as_str().is_none_or(|a| a == "RS256")
            })
        })
        .ok_or_else(invalid)?;
    let n = URL_SAFE_NO_PAD
        .decode(required(key, "n")?)
        .map_err(|_| invalid())?;
    let e = URL_SAFE_NO_PAD
        .decode(required(key, "e")?)
        .map_err(|_| invalid())?;
    let signature = URL_SAFE_NO_PAD.decode(parts[2]).map_err(|_| invalid())?;
    ring::signature::RsaPublicKeyComponents { n: &n, e: &e }
        .verify(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &signature,
        )
        .map_err(|_| invalid())?;
    let claims: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).map_err(|_| invalid())?)
            .map_err(|_| invalid())?;
    let audience = claims["aud"].as_str() == Some(client)
        || claims["aud"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(client)));
    if claims["iss"] != ISSUER
        || !audience
        || claims["nonce"] != nonce
        || claims["exp"]
            .as_i64()
            .is_none_or(|exp| exp <= chrono::Utc::now().timestamp())
        || claims.get("nbf").is_some_and(|v| {
            v.as_i64()
                .is_none_or(|nbf| nbf > chrono::Utc::now().timestamp())
        })
    {
        return Err(invalid());
    }
    required(&claims, "sub")?;
    Ok(claims)
}
