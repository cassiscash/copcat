# copcat

A command-line debugger for the [Cassis Order Protocol](https://cop.cassis.cash)
(COP), for humans and AI agents.

COP is [hypereffect](https://crates.io/crates/hypereffect) over
[iroh](https://iroh.computer) on the ALPN `cassis-order`: a merchant serves a
page of signals and elements, the wallet renders it and sends actions back, and
the flow ends with a Cassis invoice to pay (or an `invoicerequest` to answer).
`copcat` connects to a merchant the way a wallet does and shows you everything in
between: it validates `cop://` URIs and web addresses that publish them,
streams the messages both ways, draws the hypereffect tree as a wallet would
see it, presses buttons, decodes and verifies invoices, records flows as
transcripts, checks them against the spec, and can play one back as a mock
merchant to test wallets.

Every command takes `--json` to print JSON instead (one object a line when it
streams); diagnostics go to stderr, prefixed `copcat:`.

## Install

Prebuilt CLI binaries are attached to every
[release](https://github.com/cassiscash/copcat/releases): Linux, macOS and
Windows, for x86_64 and ARM64. On Linux or macOS, expand the snippet below and
paste it into bash: it detects your platform, downloads the latest release and
installs `copcat` into `~/.local/bin` (or `$COPCAT_INSTALL_DIR`), adding it to
your `PATH` if needed.

<details>
<summary>Install script (bash)</summary>

```bash
(
  set -eu

  repo=cassiscash/copcat

  # Releases name their assets after the Rust target triple.
  case "$(uname -s)" in
    Linux)  platform=unknown-linux-gnu ;;
    Darwin) platform=apple-darwin ;;
    *) echo "copcat: unsupported OS: $(uname -s); download a binary from https://github.com/$repo/releases" >&2; exit 1 ;;
  esac
  case "$(uname -m)" in
    x86_64|amd64)  arch=x86_64 ;;
    arm64|aarch64) arch=aarch64 ;;
    *) echo "copcat: unsupported architecture: $(uname -m); download a binary from https://github.com/$repo/releases" >&2; exit 1 ;;
  esac
  target=$arch-$platform

  command -v curl >/dev/null || { echo "copcat: curl is required" >&2; exit 1; }
  tag=$(curl -fsSIL -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest")
  tag=${tag##*/}
  case "$tag" in
    v*) ;;
    *) echo "copcat: no release found for $repo" >&2; exit 1 ;;
  esac
  asset=copcat-$tag-$target.tar.gz

  bindir=${COPCAT_INSTALL_DIR:-$HOME/.local/bin}
  mkdir -p "$bindir"

  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  curl -fsSL -o "$tmp/$asset" "https://github.com/$repo/releases/download/$tag/$asset"
  tar -xzf "$tmp/$asset" -C "$tmp"
  cp "$tmp/copcat-$tag-$target/copcat" "$bindir/copcat"
  chmod 755 "$bindir/copcat"
  "$bindir/copcat" --version

  # Add $bindir to the PATH of future shells, unless it is already on it.
  case ":$PATH:" in
    *":$bindir:"*) ;;
    *)
      case "${SHELL##*/}" in
        zsh)  rc=$HOME/.zshrc ;;
        bash) rc=$HOME/.bashrc; [ -f "$HOME/.bash_profile" ] && rc=$HOME/.bash_profile ;;
        *)    rc=$HOME/.profile ;;
      esac
      if ! grep -qsF "$bindir" "$rc"; then
        printf '\nexport PATH="%s:$PATH"\n' "$bindir" >>"$rc"
        echo "copcat: added $bindir to PATH in $rc; open a new shell to pick it up" >&2
      fi
      ;;
  esac
)
```

</details>

On Windows, download the `copcat-<version>-x86_64-pc-windows-msvc.zip` (or the
`aarch64-pc-windows-msvc` one) from the releases page and unzip `copcat.exe`
somewhere on your `%PATH%`. Every archive ships with the README and the release
page has a `SHA256SUMS` file.

Or build it from source:

```sh
cargo install --git https://github.com/cassiscash/copcat
```

## Commands

| Command | What it does |
|---|---|
| `copcat <address>` | Same as `copcat stream <address>`. |
| `copcat stream <address>` | Connects as a wallet, sends the `hello` and prints every message both ways as a transcript: `> ` for the client, `< ` for the server. Invoices are explained in `#` comments below them. Filter with `--tag`, `--id`, `--type` or `--actions`, stop with `--until invoice` or `--idle MS`, and save the flow with `--record FILE`. |
| `copcat tree <address>` | Prints what a wallet would show: the metadata (including `mode: single_use`), the signals, the element tree with its handlers, the invoice to pay, any `invoicerequest`s and why the flow closed. Reprints after every change; `--once` prints once the page settles. |
| `copcat ui <address>` | The tree of `copcat tree`, interactive in the terminal: type into inputs, press buttons, toggle checkboxes, change selects, sliders and tabs (keyboard or mouse), with the page changing in place as the server sends. A sidebar lists every message both ways, each kept with the signals of the moment; picking one (click it, or `[` then ↑/↓) rebuilds the page as it was right after it, from scratch, and stays there read-only while new messages arrive, until `live` is picked again. Alias `copcat tui`. |
| `copcat action <address> <name>` | Fires an action (with `--payload JSON` and `--signals JSON`) and prints it with what the server sends back. |
| `copcat press <address> <id>` | Presses a button (or runs another handler, with `--event`) as a wallet would: nothing is sent if the element is hidden or disabled. `--input ID=VALUE` fills controls beforehand; `--tree` prints the page afterwards. |
| `copcat run <address> <script.js>` | Drives a flow with JavaScript (`-e` for inline code). Besides the tree helpers (`signals`, `query`, `press`, `input`, `wait`, `printTree`, …) it has COP ones: `invoice`, `waitInvoice`, `waitInvoiceRequest`, `sendInvoice`, `decodeInvoice`, `closeReason` and `printInvoice`. |
| `copcat parse <address>` | Validates a `cop://` URI, or resolves a web address to the one its `<link rel="cassis-order">` publishes, and explains it: endpoint, relay, parameters, what clients ignore, and the `hello` it makes. `--qr` draws it as a QR code. |
| `copcat build <endpoint-id>` | Builds a `cop://` URI from an endpoint id, `--relay` and `-p KEY=VALUE` parameters; `--html` prints the `<link>` tag to publish it, `--qr` a QR code. |
| `copcat invoice [input…]` | Decodes Cassis invoices and verifies their signatures, marking the memo as unsigned. Takes files, `-`, literal JSON, `invoice` message lines or whole transcripts. Exits 1 if any invoice does not verify. |
| `copcat replay <transcript>` | Replays a recorded flow into a client and prints the page it leaves; `--each` prints it after every message, `--until LINE` stops at that line. |
| `copcat check <transcript>` | Lints a flow against the spec: session token entropy, `_relay` leaking into params, message directions, unknown `mode`s, `invoice` and `invoicerequest` fields, returned invoices matching the requested amount and networks, `force_resume` and `session_not_found`, prices in payloads, rejected elements. `--strict` fails on warnings too. |
| `copcat serve <transcript>` | Serves a transcript as a scripted merchant: prints its `cop://` URI, then plays the `< ` lines to every wallet that connects, waiting for the `> ` lines, and prints what the wallet sends. `--once`, `--hangup`, `--qr`. |

`copcat help <command>` has the details and examples for each.

## Transcripts

What `copcat stream` prints and `--record` saves, and what `check`, `replay`,
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
comment and `@sleep MS` is a pause for `copcat serve`.

## Exit status

0 on success; 1 on errors (connection failures, invalid URIs, invoices that do
not verify, transcripts with errors); 2 on bad usage.

## Limitations

- Relays on `.onion` addresses are refused: `copcat` does not speak Tor.
