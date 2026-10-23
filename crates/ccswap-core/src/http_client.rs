//! The HTTP agent every usage and token request goes through, and the parts of a
//! failed request that are recorded the same way whichever server answered.

use std::time::Duration;

/// Verifies TLS against the macOS trust store, as upstream's `truststore`
/// does, so locally trusted roots apply.
pub fn agent(timeout: Duration) -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .tls_config(tls)
        .build()
        .into()
}

/// The error kind a request that got no response is recorded under:
/// `timeout` or `network`.
pub fn classify_transport(e: &ureq::Error) -> &'static str {
    let timed_out = match e {
        ureq::Error::Timeout(_) => true,
        ureq::Error::Io(io) => io.kind() == std::io::ErrorKind::TimedOut,
        _ => false,
    };
    if timed_out { "timeout" } else { "network" }
}

/// Python `max(0.0, float(raw.strip()))`; the HTTP-date form is ignored.
pub fn parse_retry_after(raw: &str) -> Option<f64> {
    let t = raw.trim().to_ascii_lowercase();
    let value: f64 = match t.as_str() {
        "inf" | "+inf" | "infinity" | "+infinity" => f64::INFINITY,
        "-inf" | "-infinity" => f64::NEG_INFINITY,
        _ => t.parse().ok()?,
    };
    Some(if value.is_nan() {
        value
    } else {
        value.max(0.0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_takes_only_the_seconds_form() {
        assert_eq!(parse_retry_after(" 120 "), Some(120.0));
        assert_eq!(parse_retry_after("1e3"), Some(1000.0));
        assert_eq!(parse_retry_after("-5"), Some(0.0));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
    }
}
