//! The iroh side: one bidirectional stream to a server on the ALPN
//! `cassis-order`, run on a background tokio runtime, with its events
//! forwarded to a channel the blocking commands wait on.

use crate::uri::CopUri;
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayUrl, SecretKey, endpoint::presets};
use std::str::FromStr;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;
use tokio::sync::mpsc as tokio_mpsc;

pub const ALPN: &[u8] = b"cassis-order";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// More than this arriving in one flow is a server gone wrong, not a page.
const MAX_FLOW_BYTES: usize = 32 * 1024 * 1024;

/// The runtime every network task runs on.
pub fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("the tokio runtime starts")
    })
}

/// What happened on the stream to the server.
pub enum Event {
    /// The stream is open: the `hello` can go out.
    Open {
        /// The key the client presented.
        client: String,
        elapsed: Duration,
    },
    /// Bytes of hypereffect lines from the server.
    Data(Vec<u8>),
    /// The flow is over; the reason is there if it was not a clean close.
    Closed(Option<String>),
}

/// The stream of one flow. Dropping it ends the flow.
pub struct Link {
    pub events: mpsc::Receiver<Event>,
    outgoing: tokio_mpsc::UnboundedSender<String>,
    task: tokio::task::AbortHandle,
}

impl Link {
    /// Queues a line (with or without its line feed) for the server.
    pub fn send(&self, line: String) {
        let _ = self.outgoing.send(line);
    }
}

impl Drop for Link {
    /// Ends the flow cleanly, so the server sees the client leave rather
    /// than a connection that times out: no more lines, the stream
    /// finished, the connection closed. Gives up after a moment.
    fn drop(&mut self) {
        let (closed, _) = tokio_mpsc::unbounded_channel();
        drop(std::mem::replace(&mut self.outgoing, closed));
        let deadline = Instant::now() + Duration::from_secs(2);
        while let Ok(ev) = self
            .events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            if matches!(ev, Event::Closed(_)) {
                break;
            }
        }
        self.task.abort();
    }
}

/// Dials the server of `uri` and keeps the stream. `key` is the iroh
/// identity the server sees: `None` for a fresh one, which cannot be linked
/// to any other flow.
pub fn connect(uri: &CopUri, key: Option<[u8; 32]>, verbose: bool) -> Link {
    let (tx, events) = mpsc::channel();
    let (outgoing, lines) = tokio_mpsc::unbounded_channel();
    let uri = uri.clone();
    let task = runtime().spawn(async move {
        let reason = run(&uri, key, lines, &tx, verbose).await.err();
        let _ = tx.send(Event::Closed(reason));
    });
    Link {
        events,
        outgoing,
        task: task.abort_handle(),
    }
}

async fn run(
    uri: &CopUri,
    key: Option<[u8; 32]>,
    mut lines: tokio_mpsc::UnboundedReceiver<String>,
    events: &mpsc::Sender<Event>,
    verbose: bool,
) -> Result<(), String> {
    let start = Instant::now();
    let mut addr = EndpointAddr::new(
        PublicKey::from_str(&uri.endpoint).map_err(|e| format!("endpoint id: {e}"))?,
    );
    if let Some(relay) = &uri.relay {
        if uri.relay_is_onion() {
            return Err(format!(
                "the relay {relay} is a .onion address, which needs Tor; this tool does not \
                 speak Tor"
            ));
        }
        addr = addr
            .with_relay_url(RelayUrl::from_str(relay).map_err(|e| format!("relay {relay}: {e}"))?);
    }
    let mut builder = Endpoint::builder(presets::N0);
    if let Some(key) = key {
        builder = builder.secret_key(SecretKey::from(key));
    }
    let endpoint = builder
        .bind()
        .await
        .map_err(|e| format!("cannot bind an iroh endpoint: {e}"))?;
    if verbose {
        eprintln!(
            "copcat: dialing {} as {}{}",
            uri.endpoint,
            endpoint.id(),
            uri.relay
                .as_deref()
                .map(|r| format!(" through {r}"))
                .unwrap_or_default()
        );
    }
    let connection = tokio::time::timeout(CONNECT_TIMEOUT, endpoint.connect(addr, ALPN))
        .await
        .map_err(|_| {
            format!(
                "the server did not answer in {}s (is it online and serving the ALPN cassis-order?)",
                CONNECT_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| format!("cannot connect: {e:#}"))?;
    let (mut send, mut receive) = connection
        .open_bi()
        .await
        .map_err(|e| format!("cannot open the stream: {e}"))?;
    let _ = events.send(Event::Open {
        client: endpoint.id().to_string(),
        elapsed: start.elapsed(),
    });

    let mut total = 0usize;
    let mut buffer = vec![0u8; 16 * 1024];
    let result = loop {
        tokio::select! {
            line = lines.recv() => {
                let Some(line) = line else { break Ok(()) };
                let mut bytes = line.trim_end().as_bytes().to_vec();
                bytes.push(b'\n');
                if let Err(e) = send.write_all(&bytes).await {
                    break Err(format!("send: {e}"));
                }
            }
            read = receive.read(&mut buffer) => {
                match read {
                    Err(e) => break Err(format!("receive: {e}")),
                    Ok(Some(n)) => {
                        total += n;
                        if total > MAX_FLOW_BYTES {
                            break Err(format!("the server sent more than {MAX_FLOW_BYTES} bytes"));
                        }
                        if events.send(Event::Data(buffer[..n].to_vec())).is_err() {
                            break Ok(());
                        }
                    }
                    Ok(None) => break Ok(()),
                }
            }
        }
    };
    let _ = send.finish();
    connection.close(0u32.into(), b"done");
    endpoint.close().await;
    result
}
