//! Cassis invoices as a payer should see them: amount, payee, expiry and
//! where it can be paid, read from the invoice itself and checked with
//! `cassis-core` (its shape, and the signatures its receiver requires).
//!
//! The fields come from the raw JSON first, so a truncated or otherwise
//! undecodable invoice (such as the spec's examples with `…`) still shows
//! what it has, along with why it does not decode.

use crate::style::{Style, clip};
use cassis_core::{Invoice, InvoiceReceiver, NetworkId};
use hypereffect::{Value, json, serde_json};
use std::fmt::Write;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Summary {
    /// The invoice as `cassis-core` decodes it, or why it does not.
    pub decoded: Result<Invoice, String>,
    pub amount_msat: Option<u64>,
    pub expires_at: Option<u64>,
    pub payment_hash: Option<String>,
    pub payee: Option<String>,
    pub gateway: Option<String>,
    pub preimage_server: Option<String>,
    pub peer: Option<String>,
    pub networks: Vec<Network>,
    pub description: Option<String>,
    /// The size of the metadata once base64-decoded, or of its text if it
    /// does not decode.
    pub metadata_bytes: Option<usize>,
    pub relay: Option<String>,
}

pub struct Network {
    pub id: String,
    /// The kind of key the payer locks to, such as `XOnlyPubKey`.
    pub kind: Option<String>,
    pub target: Option<String>,
    pub incoming_delta_secs: Option<u64>,
    /// Why the network id is not one Cassis knows.
    pub problem: Option<String>,
}

impl Summary {
    pub fn of(raw: &Value) -> Summary {
        let s = |k: &str| raw.get(k).and_then(Value::as_str).map(str::to_owned);
        let decoded = serde_json::from_value::<Invoice>(raw.clone()).map_err(|e| e.to_string());
        let networks = raw
            .get("networks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|n| {
                let id = n
                    .get("network")
                    .and_then(Value::as_str)
                    .unwrap_or("?")
                    .to_string();
                let target = n.get("target").and_then(Value::as_object);
                let (kind, target) = match target.and_then(|t| t.iter().next()) {
                    Some((k, v)) => (
                        Some(k.clone()),
                        Some(v.as_str().map_or_else(|| v.to_string(), str::to_owned)),
                    ),
                    None => (None, None),
                };
                Network {
                    problem: NetworkId::parse(&id).err(),
                    id,
                    kind,
                    target,
                    incoming_delta_secs: n.get("incoming_delta_secs").and_then(Value::as_u64),
                }
            })
            .collect();
        let metadata_bytes = s("metadata").map(|m| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(&m)
                .map_or(m.len(), |b| b.len())
        });
        Summary {
            amount_msat: raw.get("amount_msat").and_then(Value::as_u64),
            expires_at: raw.get("expires_at").and_then(Value::as_u64),
            payment_hash: s("payment_hash"),
            payee: s("payee"),
            gateway: s("gateway"),
            preimage_server: s("preimage_server"),
            peer: s("iroh_peer_id"),
            networks,
            description: s("description"),
            metadata_bytes,
            relay: s("iroh_relay"),
            decoded,
        }
    }

    /// `direct`, `gateway` or `delegated`, once decoded.
    pub fn kind(&self) -> Option<&'static str> {
        Some(match &self.decoded.as_ref().ok()?.receiver {
            InvoiceReceiver::Direct(_) => "direct",
            InvoiceReceiver::Gateway(_) => "gateway",
            InvoiceReceiver::Delegated(_) => "delegated",
        })
    }

    /// Whether its signatures check out, once decoded.
    pub fn verified(&self) -> Option<Result<(), String>> {
        self.decoded.as_ref().ok().map(Invoice::verify)
    }

    /// Whether it decodes and its signatures check out.
    pub fn is_valid(&self) -> bool {
        matches!(self.verified(), Some(Ok(())))
    }

    /// Seconds until it expires; negative once expired.
    pub fn expires_in(&self) -> Option<i64> {
        Some(self.expires_at? as i64 - now() as i64)
    }

    pub fn to_json(&self, memo: Option<&str>) -> Value {
        let networks: Vec<Value> = self
            .networks
            .iter()
            .map(|n| {
                json!({
                    "network": n.id,
                    "kind": n.kind,
                    "target": n.target,
                    "incoming_delta_secs": n.incoming_delta_secs,
                    "problem": n.problem,
                })
            })
            .collect();
        let (signature, problem) = match (&self.decoded, self.verified()) {
            (Err(e), _) => ("undecodable", Some(e.clone())),
            (_, Some(Err(e))) => ("invalid", Some(e)),
            _ => ("valid", None),
        };
        json!({
            "valid": self.is_valid(),
            "signature": signature,
            "problem": problem,
            "kind": self.kind(),
            "amount_msat": self.amount_msat,
            "amount_sat": self.amount_msat.map(|m| m as f64 / 1000.0),
            "expires_at": self.expires_at,
            "expires_in_secs": self.expires_in(),
            "expired": self.expires_in().map(|s| s <= 0),
            "payment_hash": self.payment_hash,
            "payee": self.payee,
            "gateway": self.gateway,
            "preimage_server": self.preimage_server,
            "iroh_peer_id": self.peer,
            "iroh_relay": self.relay,
            "networks": networks,
            "description": self.description,
            "metadata_bytes": self.metadata_bytes,
            "memo": memo,
        })
    }

    /// A block of `label  value` lines, each after `indent`.
    pub fn render(&self, memo: Option<&str>, indent: &str, st: &Style) -> String {
        let mut out = String::new();
        let mut row = |label: &str, value: String| {
            let _ = writeln!(out, "{indent}{}  {value}", st.dim(&format!("{label:<12}")));
        };
        let status = match (&self.decoded, self.verified()) {
            (Err(e), _) => st.red(&format!("✗ does not decode: {e}")),
            (_, Some(Err(e))) => st.red(&format!("✗ {e}")),
            _ => st.green("✓ signatures valid"),
        };
        row(
            "invoice",
            match self.kind() {
                Some(k) => format!("{k} · {status}"),
                None => status,
            },
        );
        row(
            "amount",
            self.amount_msat
                .map_or_else(|| st.red("missing"), |m| st.bold(&amount(m))),
        );
        row(
            "expires",
            match (self.expires_at, self.expires_in()) {
                (Some(at), Some(left)) if left > 0 => {
                    format!("{} (in {})", utc(at), duration(left as u64))
                }
                (Some(at), Some(left)) => st.red(&format!(
                    "{} (expired {} ago)",
                    utc(at),
                    duration(left.unsigned_abs())
                )),
                _ => st.red("missing"),
            },
        );
        if let Some(h) = &self.payment_hash {
            row("hash", h.clone());
        }
        if let Some(p) = &self.payee {
            row("payee", p.clone());
        }
        if let Some(g) = &self.gateway {
            row("gateway", g.clone());
        }
        if let Some(p) = &self.preimage_server {
            row("preimages", p.clone());
        }
        if let Some(r) = &self.relay {
            row("relay", r.clone());
        }
        if self.networks.is_empty() {
            row("networks", st.red("none"));
        }
        let width = self.networks.iter().map(|n| n.id.len()).max().unwrap_or(0);
        for (i, n) in self.networks.iter().enumerate() {
            let mut v = format!("{:<width$}", n.id);
            if let (Some(k), Some(t)) = (&n.kind, &n.target) {
                let _ = write!(v, "  {k} {}", clip(t, 70));
            }
            if let Some(d) = n.incoming_delta_secs {
                let _ = write!(v, "  {}", st.dim(&format!("delta {}", duration(d))));
            }
            if let Some(p) = &n.problem {
                let _ = write!(v, "  {}", st.yellow(&format!("({p})")));
            }
            row(if i == 0 { "networks" } else { "" }, v);
        }
        if let Some(d) = &self.description {
            row("description", quote(d));
        }
        if let Some(m) = memo {
            row(
                "memo",
                format!(
                    "{} {}",
                    quote(m),
                    st.dim("(from the COP message, unsigned: trust the invoice, not this)")
                ),
            );
        }
        if let Some(b) = self.metadata_bytes {
            row("metadata", format!("{b} bytes"));
        }
        out
    }
}

fn quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `10,750 sat (10,750,000 msat)`, with the fraction of a satoshi when
/// there is one.
pub fn amount(msat: u64) -> String {
    let sat = match msat % 1000 {
        0 => group(msat / 1000),
        r => format!("{}.{:03}", group(msat / 1000), r)
            .trim_end_matches('0')
            .to_string(),
    };
    format!("{sat} sat ({} msat)", group(msat))
}

fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `1h 2m 3s`, the two largest units only.
pub fn duration(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86400, secs / 3600 % 24, secs / 60 % 60, secs % 60);
    let parts: Vec<String> = [(d, "d"), (h, "h"), (m, "m"), (s, "s")]
        .into_iter()
        .skip_while(|(v, _)| *v == 0)
        .take(2)
        .filter(|(v, _)| *v > 0)
        .map(|(v, u)| format!("{v}{u}"))
        .collect();
    if parts.is_empty() {
        "0s".into()
    } else {
        parts.join(" ")
    }
}

/// A unix time as `2026-11-25 14:03:20 UTC`.
pub fn utc(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(amount(10_750_000), "10,750 sat (10,750,000 msat)");
        assert_eq!(amount(1_500), "1.5 sat (1,500 msat)");
        assert_eq!(duration(3725), "1h 2m");
        assert_eq!(duration(59), "59s");
        assert_eq!(duration(3600), "1h");
        assert_eq!(utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc(1793875200), "2026-11-05 10:40:00 UTC");
        assert_eq!(utc(951782400), "2000-02-29 00:00:00 UTC");
    }

    /// A direct invoice signed by the payee whose secret key is `[7; 32]`.
    pub fn signed() -> Value {
        let body: cassis_core::InvoiceBody = serde_json::from_value(json!({
            "payment_hash": "3f9a".repeat(16),
            "amount_msat": 10_750_000,
            "expires_at": now() + 900,
            "networks": [{
                "network": "cashu::mint.example.com",
                "target": {"XOnlyPubKey": "17162c921dc4d2518f9a101db33695df1afb56ab82f5ff3e5da6eec3ca5cd917"},
            }],
            "description": "Pepperoni + chili",
            "metadata": null,
            "iroh_relay": null,
        }))
        .unwrap();
        serde_json::to_value(Invoice::direct(body, &[7; 32])).unwrap()
    }

    #[test]
    fn verifies_signatures() {
        let good = signed();
        if let Ok(path) = std::env::var("COP_WRITE_FIXTURE") {
            std::fs::write(path, good.to_string()).unwrap();
        }
        let s = Summary::of(&good);
        assert!(s.is_valid(), "{:?}", s.verified());
        assert_eq!(s.kind(), Some("direct"));
        assert_eq!(s.networks[0].kind.as_deref(), Some("XOnlyPubKey"));
        assert!(s.expires_in().unwrap() > 0);

        let mut tampered = good.clone();
        tampered["amount_msat"] = json!(1);
        let s = Summary::of(&tampered);
        assert!(s.decoded.is_ok());
        assert!(matches!(s.verified(), Some(Err(_))));
        assert_eq!(s.to_json(None)["signature"], "invalid");
    }

    #[test]
    fn summarizes_what_does_not_decode() {
        let s = Summary::of(
            &json!({"payment_hash": "3f9a…e21c", "amount_msat": 10750000, "expires_at": 1793875200}),
        );
        assert!(s.decoded.is_err());
        assert!(!s.is_valid());
        assert_eq!(s.amount_msat, Some(10750000));
    }
}
