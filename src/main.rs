//! cop: debugs Cassis Order Protocol endpoints from the command line.
//!
//! ```text
//! cop <address>                     the messages of a flow, as a transcript
//! cop tree <address>                its metadata, signals, tree and invoices
//! cop action|press <address> ...    fires an action, or presses a button
//! cop run <address> [script.js]     drives a flow with a script
//! cop parse|build                   reads and writes cop:// URIs
//! cop invoice [input]               decodes and verifies Cassis invoices
//! cop replay|check <transcript>     renders or lints a recorded flow
//! cop serve <transcript>            plays a scripted merchant
//! ```

mod check;
mod invoice;
mod link;
mod page;
mod script;
mod serve;
mod session;
mod style;
mod transcript;
mod uri;
mod view;
mod web;

use clap::{Args, Parser, Subcommand, ValueEnum};
use hypereffect::message::parse_line;
use hypereffect::{Component, Map, ServerMessage, Value, json, serde_json};
use session::{Echo, Opening, Session, Settled, Update};
use std::collections::HashSet;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};
use style::Style;
use uri::CopUri;

#[derive(Parser)]
#[command(
    name = "cop",
    version,
    about = "Debugs Cassis Order Protocol endpoints: URIs, message streams, hypereffect trees and invoices",
    long_about = "Debugs Cassis Order Protocol (COP) endpoints: URIs, message streams, \
        hypereffect trees and Cassis invoices.\n\n\
        Without a command, connects to the address as a wallet would (iroh, ALPN \
        cassis-order), sends the hello and prints every message both ways as a \
        transcript: `> ` lines are the client's, `< ` lines the server's. Addresses are \
        cop:// URIs or web addresses that publish one.",
    after_long_help = OVERVIEW,
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    target: Target,
    #[command(flatten)]
    stream: StreamArgs,
    #[command(flatten)]
    global: Global,
}

const OVERVIEW: &str = "\
Commands at a glance:
  cop <address>                         stream a flow (same as `cop stream`)
  cop tree <address> --once             what a wallet would show, once it settles
  cop press <address> pay --input address='221B Baker St' --tree
  cop run <address> -e 'press(\"margherita\"); printInvoice(waitInvoice())'
  cop parse 'cop://…#table=12'          validate a URI and show the hello it makes
  cop build <endpoint-id> -p name='Luigi' --qr
  cop invoice invoice.json              decode and verify a Cassis invoice
  cop stream <address> --record flow.cop; cop check flow.cop; cop replay flow.cop
  cop serve flow.cop --qr               play a scripted merchant to wallets

Transcripts are what `cop stream` prints and --record saves: one message a line, `> `
for the client and `< ` for the server, `#` comments, and `@sleep MS` pauses (for
serve). Lines without a prefix take the direction of their tag.

For agents: --json makes every command print JSON (one object a line when it
streams). Diagnostics go to stderr, prefixed `cop:`.

Exit status: 0 on success; 1 on errors (connection failures, invalid URIs, invoices
that do not verify, transcripts with errors); 2 on bad usage.";

#[derive(Subcommand)]
enum Command {
    /// Prints every message of a flow, both ways, as a transcript
    #[command(after_help = STREAM_EXAMPLES)]
    Stream {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        stream: StreamArgs,
    },
    /// Prints what a wallet would show: metadata, signals, tree, invoices; again after every change
    #[command(after_help = TREE_EXAMPLE)]
    Tree {
        #[command(flatten)]
        target: Target,
        /// Close the connection after this long without updates, in milliseconds
        #[arg(long, value_name = "MS")]
        idle: Option<u64>,
        /// Wait this long without changes before printing again, in milliseconds
        #[arg(long, value_name = "MS", default_value_t = 1000)]
        debounce: u64,
        /// Print once, when the server has gone quiet, and exit
        #[arg(long)]
        once: bool,
        /// With --once: how long the server must be quiet, in milliseconds
        #[arg(long, value_name = "MS", default_value_t = 500)]
        settle: u64,
    },
    /// Fires an action, then prints it and what the server sends back
    #[command(after_help = ACTION_EXAMPLES)]
    Action {
        #[command(flatten)]
        target: Target,
        /// The name of the action
        name: String,
        /// The payload, as JSON
        #[arg(long, value_name = "JSON")]
        payload: Option<String>,
        /// Signals to send, as a JSON object merged into the current ones
        #[arg(long, value_name = "JSON")]
        signals: Option<String>,
        #[command(flatten)]
        after: After,
    },
    /// Presses a button (or runs another handler) as a wallet would, after editing controls
    #[command(after_help = PRESS_EXAMPLES)]
    Press {
        #[command(flatten)]
        target: Target,
        /// The element's id
        id: String,
        /// The handler to run
        #[arg(long, value_name = "HANDLER", default_value = "onPress")]
        event: String,
        /// Edit a bound control first: ID=VALUE, VALUE as JSON or else as a string (repeatable)
        #[arg(long = "input", value_name = "ID=VALUE", value_parser = key_value)]
        inputs: Vec<(String, String)>,
        #[command(flatten)]
        after: After,
    },
    /// Runs a JavaScript script against a flow
    #[command(after_help = RUN_EXAMPLES, after_long_help = SCRIPT_HELP)]
    Run {
        #[command(flatten)]
        target: Target,
        /// The script file; standard input if absent or `-`
        #[arg(conflicts_with = "eval")]
        script: Option<PathBuf>,
        /// The script itself
        #[arg(short, long, value_name = "CODE")]
        eval: Option<String>,
        /// Before and after the script, wait for the server to send nothing for this long, in milliseconds
        #[arg(long, value_name = "MS", default_value_t = 300)]
        settle: u64,
        /// How long wait(), waitInvoice() and settle() wait by default, in milliseconds
        #[arg(long, value_name = "MS", default_value_t = 10_000)]
        timeout: u64,
    },
    /// Validates a cop:// URI or web address and explains it, with the hello it makes
    #[command(after_help = PARSE_EXAMPLES)]
    Parse {
        /// A cop:// URI, or a web address that publishes one
        address: String,
        /// Do not fetch web addresses
        #[arg(long)]
        no_fetch: bool,
        #[command(flatten)]
        hello: HelloArgs,
        /// Also print the URI as a QR code
        #[arg(long)]
        qr: bool,
    },
    /// Builds a cop:// URI from an endpoint id, a relay and parameters
    #[command(after_help = BUILD_EXAMPLES)]
    Build {
        /// The server's iroh endpoint id (64 hex characters)
        endpoint: String,
        /// A custom iroh relay URL
        #[arg(long, value_name = "URL")]
        relay: Option<String>,
        /// A fragment parameter, KEY=VALUE (repeatable)
        #[arg(short = 'p', long = "param", value_name = "KEY=VALUE", value_parser = key_value)]
        params: Vec<(String, String)>,
        /// Also print the URI as a QR code
        #[arg(long)]
        qr: bool,
        /// Also print the <link> tag a web page publishes it with
        #[arg(long)]
        html: bool,
    },
    /// Decodes Cassis invoices and verifies their signatures
    #[command(after_help = INVOICE_EXAMPLES)]
    Invoice {
        /// Files, `-` for standard input, or literal JSON or message lines; standard input if none
        inputs: Vec<String>,
    },
    /// Replays a transcript into a client and prints the page it leaves
    #[command(after_help = REPLAY_EXAMPLES)]
    Replay {
        /// The transcript; `-` for standard input
        file: PathBuf,
        /// Print the page after every server message, not only at the end
        #[arg(long)]
        each: bool,
        /// Stop after this line of the transcript
        #[arg(long, value_name = "LINE")]
        until: Option<usize>,
        /// Print the client's diagnostics, such as rejected elements
        #[arg(short, long)]
        verbose: bool,
    },
    /// Checks a transcript against the COP spec
    #[command(after_help = CHECK_EXAMPLES)]
    Check {
        /// The transcript; `-` for standard input
        file: PathBuf,
        /// Fail on warnings too
        #[arg(long)]
        strict: bool,
    },
    /// Serves a transcript as a scripted merchant, for testing wallets
    #[command(after_help = SERVE_EXAMPLES)]
    Serve {
        /// The transcript to play to every client
        file: PathBuf,
        /// Serve as this ed25519 secret key, in hex, for a stable endpoint id
        #[arg(long, value_name = "HEX", value_parser = secret_key)]
        key: Option<[u8; 32]>,
        /// A fragment parameter for the printed URI, KEY=VALUE (repeatable)
        #[arg(short = 'p', long = "param", value_name = "KEY=VALUE", value_parser = key_value)]
        params: Vec<(String, String)>,
        /// Exit after the first flow
        #[arg(long)]
        once: bool,
        /// Close the connection when the script is done, instead of waiting for the client
        #[arg(long)]
        hangup: bool,
        /// Also print the URI as a QR code, on stderr
        #[arg(long)]
        qr: bool,
    },
}

const STREAM_EXAMPLES: &str = "\
Examples:
  cop stream 'cop://5f1c…0a9b#name=Luigi%27s%20Pizza'
  cop stream example.com/order/123 --idle 3000 --record flow.cop
  cop stream <uri> --until invoice --json
  cop stream <uri> --session f1K9xQ2v8Tn4LmZp7Rb3wq --force-resume
  cop stream <uri> --type button --actions";

const TREE_EXAMPLE: &str = "\
Example:
  $ cop tree 'cop://5f1c…0a9b#name=Luigi%27s%20Pizza' --once
  metadata \"Luigi's Pizza\" — \"Wood-fired pizza, Via Roma 12\"
  {}
  tree {
    text title \"Choose your pizza\" h1
    list menu
      button margherita \"Margherita — $8.50\"  ⚡ onPress → selectPizza {\"pizza\":\"margherita\"}
      button pepperoni \"Pepperoni — $10.00\"  ⚡ onPress → selectPizza {\"pizza\":\"pepperoni\"}
  }

Each line is an element's type and id, what it shows, the signal it is bound to with
its value, and what its handlers do. Below the tree: the invoice to pay (amount,
expiry, networks, whether its signatures verify, memo), the invoicerequests, and
how the flow ended.";

const ACTION_EXAMPLES: &str = "\
Examples:
  cop action <uri> selectPizza --payload '{\"pizza\":\"pepperoni\"}'
  cop action <uri> pay --signals '{\"order\":{\"address\":\"221B Baker St\"}}' --tree";

const PRESS_EXAMPLES: &str = "\
Examples:
  cop press <uri> pepperoni
  cop press <uri> pay --input address='221B Baker St' --tree
  cop press <uri> search --event onSubmit --input q='\"margherita\"'

Unlike `action`, this runs the element's own handler: its payload, patches and
indicator, and nothing at all if the element is hidden or disabled. Only the
first page is reachable: use `run` for flows of several steps.";

macro_rules! run_examples {
    () => {
        "\
Examples:
  cop run <uri> order.js
  cop run <uri> -e 'press(\"pepperoni\"); settle(); printTree()'
  cop run <uri> -e '
    press(\"pepperoni\"); settle();
    press(\"chili\"); settle();
    input(\"address\", \"221B Baker St\");
    press(\"pay\");
    printInvoice(waitInvoice())'
  cop run <refund-uri> -e \"
    const r = waitInvoiceRequest();
    print(r.amount, r.networks);
    sendInvoice($(cat my-invoice.json))\""
    };
}

const RUN_EXAMPLES: &str = run_examples!();

const SCRIPT_HELP: &str = concat!(
    "\
The script runs once the server has sent its first page and gone quiet. Its helpers:

  uri, session                        the URI and the hello's session token
  signals(), signal(path)             the current signals, or one (\"order.address\")
  metadata()                          what the server says about itself, mode included
  tree()                              the resolved tree
  element(id)                         one element, or null
  query(filter), queryOne(filter)     elements passing a filter: a function of the
                                      element, or an object such as
                                      {type: \"button\", action: \"pay\"}
  wait(fn, ms?)                       calls fn(message) for every message the server
                                      sends from now on, until it returns true;
                                      returns that message: {kind, tag, body, ...}
                                      with id/element for elements, signals for
                                      signals and summary for invoices
  action(name, payload?, signals?)    sends an action; `signals` is merged in
  press(id)                           presses a button
  input(id, value)                    edits a control: writes its signal, runs onChange
  trigger(id, event)                  runs any handler, such as trigger(id, \"onSubmit\")
  write(path, value)                  writes a signal without sending anything
  sleep(ms)                           waits, taking in what the server sends
  settle(ms = 300, max?)              waits until the server is quiet for `ms`
  invoice(), invoices()               the invoice to pay (the latest) and all of them:
                                      {invoice, memo, summary}
  invoiceRequests()                   the invoicerequests received
  waitInvoice(ms?)                    waits for the next invoice, returns it
  waitInvoiceRequest(ms?)             waits for the next invoicerequest, returns it
  sendInvoice(invoice)                answers an invoicerequest with an invoice object
  decodeInvoice(invoice)              what `cop invoice --json` says of an invoice
  closeReason()                       undefined while open, else the close reason
  print(...), console.log(...)        prints, objects as JSON
  printTree()                         prints the flow as `cop tree` does
  printInvoice(inv?)                  prints an invoice as `cop invoice` does

Each message reaches wait() already applied, so the other helpers show the state
right after it. Waits throw after their timeout (--timeout by default) or if the flow
ends, and an uncaught exception fails the run. There is no file access: pass data in
with -e, e.g. -e \"const inv = $(cat inv.json); sendInvoice(inv)\".

",
    run_examples!()
);

const PARSE_EXAMPLES: &str = "\
Examples:
  cop parse 'cop://5f1c9d0a27e4b6c38d2f71a09e4b5c6d8a3f2e1b0c9d8e7f6a5b4c3d2e1f0a9b#table=12'
  cop parse example.com/order/123
  cop parse <uri> --session f1K9xQ2v8Tn4LmZp7Rb3wq --force-resume --json";

const BUILD_EXAMPLES: &str = "\
Examples:
  cop build 5f1c9d0a27e4b6c38d2f71a09e4b5c6d8a3f2e1b0c9d8e7f6a5b4c3d2e1f0a9b -p table=12 -p \"name=Luigi's Pizza\"
  cop build <endpoint-id> --relay https://relay.example.com --qr --html";

const INVOICE_EXAMPLES: &str = "\
Examples:
  cop invoice invoice.json
  cop invoice flow.cop                  every invoice and invoicerequest in a transcript
  cop stream <uri> --until invoice | cop invoice
  cop invoice '{\"payment_hash\":\"…\",\"amount_msat\":10750000,…}' --json

Inputs may be a bare Cassis invoice, an `invoice` message body ({\"invoice\":…,
\"memo\":…}), a message line, or a transcript. Amount and payee are read from the
invoice, never from the memo. Exits 1 if any invoice fails to decode or verify.";

const REPLAY_EXAMPLES: &str = "\
Examples:
  cop replay flow.cop
  cop replay flow.cop --each
  cop replay flow.cop --until 12 --json";

const CHECK_EXAMPLES: &str = "\
Examples:
  cop check flow.cop
  cop stream <uri> --idle 3000 | cop check -
  cop check flow.cop --strict --json

Checks the hello (session entropy, params, force_resume), message directions and
order, metadata mode, invoice and invoicerequest fields, Cassis invoice signatures,
that client invoices match the request's amount and networks, session_not_found
handling, and hypereffect elements.";

const SERVE_EXAMPLES: &str = "\
Example:
  $ cat till.cop
  < metadata {\"name\":\"Luigi's Pizza\",\"description\":\"Till 2\"}
  < element {\"id\":\"t\",\"type\":\"text\",\"text\":\"Pay at the till\"}
  < element {\"id\":\"ok\",\"type\":\"button\",\"label\":\"Pay\",\"onPress\":{\"action\":\"pay\"}}
  > action {\"name\":\"pay\"}
  @sleep 300
  < invoice {\"invoice\":{…},\"memo\":\"1 × flat white\"}
  $ cop serve till.cop -p till=2 --qr

The first line printed is `# cop://…`, the URI to connect to. `<` lines are sent,
`>` lines are waited for (same tag; for actions, same name and payload if given),
and the hello is always waited for first. What clients send is printed as `>` lines.";

#[derive(Args)]
struct Global {
    /// Print JSON instead of text (one object a line when streaming)
    #[arg(long, global = true)]
    json: bool,
    /// When to color the output
    #[arg(long, global = true, value_enum, default_value_t = ColorWhen::Auto)]
    color: ColorWhen,
    /// Cut texts longer than this, in characters
    #[arg(long, global = true, value_name = "N", default_value_t = 48)]
    max_text: usize,
}

#[derive(Clone, Copy, ValueEnum)]
enum ColorWhen {
    Auto,
    Always,
    Never,
}

impl Global {
    fn style(&self) -> Style {
        let color = match self.color {
            ColorWhen::Always => true,
            ColorWhen::Never => false,
            ColorWhen::Auto => {
                std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
            }
        };
        Style {
            color: color && !self.json,
            max_text: self.max_text.max(2),
        }
    }
}

/// How the hello is made.
#[derive(Args)]
struct HelloArgs {
    /// The session token to send, e.g. to resume a flow; a fresh 128-bit one if absent
    #[arg(long, value_name = "TOKEN")]
    session: Option<String>,
    /// Ask the server to resume --session or close with session_not_found
    #[arg(long)]
    force_resume: bool,
    /// Add or replace a URI parameter, KEY=VALUE (repeatable)
    #[arg(short = 'p', long = "param", value_name = "KEY=VALUE", value_parser = key_value)]
    params: Vec<(String, String)>,
    /// Send no params, whatever the URI has
    #[arg(long)]
    no_params: bool,
    /// Send this JSON object as the hello instead, to test how servers handle odd ones
    #[arg(long, value_name = "JSON", conflicts_with_all = ["session", "force_resume", "params", "no_params"])]
    hello: Option<String>,
}

#[derive(Args)]
struct Target {
    /// A cop:// URI, or a web address that publishes one
    #[arg(required = true)]
    address: Option<String>,
    #[command(flatten)]
    hello: HelloArgs,
    /// Connect through this iroh relay instead of the URI's
    #[arg(long, value_name = "URL")]
    relay: Option<String>,
    /// Connect with this ed25519 secret key, in hex, instead of a fresh one.
    /// Missing trailing bytes are zeroes, so `--key 01` is 01 followed by 31 zero bytes
    #[arg(long, value_name = "HEX", value_parser = secret_key)]
    key: Option<[u8; 32]>,
    /// Save the flow as a transcript, both ways
    #[arg(long, value_name = "FILE")]
    record: Option<PathBuf>,
    /// Print connection details and the client's diagnostics, such as rejected elements
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Args, Default)]
struct StreamArgs {
    /// Close the connection after this long without messages, in milliseconds
    #[arg(long, value_name = "MS")]
    idle: Option<u64>,
    /// Exit after the first message with this tag, such as invoice (repeatable)
    #[arg(long, value_name = "TAG")]
    until: Vec<String>,
    /// Print only messages with this tag (repeatable)
    #[arg(long = "tag", value_name = "TAG")]
    tags: Vec<String>,
    /// Print only the elements with this id (repeatable)
    #[arg(long = "id", value_name = "ID")]
    ids: Vec<String>,
    /// Print only the elements of this type (repeatable)
    #[arg(long = "type", value_name = "TYPE", value_parser = component)]
    types: Vec<Component>,
    /// Print only the elements that can fire actions
    #[arg(long)]
    actions: bool,
    /// Do not explain invoices in `#` comments below them
    #[arg(long)]
    raw: bool,
}

/// What `action` and `press` do once they have fired.
#[derive(Args)]
struct After {
    /// Before firing, wait for the server to send nothing for this long, in milliseconds
    #[arg(long, value_name = "MS", default_value_t = 300)]
    settle: u64,
    /// Close the connection after this long without messages, in milliseconds
    #[arg(long, value_name = "MS", default_value_t = 1000)]
    idle: u64,
    /// Print the page at the end, as `cop tree` does
    #[arg(long)]
    tree: bool,
}

fn key_value(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some(("", _)) => Err("the key is empty".into()),
        Some((k, v)) => Ok((k.to_string(), v.to_string())),
        None => Err(format!("expected KEY=VALUE, got {s:?}")),
    }
}

/// A secret key of up to 32 bytes in hex, padded with zero bytes.
fn secret_key(s: &str) -> Result<[u8; 32], String> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if !s.len().is_multiple_of(2) {
        return Err("expected whole bytes: an even number of hex digits".into());
    }
    if s.len() > 64 {
        return Err("a secret key is at most 32 bytes (64 hex digits)".into());
    }
    let mut key = [0u8; 32];
    for (i, pair) in s.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(pair).map_err(|_| "not hex")?;
        key[i] = u8::from_str_radix(pair, 16).map_err(|_| format!("not hex: {pair}"))?;
    }
    Ok(key)
}

fn component(s: &str) -> Result<Component, String> {
    Component::from_name(s).ok_or_else(|| {
        let all: Vec<&str> = Component::ALL.iter().map(|c| c.name()).collect();
        format!("expected one of {}", all.join(", "))
    })
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let g = &cli.global;
    let result = match cli.command {
        None => stream_cmd(&cli.target, &cli.stream, g),
        Some(Command::Stream { target, stream }) => stream_cmd(&target, &stream, g),
        Some(Command::Tree {
            target,
            idle,
            debounce,
            once,
            settle,
        }) => tree_cmd(&target, idle, debounce, once, settle, g),
        Some(Command::Action {
            target,
            name,
            payload,
            signals,
            after,
        }) => action_cmd(&target, &name, payload, signals, &after, g),
        Some(Command::Press {
            target,
            id,
            event,
            inputs,
            after,
        }) => press_cmd(&target, &id, &event, &inputs, &after, g),
        Some(Command::Run {
            target,
            script,
            eval,
            settle,
            timeout,
        }) => run_cmd(&target, script, eval, settle, timeout, g),
        Some(Command::Parse {
            address,
            no_fetch,
            hello,
            qr,
        }) => parse_cmd(&address, no_fetch, &hello, qr, g),
        Some(Command::Build {
            endpoint,
            relay,
            params,
            qr,
            html,
        }) => build_cmd(&endpoint, relay, params, qr, html, g),
        Some(Command::Invoice { inputs }) => invoice_cmd(&inputs, g),
        Some(Command::Replay {
            file,
            each,
            until,
            verbose,
        }) => replay_cmd(&file, each, until, verbose, g),
        Some(Command::Check { file, strict }) => check_cmd(&file, strict, g),
        Some(Command::Serve {
            file,
            key,
            params,
            once,
            hangup,
            qr,
        }) => (|| {
            let script = transcript::parse(&read_input(&file)?)?;
            serve::serve(serve::Opts {
                script,
                key,
                params,
                once,
                hangup,
                json: g.json,
                qr,
            })
        })(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("cop: {e}");
            }
            ExitCode::FAILURE
        }
    }
}

/// Writes to standard output. A closed pipe, as in `cop … | head`, ends
/// the program quietly.
pub fn out(text: &str) {
    let mut o = std::io::stdout().lock();
    if let Err(e) = o.write_all(text.as_bytes()).and_then(|()| o.flush()) {
        if e.kind() != std::io::ErrorKind::BrokenPipe {
            eprintln!("cop: cannot write the output: {e}");
            std::process::exit(1);
        }
        std::process::exit(0);
    }
}

/// A message line as JSON: `{t, dir, tag, body}`, with the decoded
/// invoice for invoices.
pub fn line_json(t: f64, dir: &str, line: &str) -> Value {
    match parse_line(line) {
        Ok((tag, body)) => {
            let mut v = json!({ "t": round(t), "dir": dir, "tag": tag, "body": body });
            if tag == "invoice"
                && let Some(raw) = body.get("invoice").filter(|i| i.is_object())
            {
                let memo = body.get("memo").and_then(Value::as_str);
                v["invoice"] = invoice::Summary::of(raw).to_json(memo);
            }
            v
        }
        Err(e) => json!({ "t": round(t), "dir": dir, "line": line, "error": e.to_string() }),
    }
}

fn round(t: f64) -> f64 {
    (t * 1000.0).round() / 1000.0
}

/// A QR code of `text` for the terminal.
pub fn qr(text: &str) -> Result<String, String> {
    use qrcode::render::unicode::Dense1x2;
    let code = qrcode::QrCode::new(text.as_bytes()).map_err(|e| format!("QR code: {e}"))?;
    let mut s = code
        .render::<Dense1x2>()
        .dark_color(Dense1x2::Light)
        .light_color(Dense1x2::Dark)
        .build();
    s.push('\n');
    Ok(s)
}

fn read_input(path: &Path) -> Result<String, String> {
    if path.as_os_str() == "-" {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| format!("cannot read standard input: {e}"))?;
        return Ok(s);
    }
    std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

fn json_arg(s: &str, flag: &str) -> Result<Value, String> {
    serde_json::from_str(s).map_err(|e| format!("{flag} is not JSON: {e}"))
}

// --- addresses ----------------------------------------------------------------

/// A `cop://` URI, or the one a web address publishes.
fn resolve(
    address: &str,
    fetch: bool,
    verbose: bool,
) -> Result<(CopUri, Option<web::Resolution>), String> {
    if uri::is_cop(address) {
        return CopUri::parse(address)
            .map(|u| (u, None))
            .map_err(|e| format!("invalid cop:// URI: {e}"));
    }
    if !fetch {
        return Err(format!(
            "{address:?} is not a cop:// URI (and --no-fetch forbids fetching it as a web address)"
        ));
    }
    let r = web::resolve(address)?;
    if verbose {
        eprintln!("cop: {} publishes {}", r.final_url, r.href);
    }
    Ok((r.uri.clone(), Some(r)))
}

/// The URI with the parameters of `h` applied, the session token and the
/// hello.
fn make_hello(
    mut uri: CopUri,
    h: &HelloArgs,
) -> Result<(CopUri, String, Map<String, Value>), String> {
    if let Some(raw) = &h.hello {
        let Value::Object(hello) = json_arg(raw, "--hello")? else {
            return Err("--hello must be a JSON object".into());
        };
        let token = hello
            .get("session")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        return Ok((uri, token, hello));
    }
    if h.no_params {
        uri.params.clear();
    }
    for (k, v) in &h.params {
        uri.set_param(k.clone(), v.clone());
    }
    let token = h.session.clone().unwrap_or_else(uri::new_session);
    if h.force_resume && h.session.is_none() {
        eprintln!(
            "cop: --force-resume with a fresh session: the server should close with session_not_found"
        );
    }
    let hello = session::hello(&uri, &token, h.force_resume);
    Ok((uri, token, hello))
}

fn connect(t: &Target, echo: Echo) -> Result<Session, String> {
    let address = t.address.as_deref().unwrap_or_default();
    let (mut uri, _) = resolve(address, true, t.verbose)?;
    if let Some(r) = &t.relay {
        uri.relay = Some(r.clone());
    }
    let (uri, token, hello) = make_hello(uri, &t.hello)?;
    let record = match &t.record {
        Some(p) => {
            let mut f = std::fs::File::create(p)
                .map_err(|e| format!("cannot create {}: {e}", p.display()))?;
            let _ = writeln!(f, "# {uri}");
            Some(f)
        }
        None => None,
    };
    Ok(Session::open(Opening {
        uri,
        hello,
        token,
        key: t.key,
        record,
        verbose: t.verbose,
        echo,
    }))
}

fn echo(g: &Global) -> Echo {
    if g.json { Echo::Json } else { Echo::Text }
}

/// How long the server has to send its first message.
const FIRST: Duration = Duration::from_secs(45);

/// How much longer than its quiet time a settle waits for a busy server,
/// such as one with a running clock.
const SETTLE: Duration = Duration::from_secs(3);

/// Waits until the server has sent its first messages and then nothing for
/// `quiet` (or for at most [`SETTLE`] more).
fn ready(s: &mut Session, quiet: Duration) -> Result<(), String> {
    match s.next(Some(Instant::now() + FIRST)) {
        None => return Err(format!("the server sent nothing in {}s", FIRST.as_secs())),
        Some(Update::Closed(r)) => return Err(closed_before(s, r)),
        Some(Update::Lines(_)) => {}
    }
    if s.settle(quiet, Instant::now() + quiet + SETTLE) == Settled::Closed {
        let Some(Update::Closed(r)) = s.next(None) else {
            unreachable!("the flow has closed")
        };
        return Err(closed_before(s, r));
    }
    Ok(())
}

fn closed_before(s: &Session, reason: Option<String>) -> String {
    match (s.page.close_reason(), reason) {
        (Some(Some(r)), _) => format!("the server closed the flow: {r}"),
        (_, Some(r)) => r,
        _ => "the server closed the flow before it was ready".into(),
    }
}

/// The end of a flow: an error if the transport failed.
fn closed(s: &Session, reason: Option<String>, g: &Global) -> Result<(), String> {
    let server = s.page.close_reason().map(|r| r.map(str::to_owned));
    if g.json {
        out(&format!(
            "{}\n",
            json!({
                "t": round(s.elapsed()),
                "event": "closed",
                "close": server.clone().map(|r| json!({"reason": r})),
                "error": reason,
            })
        ));
    }
    match (server, reason) {
        (_, Some(r)) => Err(format!("the flow ended: {r}")),
        (Some(Some(r)), None) => {
            if r == "session_not_found" {
                eprintln!("cop: the server does not know this session (session_not_found)");
            } else {
                eprintln!("cop: the server closed the flow: {r}");
            }
            Ok(())
        }
        _ => {
            eprintln!("cop: the server closed the flow");
            Ok(())
        }
    }
}

// --- stream -------------------------------------------------------------------

fn stream_cmd(t: &Target, a: &StreamArgs, g: &Global) -> Result<(), String> {
    let mut s = connect(t, echo(g))?;
    stream(&mut s, a, a.idle.map(Duration::from_millis), g)
}

/// Prints the messages the server sends, as `< ` lines (or JSON), until it
/// closes, is quiet for `idle`, or sends a tag in `--until`.
fn stream(
    s: &mut Session,
    a: &StreamArgs,
    idle: Option<Duration>,
    g: &Global,
) -> Result<(), String> {
    let st = g.style();
    let filtering = !a.ids.is_empty() || !a.types.is_empty() || a.actions || !a.tags.is_empty();
    // The elements printed, so their removals are printed too.
    let mut shown = HashSet::new();
    loop {
        let deadline = idle.map(|d| Instant::now() + d);
        let lines = match s.next(deadline) {
            None => return Ok(()),
            Some(Update::Closed(r)) => return closed(s, r, g),
            Some(Update::Lines(lines)) => lines,
        };
        for line in lines {
            let parsed = parse_line(&line).ok();
            let tag = parsed.as_ref().map_or("", |(t, _)| *t).to_string();
            let print = match ServerMessage::parse(&line) {
                _ if !filtering => true,
                _ if !a.tags.is_empty() && !a.tags.contains(&tag) => tag == "close",
                _ if a.ids.is_empty() && a.types.is_empty() && !a.actions => true,
                Ok(ServerMessage::Element(e)) => {
                    let ok = (a.ids.is_empty() || a.ids.contains(&e.id))
                        && (a.types.is_empty() || a.types.contains(&e.component))
                        && (!a.actions || view::fires_actions(e.component, &e.attrs));
                    if ok {
                        shown.insert(e.id.clone());
                    }
                    ok
                }
                Ok(ServerMessage::Remove { id }) => shown.remove(&id) || a.ids.contains(&id),
                // It ends the flow, whatever the filters.
                Ok(ServerMessage::Close(_)) => true,
                // Invoices are what COP is about: element filters keep them.
                Err(_) => matches!(tag.as_str(), "invoice" | "invoicerequest"),
                Ok(_) => false,
            };
            if print {
                if g.json {
                    out(&format!("{}\n", line_json(s.elapsed(), "in", &line)));
                } else {
                    out(&format!("< {line}\n"));
                    if !a.raw {
                        out(&explain(&line, &st));
                    }
                }
            }
            if a.until.contains(&tag) {
                return Ok(());
            }
        }
    }
}

/// `#` comments below a COP message, saying what it asks for.
fn explain(line: &str, st: &Style) -> String {
    let Ok((tag, body)) = parse_line(line) else {
        return String::new();
    };
    let comment = |text: String| {
        text.lines()
            .map(|l| format!("{}\n", st.dim(&format!("#   {l}"))))
            .collect::<String>()
    };
    let plain = Style {
        color: false,
        ..*st
    };
    match tag {
        "invoice" => match body.get("invoice").filter(|i| i.is_object()) {
            Some(raw) => comment(invoice::Summary::of(raw).render(
                body.get("memo").and_then(Value::as_str),
                "",
                &plain,
            )),
            None => comment("ignored: no \"invoice\" object".into()),
        },
        "invoicerequest" => {
            let networks: Vec<String> = body
                .get("networks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|n| n.as_str().map(str::to_owned))
                .collect();
            match body.get("amount").and_then(Value::as_u64) {
                Some(amount) if !networks.is_empty() => comment(view::request_line(
                    amount,
                    &networks,
                    body.get("memo").and_then(Value::as_str),
                    body.get("expiresIn").and_then(Value::as_u64),
                    &plain,
                )),
                _ => comment("ignored: it needs an integer amount and a non-empty networks".into()),
            }
        }
        _ => String::new(),
    }
}

// --- tree ---------------------------------------------------------------------

fn tree_cmd(
    t: &Target,
    idle: Option<u64>,
    debounce: u64,
    once: bool,
    settle: u64,
    g: &Global,
) -> Result<(), String> {
    let mut s = connect(t, Echo::Off)?;
    let st = g.style();
    if once {
        ready(&mut s, Duration::from_millis(settle))?;
        show(&s, &st, g, false);
        return Ok(());
    }
    let debounce = Duration::from_millis(debounce);
    let idle = idle.map(Duration::from_millis);
    let mut printed = false;
    // The first and the last change not printed yet.
    let mut pending: Option<(Instant, Instant)> = None;
    let mut quiet_until = idle.map(|d| Instant::now() + d);
    loop {
        let due = pending.map(|(first, last)| (last + debounce).min(first + 5 * debounce));
        let deadline = due.into_iter().chain(quiet_until).min();
        let update = s.next(deadline);
        let now = Instant::now();
        match update {
            Some(Update::Lines(_)) => {
                pending = Some((pending.map_or(now, |(first, _)| first), now));
                quiet_until = idle.map(|d| now + d);
            }
            Some(Update::Closed(r)) => {
                show(&s, &st, g, printed);
                return closed(&s, r, g);
            }
            None => {}
        }
        let due = pending.map(|(first, last)| (last + debounce).min(first + 5 * debounce));
        if due.is_some_and(|d| now >= d) {
            show(&s, &st, g, printed);
            printed = true;
            pending = None;
        }
        if quiet_until.is_some_and(|d| now >= d) {
            if pending.is_some() {
                show(&s, &st, g, printed);
            }
            return Ok(());
        }
    }
}

/// Prints the flow as it is now, after a `── +1.2s` rule (or as one JSON
/// object).
fn show(s: &Session, st: &Style, g: &Global, gap: bool) {
    if g.json {
        let mut v = view::snapshot_json(&s.page);
        v["t"] = round(s.elapsed()).into();
        v["session"] = s.token.clone().into();
        out(&format!("{v}\n"));
        return;
    }
    let rule = st.dim(&format!("── +{:.1}s", s.elapsed()));
    let gap = if gap { "\n" } else { "" };
    out(&format!("{gap}{rule}\n{}", view::snapshot(&s.page, st)));
}

// --- action, press ------------------------------------------------------------

fn action_cmd(
    t: &Target,
    name: &str,
    payload: Option<String>,
    signals: Option<String>,
    after: &After,
    g: &Global,
) -> Result<(), String> {
    let payload = payload
        .as_deref()
        .map(|p| json_arg(p, "--payload"))
        .transpose()?;
    let signals = signals
        .as_deref()
        .map(|p| json_arg(p, "--signals"))
        .transpose()?;
    if signals.as_ref().is_some_and(|s| !s.is_object()) {
        return Err("--signals must be a JSON object".into());
    }
    let mut s = connect(t, Echo::Off)?;
    ready(&mut s, Duration::from_millis(after.settle))?;
    s.echo = echo(g);
    let line = s.action_line(name, payload, signals.as_ref());
    s.send(line);
    finish(&mut s, after, g)
}

fn press_cmd(
    t: &Target,
    id: &str,
    event: &str,
    inputs: &[(String, String)],
    after: &After,
    g: &Global,
) -> Result<(), String> {
    let mut s = connect(t, Echo::Off)?;
    ready(&mut s, Duration::from_millis(after.settle))?;
    s.echo = echo(g);
    for (input, raw) in inputs {
        let value = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.clone()));
        let Some(Value::String(path)) = s.page.client.attr(input, "bind") else {
            return Err(match s.page.client.contains(input) {
                true => format!("element {input} is not bound to a signal"),
                false => format!("no element {input}"),
            });
        };
        s.page.client.write(&path, value);
        s.trigger(input, "onChange");
    }
    if !s.page.client.contains(id) {
        return Err(format!(
            "no element {id}; `cop tree --once` shows the ones there are"
        ));
    }
    if !s.trigger(id, event) {
        return Err(format!(
            "{id}.{event} sent no action: the element has no such handler, it only patches \
             signals or opens a URL, or it (or a parent) is hidden or disabled"
        ));
    }
    finish(&mut s, after, g)
}

/// Prints what the server sends back until it is quiet for `--idle`, then
/// the page if asked.
fn finish(s: &mut Session, after: &After, g: &Global) -> Result<(), String> {
    let result = stream(
        s,
        &StreamArgs::default(),
        Some(Duration::from_millis(after.idle)),
        g,
    );
    if after.tree {
        let st = g.style();
        show(s, &st, g, false);
    }
    result
}

// --- run ----------------------------------------------------------------------

fn run_cmd(
    t: &Target,
    script: Option<PathBuf>,
    eval: Option<String>,
    settle: u64,
    timeout: u64,
    g: &Global,
) -> Result<(), String> {
    let (src, path) = match (eval, script) {
        (Some(code), _) => (code, None),
        (None, Some(p)) if p.as_os_str() != "-" => (read_input(&p)?, Some(p)),
        (None, _) => (read_input(Path::new("-"))?, None),
    };
    let mut session = connect(t, Echo::Off)?;
    let settle = Duration::from_millis(settle);
    ready(&mut session, settle)?;
    let host = script::Host {
        session,
        timeout: Duration::from_millis(timeout),
        style: g.style(),
        settle,
    };
    script::run(host, &src, path.as_deref())
}

// --- parse, build -------------------------------------------------------------

fn parse_cmd(
    address: &str,
    no_fetch: bool,
    h: &HelloArgs,
    qr_code: bool,
    g: &Global,
) -> Result<(), String> {
    let st = g.style();
    let (uri, resolution) = resolve(address, !no_fetch, false)?;
    let key = endpoint_key(&uri.endpoint);
    let (sent, token, hello) = make_hello(uri.clone(), h)?;
    let hello_line = hypereffect::RendererMessage::Hello(hello.clone()).to_line();
    if g.json {
        let v = json!({
            "valid": key.is_ok(),
            "uri": uri.to_string(),
            "endpoint": uri.endpoint,
            "endpoint_problem": key.as_ref().err(),
            "relay": uri.relay,
            "relay_needs_tor": uri.relay_is_onion(),
            "params": uri.params.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect::<Map<_, _>>(),
            "name": uri.name(),
            "ignored_query": uri.ignored,
            "session": token,
            "session_bits": uri::entropy_bits(&token).floor(),
            "hello": hello,
            "resolved": resolution.as_ref().map(|r| json!({
                "url": r.url,
                "final_url": r.final_url,
                "status": r.status,
                "href": r.href,
                "skipped": r.skipped.iter().map(|(h, e)| json!({"href": h, "problem": e})).collect::<Vec<_>>(),
            })),
        });
        out(&format!("{v}\n"));
    } else {
        let mut o = String::new();
        let mut row = |label: &str, value: String| {
            o.push_str(&format!("{}  {value}\n", st.dim(&format!("{label:<9}"))));
        };
        if let Some(r) = &resolution {
            row("web", format!("{} → {} ({})", r.url, r.final_url, r.status));
            for (href, why) in &r.skipped {
                row("skipped", st.yellow(&format!("{href:?}: {why}")));
            }
        }
        row("uri", st.bold(&uri.to_string()));
        row(
            "endpoint",
            match &key {
                Ok(()) => format!("{} {}", uri.endpoint, st.green("✓ ed25519 key")),
                Err(e) => format!("{} {}", uri.endpoint, st.red(&format!("✗ {e}"))),
            },
        );
        row(
            "relay",
            match &uri.relay {
                None => st.dim("default (n0) relays"),
                Some(r) if uri.relay_is_onion() => {
                    format!(
                        "{r} {}",
                        st.yellow("(.onion: clients must connect via Tor)")
                    )
                }
                Some(r) => r.clone(),
            },
        );
        if uri.params.is_empty() {
            row("params", st.dim("none"));
        }
        let width = uri.params.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
        for (i, (k, v)) in uri.params.iter().enumerate() {
            let note = if k == "name" {
                st.dim("  (shown while connecting, until metadata arrives; unverified)")
            } else {
                String::new()
            };
            row(
                if i == 0 { "params" } else { "" },
                format!("{k:<width$} = {}{note}", style::quote(v, 200)),
            );
        }
        for q in &uri.ignored {
            row(
                "ignored",
                st.yellow(&format!(
                    "?{q} (only _relay means anything; clients ignore the rest)"
                )),
            );
        }
        if sent.params != uri.params {
            row("sends", st.dim("the params changed by the options above"));
        }
        let bits = uri::entropy_bits(&token);
        if h.hello.is_none() && bits < 128.0 {
            row(
                "session",
                st.yellow(&format!(
                    "{token:?} carries at most {bits:.0} bits; the spec requires 128"
                )),
            );
        }
        row("hello", hello_line.trim_end().to_string());
        out(&o);
    }
    if qr_code {
        out(&qr(&uri.to_string())?);
    }
    key.map_err(|_| String::new())
}

/// Whether an endpoint id is an Ed25519 public key iroh can dial.
fn endpoint_key(id: &str) -> Result<(), String> {
    use std::str::FromStr;
    iroh::PublicKey::from_str(id)
        .map(|_| ())
        .map_err(|e| format!("not a valid Ed25519 public key: {e}"))
}

fn build_cmd(
    endpoint: &str,
    relay: Option<String>,
    params: Vec<(String, String)>,
    qr_code: bool,
    html: bool,
    g: &Global,
) -> Result<(), String> {
    let mut uri = CopUri::parse(&format!("cop://{}", endpoint.trim()))
        .map_err(|e| format!("invalid endpoint id: {e}"))?;
    endpoint_key(&uri.endpoint)?;
    uri.relay = relay;
    for (k, v) in params {
        if uri.param(&k).is_some() {
            return Err(format!("parameter {k:?} is given twice"));
        }
        uri.params.push((k, v));
    }
    let text = uri.to_string();
    // What clients will read back.
    CopUri::parse(&text).map_err(|e| format!("the URI does not parse back: {e}"))?;
    let link = format!(
        "<link rel=\"cassis-order\" href=\"{}\">",
        text.replace('&', "&amp;").replace('"', "&quot;")
    );
    if g.json {
        out(&format!("{}\n", json!({ "uri": text, "html": link })));
    } else {
        out(&format!("{text}\n"));
        if html {
            out(&format!("{link}\n"));
        }
    }
    if qr_code {
        out(&qr(&text)?);
    }
    Ok(())
}

// --- invoice ------------------------------------------------------------------

/// A COP payment message found in an input.
enum Found {
    Invoice {
        raw: Value,
        memo: Option<String>,
        from: Option<transcript::Dir>,
        line: Option<usize>,
    },
    Request {
        body: Map<String, Value>,
        line: Option<usize>,
    },
}

fn invoice_cmd(inputs: &[String], g: &Global) -> Result<(), String> {
    let st = g.style();
    let inputs: Vec<String> = if inputs.is_empty() {
        vec!["-".into()]
    } else {
        inputs.to_vec()
    };
    let mut found = Vec::new();
    for input in &inputs {
        let looks_literal = input.trim_start().starts_with('{') || parse_line(input.trim()).is_ok();
        let text = if looks_literal {
            input.clone()
        } else {
            read_input(Path::new(input))?
        };
        let before = found.len();
        collect(&text, &mut found)?;
        if found.len() == before {
            return Err(format!(
                "{}: no invoice or invoicerequest in it",
                if looks_literal { "the argument" } else { input }
            ));
        }
    }
    let mut bad = 0;
    for (i, f) in found.iter().enumerate() {
        match f {
            Found::Invoice {
                raw,
                memo,
                from,
                line,
            } => {
                let s = invoice::Summary::of(raw);
                if !s.is_valid() {
                    bad += 1;
                }
                if g.json {
                    let mut v = s.to_json(memo.as_deref());
                    v["line"] = json!(line);
                    v["from"] = json!(from.map(|d| match d {
                        transcript::Dir::Client => "client",
                        transcript::Dir::Server => "server",
                    }));
                    out(&format!("{v}\n"));
                } else {
                    let mut head = Vec::new();
                    if let Some(l) = line {
                        head.push(format!("line {l}"));
                    }
                    match from {
                        Some(transcript::Dir::Client) => {
                            head.push("from the client, answering an invoicerequest".into())
                        }
                        Some(transcript::Dir::Server) => {
                            head.push("from the server, asking to be paid".into())
                        }
                        None => {}
                    }
                    let gap = if i > 0 { "\n" } else { "" };
                    let head = if head.is_empty() {
                        String::new()
                    } else {
                        format!("{}\n", st.dim(&format!("── {}", head.join(", "))))
                    };
                    out(&format!(
                        "{gap}{head}{}",
                        s.render(memo.as_deref(), "", &st)
                    ));
                }
            }
            Found::Request { body, line } => {
                if g.json {
                    let mut v = json!({ "invoicerequest": body });
                    v["line"] = json!(line);
                    out(&format!("{v}\n"));
                } else {
                    let gap = if i > 0 { "\n" } else { "" };
                    let at = line.map(|l| format!("line {l}, ")).unwrap_or_default();
                    let networks: Vec<String> = body
                        .get("networks")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|n| n.as_str().map(str::to_owned))
                        .collect();
                    let text = match body.get("amount").and_then(Value::as_u64) {
                        Some(a) if !networks.is_empty() => view::request_line(
                            a,
                            &networks,
                            body.get("memo").and_then(Value::as_str),
                            body.get("expiresIn").and_then(Value::as_u64),
                            &st,
                        ),
                        _ => st.red("ignored by clients: it needs an integer amount and networks"),
                    };
                    out(&format!(
                        "{gap}{}\n{}  {text}\n",
                        st.dim(&format!("── {at}from the server, offering money")),
                        st.dim("request     ")
                    ));
                }
            }
        }
    }
    if bad > 0 {
        return Err(format!(
            "{bad} invoice{} did not decode or verify",
            if bad == 1 { "" } else { "s" }
        ));
    }
    Ok(())
}

/// The invoices and invoicerequests in `text`: one JSON object (a bare
/// invoice, or an `invoice` body), or lines of messages.
fn collect(text: &str, found: &mut Vec<Found>) -> Result<(), String> {
    if let Ok(Value::Object(o)) = serde_json::from_str::<Value>(text.trim()) {
        match o.get("invoice") {
            Some(raw @ Value::Object(_)) => found.push(Found::Invoice {
                raw: raw.clone(),
                memo: o.get("memo").and_then(Value::as_str).map(str::to_owned),
                from: None,
                line: None,
            }),
            _ => found.push(Found::Invoice {
                raw: Value::Object(o),
                memo: None,
                from: None,
                line: None,
            }),
        }
        return Ok(());
    }
    let entries = transcript::parse(text)?;
    let single = entries.len() == 1;
    for e in entries {
        let transcript::Step::Message { dir, line } = e.step else {
            continue;
        };
        let Ok((tag, body)) = parse_line(&line) else {
            continue;
        };
        let at = (!single).then_some(e.no);
        match tag {
            "invoice" => match body.get("invoice") {
                Some(raw @ Value::Object(_)) => found.push(Found::Invoice {
                    raw: raw.clone(),
                    memo: body.get("memo").and_then(Value::as_str).map(str::to_owned),
                    from: Some(dir),
                    line: at,
                }),
                _ => return Err(format!("line {}: invoice has no \"invoice\" object", e.no)),
            },
            "invoicerequest" => found.push(Found::Request { body, line: at }),
            _ => {}
        }
    }
    Ok(())
}

// --- replay, check ------------------------------------------------------------

fn replay_cmd(
    file: &Path,
    each: bool,
    until: Option<usize>,
    verbose: bool,
    g: &Global,
) -> Result<(), String> {
    let st = g.style();
    let entries = transcript::parse(&read_input(file)?)?;
    let mut page = page::Page::new();
    page.client.hello();
    let mut first = true;
    let mut print = |page: &page::Page, head: String| {
        if g.json {
            let mut v = view::snapshot_json(page);
            v["after"] = head.into();
            out(&format!("{v}\n"));
        } else {
            let gap = if first { "" } else { "\n" };
            out(&format!(
                "{gap}{}\n{}",
                st.dim(&format!("── {head}")),
                view::snapshot(page, &st)
            ));
        }
        first = false;
    };
    let mut last = 0;
    for e in &entries {
        if until.is_some_and(|u| e.no > u) {
            break;
        }
        let transcript::Step::Message { dir, line } = &e.step else {
            continue;
        };
        last = e.no;
        match dir {
            transcript::Dir::Server => {
                page.receive_line(line);
                for d in page.take_diagnostics() {
                    if verbose {
                        eprintln!("cop: line {}: {d}", e.no);
                    }
                }
                if each {
                    let tag = transcript::tag(line).unwrap_or("?");
                    print(&page, format!("line {}: < {tag}", e.no));
                }
            }
            transcript::Dir::Client => {
                // What the wallet sent: the signals it carried are what the
                // user had typed.
                if let Ok((tag, body)) = parse_line(line)
                    && tag == "action"
                    && let Some(s @ Value::Object(_)) = body.get("signals")
                {
                    page.client.write_signals(s);
                }
            }
        }
    }
    if !each {
        print(&page, format!("after line {last}"));
    }
    Ok(())
}

/// Makes [`hypereffect::client::Client`] take the signals an action
/// carried, as if the user had typed them.
trait WriteSignals {
    fn write_signals(&mut self, signals: &Value);
}

impl WriteSignals for hypereffect::client::Client {
    fn write_signals(&mut self, signals: &Value) {
        fn walk(c: &mut hypereffect::client::Client, prefix: &str, v: &Value) {
            match v {
                Value::Object(m) if !m.is_empty() => {
                    for (k, v) in m {
                        let path = if prefix.is_empty() {
                            k.clone()
                        } else {
                            format!("{prefix}.{k}")
                        };
                        walk(c, &path, v);
                    }
                }
                v if !prefix.is_empty() => c.write(prefix, v.clone()),
                _ => {}
            }
        }
        walk(self, "", signals);
    }
}

fn check_cmd(file: &Path, strict: bool, g: &Global) -> Result<(), String> {
    let st = g.style();
    let entries = transcript::parse(&read_input(file)?)?;
    let findings = check::check(&entries);
    let name = if file.as_os_str() == "-" {
        "<stdin>".to_string()
    } else {
        file.display().to_string()
    };
    if g.json {
        let errors = findings
            .iter()
            .filter(|f| f.level == check::Level::Error)
            .count();
        let warnings = findings
            .iter()
            .filter(|f| f.level == check::Level::Warning)
            .count();
        out(&format!(
            "{}\n",
            json!({
                "file": name,
                "messages": entries.len(),
                "errors": errors,
                "warnings": warnings,
                "findings": findings.iter().map(check::Finding::to_json).collect::<Vec<_>>(),
            })
        ));
    } else {
        for f in &findings {
            let level = match f.level {
                check::Level::Error => st.red("error"),
                check::Level::Warning => st.yellow("warning"),
                check::Level::Hint => st.cyan("hint"),
            };
            out(&format!("{name}:{}: {level}: {}\n", f.line, f.message));
        }
    }
    let count = |l| findings.iter().filter(|f| f.level == l).count();
    let (errors, warnings, hints) = (
        count(check::Level::Error),
        count(check::Level::Warning),
        count(check::Level::Hint),
    );
    if !g.json {
        eprintln!(
            "cop: {} message(s), {errors} error(s), {warnings} warning(s), {hints} hint(s)",
            entries.len()
        );
    }
    if errors > 0 || (strict && warnings > 0) {
        return Err(String::new());
    }
    Ok(())
}
