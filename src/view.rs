//! A short depiction of a flow: the server's metadata, the signals, the
//! hypereffect tree (one line per element with its type, id and the one or
//! two attributes that say what it shows, and the handlers of the ones that
//! do something), then what COP adds: the invoice to pay, the
//! `invoicerequest`s, and how the flow ended.
//!
//! ```text
//! metadata "Luigi's Pizza" — "Wood-fired pizza, Via Roma 12"
//! {
//!   "order": { "address": "" }
//! }
//! tree {
//!   text title "Where should we deliver?" h1
//!   input address "Delivery address" [order.address = ""]
//!   button pay "Pay $10.75" primary (disabled)  ⚡ onPress → pay
//! }
//! invoice 10,750 sat (10,750,000 msat) · expires in 14m 32s · direct ✓ · memo "Pepperoni + chili"
//! ```

use crate::invoice::{self, Summary};
use crate::page::Page;
use crate::style::{Style, clip, compact, quote};
use hypereffect::client::{Client, ResolvedTree};
use hypereffect::component::AttrType;
use hypereffect::{Component, Map, Value, json, serde_json};
use std::fmt::Write;

/// Everything about the flow, as text.
pub fn snapshot(page: &Page, st: &Style) -> String {
    let mut out = String::new();
    out.push_str(&metadata(page, st));
    let signals = serde_json::to_string_pretty(page.client.signals()).unwrap_or_default();
    out.push_str(&signals);
    out.push('\n');
    out.push_str(&render(&page.client.tree(), &page.client, st));
    out.push_str(&cop(page, st));
    out
}

/// Everything about the flow, as JSON.
pub fn snapshot_json(page: &Page) -> Value {
    let invoice = page
        .invoice()
        .map(|o| Summary::of(&o.invoice).to_json(o.memo.as_deref()));
    let close = page
        .close_reason()
        .map(|r| json!({ "reason": r }))
        .unwrap_or(Value::Null);
    json!({
        "metadata": page.client.metadata(),
        "signals": page.client.signals(),
        "tree": page.client.tree(),
        "invoice": invoice,
        "invoice_raw": page.invoice().map(|o| o.invoice.clone()),
        "invoices": page.invoices.len(),
        "invoicerequests": page.requests.iter().map(|r| r.to_json()).collect::<Vec<_>>(),
        "connected": page.client.is_connected(),
        "close": close,
    })
}

/// `metadata "name" — "description" single_use`, or nothing before the
/// server sends any.
pub fn metadata(page: &Page, st: &Style) -> String {
    let m = page.client.metadata();
    let mut parts = Vec::new();
    parts.extend(m.name.as_deref().map(|n| st.bold(&quote(n, st.max_text))));
    parts.extend(
        m.description
            .as_deref()
            .map(|d| format!("— {}", quote(d, st.max_text))),
    );
    match m.extra.get("mode").map(String::as_str) {
        Some("single_use") => parts.push(st.magenta("single_use")),
        Some(other) => parts.push(st.yellow(&format!("mode {other:?} (unknown, ignored)"))),
        None => {}
    }
    for (k, v) in m.extra.iter().filter(|(k, _)| *k != "mode") {
        parts.push(st.dim(&format!("{k}={}", quote(v, st.max_text))));
    }
    if m.icon.is_some() {
        parts.push(st.dim("(icon)"));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("{} {}\n", st.dim("metadata"), parts.join(" "))
}

/// The invoice to pay, the `invoicerequest`s, and the end of the flow.
pub fn cop(page: &Page, st: &Style) -> String {
    let mut out = String::new();
    if let Some(o) = page.invoice() {
        let _ = writeln!(
            out,
            "{} {}",
            st.yellow("invoice"),
            invoice_line(&o.invoice, o.memo.as_deref(), st)
        );
        if page.invoices.len() > 1 {
            let _ = writeln!(
                out,
                "  {}",
                st.dim(&format!(
                    "(replaces {} earlier invoice{})",
                    page.invoices.len() - 1,
                    if page.invoices.len() == 2 { "" } else { "s" }
                ))
            );
        }
    }
    for r in &page.requests {
        let _ = writeln!(
            out,
            "{} {}",
            st.yellow("invoicerequest"),
            request_line(
                r.amount_sat,
                &r.networks,
                r.memo.as_deref(),
                r.expires_in,
                st
            )
        );
    }
    match page.close_reason() {
        Some(Some(reason)) => {
            let _ = writeln!(out, "{}", st.red(&format!("closed: {reason}")));
        }
        Some(None) => {
            let _ = writeln!(out, "{}", st.red("closed"));
        }
        None if !page.client.is_connected() => {
            let _ = writeln!(out, "{}", st.dim("disconnected"));
        }
        None => {}
    }
    out
}

/// One line about an invoice: amount, expiry, receiver, validity, memo.
pub fn invoice_line(raw: &Value, memo: Option<&str>, st: &Style) -> String {
    let s = Summary::of(raw);
    let mut parts = vec![
        s.amount_msat
            .map_or_else(|| st.red("no amount"), |m| st.bold(&invoice::amount(m))),
    ];
    parts.push(match s.expires_in() {
        Some(left) if left > 0 => format!("expires in {}", invoice::duration(left as u64)),
        Some(left) => st.red(&format!(
            "expired {} ago",
            invoice::duration(left.unsigned_abs())
        )),
        None => st.red("no expiry"),
    });
    let nets: Vec<&str> = s.networks.iter().map(|n| n.id.as_str()).collect();
    if !nets.is_empty() {
        parts.push(format!("on {}", nets.join(", ")));
    }
    parts.push(match (&s.decoded, s.verified()) {
        (Err(_), _) => st.red("✗ undecodable"),
        (_, Some(Err(e))) => st.red(&format!("{} ✗ {e}", s.kind().unwrap_or("?"))),
        _ => st.green(&format!("{} ✓", s.kind().unwrap_or("?"))),
    });
    if let Some(m) = memo {
        parts.push(format!("memo {}", quote(m, st.max_text)));
    }
    parts.join(" · ")
}

/// One line about an `invoicerequest`.
pub fn request_line(
    amount_sat: u64,
    networks: &[String],
    memo: Option<&str>,
    expires_in: Option<u64>,
    st: &Style,
) -> String {
    let mut parts = vec![
        format!(
            "{} offered",
            st.bold(&invoice::amount(amount_sat.saturating_mul(1000)))
        ),
        format!("on {}", networks.join(", ")),
    ];
    if let Some(e) = expires_in {
        parts.push(format!("invoice to expire in {}", invoice::duration(e)));
    }
    if let Some(m) = memo {
        parts.push(format!("memo {}", quote(m, st.max_text)));
    }
    parts.join(" · ")
}

/// The tree in a `tree { … }` block, or `tree {}` when it is empty.
pub fn render(tree: &[ResolvedTree], client: &Client, st: &Style) -> String {
    if tree.is_empty() {
        return "tree {}\n".into();
    }
    let mut out = String::from("tree {\n");
    for node in tree {
        node_into(&mut out, node, 1, client, st);
    }
    out.push_str("}\n");
    out
}

fn node_into(out: &mut String, n: &ResolvedTree, depth: usize, client: &Client, st: &Style) {
    let parts = describe(n, client, st, false);
    let indent = "  ".repeat(depth);
    let _ = write!(
        out,
        "{indent}{} {}",
        st.bold(n.component.name()),
        st.dim(&n.id)
    );
    for p in &parts {
        out.push(' ');
        out.push_str(p);
    }
    for (event, h) in handlers(n.component, &n.attrs) {
        let _ = write!(out, "  {}", st.yellow(&format!("⚡ {event} →")));
        out.push_str(&handler(h, st));
    }
    out.push('\n');
    for c in &n.children {
        node_into(out, c, depth + 1, client, st);
    }
}

/// The one or two attributes that say what an element shows, the signal
/// it edits and its state (hidden, disabled, closed). `interactive` leaves
/// out what `copcat ui` draws as a widget: the bound value and the options of
/// a select.
pub fn describe(n: &ResolvedTree, client: &Client, st: &Style, interactive: bool) -> Vec<String> {
    let a = &n.attrs;
    let s = |k: &str| a.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    let q = |t: &str| quote(t, st.max_text);
    // The variant, unless it is the component's default.
    let variant = s("variant")
        .filter(|v| {
            let default = n.component.attr("variant").map(|a| a.default.value(&n.id));
            default.as_ref().and_then(Value::as_str) != Some(*v)
        })
        .map(str::to_string);
    let mut parts: Vec<String> = Vec::new();
    // What the element shows.
    match n.component {
        Component::Text => {
            parts.extend(s("text").map(q));
            parts.extend(variant);
        }
        Component::Notice => {
            parts.extend(s("title").map(q));
            parts.extend(s("text").map(q));
            parts.extend(variant);
        }
        Component::Badge => parts.extend(s("text").map(q)),
        Component::Image => parts.extend(s("alt").map(q)),
        Component::Card | Component::Dialog => parts.extend(s("title").map(q)),
        Component::Tab | Component::Checkbox => parts.extend(s("label").map(q)),
        Component::Button => {
            parts.extend(s("label").map(q));
            parts.extend(variant);
        }
        Component::Input => parts.extend(s("label").or(s("placeholder")).map(q)),
        Component::Select | Component::Slider | Component::Progress => {
            parts.extend(s("label").map(q))
        }
        Component::List if n.children.is_empty() => {
            parts.extend(s("empty").map(|e| format!("empty: {}", q(e))))
        }
        _ => {}
    }
    match n.component {
        Component::Progress => match a.get("value") {
            Some(v @ Value::Number(_)) => parts.push(num(v)),
            _ => parts.push("indeterminate".into()),
        },
        Component::Slider => {
            let get = |k| a.get(k).map_or("?".into(), num);
            parts.push(format!("{}..{}", get("min"), get("max")))
        }
        Component::Select if !interactive => {
            let opts: Vec<&str> = a
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|o| o.get("value")?.as_str())
                .collect();
            parts.push(clip(&format!("<{}>", opts.join("|")), 2 * st.max_text));
        }
        _ => {}
    }
    // The signal a control or tabs edit, with its value.
    if let Some(path) = s("bind").filter(|_| !interactive) {
        let v = client.value(path).map_or("null".into(), compact);
        let v = clip(&v, 2 * st.max_text);
        parts.push(st.cyan(&format!("[{path} = {v}]")));
    }
    if a.get("visible") == Some(&Value::Bool(false)) {
        parts.push(st.dim("(hidden)"));
    }
    if a.get("disabled") == Some(&Value::Bool(true)) {
        parts.push(st.dim("(disabled)"));
    }
    if n.component == Component::Dialog && a.get("open") == Some(&Value::Bool(false)) {
        parts.push(st.dim("(closed)"));
    }
    parts
}

/// The handlers an element has, as `(attribute, handler object)`.
pub fn handlers<'a>(
    component: Component,
    attrs: &'a Map<String, Value>,
) -> impl Iterator<Item = (&'static str, &'a Map<String, Value>)> + 'a {
    component
        .attrs()
        .iter()
        .filter(|a| a.ty == AttrType::Handler)
        .filter_map(|a| Some((a.name, attrs.get(a.name)?.as_object()?)))
}

/// Whether an element has a handler that sends an action.
pub fn fires_actions(component: Component, attrs: &Map<String, Value>) -> bool {
    handlers(component, attrs).any(|(_, h)| h.get("action").is_some())
}

/// A handler as ` action payload`, ` patch{keys}` and ` open url`.
pub fn handler(h: &Map<String, Value>, st: &Style) -> String {
    let mut out = String::new();
    if let Some(a) = h.get("action").and_then(Value::as_str) {
        let _ = write!(out, " {}", st.green(a));
        if let Some(p) = h.get("payload") {
            let _ = write!(out, " {}", clip(&compact(p), 2 * st.max_text));
        }
    }
    if let Some(Value::Object(p)) = h.get("patch") {
        let keys: Vec<&str> = p.keys().map(String::as_str).collect();
        let _ = write!(out, " patch{{{}}}", keys.join(","));
    }
    if let Some(o) = h.get("open") {
        let o = o.as_str().map_or_else(|| compact(o), str::to_string);
        let _ = write!(out, " open {}", clip(&o, 2 * st.max_text));
    }
    out
}

/// A number without the `.0` of whole floats.
fn num(v: &Value) -> String {
    match v.as_f64() {
        Some(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", f as i64),
        _ => compact(v),
    }
}
