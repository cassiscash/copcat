//! `cop://` connection URIs and session tokens.
//!
//! ```text
//! cop://<endpoint-id>[?_relay=<relay-url>][#<key>=<value>&...]
//! ```
//!
//! Parsing follows the spec to the letter: the endpoint id is 64 hex
//! characters (lowercased), `_relay` is the only query parameter that
//! means anything, and the fragment is `&`-separated `key=value` pairs
//! where a missing `=`, an empty key, a repeated key or invalid UTF-8
//! makes the whole URI invalid. What the spec says to ignore is kept in
//! [`CopUri::ignored`], so `cop parse` can point it out.

use base64::Engine;
use rand::RngCore;
use std::fmt;

/// A parsed `cop://` URI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopUri {
    /// The server's iroh endpoint id: 64 lowercase hex characters.
    pub endpoint: String,
    /// The relay to reach the server through, instead of the default ones.
    pub relay: Option<String>,
    /// The fragment parameters, in the order they appear. Sent as `params`
    /// in the `hello`.
    pub params: Vec<(String, String)>,
    /// Query parameters other than `_relay`, which clients ignore.
    pub ignored: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UriError(pub String);

impl fmt::Display for UriError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn bad(message: impl Into<String>) -> UriError {
    UriError(message.into())
}

impl CopUri {
    pub fn parse(input: &str) -> Result<Self, UriError> {
        let input = input.trim();
        let (scheme, rest) = input
            .split_once("://")
            .ok_or_else(|| bad("not a cop:// URI: there is no scheme"))?;
        if !scheme.eq_ignore_ascii_case("cop") {
            return Err(bad(format!("not a cop:// URI: the scheme is {scheme:?}")));
        }
        let (rest, fragment) = match rest.split_once('#') {
            Some((rest, fragment)) => (rest, Some(fragment)),
            None => (rest, None),
        };
        let (endpoint, query) = match rest.split_once('?') {
            Some((endpoint, query)) => (endpoint, Some(query)),
            None => (rest, None),
        };
        if let Some((_, path)) = endpoint.split_once('/') {
            return Err(bad(format!(
                "a cop:// URI has no path, but this one has /{path}"
            )));
        }
        let endpoint = endpoint.to_ascii_lowercase();
        if endpoint.len() != 64 {
            return Err(bad(format!(
                "the endpoint id must be 64 hex characters, this one has {}",
                endpoint.len()
            )));
        }
        if let Some(c) = endpoint.chars().find(|c| !c.is_ascii_hexdigit()) {
            return Err(bad(format!(
                "the endpoint id must be hex, but it has {c:?}"
            )));
        }

        let mut relay = None;
        let mut ignored = Vec::new();
        for pair in query.into_iter().flat_map(|q| q.split('&')) {
            match pair.split_once('=') {
                Some(("_relay", value)) => {
                    let value = decode(value).map_err(|e| bad(format!("_relay: {e}")))?;
                    if relay.replace(value).is_some() {
                        return Err(bad("_relay is repeated"));
                    }
                }
                _ if pair.is_empty() => {}
                _ => ignored.push(pair.to_string()),
            }
        }

        let mut params: Vec<(String, String)> = Vec::new();
        if let Some(fragment) = fragment.filter(|f| !f.is_empty()) {
            for (i, pair) in fragment.split('&').enumerate() {
                let n = i + 1;
                let (key, value) = pair
                    .split_once('=')
                    .ok_or_else(|| bad(format!("parameter {n} ({pair:?}) has no '='")))?;
                if key.is_empty() {
                    return Err(bad(format!("parameter {n} ({pair:?}) has an empty name")));
                }
                let key = decode(key).map_err(|e| bad(format!("parameter {n} name: {e}")))?;
                let value = decode(value).map_err(|e| bad(format!("parameter {key:?}: {e}")))?;
                if params.iter().any(|(k, _)| *k == key) {
                    return Err(bad(format!("parameter {key:?} is repeated")));
                }
                params.push((key, value));
            }
        }
        Ok(Self {
            endpoint,
            relay,
            params,
            ignored,
        })
    }

    /// The display name the URI suggests, until the server sends its metadata.
    pub fn name(&self) -> Option<&str> {
        self.param("name")
    }

    pub fn param(&self, key: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Sets a fragment parameter, replacing the one with the same key.
    pub fn set_param(&mut self, key: String, value: String) {
        match self.params.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => *v = value,
            None => self.params.push((key, value)),
        }
    }

    /// Whether the relay is a Tor hidden service, which the client must
    /// reach through Tor.
    pub fn relay_is_onion(&self) -> bool {
        self.relay.as_deref().is_some_and(host_is_onion)
    }
}

impl fmt::Display for CopUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cop://{}", self.endpoint)?;
        if let Some(relay) = &self.relay {
            write!(f, "?_relay={}", encode(relay))?;
        }
        let mut sep = '#';
        for (key, value) in &self.params {
            write!(f, "{sep}{}={}", encode(key), encode(value))?;
            sep = '&';
        }
        Ok(())
    }
}

pub fn host_is_onion(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = rest.split(['/', ':', '?', '#']).next().unwrap_or("");
    host.to_ascii_lowercase().ends_with(".onion")
}

/// Whether `input` is a `cop://` URI rather than a web address.
pub fn is_cop(input: &str) -> bool {
    let input = input.trim();
    input.len() >= 6 && input[..6].eq_ignore_ascii_case("cop://")
}

fn decode(text: &str) -> Result<String, String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| format!("bad percent-encoding at byte {i}"))?;
            out.push(hex);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| "not UTF-8 once percent-decoded".to_string())
}

pub fn encode(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// A fresh `session` token: 128 random bits, as the spec requires, in
/// base64url.
pub fn new_session() -> String {
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// The most entropy `token` can carry, in bits, judging by its length and
/// the characters it uses: a hex token holds 4 bits a character, a
/// base64url one 6. Tokens under 128 bits break the spec.
pub fn entropy_bits(token: &str) -> f64 {
    let hex = token.chars().all(|c| c.is_ascii_hexdigit());
    let alphabet: f64 = if hex {
        16.0
    } else {
        let mut size = 0.0;
        if token.chars().any(|c| c.is_ascii_digit()) {
            size += 10.0;
        }
        if token.chars().any(|c| c.is_ascii_lowercase()) {
            size += 26.0;
        }
        if token.chars().any(|c| c.is_ascii_uppercase()) {
            size += 26.0;
        }
        if token.chars().any(|c| c == '-' || c == '_') {
            size += 2.0;
        }
        if token
            .chars()
            .any(|c| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
        {
            size += 32.0;
        }
        f64::max(size, 2.0)
    };
    token.chars().count() as f64 * alphabet.log2()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "5f1c9d0a27e4b6c38d2f71a09e4b5c6d8a3f2e1b0c9d8e7f6a5b4c3d2e1f0a9b";

    #[test]
    fn parses_and_prints() {
        let uri = CopUri::parse(&format!(
            "COP://{}?_relay=http%3A%2F%2Fr.example&x=1#table=12&name=Luigi%27s%20Pizza",
            ID.to_uppercase()
        ))
        .unwrap();
        assert_eq!(uri.endpoint, ID);
        assert_eq!(uri.relay.as_deref(), Some("http://r.example"));
        assert_eq!(uri.name(), Some("Luigi's Pizza"));
        assert_eq!(uri.params[0], ("table".into(), "12".into()));
        assert_eq!(uri.ignored, vec!["x=1".to_string()]);
        let again = CopUri::parse(&uri.to_string()).unwrap();
        assert_eq!(again.params, uri.params);
    }

    #[test]
    fn empty_values_are_fine() {
        let uri = CopUri::parse(&format!("cop://{ID}#a=&b=2")).unwrap();
        assert_eq!(uri.param("a"), Some(""));
    }

    #[test]
    fn rejects_invalid() {
        assert!(CopUri::parse("cop://abc").is_err());
        assert!(CopUri::parse(&format!("cop://{ID}/path")).is_err());
        assert!(CopUri::parse(&format!("cop://{ID}#a")).is_err());
        assert!(CopUri::parse(&format!("cop://{ID}#a=1&a=2")).is_err());
        assert!(CopUri::parse(&format!("cop://{ID}#=1")).is_err());
        assert!(CopUri::parse(&format!("cop://{ID}#a=%ff")).is_err());
        assert!(CopUri::parse(&format!("http://{ID}")).is_err());
    }

    #[test]
    fn onion_relays() {
        let uri = CopUri::parse(&format!(
            "cop://{ID}?_relay=http%3A%2F%2Fabc.onion%3A80#till=3"
        ))
        .unwrap();
        assert!(uri.relay_is_onion());
    }

    #[test]
    fn sessions_have_enough_entropy() {
        let (a, b) = (new_session(), new_session());
        assert_ne!(a, b);
        assert!(entropy_bits(&a) >= 128.0);
        assert!(entropy_bits("f1K9xQ2v8Tn4LmZp7Rb3wq") >= 128.0);
        assert!(entropy_bits("abc123") < 128.0);
        assert!(entropy_bits(&"a".repeat(32)) >= 128.0);
    }
}
