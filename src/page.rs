//! What a client knows of a flow: the hypereffect [`Client`] with the tree,
//! signals and metadata, plus what COP adds: the invoices the server asked
//! to be paid and the `invoicerequest`s it made.
//!
//! The same state serves a live session and a replayed transcript.

use hypereffect::client::Client;
use hypereffect::{Map, Value};
use std::sync::{Arc, Mutex};

/// An `invoice` from the server: a request for payment.
#[derive(Clone, Debug)]
pub struct Offer {
    /// The Cassis invoice, as sent.
    pub invoice: Value,
    /// A note to show alongside. Never the source of amount or payee.
    pub memo: Option<String>,
}

/// An `invoicerequest`: the server offers money and asks for an invoice.
#[derive(Clone, Debug)]
pub struct Request {
    pub amount_sat: u64,
    pub networks: Vec<String>,
    pub memo: Option<String>,
    pub expires_in: Option<u64>,
}

impl Request {
    pub fn to_json(&self) -> Value {
        hypereffect::json!({
            "amount": self.amount_sat,
            "amount_msat": self.amount_sat.saturating_mul(1000),
            "networks": self.networks,
            "memo": self.memo,
            "expiresIn": self.expires_in,
        })
    }
}

/// A COP message, as its handler took it.
enum Cop {
    Invoice(Offer),
    Request(Request),
    /// A COP message missing what it needs: clients ignore it.
    Ignored(String),
}

pub struct Page {
    pub client: Client,
    /// Every `invoice` received, oldest first. The last one replaces the
    /// ones before it, as long as they are unpaid.
    pub invoices: Vec<Offer>,
    pub requests: Vec<Request>,
    inbox: Arc<Mutex<Vec<Cop>>>,
    /// Why COP messages were ignored, for the diagnostics.
    ignored: Vec<String>,
}

impl Default for Page {
    fn default() -> Self {
        Self::new()
    }
}

impl Page {
    pub fn new() -> Self {
        let mut client = Client::new();
        let inbox: Arc<Mutex<Vec<Cop>>> = Arc::default();
        let sink = inbox.clone();
        client.on_message("invoice", move |body| {
            push(&sink, parse_offer(&body));
        });
        let sink = inbox.clone();
        client.on_message("invoicerequest", move |body| {
            push(&sink, parse_request(&body));
        });
        Self {
            client,
            invoices: Vec::new(),
            requests: Vec::new(),
            inbox,
            ignored: Vec::new(),
        }
    }

    /// Applies one line from the server.
    pub fn receive_line(&mut self, line: &str) {
        self.client.receive_line(line);
        let taken = std::mem::take(&mut *self.inbox.lock().unwrap_or_else(|e| e.into_inner()));
        for m in taken {
            match m {
                Cop::Invoice(o) => self.invoices.push(o),
                Cop::Request(r) => self.requests.push(r),
                Cop::Ignored(why) => self.ignored.push(why),
            }
        }
    }

    /// The invoice to pay now: the latest one.
    pub fn invoice(&self) -> Option<&Offer> {
        self.invoices.last()
    }

    /// The client's diagnostics, and why COP messages were ignored.
    pub fn take_diagnostics(&mut self) -> Vec<String> {
        let mut d = self.client.take_diagnostics();
        d.append(&mut self.ignored);
        d
    }

    /// The `close` reason the server gave, if it sent `close`.
    pub fn close_reason(&self) -> Option<Option<&str>> {
        self.client.closed().map(|c| c.reason.as_deref())
    }
}

fn push(inbox: &Arc<Mutex<Vec<Cop>>>, m: Cop) {
    inbox.lock().unwrap_or_else(|e| e.into_inner()).push(m);
}

fn parse_offer(body: &Map<String, Value>) -> Cop {
    let Some(invoice) = body.get("invoice").filter(|i| i.is_object()) else {
        return Cop::Ignored("ignored invoice: it has no \"invoice\" object".into());
    };
    Cop::Invoice(Offer {
        invoice: invoice.clone(),
        memo: body.get("memo").and_then(Value::as_str).map(str::to_owned),
    })
}

fn parse_request(body: &Map<String, Value>) -> Cop {
    let ignored = |why: &str| Cop::Ignored(format!("ignored invoicerequest: {why}"));
    let Some(amount_sat) = body.get("amount").and_then(Value::as_u64) else {
        return ignored("\"amount\" is not a non-negative integer");
    };
    let Some(networks) = body.get("networks").and_then(Value::as_array) else {
        return ignored("\"networks\" is not an array");
    };
    let networks: Vec<String> = networks
        .iter()
        .filter_map(|n| n.as_str().map(str::to_owned))
        .collect();
    if networks.is_empty() {
        return ignored("\"networks\" names no network");
    }
    Cop::Request(Request {
        amount_sat,
        networks,
        memo: body.get("memo").and_then(Value::as_str).map(str::to_owned),
        expires_in: body.get("expiresIn").and_then(Value::as_u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_cop_messages() {
        let mut p = Page::new();
        p.client.hello();
        p.receive_line(r#"metadata {"name":"Luigi","mode":"single_use"}"#);
        p.receive_line(r#"invoice {"invoice":{"amount_msat":1000},"memo":"x"}"#);
        p.receive_line(r#"invoice {"memo":"no invoice"}"#);
        p.receive_line(r#"invoicerequest {"amount":24000,"networks":["liquid"],"expiresIn":900}"#);
        p.receive_line(r#"invoicerequest {"amount":1,"networks":[]}"#);
        assert_eq!(p.invoices.len(), 1);
        assert_eq!(p.invoice().unwrap().memo.as_deref(), Some("x"));
        assert_eq!(p.requests[0].amount_sat, 24000);
        assert_eq!(p.take_diagnostics().len(), 2);
        p.receive_line(r#"close {"reason":"session_not_found"}"#);
        assert_eq!(p.close_reason(), Some(Some("session_not_found")));
    }
}
