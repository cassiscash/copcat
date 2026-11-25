//! `cop serve`: a scripted merchant. Every client that connects gets the
//! transcript played to it: `<` lines are sent, `>` lines are waited for
//! (the client must send a message with that tag, and for an action, that
//! name), `@sleep` pauses. A `hello` is always waited for before anything
//! is sent, as a real server would.
//!
//! What clients send is printed as `> ` lines, so a wallet's side of a flow
//! can be watched or recorded.

use crate::link::{ALPN, runtime};
use crate::transcript::{Dir, Entry, Step};
use hypereffect::message::parse_line;
use hypereffect::{Value, json};
use iroh::{Endpoint, SecretKey, endpoint::presets};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

pub struct Opts {
    pub script: Vec<Entry>,
    pub key: Option<[u8; 32]>,
    /// Parameters for the printed URI.
    pub params: Vec<(String, String)>,
    /// Stop after the first flow.
    pub once: bool,
    /// Close the connection once the script is done.
    pub hangup: bool,
    pub json: bool,
    pub qr: bool,
}

pub fn serve(o: Opts) -> Result<(), String> {
    runtime().block_on(run(o))
}

async fn run(o: Opts) -> Result<(), String> {
    let key = o.key.unwrap_or_else(|| {
        let mut k = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut k);
        k
    });
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(SecretKey::from(key))
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .map_err(|e| format!("cannot bind an iroh endpoint: {e}"))?;
    if tokio::time::timeout(Duration::from_secs(10), endpoint.online())
        .await
        .is_err()
    {
        eprintln!("cop: not online after 10s (no relay?); serving anyway");
    }
    let uri = crate::uri::CopUri {
        endpoint: endpoint.id().to_string(),
        relay: None,
        params: o.params.clone(),
        ignored: Vec::new(),
    };
    if o.json {
        crate::out(&format!(
            "{}\n",
            json!({"event": "serving", "uri": uri.to_string()})
        ));
    } else {
        crate::out(&format!("# {uri}\n"));
    }
    if o.qr {
        eprint!("{}", crate::qr(&uri.to_string())?);
    }
    eprintln!("cop: serving {} step(s) on {uri}", o.script.len());

    let script = Arc::new(o.script);
    let counter = Arc::new(AtomicUsize::new(0));
    loop {
        let Some(incoming) = endpoint.accept().await else {
            return Ok(());
        };
        let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
        let script = script.clone();
        let (hangup, json) = (o.hangup, o.json);
        let flow = async move {
            let result = async {
                let conn = incoming.await.map_err(|e| format!("cannot accept: {e}"))?;
                eprintln!("cop: [{n}] {} connected", conn.remote_id());
                let (send, recv) = conn
                    .accept_bi()
                    .await
                    .map_err(|e| format!("no stream: {e}"))?;
                let r = play(n, &script, send, recv, hangup, json).await;
                // A wallet that leaves closes the connection, which reading
                // reports as an error: it is the end of the flow.
                let left = matches!(
                    conn.close_reason(),
                    Some(iroh::endpoint::ConnectionError::ApplicationClosed(_))
                );
                conn.close(0u32.into(), b"done");
                match r {
                    Err(e) if left => {
                        eprintln!("cop: [{n}] the client left ({e})");
                        Ok(())
                    }
                    r => r,
                }
            }
            .await;
            match result {
                Ok(()) => eprintln!("cop: [{n}] flow over"),
                Err(e) => eprintln!("cop: [{n}] {e}"),
            }
        };
        if o.once {
            flow.await;
            endpoint.close().await;
            return Ok(());
        }
        tokio::spawn(flow);
    }
}

async fn play(
    n: usize,
    script: &[Entry],
    mut send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
    hangup: bool,
    json: bool,
) -> Result<(), String> {
    let mut lines = BufReader::new(recv).lines();
    let print = |line: &str| {
        if json {
            let mut v = crate::line_json(0.0, "in", line);
            v["conn"] = Value::from(n);
            crate::out(&format!("{v}\n"));
        } else {
            crate::out(&format!("> {line}\n"));
        }
    };
    // The next line from the client, printed; `None` once it hangs up.
    async fn read(
        lines: &mut tokio::io::Lines<BufReader<iroh::endpoint::RecvStream>>,
    ) -> Result<Option<String>, String> {
        loop {
            match lines.next_line().await {
                Ok(Some(l)) if l.trim().is_empty() => continue,
                Ok(l) => return Ok(l),
                Err(e) => return Err(format!("receive: {e}")),
            }
        }
    }

    let mut greeted = false;
    let mut closed = false;
    for e in script {
        match &e.step {
            Step::Sleep(ms) => tokio::time::sleep(Duration::from_millis(*ms)).await,
            Step::Message {
                dir: Dir::Client,
                line,
            } => {
                let want = parse_line(line).ok();
                loop {
                    let Some(got) = read(&mut lines).await? else {
                        return Err(format!(
                            "the client hung up while the script waited for line {}",
                            e.no
                        ));
                    };
                    print(&got);
                    let got_parsed = parse_line(&got).ok();
                    if got_parsed.as_ref().is_some_and(|(t, _)| *t == "hello") {
                        greeted = true;
                    }
                    if matches(want.as_ref(), got_parsed.as_ref()) {
                        break;
                    }
                    eprintln!(
                        "cop: [{n}] waiting for line {} ({}), skipping what the client sent",
                        e.no,
                        crate::style::clip(line, 60)
                    );
                }
            }
            Step::Message {
                dir: Dir::Server,
                line,
            } => {
                if !greeted {
                    let Some(got) = read(&mut lines).await? else {
                        return Err("the client hung up before its hello".into());
                    };
                    print(&got);
                    if !got.starts_with("hello ") {
                        eprintln!("cop: [{n}] the client's first message is not a hello");
                    }
                    greeted = true;
                }
                let mut bytes = line.trim_end().as_bytes().to_vec();
                bytes.push(b'\n');
                send.write_all(&bytes)
                    .await
                    .map_err(|e| format!("send: {e}"))?;
                closed = line.starts_with("close ");
                if closed {
                    break;
                }
            }
        }
    }
    if closed || hangup {
        let _ = send.finish();
        let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
        return Ok(());
    }
    eprintln!("cop: [{n}] script done; echoing the client until it hangs up");
    while let Some(got) = read(&mut lines).await? {
        print(&got);
    }
    Ok(())
}

/// Whether the client's message is the one the script waits for: the same
/// tag and, for an action, the same name and (if the script gives one)
/// payload.
fn matches(
    want: Option<&(&str, hypereffect::Map<String, Value>)>,
    got: Option<&(&str, hypereffect::Map<String, Value>)>,
) -> bool {
    let (Some((wt, wb)), Some((gt, gb))) = (want, got) else {
        return false;
    };
    if wt != gt {
        return false;
    }
    if *wt == "action" {
        let same = |k: &str| wb.get(k).is_none() || wb.get(k) == gb.get(k);
        return same("name") && same("payload");
    }
    true
}
