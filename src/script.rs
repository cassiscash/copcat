//! `copcat run`: a JavaScript script, on the boa engine, driving one flow.
//!
//! The script runs once the server has sent its first page and gone quiet.
//! Every helper is synchronous: the ones that wait (`wait`, `waitInvoice`,
//! `sleep`, `settle`) take in what the server sends while they block, so
//! the tree, signals and invoices they look at are always current.
//!
//! ```js
//! press("pepperoni");
//! settle();
//! press("chili");
//! settle();
//! input("address", "221B Baker St");
//! press("pay");
//! const inv = waitInvoice();
//! print(inv.summary.amount_msat, inv.memo);
//! ```

use crate::invoice::Summary;
use crate::session::{Received, Session, Settled, Update};
use crate::style::{Style, compact};
use crate::view;
use boa_engine::object::ObjectInitializer;
use boa_engine::{
    Context, JsArgs, JsError, JsResult, JsValue, NativeFunction, Source, js_error, js_string,
};
use hypereffect::client::Resolved;
use hypereffect::message::parse_line;
use hypereffect::{Value, json, serde_json};
use std::cell::RefCell;
use std::path::Path;
use std::time::{Duration, Instant};

/// What the helpers act on.
pub struct Host {
    pub session: Session,
    /// How long `wait` waits by default.
    pub timeout: Duration,
    pub style: Style,
    /// How long the server must be quiet after the script, so that the
    /// last actions it fired are answered.
    pub settle: Duration,
}

thread_local! {
    static HOST: RefCell<Option<Host>> = const { RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Host) -> R) -> R {
    HOST.with_borrow_mut(|h| f(h.as_mut().expect("the script host is set")))
}

/// Runs `src` against the flow in `host`. `path` names the script in error
/// messages.
pub fn run(host: Host, src: &str, path: Option<&Path>) -> Result<(), String> {
    let uri = host.session.uri.to_string();
    let token = host.session.token.clone();
    HOST.set(Some(host));
    let mut ctx = Context::default();
    let result = install(&mut ctx, &uri, &token).and_then(|()| {
        let source = Source::from_bytes(src);
        let source = match path {
            Some(p) => source.with_path(p),
            None => source,
        };
        ctx.eval(source)?;
        ctx.run_jobs()
    });
    with(|h| {
        h.session
            .settle(h.settle, Instant::now() + h.settle + Duration::from_secs(1))
    });
    HOST.set(None);
    result.map_err(|e| describe(e, &mut ctx))
}

fn describe(e: JsError, ctx: &mut Context) -> String {
    match e.into_opaque(ctx) {
        Ok(v) => v
            .to_string(ctx)
            .map_or_else(|_| v.display().to_string(), |s| s.to_std_string_escaped()),
        Err(e) => e.to_string(),
    }
}

type Native = fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>;

fn install(ctx: &mut Context, uri: &str, token: &str) -> JsResult<()> {
    let globals: &[(&str, usize, Native)] = &[
        ("signals", 0, signals),
        ("signal", 1, signal),
        ("metadata", 0, metadata),
        ("tree", 0, tree_fn),
        ("element", 1, element),
        ("query", 1, query),
        ("queryOne", 1, query_one),
        ("wait", 2, wait),
        ("action", 3, action),
        ("trigger", 2, trigger),
        ("press", 1, press),
        ("input", 2, input),
        ("write", 2, write),
        ("sleep", 1, sleep),
        ("settle", 2, settle),
        ("invoice", 0, invoice),
        ("invoices", 0, invoices),
        ("invoiceRequests", 0, invoice_requests),
        ("waitInvoice", 1, wait_invoice),
        ("waitInvoiceRequest", 1, wait_invoice_request),
        ("sendInvoice", 1, send_invoice),
        ("decodeInvoice", 1, decode_invoice),
        ("closeReason", 0, close_reason),
        ("print", 0, print),
        ("printTree", 0, print_tree),
        ("printInvoice", 1, print_invoice),
    ];
    for (name, len, f) in globals {
        ctx.register_global_callable(js_string!(*name), *len, NativeFunction::from_fn_ptr(*f))?;
    }
    let console = ObjectInitializer::new(ctx)
        .function(NativeFunction::from_fn_ptr(print), js_string!("log"), 0)
        .function(NativeFunction::from_fn_ptr(print), js_string!("info"), 0)
        .function(NativeFunction::from_fn_ptr(eprint), js_string!("error"), 0)
        .function(NativeFunction::from_fn_ptr(eprint), js_string!("warn"), 0)
        .build();
    let attr = boa_engine::property::Attribute::all();
    ctx.register_global_property(js_string!("console"), console, attr)?;
    ctx.register_global_property(js_string!("uri"), js_string!(uri), attr)?;
    ctx.register_global_property(js_string!("session"), js_string!(token), attr)?;
    Ok(())
}

// --- conversions -------------------------------------------------------------

fn to_js(v: &Value, ctx: &mut Context) -> JsResult<JsValue> {
    JsValue::from_json(v, ctx)
}

/// A JS value as JSON: `undefined` is `None`, and whole floats are
/// integers, so that `2.0` reaches the server as `2`.
fn from_js(v: &JsValue, ctx: &mut Context) -> JsResult<Option<Value>> {
    Ok(v.to_json(ctx)?.map(integral))
}

fn integral(v: Value) -> Value {
    match v {
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() && f.fract() == 0.0 && f.abs() < 2f64.powi(53) => {
                (f as i64).into()
            }
            _ => Value::Number(n),
        },
        Value::Array(a) => Value::Array(a.into_iter().map(integral).collect()),
        Value::Object(o) => Value::Object(o.into_iter().map(|(k, v)| (k, integral(v))).collect()),
        v => v,
    }
}

fn string_arg(args: &[JsValue], i: usize, what: &str, ctx: &mut Context) -> JsResult<String> {
    let v = args.get_or_undefined(i);
    if v.is_undefined() {
        return Err(js_error!(TypeError: "missing {}", what));
    }
    Ok(v.to_string(ctx)?.to_std_string_escaped())
}

/// The timeout argument at `i`, in milliseconds, or the default.
fn timeout_arg(args: &[JsValue], i: usize, ctx: &mut Context) -> JsResult<Duration> {
    let v = args.get_or_undefined(i);
    if v.is_null_or_undefined() {
        return Ok(with(|h| h.timeout));
    }
    Ok(Duration::from_millis(v.to_number(ctx)?.max(0.0) as u64))
}

/// Deep equality, with numbers compared by value (`1` equals `1.0`).
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| json_eq(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| json_eq(v, w)))
        }
        (a, b) => a == b,
    }
}

// --- elements ----------------------------------------------------------------

/// An element as scripts see it: its resolved attributes, its children's
/// ids, the value of the signal it is bound to, and what its handlers do.
fn element_json(r: &Resolved, host: &Host) -> Value {
    let client = &host.session.page.client;
    let value = r
        .attrs
        .get("bind")
        .and_then(Value::as_str)
        .map(|p| client.value(p).cloned().unwrap_or(Value::Null));
    let handlers: Vec<Value> = view::handlers(r.component, &r.attrs)
        .map(|(event, h)| {
            let mut h = h.clone();
            h.insert("event".into(), event.into());
            Value::Object(h)
        })
        .collect();
    json!({
        "id": r.id,
        "type": r.component.name(),
        "parent": r.parent,
        "children": r.children,
        "attrs": r.attrs,
        "value": value,
        "handlers": handlers,
    })
}

/// Every element, depth first in display order.
fn all_elements(host: &Host) -> Vec<Value> {
    fn walk(host: &Host, ids: &[String], out: &mut Vec<Value>) {
        for id in ids {
            if let Some(r) = host.session.page.client.get(id) {
                out.push(element_json(&r, host));
                walk(host, &r.children, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(host, host.session.page.client.children(None), &mut out);
    out
}

/// Whether an element (as [`element_json`]) has everything in `filter`:
/// `id`, `type`, `parent` and `value` are compared with the element's,
/// `action` with the actions of its handlers, and any other key with the
/// attribute of that name.
fn matches(el: &Value, filter: &serde_json::Map<String, Value>) -> bool {
    filter.iter().all(|(k, want)| match k.as_str() {
        "id" | "type" | "parent" | "value" => json_eq(&el[k], want),
        "action" => el["handlers"]
            .as_array()
            .is_some_and(|hs| hs.iter().any(|h| json_eq(&h["action"], want))),
        _ => json_eq(&el["attrs"][k], want),
    })
}

/// Whether `el` passes `filter`: a function called with it, or an object
/// as in [`matches`].
fn passes(el: &Value, filter: &JsValue, ctx: &mut Context) -> JsResult<bool> {
    if let Some(f) = filter.as_callable() {
        let arg = to_js(el, ctx)?;
        return Ok(f.call(&JsValue::undefined(), &[arg], ctx)?.to_boolean());
    }
    match from_js(filter, ctx)? {
        Some(Value::Object(f)) => Ok(matches(el, &f)),
        None => Ok(true),
        Some(_) => Err(js_error!(TypeError: "a filter is a function or an object")),
    }
}

fn select(filter: &JsValue, ctx: &mut Context) -> JsResult<Vec<Value>> {
    let all = with(|h| all_elements(h));
    let mut out = Vec::new();
    for el in all {
        if passes(&el, filter, ctx)? {
            out.push(el);
        }
    }
    Ok(out)
}

// --- invoices ----------------------------------------------------------------

/// An invoice as scripts see it: `{invoice, memo, summary}`, where
/// `summary` is what `copcat invoice --json` prints.
fn offer_json(raw: &Value, memo: Option<&str>) -> Value {
    json!({
        "invoice": raw,
        "memo": memo,
        "summary": Summary::of(raw).to_json(memo),
    })
}

// --- waiting -----------------------------------------------------------------

/// Takes in what the server sends until `deadline`. Returns false if the
/// flow has ended.
fn pump(deadline: Instant) -> bool {
    with(|h| !matches!(h.session.next(Some(deadline)), Some(Update::Closed(_))))
}

/// A message as `wait` hands it over: `{kind, tag, body, ...}`, with for
/// elements `id` and `element` (as `element(id)` gives it, `null` if it
/// was rejected), for signals the whole `signals`, and for invoices the
/// decoded `summary`.
fn message_json(line: &str, host: &Host) -> Value {
    let client = &host.session.page.client;
    let Ok((tag, body)) = parse_line(line) else {
        return json!({ "kind": "invalid", "line": line });
    };
    let mut m = json!({ "kind": tag, "tag": tag, "body": body });
    match tag {
        "element" => {
            let id = body.get("id").and_then(Value::as_str).unwrap_or_default();
            m["id"] = id.into();
            m["element"] = client
                .get(id)
                .map_or(Value::Null, |r| element_json(&r, host));
        }
        "remove" => m["id"] = body.get("id").cloned().unwrap_or_default(),
        "signal" => m["signals"] = client.signals().clone(),
        "invoice" => {
            if let Some(raw) = body.get("invoice").filter(|i| i.is_object()) {
                let memo = body.get("memo").and_then(Value::as_str);
                m["summary"] = Summary::of(raw).to_json(memo);
            }
        }
        _ => {}
    }
    m
}

/// `wait(fn, timeoutMs?)`: calls `fn` with every message the server sends
/// from now on, already applied, until it returns true. Returns that
/// message; throws on timeout or if the flow ends first.
fn wait(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(f) = args.get_or_undefined(0).as_callable() else {
        return Err(js_error!(TypeError: "wait takes a function"));
    };
    let timeout = timeout_arg(args, 1, ctx)?;
    let deadline = Instant::now() + timeout;
    loop {
        let line = next_line(deadline, timeout)?;
        let m = with(|h| message_json(&line, h));
        let m = to_js(&m, ctx)?;
        if f.call(&JsValue::undefined(), std::slice::from_ref(&m), ctx)?
            .to_boolean()
        {
            return Ok(m);
        }
    }
}

fn next_line(deadline: Instant, timeout: Duration) -> JsResult<String> {
    match with(|h| h.session.next_line(Some(deadline))) {
        Some(Received::Line(l)) => Ok(l),
        Some(Received::Closed(_)) => {
            let reason = with(|h| h.session.page.close_reason().flatten().map(str::to_owned));
            Err(match reason {
                Some(r) => js_error!(Error: "the flow closed while waiting: {}", r),
                None => js_error!(Error: "the flow closed while waiting"),
            })
        }
        None => Err(js_error!(Error: "timed out after {}ms waiting", timeout.as_millis())),
    }
}

/// Waits for the next message with `tag` and returns it.
fn wait_tag(tag: &str, args: &[JsValue], ctx: &mut Context) -> JsResult<Value> {
    let timeout = timeout_arg(args, 0, ctx)?;
    let deadline = Instant::now() + timeout;
    loop {
        let line = next_line(deadline, timeout)?;
        if let Ok((t, body)) = parse_line(&line)
            && t == tag
        {
            return Ok(Value::Object(body));
        }
    }
}

// --- helpers -----------------------------------------------------------------

/// `signals()`: the current signals.
fn signals(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let v = with(|h| h.session.page.client.signals().clone());
    to_js(&v, ctx)
}

/// `signal(path)`: the value at a signal path such as `order.address`, or
/// `undefined`.
fn signal(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let path = string_arg(args, 0, "signal path", ctx)?;
    match with(|h| h.session.page.client.value(&path).cloned()) {
        Some(v) => to_js(&v, ctx),
        None => Ok(JsValue::undefined()),
    }
}

/// `metadata()`: what the server said about itself, `mode` included.
fn metadata(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let v = with(|h| serde_json::to_value(h.session.page.client.metadata()).unwrap_or_default());
    to_js(&v, ctx)
}

/// `tree()`: the whole resolved tree, as nested elements.
fn tree_fn(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let v = with(|h| serde_json::to_value(h.session.page.client.tree()).unwrap_or_default());
    to_js(&v, ctx)
}

/// `element(id)`: the element, or `null`.
fn element(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = string_arg(args, 0, "element id", ctx)?;
    match with(|h| h.session.page.client.get(&id).map(|r| element_json(&r, h))) {
        Some(v) => to_js(&v, ctx),
        None => Ok(JsValue::null()),
    }
}

/// `query(filter)`: the elements that pass a filter.
fn query(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let found = select(args.get_or_undefined(0), ctx)?;
    to_js(&Value::Array(found), ctx)
}

/// `queryOne(filter)`: the first element that passes a filter, or `null`.
fn query_one(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    match select(args.get_or_undefined(0), ctx)?.into_iter().next() {
        Some(v) => to_js(&v, ctx),
        None => Ok(JsValue::null()),
    }
}

/// `action(name, payload?, signals?)`: sends an action. It carries the
/// current signals, with `signals` merged into them.
fn action(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = string_arg(args, 0, "action name", ctx)?;
    let payload = from_js(args.get_or_undefined(1), ctx)?;
    let patch = match from_js(args.get_or_undefined(2), ctx)? {
        None | Some(Value::Null) => None,
        Some(p @ Value::Object(_)) => Some(p),
        Some(_) => return Err(js_error!(TypeError: "signals must be an object")),
    };
    with(|h| {
        let line = h.session.action_line(&name, payload, patch.as_ref());
        h.session.send(line);
    });
    Ok(JsValue::undefined())
}

/// Runs the handler of element `id` in attribute `event`, as a wallet
/// does. Returns whether it sent an action.
fn fire(id: &str, event: &str) -> JsResult<JsValue> {
    let sent = with(|h| {
        if !h.session.page.client.contains(id) {
            return Err(js_error!(Error: "no element {}", id));
        }
        Ok(h.session.trigger(id, event))
    })?;
    Ok(sent.into())
}

/// `trigger(id, event)`: runs one of the element's handlers, such as
/// `onSubmit`. Returns whether it sent an action.
fn trigger(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = string_arg(args, 0, "element id", ctx)?;
    let event = string_arg(args, 1, "event", ctx)?;
    fire(&id, &event)
}

/// `press(id)`: presses a button.
fn press(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = string_arg(args, 0, "element id", ctx)?;
    fire(&id, "onPress")
}

/// `input(id, value)`: edits a control: writes its bound signal and runs
/// its `onChange`. Returns whether that sent an action.
fn input(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let id = string_arg(args, 0, "element id", ctx)?;
    let value = from_js(args.get_or_undefined(1), ctx)?.unwrap_or(Value::Null);
    with(|h| {
        let client = &mut h.session.page.client;
        let Some(Value::String(path)) = client.attr(&id, "bind") else {
            return Err(js_error!(Error: "element {} is not bound to a signal", id));
        };
        client.write(&path, value);
        Ok(())
    })?;
    fire(&id, "onChange")
}

/// `write(path, value)`: writes a signal, as a bound control does. Nothing
/// is sent: the server sees it with the next action.
fn write(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let path = string_arg(args, 0, "signal path", ctx)?;
    let value = from_js(args.get_or_undefined(1), ctx)?.unwrap_or(Value::Null);
    with(|h| h.session.page.client.write(&path, value));
    Ok(JsValue::undefined())
}

/// `sleep(ms)`: waits, taking in what the server sends meanwhile.
fn sleep(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let ms = args.get_or_undefined(0).to_number(ctx)?.max(0.0) as u64;
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline && pump(deadline) {}
    Ok(JsValue::undefined())
}

/// `settle(ms = 300, maxMs?)`: waits until the server has sent nothing for
/// `ms`, for at most `maxMs`. Returns whether it went quiet.
fn settle(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let v = args.get_or_undefined(0);
    let ms = if v.is_undefined() {
        300.0
    } else {
        v.to_number(ctx)?.max(0.0)
    };
    let quiet = Duration::from_millis(ms as u64);
    let max = timeout_arg(args, 1, ctx)?;
    let settled = with(|h| h.session.settle(quiet, Instant::now() + max));
    Ok((settled == Settled::Quiet).into())
}

/// `invoice()`: the invoice to pay now (the latest), or `null`.
fn invoice(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let v = with(|h| {
        h.session
            .page
            .invoice()
            .map_or(Value::Null, |o| offer_json(&o.invoice, o.memo.as_deref()))
    });
    to_js(&v, ctx)
}

/// `invoices()`: every invoice received, oldest first.
fn invoices(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let v = with(|h| {
        Value::Array(
            h.session
                .page
                .invoices
                .iter()
                .map(|o| offer_json(&o.invoice, o.memo.as_deref()))
                .collect(),
        )
    });
    to_js(&v, ctx)
}

/// `invoiceRequests()`: every `invoicerequest` received, oldest first.
fn invoice_requests(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let v = with(|h| {
        Value::Array(
            h.session
                .page
                .requests
                .iter()
                .map(|r| r.to_json())
                .collect(),
        )
    });
    to_js(&v, ctx)
}

/// `waitInvoice(ms?)`: waits for the next `invoice` and returns it as
/// `invoice()` does.
fn wait_invoice(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let body = wait_tag("invoice", args, ctx)?;
    let raw = body.get("invoice").cloned().unwrap_or_default();
    let memo = body.get("memo").and_then(Value::as_str);
    to_js(&offer_json(&raw, memo), ctx)
}

/// `waitInvoiceRequest(ms?)`: waits for the next `invoicerequest` and
/// returns its body.
fn wait_invoice_request(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let body = wait_tag("invoicerequest", args, ctx)?;
    to_js(&body, ctx)
}

/// `sendInvoice(invoice)`: answers an `invoicerequest`.
fn send_invoice(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let Some(inv @ Value::Object(_)) = from_js(args.get_or_undefined(0), ctx)? else {
        return Err(js_error!(TypeError: "sendInvoice takes an invoice object"));
    };
    with(|h| h.session.send_invoice(inv));
    Ok(JsValue::undefined())
}

/// `decodeInvoice(invoice)`: what `copcat invoice --json` says of an invoice
/// object.
fn decode_invoice(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let raw = from_js(args.get_or_undefined(0), ctx)?.unwrap_or_default();
    to_js(&Summary::of(&raw).to_json(None), ctx)
}

/// `closeReason()`: `undefined` while the flow is open and the server has
/// not sent `close`, else its reason (`null` if it gave none).
fn close_reason(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let r = with(|h| h.session.page.close_reason().map(|r| r.map(str::to_owned)));
    match r {
        None => Ok(JsValue::undefined()),
        Some(None) => Ok(JsValue::null()),
        Some(Some(r)) => to_js(&Value::String(r), ctx),
    }
}

/// The arguments joined by spaces: strings as they are, anything else as
/// JSON.
fn line(args: &[JsValue], ctx: &mut Context) -> JsResult<String> {
    let mut out = Vec::with_capacity(args.len());
    for a in args {
        if let Some(s) = a.as_string() {
            out.push(s.to_std_string_escaped());
        } else if a.is_callable() || a.is_undefined() {
            out.push(a.display().to_string());
        } else {
            out.push(from_js(a, ctx)?.map_or_else(String::new, |v| compact(&v)));
        }
    }
    Ok(out.join(" "))
}

/// `print(...)`, `console.log(...)`.
fn print(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    crate::out(&format!("{}\n", line(args, ctx)?));
    Ok(JsValue::undefined())
}

/// `console.error(...)`.
fn eprint(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    eprintln!("{}", line(args, ctx)?);
    Ok(JsValue::undefined())
}

/// `printTree()`: prints the flow as `copcat tree` does.
fn print_tree(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let text = with(|h| view::snapshot(&h.session.page, &h.style));
    crate::out(&text);
    Ok(JsValue::undefined())
}

/// `printInvoice(invoice?)`: prints an invoice (the latest by default) as
/// `copcat invoice` does.
fn print_invoice(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let given = from_js(args.get_or_undefined(0), ctx)?;
    let text = with(|h| {
        let (raw, memo) = match given {
            // `invoice()`'s `{invoice, memo}` or a bare invoice.
            Some(v) if v.get("invoice").is_some_and(Value::is_object) => (
                v["invoice"].clone(),
                v.get("memo").and_then(Value::as_str).map(str::to_owned),
            ),
            Some(v) if !v.is_null() => (v, None),
            _ => match h.session.page.invoice() {
                Some(o) => (o.invoice.clone(), o.memo.clone()),
                None => return Err(js_error!(Error: "no invoice received")),
            },
        };
        Ok(Summary::of(&raw).render(memo.as_deref(), "", &h.style))
    })?;
    crate::out(&text);
    Ok(JsValue::undefined())
}
