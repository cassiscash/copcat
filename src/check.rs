//! `copcat check`: whether a transcript follows the spec, message by message.
//!
//! Errors are what the spec forbids or a peer must reject; warnings are
//! what peers ignore or what is likely a mistake; hints are advice from the
//! spec's prose (such as keeping prices out of action payloads).

use crate::invoice::Summary;
use crate::page::Page;
use crate::transcript::{Dir, Entry, Step};
use crate::uri::entropy_bits;
use cassis_core::NetworkId;
use hypereffect::message::parse_line;
use hypereffect::{Map, Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Hint,
    Warning,
    Error,
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Level::Hint => "hint",
            Level::Warning => "warning",
            Level::Error => "error",
        }
    }
}

pub struct Finding {
    pub line: usize,
    pub level: Level,
    pub message: String,
}

impl Finding {
    pub fn to_json(&self) -> Value {
        json!({ "line": self.line, "level": self.level.name(), "message": self.message })
    }
}

/// An `invoicerequest` waiting for the client's invoice.
struct Asked {
    line: usize,
    amount_sat: u64,
    networks: Vec<String>,
    memo: Option<String>,
}

#[derive(Default)]
struct Checker {
    out: Vec<Finding>,
    hello: Option<usize>,
    force_resume: bool,
    /// Server messages since the hello, other than `close`.
    served: usize,
    closed: Option<usize>,
    asked: Vec<Asked>,
    page: Page,
    /// A script for `copcat serve`, which has no hello: the server's side only.
    script: bool,
    /// Whether a message before the hello was reported already: once is
    /// enough.
    early: bool,
}

pub fn check(entries: &[Entry]) -> Vec<Finding> {
    let mut c = Checker {
        script: !entries.iter().any(|e| {
            matches!(&e.step, Step::Message { dir: Dir::Client, line }
                if crate::transcript::tag(line) == Some("hello"))
        }),
        ..Checker::default()
    };
    if c.script {
        c.at(
            1,
            Level::Hint,
            "no client hello: checking this as a script for `copcat serve`, which waits for the \
             hello before sending anything",
        );
    }
    for e in entries {
        let Step::Message { dir, line } = &e.step else {
            continue;
        };
        c.message(e.no, *dir, line);
    }
    c.out.sort_by_key(|f| f.line);
    c.out
}

impl Checker {
    fn at(&mut self, line: usize, level: Level, message: impl Into<String>) {
        self.out.push(Finding {
            line,
            level,
            message: message.into(),
        });
    }

    fn message(&mut self, no: usize, dir: Dir, line: &str) {
        let (tag, body) = match parse_line(line) {
            Ok((t, b)) => (t.to_string(), b),
            Err(e) => {
                return self.at(no, Level::Error, format!("not a hypereffect message: {e}"));
            }
        };
        if let Some(c) = self.closed {
            let who = if dir == Dir::Server {
                "server"
            } else {
                "client"
            };
            self.at(
                no,
                Level::Error,
                format!("the {who} sent {tag} after the server closed the flow (line {c})"),
            );
        }
        if dir == Dir::Client
            && self.hello.is_none()
            && tag != "hello"
            && !self.script
            && !self.early
        {
            self.at(
                no,
                Level::Error,
                format!("the client's first message must be hello, not {tag}"),
            );
            self.early = true;
        }
        match (dir, tag.as_str()) {
            (Dir::Client, "hello") => self.hello(no, &body),
            (Dir::Client, "action") => self.action(no, &body),
            (Dir::Client, "invoice") => self.client_invoice(no, &body),
            (Dir::Client, t) => self.at(
                no,
                Level::Error,
                format!("{t} is not a client message (clients send hello, action and invoice)"),
            ),
            (Dir::Server, t @ ("hello" | "action")) => self.at(
                no,
                Level::Error,
                format!("{t} is a client message, but the server sent it"),
            ),
            (Dir::Server, t) => {
                if self.hello.is_none() && !self.script && !self.early {
                    self.early = true;
                    self.at(
                        no,
                        Level::Warning,
                        format!("the server sent {t} before the client's hello"),
                    );
                }
                match t {
                    "close" => self.close(no, &body),
                    _ => self.served += 1,
                }
                match t {
                    "metadata" => self.metadata(no, &body),
                    "invoice" => self.server_invoice(no, &body),
                    "invoicerequest" => self.invoicerequest(no, &body),
                    "element" => self.element(no, &body),
                    "remove" | "signal" | "log" | "close" => {}
                    other => self.at(
                        no,
                        Level::Warning,
                        format!("unknown tag {other:?}: clients ignore it"),
                    ),
                }
                self.page.receive_line(line);
                for d in self.page.take_diagnostics() {
                    // The page explains malformed COP messages, which are
                    // reported above already.
                    if !d.starts_with("ignored invoice") && !d.contains("unknown tag") {
                        self.at(no, Level::Warning, d);
                    }
                }
            }
        }
    }

    fn hello(&mut self, no: usize, body: &Map<String, Value>) {
        if let Some(first) = self.hello {
            self.at(
                no,
                Level::Error,
                format!("a second hello (the first is on line {first})"),
            );
            return;
        }
        self.hello = Some(no);
        self.page.client.hello();
        match body.get("session") {
            None => self.at(
                no,
                Level::Error,
                "hello has no session: the server may close the connection",
            ),
            Some(Value::String(s)) if s.is_empty() => self.at(
                no,
                Level::Error,
                "hello's session is empty: the server may close the connection",
            ),
            Some(Value::String(s)) => {
                let bits = entropy_bits(s);
                if bits < 128.0 {
                    self.at(
                        no,
                        Level::Error,
                        format!(
                            "the session token carries at most {bits:.0} bits; the spec requires \
                             128 bits of randomness (e.g. 22 base64url characters)"
                        ),
                    );
                }
            }
            Some(v) => self.at(
                no,
                Level::Error,
                format!("hello's session must be a string, not {v}"),
            ),
        }
        match body.get("params") {
            None => {}
            Some(Value::Object(p)) => {
                if p.is_empty() {
                    self.at(
                        no,
                        Level::Hint,
                        "params is empty; omit it when the URI has no parameters",
                    );
                }
                for (k, v) in p {
                    if !v.is_string() {
                        self.at(
                            no,
                            Level::Warning,
                            format!("params.{k} is not a string: servers ignore it"),
                        );
                    }
                }
                if p.contains_key("_relay") {
                    self.at(
                        no,
                        Level::Error,
                        "params has _relay: it is a query parameter for routing, never sent in hello",
                    );
                }
            }
            Some(_) => self.at(
                no,
                Level::Warning,
                "params is not an object of strings: servers ignore it",
            ),
        }
        match body.get("force_resume") {
            None => {}
            Some(Value::Bool(b)) => self.force_resume = *b,
            Some(v) => self.at(
                no,
                Level::Error,
                format!("force_resume must be a boolean, not {v}"),
            ),
        }
    }

    fn action(&mut self, no: usize, body: &Map<String, Value>) {
        if !body.get("name").is_some_and(Value::is_string) {
            self.at(no, Level::Error, "action has no string name");
        }
        if body.get("signals").is_some_and(|s| !s.is_object()) {
            self.at(no, Level::Error, "action signals must be an object");
        }
    }

    fn close(&mut self, no: usize, body: &Map<String, Value>) {
        self.closed = Some(no);
        let reason = body.get("reason").and_then(Value::as_str);
        match reason {
            Some("session_not_found") if !self.force_resume => self.at(
                no,
                Level::Hint,
                "session_not_found without force_resume: the server could have started a fresh flow",
            ),
            Some("session_not_found") if self.served > 0 => self.at(
                no,
                Level::Error,
                format!(
                    "with force_resume the server must not start a new flow, but it sent {} \
                     message(s) before closing with session_not_found",
                    self.served
                ),
            ),
            _ => {}
        }
    }

    fn metadata(&mut self, no: usize, body: &Map<String, Value>) {
        for (k, v) in body {
            if !v.is_string() {
                self.at(
                    no,
                    Level::Warning,
                    format!("metadata.{k} is not a string: clients ignore it"),
                );
            }
        }
        if let Some(mode) = body.get("mode").and_then(Value::as_str)
            && mode != "single_use"
        {
            self.at(
                no,
                Level::Warning,
                format!(
                    "unknown metadata mode {mode:?}: clients ignore it (COP defines single_use)"
                ),
            );
        }
    }

    fn server_invoice(&mut self, no: usize, body: &Map<String, Value>) {
        let Some(raw) = body.get("invoice").filter(|i| i.is_object()) else {
            return self.at(
                no,
                Level::Error,
                "invoice has no \"invoice\" object: clients ignore it",
            );
        };
        if body.get("memo").is_some_and(|m| !m.is_string()) {
            self.at(no, Level::Warning, "invoice memo is not a string");
        }
        self.cassis(no, raw);
    }

    /// What `cassis-core` thinks of an invoice.
    fn cassis(&mut self, no: usize, raw: &Value) {
        let s = Summary::of(raw);
        match (&s.decoded, s.verified()) {
            (Err(e), _) => self.at(
                no,
                Level::Warning,
                format!("the Cassis invoice does not decode: {e}"),
            ),
            (_, Some(Err(e))) => self.at(
                no,
                Level::Error,
                format!("the Cassis invoice is not valid: {e}"),
            ),
            _ => {}
        }
        if s.expires_in().is_some_and(|left| left <= 0) {
            self.at(no, Level::Hint, "the invoice has expired (as of now)");
        }
        for n in &s.networks {
            if let Some(p) = &n.problem {
                self.at(
                    no,
                    Level::Warning,
                    format!("invoice network {:?}: {p}", n.id),
                );
            }
        }
    }

    fn invoicerequest(&mut self, no: usize, body: &Map<String, Value>) {
        let amount = body.get("amount");
        let amount_sat = amount.and_then(Value::as_u64);
        match amount {
            None => self.at(
                no,
                Level::Error,
                "invoicerequest has no amount: clients ignore it",
            ),
            Some(_) if amount_sat.is_none() => self.at(
                no,
                Level::Error,
                "invoicerequest amount must be a non-negative integer of satoshis",
            ),
            Some(_) if amount_sat == Some(0) => {
                self.at(no, Level::Warning, "invoicerequest offers 0 sat")
            }
            _ => {}
        }
        let networks: Vec<String> = match body.get("networks") {
            Some(Value::Array(a)) => {
                let names: Vec<String> = a
                    .iter()
                    .filter_map(|n| n.as_str().map(str::to_owned))
                    .collect();
                if names.len() != a.len() {
                    self.at(
                        no,
                        Level::Warning,
                        "invoicerequest networks has entries that are not strings",
                    );
                }
                if names.is_empty() {
                    self.at(
                        no,
                        Level::Error,
                        "invoicerequest networks must not be empty",
                    );
                }
                for n in &names {
                    if let Err(e) = NetworkId::parse(n) {
                        self.at(no, Level::Warning, format!("network {n:?}: {e}"));
                    }
                }
                names
            }
            _ => {
                self.at(
                    no,
                    Level::Error,
                    "invoicerequest has no networks array: clients ignore it",
                );
                Vec::new()
            }
        };
        if body.get("memo").is_some_and(|m| !m.is_string()) {
            self.at(no, Level::Warning, "invoicerequest memo is not a string");
        }
        if body.get("expiresIn").is_some_and(|e| e.as_u64().is_none()) {
            self.at(
                no,
                Level::Warning,
                "invoicerequest expiresIn must be an integer of seconds",
            );
        }
        if body.contains_key("expires_in") {
            self.at(no, Level::Warning, "the field is expiresIn, not expires_in");
        }
        if let (Some(amount_sat), false) = (amount_sat, networks.is_empty()) {
            self.asked.push(Asked {
                line: no,
                amount_sat,
                networks,
                memo: body.get("memo").and_then(Value::as_str).map(str::to_owned),
            });
        }
    }

    fn client_invoice(&mut self, no: usize, body: &Map<String, Value>) {
        let Some(raw) = body.get("invoice").filter(|i| i.is_object()) else {
            return self.at(no, Level::Error, "invoice has no \"invoice\" object");
        };
        self.cassis(no, raw);
        let Some(asked) = self.asked.pop() else {
            return self.at(
                no,
                Level::Warning,
                "the client sent an invoice no invoicerequest asked for",
            );
        };
        let s = Summary::of(raw);
        let want = asked.amount_sat.saturating_mul(1000);
        if let Some(got) = s.amount_msat
            && got != want
        {
            self.at(
                no,
                Level::Error,
                format!(
                    "the invoice is for {got} msat, but the invoicerequest on line {} offered {} sat \
                     ({want} msat)",
                    asked.line, asked.amount_sat
                ),
            );
        }
        let offered: Vec<&str> = s
            .networks
            .iter()
            .map(|n| n.id.as_str())
            .filter(|n| asked.networks.iter().any(|a| a == n))
            .collect();
        if offered.is_empty() {
            self.at(
                no,
                Level::Error,
                format!(
                    "the invoice is payable on none of the networks offered on line {} ({})",
                    asked.line,
                    asked.networks.join(", ")
                ),
            );
        }
        if let (Some(memo), d) = (&asked.memo, &s.description)
            && d.as_deref() != Some(memo.as_str())
        {
            self.at(
                no,
                Level::Hint,
                "the invoice's description is not the invoicerequest's memo, which it should include",
            );
        }
    }

    fn element(&mut self, no: usize, body: &Map<String, Value>) {
        // "Action payloads carry item identifiers, never prices."
        let mut priced = Vec::new();
        for (attr, h) in body {
            let Some(payload) = h.get("payload") else {
                continue;
            };
            if h.get("action").is_some() && mentions_price(payload) {
                priced.push(attr.clone());
            }
        }
        for attr in priced {
            self.at(
                no,
                Level::Hint,
                format!(
                    "{attr} sends a payload that looks like a price: payloads should carry item \
                     identifiers and the server should compute prices itself"
                ),
            );
        }
    }
}

fn mentions_price(v: &Value) -> bool {
    const KEYS: [&str; 6] = ["price", "amount", "total", "cost", "amount_msat", "sats"];
    match v {
        Value::Object(m) => m
            .iter()
            .any(|(k, v)| KEYS.contains(&k.to_ascii_lowercase().as_str()) || mentions_price(v)),
        Value::Array(a) => a.iter().any(mentions_price),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::parse;

    fn levels(t: &str) -> Vec<(usize, Level)> {
        check(&parse(t).unwrap())
            .into_iter()
            .map(|f| (f.line, f.level))
            .collect()
    }

    #[test]
    fn the_pizza_flow_is_fine() {
        let t = r#"
> hello {"params":{"name":"Luigi's Pizza"},"session":"f1K9xQ2v8Tn4LmZp7Rb3wq"}
< metadata {"name":"Luigi's Pizza","description":"Wood-fired pizza, Via Roma 12"}
< element {"id":"menu","type":"list"}
< element {"id":"pepperoni","type":"button","parent":"menu","label":"Pepperoni","onPress":{"action":"selectPizza","payload":{"pizza":"pepperoni"}}}
> action {"name":"selectPizza","payload":{"pizza":"pepperoni"},"signals":{}}
"#;
        assert_eq!(levels(t), vec![]);
    }

    #[test]
    fn finds_mistakes() {
        let t = r#"
< metadata {"name":"x","mode":"reusable"}
> hello {"session":"abc","params":{"_relay":"x","n":1}}
< element {"id":"b","type":"button","label":"Pay","onPress":{"action":"pay","payload":{"price":10}}}
< invoicerequest {"amount":24,"networks":["liquid"],"memo":"Refund"}
> invoice {"invoice":{"amount_msat":1000,"networks":[{"network":"bitcoin","target":{}}]}}
< close {"reason":"session_not_found"}
< signal {}
"#;
        let found = levels(t);
        let has = |line, level| found.contains(&(line, level));
        assert!(has(2, Level::Warning), "{found:?}"); // before hello, unknown mode
        assert!(has(3, Level::Error)); // short session, _relay
        assert!(has(3, Level::Warning)); // non-string param
        assert!(has(4, Level::Hint)); // price in payload
        assert!(has(6, Level::Error)); // wrong amount, wrong network
        assert!(has(7, Level::Hint)); // session_not_found without force_resume
        assert!(has(8, Level::Error)); // after close
    }

    #[test]
    fn force_resume_must_not_start_a_flow() {
        let t = r#"
> hello {"session":"f1K9xQ2v8Tn4LmZp7Rb3wq","force_resume":true}
< element {"id":"t","type":"text","text":"Welcome"}
< close {"reason":"session_not_found"}
"#;
        assert!(levels(t).contains(&(4, Level::Error)));
    }
}
