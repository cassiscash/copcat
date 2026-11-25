# cop

A command-line debugger for the [Cassis Order Protocol](https://cassis.cash)
(COP), for humans and AI agents alike.

COP is [hypereffect](https://crates.io/crates/hypereffect) over
[iroh](https://iroh.computer) on the ALPN `cassis-order`: a merchant serves a
page of signals and elements, the wallet renders it and sends actions back, and
the flow ends with a Cassis invoice to pay (or an `invoicerequest` to answer).
`cop` connects to a merchant the way a wallet does and shows you everything in
between: it validates `cop://` URIs and web addresses that publish them,
streams the messages both ways, draws the hypereffect tree as a wallet would
see it, presses buttons, decodes and verifies invoices, records flows as
transcripts, checks them against the spec, and can play one back as a mock
merchant to test wallets.

Every command takes `--json` to print JSON instead (one object a line when it
streams); diagnostics go to stderr, prefixed `cop:`.

## Install

Download a binary for your platform from the
[releases](https://github.com/cassiscash/cop/releases), or build it:

```sh
cargo install --git https://github.com/cassiscash/cop
```

## Commands

| Command | What it does |
|---|---|
| `cop <address>` | Same as `cop stream <address>`. |
| `cop stream <address>` | Connects as a wallet, sends the `hello` and prints every message both ways as a transcript: `> ` for the client, `< ` for the server. Invoices are explained in `#` comments below them. Filter with `--tag`, `--id`, `--type` or `--actions`, stop with `--until invoice` or `--idle MS`, and save the flow with `--record FILE`. |
| `cop tree <address>` | Prints what a wallet would show: the metadata (including `mode: single_use`), the signals, the element tree with its handlers, the invoice to pay, any `invoicerequest`s and why the flow closed. Reprints after every change; `--once` prints once the page settles. |
| `cop ui <address>` | The tree of `cop tree`, interactive in the terminal: type into inputs, press buttons, toggle checkboxes, change selects, sliders and tabs (keyboard or mouse), with the page changing in place as the server sends. A sidebar lists every message both ways, each kept with the signals of the moment; picking one (click it, or `[` then ↑/↓) rebuilds the page as it was right after it, from scratch, and stays there read-only while new messages arrive, until `live` is picked again. Alias `cop tui`. |
| `cop action <address> <name>` | Fires an action (with `--payload JSON` and `--signals JSON`) and prints it with what the server sends back. |
| `cop press <address> <id>` | Presses a button (or runs another handler, with `--event`) as a wallet would: nothing is sent if the element is hidden or disabled. `--input ID=VALUE` fills controls beforehand; `--tree` prints the page afterwards. |
| `cop run <address> <script.js>` | Drives a flow with JavaScript (`-e` for inline code). Besides the tree helpers (`signals`, `query`, `press`, `input`, `wait`, `printTree`, …) it has COP ones: `invoice`, `waitInvoice`, `waitInvoiceRequest`, `sendInvoice`, `decodeInvoice`, `closeReason` and `printInvoice`. |
| `cop parse <address>` | Validates a `cop://` URI, or resolves a web address to the one its `<link rel="cassis-order">` publishes, and explains it: endpoint, relay, parameters, what clients ignore, and the `hello` it makes. `--qr` draws it as a QR code. |
| `cop build <endpoint-id>` | Builds a `cop://` URI from an endpoint id, `--relay` and `-p KEY=VALUE` parameters; `--html` prints the `<link>` tag to publish it, `--qr` a QR code. |
| `cop invoice [input…]` | Decodes Cassis invoices and verifies their signatures, marking the memo as unsigned. Takes files, `-`, literal JSON, `invoice` message lines or whole transcripts. Exits 1 if any invoice does not verify. |
| `cop replay <transcript>` | Replays a recorded flow into a client and prints the page it leaves; `--each` prints it after every message, `--until LINE` stops at that line. |
| `cop check <transcript>` | Lints a flow against the spec: session token entropy, `_relay` leaking into params, message directions, unknown `mode`s, `invoice` and `invoicerequest` fields, returned invoices matching the requested amount and networks, `force_resume` and `session_not_found`, prices in payloads, rejected elements. `--strict` fails on warnings too. |
| `cop serve <transcript>` | Serves a transcript as a scripted merchant: prints its `cop://` URI, then plays the `< ` lines to every wallet that connects, waiting for the `> ` lines, and prints what the wallet sends. `--once`, `--hangup`, `--qr`. |

`cop help <command>` has the details and examples for each.

## Transcripts

What `cop stream` prints and `--record` saves, and what `check`, `replay`,
`invoice` and `serve` read: one message a line.

```text
# Luigi's Pizza, pay at the till
> hello {"params":{"till":"2"},"session":"f1K9xQ2v8Tn4LmZp7Rb3wq"}
< metadata {"name":"Luigi's Pizza","description":"Till 2"}
@sleep 500
< invoice {"invoice":{...},"memo":"1 × flat white"}
```

`> ` is the client, `< ` the server; a line without a prefix takes the
direction of its tag (`hello` and `action` are the client's). `#` starts a
comment and `@sleep MS` is a pause for `cop serve`.

## Exit status

0 on success; 1 on errors (connection failures, invalid URIs, invoices that do
not verify, transcripts with errors); 2 on bad usage.

## Limitations

- Relays on `.onion` addresses are refused: `cop` does not speak Tor.
