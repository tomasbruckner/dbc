//! Connection-string construction for the Microsoft "ODBC Driver 18 for SQL
//! Server" ODBC driver.
//!
//! Driver 18 flips the pre-17 default and validates the server certificate
//! by default (`Encrypt=yes`), so both `encrypt` and
//! `trust_server_certificate` are first-class, explicit fields here rather
//! than being left to driver defaults — see the module-level integration
//! notes in `lib.rs` for why these need to surface as connect-dialog options
//! upstream.

/// Builder for an ODBC "Driver 18 for SQL Server" connection string.
///
/// Renders a `Key=Value;...` string per the ODBC keyword/value syntax. Values
/// are escaped with [`escape_odbc_value`] wherever they might contain
/// characters significant to that syntax — see [`BRACE_TRIGGER_CHARS`] —
/// or leading/trailing whitespace, so a password or database name
/// containing those characters round-trips correctly instead of truncating
/// the string or corrupting a later key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MssqlConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
    /// `Encrypt=yes|no`. Driver 18 defaults this to `yes` server-side, but we
    /// always render it explicitly so the rendered string is self-describing
    /// regardless of driver version.
    pub encrypt: bool,
    /// `TrustServerCertificate=yes|no`. Only meaningful when `encrypt` is
    /// true; still rendered either way for explicitness.
    pub trust_server_certificate: bool,
    /// `Connection Timeout=<seconds>`, omitted from the rendered string when
    /// `None` (driver default applies).
    pub connect_timeout_sec: Option<u32>,
    /// ODBC driver name, e.g. `"ODBC Driver 18 for SQL Server"`. Exposed so
    /// callers can target a differently-named installed driver (e.g. 17)
    /// without forking this config type.
    pub driver: String,
}

impl MssqlConfig {
    /// `encrypt` defaults to `true` and `trust_server_certificate` to
    /// `false` — the secure-by-default Driver 18 posture. Callers that need
    /// to talk to a server with a self-signed/dev certificate must opt into
    /// `trust_server_certificate(true)` explicitly.
    pub fn new(
        host: impl Into<String>,
        port: u16,
        database: impl Into<String>,
        user: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            database: database.into(),
            user: user.into(),
            password: password.into(),
            encrypt: true,
            trust_server_certificate: false,
            connect_timeout_sec: None,
            driver: "ODBC Driver 18 for SQL Server".to_string(),
        }
    }

    pub fn encrypt(mut self, encrypt: bool) -> Self {
        self.encrypt = encrypt;
        self
    }

    pub fn trust_server_certificate(mut self, trust: bool) -> Self {
        self.trust_server_certificate = trust;
        self
    }

    pub fn connect_timeout_sec(mut self, secs: u32) -> Self {
        self.connect_timeout_sec = Some(secs);
        self
    }

    pub fn driver(mut self, driver: impl Into<String>) -> Self {
        self.driver = driver.into();
        self
    }

    /// Renders the ODBC connection string used by
    /// `Environment::connect_with_connection_string`.
    pub fn to_connection_string(&self) -> String {
        let mut out = String::new();
        push_kv(&mut out, "Driver", &self.driver);
        push_kv(&mut out, "Server", &format!("tcp:{},{}", self.host, self.port));
        push_kv(&mut out, "Database", &self.database);
        push_kv(&mut out, "Uid", &self.user);
        push_kv(&mut out, "Pwd", &self.password);
        push_kv(&mut out, "Encrypt", if self.encrypt { "yes" } else { "no" });
        push_kv(
            &mut out,
            "TrustServerCertificate",
            if self.trust_server_certificate { "yes" } else { "no" },
        );
        if let Some(t) = self.connect_timeout_sec {
            push_kv(&mut out, "Connection Timeout", &t.to_string());
        }
        out
    }
}

fn push_kv(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push('=');
    out.push_str(&escape_odbc_value(value));
    out.push(';');
}

/// Characters Microsoft's "Connection String Keywords and Data Source
/// Names" docs list as significant to the ODBC keyword/value syntax (or to
/// DSN-file parsing, which shares the same escaping rules) and that
/// therefore force a value to be brace-wrapped: `[ ] { } ( ) , ; ? * = ! @`.
/// This is deliberately wider than the minimum the driver manager's own
/// parser strictly requires (`;` and a leading `{` are the only characters
/// that actually change how the *keyword/value* grammar tokenizes) —
/// this is the password path, so defensive over-escaping (wrapping a value
/// that didn't strictly need it) is the safe direction to err in; it is
/// never wrong to wrap a value in `{...}` that didn't need it, only to fail
/// to wrap one that did.
const BRACE_TRIGGER_CHARS: [char; 13] =
    ['[', ']', '{', '}', '(', ')', ',', ';', '?', '*', '=', '!', '@'];

/// Escapes a single ODBC connection-string value per the keyword/value
/// syntax (see "Connection String Keywords and Data Source Names" in the
/// Microsoft ODBC docs): a value containing any of [`BRACE_TRIGGER_CHARS`],
/// leading/trailing whitespace, or that is empty must be wrapped in
/// `{...}`; any `}` inside the braces is doubled (`}}`) since the driver
/// manager treats a single `}` as the closing brace. Values needing no
/// escaping are passed through unchanged so the common case stays
/// readable.
pub fn escape_odbc_value(value: &str) -> String {
    let needs_braces = value.is_empty()
        || value.starts_with(' ')
        || value.ends_with(' ')
        || value.chars().any(|c| BRACE_TRIGGER_CHARS.contains(&c));
    if needs_braces {
        format!("{{{}}}", value.replace('}', "}}"))
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_default_connection_string() {
        let cfg = MssqlConfig::new("dbhost", 1433, "mydb", "sa", "pw");
        let s = cfg.to_connection_string();
        // `Server`'s value always contains a `,` (host,port) — which, per
        // the widened `BRACE_TRIGGER_CHARS` set (finding: widen brace
        // triggers to Microsoft's documented list, which includes `,`),
        // now brace-wraps every rendered connection string's `Server` key.
        // This is still valid, equivalent ODBC syntax; it's just no longer
        // the bare `tcp:host,port` form.
        assert_eq!(
            s,
            "Driver=ODBC Driver 18 for SQL Server;Server={tcp:dbhost,1433};Database=mydb;\
             Uid=sa;Pwd=pw;Encrypt=yes;TrustServerCertificate=no;"
        );
    }

    #[test]
    fn trust_server_certificate_and_encrypt_toggle() {
        let cfg = MssqlConfig::new("h", 1433, "db", "u", "p")
            .encrypt(false)
            .trust_server_certificate(true);
        let s = cfg.to_connection_string();
        assert!(s.contains("Encrypt=no;"));
        assert!(s.contains("TrustServerCertificate=yes;"));
    }

    #[test]
    fn connect_timeout_is_omitted_when_unset_and_present_when_set() {
        let cfg = MssqlConfig::new("h", 1433, "db", "u", "p");
        assert!(!cfg.to_connection_string().contains("Connection Timeout"));
        let cfg = cfg.connect_timeout_sec(30);
        assert!(cfg.to_connection_string().contains("Connection Timeout=30;"));
    }

    #[test]
    fn custom_driver_name_is_used() {
        let cfg = MssqlConfig::new("h", 1433, "db", "u", "p").driver("ODBC Driver 17 for SQL Server");
        assert!(cfg.to_connection_string().starts_with("Driver=ODBC Driver 17 for SQL Server;"));
    }

    #[test]
    fn escape_passthrough_for_plain_values() {
        assert_eq!(escape_odbc_value("plainvalue"), "plainvalue");
        assert_eq!(escape_odbc_value("sa"), "sa");
    }

    #[test]
    fn escape_wraps_values_with_semicolon() {
        assert_eq!(escape_odbc_value("a;b"), "{a;b}");
    }

    #[test]
    fn escape_wraps_values_starting_with_brace() {
        assert_eq!(escape_odbc_value("{weird"), "{{weird}");
    }

    #[test]
    fn escape_doubles_closing_braces_inside_wrapped_value() {
        // `}` alone is now a trigger character (finding: widen the trigger
        // set), so a lone `}` gets wrapped and doubled.
        assert_eq!(escape_odbc_value("a}b"), "{a}}b}");
        assert_eq!(escape_odbc_value("a;b}c"), "{a;b}}c}");
    }

    #[test]
    fn escape_wraps_values_containing_documented_trigger_characters() {
        for c in ['[', ']', '(', ')', ',', '?', '*', '=', '!', '@'] {
            let value = format!("a{c}b");
            let escaped = escape_odbc_value(&value);
            assert_eq!(escaped, format!("{{a{c}b}}"), "expected {c:?} to trigger brace-wrapping");
        }
    }

    #[test]
    fn escape_wraps_leading_and_trailing_whitespace() {
        assert_eq!(escape_odbc_value(" leading"), "{ leading}");
        assert_eq!(escape_odbc_value("trailing "), "{trailing }");
    }

    #[test]
    fn escape_wraps_empty_value() {
        assert_eq!(escape_odbc_value(""), "{}");
    }

    #[test]
    fn password_with_semicolon_round_trips_through_connection_string() {
        let cfg = MssqlConfig::new("h", 1433, "db", "u", "p;w{ird}pw");
        let s = cfg.to_connection_string();
        assert!(s.contains("Pwd={p;w{ird}}pw};"));
    }
}
