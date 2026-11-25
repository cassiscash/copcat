//! One live flow with a server: the [`Link`], the [`Page`] it builds, and a
//! blocking way to wait for what the server sends.
//!
//! The `hello` goes out as soon as the stream opens. Every line sent and
//! received can be echoed (`> ` and `< `, the transcript format) and
//! recorded to a file.

use crate::link::{self, Event, Link};
use crate::page::Page;
use crate::uri::CopUri;
use hypereffect::signals::{merge_patch, strip_local_map};
use hypereffect::{Action, LineBuffer, Map, RendererMessage, Value, json};
use std::collections::VecDeque;
use std::fs::File;
use std::io::Write;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

/// How what is sent is shown as it goes out.
#[derive(Clone, Copy, PartialEq)]
pub enum Echo {
    Off,
    /// `> line`, as in a transcript.
    Text,
    /// One JSON object a line.
    Json,
}

pub struct Session {
    pub page: Page,
    pub uri: CopUri,
    /// The `session` token of the `hello`.
    pub token: String,
    pub hello: Map<String, Value>,
    pub echo: Echo,
    pub start: Instant,
    link: Link,
    reader: LineBuffer,
    /// Lines received but not applied yet.
    pending: VecDeque<String>,
    /// Set once the transport has closed, with the reason if it failed.
    closed: Option<Option<String>>,
    record: Option<File>,
    verbose: bool,
}

/// How [`Session::settle`] ended.
#[derive(PartialEq)]
pub enum Settled {
    /// The server went quiet.
    Quiet,
    /// The server kept sending until the deadline.
    Busy,
    Closed,
}

pub enum Received {
    /// A line from the server, already applied to the page.
    Line(String),
    /// The flow ended, with the reason if it was an error.
    Closed(Option<String>),
}

pub enum Update {
    /// Lines from the server, already applied to the page.
    Lines(Vec<String>),
    /// The flow ended, with the reason if it was an error.
    Closed(Option<String>),
}

/// Everything about how to open a flow.
pub struct Opening {
    pub uri: CopUri,
    pub hello: Map<String, Value>,
    pub token: String,
    pub key: Option<[u8; 32]>,
    pub record: Option<File>,
    pub verbose: bool,
    pub echo: Echo,
}

/// The `hello` object: the fragment parameters as `params` (omitted when
/// there are none), the `session` token, and `force_resume` when asked.
pub fn hello(uri: &CopUri, token: &str, force_resume: bool) -> Map<String, Value> {
    let mut hello = Map::new();
    if !uri.params.is_empty() {
        let params = uri
            .params
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        hello.insert("params".into(), Value::Object(params));
    }
    hello.insert("session".into(), Value::String(token.to_owned()));
    if force_resume {
        hello.insert("force_resume".into(), Value::Bool(true));
    }
    hello
}

impl Session {
    pub fn open(o: Opening) -> Session {
        let link = link::connect(&o.uri, o.key, o.verbose);
        Session {
            page: Page::new(),
            uri: o.uri,
            token: o.token,
            hello: o.hello,
            echo: o.echo,
            start: Instant::now(),
            link,
            reader: LineBuffer::new(),
            pending: VecDeque::new(),
            closed: None,
            record: o.record,
            verbose: o.verbose,
        }
    }

    pub fn elapsed(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    fn record(&mut self, dir: char, line: &str) {
        if let Some(f) = &mut self.record {
            let _ = writeln!(f, "{dir} {}", line.trim_end());
        }
    }

    /// Waits until the server sends a line or the flow ends, or until
    /// `deadline` (forever without one), which returns `None`. The line is
    /// applied to the page first. Once the flow has ended this returns
    /// `Closed` at once.
    pub fn next_line(&mut self, deadline: Option<Instant>) -> Option<Received> {
        loop {
            if let Some(line) = self.pending.pop_front() {
                self.record('<', &line);
                self.page.receive_line(&line);
                self.report();
                return Some(Received::Line(line));
            }
            if let Some(reason) = &self.closed {
                return Some(Received::Closed(reason.clone()));
            }
            let ev = match deadline {
                None => self
                    .link
                    .events
                    .recv()
                    .map_err(|_| RecvTimeoutError::Disconnected),
                Some(d) => self
                    .link
                    .events
                    .recv_timeout(d.saturating_duration_since(Instant::now())),
            };
            let ev = match ev {
                Ok(ev) => ev,
                Err(RecvTimeoutError::Timeout) => return None,
                Err(RecvTimeoutError::Disconnected) => Event::Closed(None),
            };
            match ev {
                Event::Open { client, elapsed } => {
                    if self.verbose {
                        eprintln!(
                            "cop: connected as {client} in {:.2}s",
                            elapsed.as_secs_f64()
                        );
                    }
                    if self.echo == Echo::Json {
                        crate::out(&format!(
                            "{}\n",
                            json!({
                                "t": crate::round(self.elapsed()),
                                "event": "connected",
                                "server": self.uri.endpoint,
                                "client": client,
                                "connect_secs": elapsed.as_secs_f64(),
                                "session": self.token,
                            })
                        ));
                    }
                    let line = self.page.client.hello_with(&self.hello);
                    self.reader = LineBuffer::new();
                    self.send(line);
                }
                Event::Data(data) => self.pending.extend(self.reader.push(&data)),
                Event::Closed(reason) => {
                    self.page.client.close();
                    self.closed = Some(reason);
                }
            }
        }
    }

    /// Like [`next_line`](Self::next_line), but takes every line that has
    /// arrived along with the first.
    pub fn next(&mut self, deadline: Option<Instant>) -> Option<Update> {
        let first = match self.next_line(deadline)? {
            Received::Line(l) => l,
            Received::Closed(reason) => return Some(Update::Closed(reason)),
        };
        let mut lines = vec![first];
        while let Some(line) = self.pending.pop_front() {
            self.record('<', &line);
            self.page.receive_line(&line);
            lines.push(line);
        }
        self.report();
        Some(Update::Lines(lines))
    }

    /// Waits until the server has said nothing for `quiet`, the flow ends
    /// or `deadline` passes.
    pub fn settle(&mut self, quiet: Duration, deadline: Instant) -> Settled {
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Settled::Busy;
            }
            match self.next(Some((now + quiet).min(deadline))) {
                None if Instant::now() < deadline => return Settled::Quiet,
                None | Some(Update::Lines(_)) => {}
                Some(Update::Closed(_)) => return Settled::Closed,
            }
        }
    }

    /// Sends a line to the server, echoing and recording it.
    pub fn send(&mut self, line: String) {
        let line = line.trim_end().to_string();
        match self.echo {
            Echo::Off => {}
            Echo::Text => crate::out(&format!("> {line}\n")),
            Echo::Json => crate::out(&format!(
                "{}\n",
                crate::line_json(self.elapsed(), "out", &line)
            )),
        }
        self.record('>', &line);
        self.link.send(line);
    }

    /// The action line for `name`, carrying the current signals (without
    /// local ones) with `signals` merged into them.
    pub fn action_line(
        &self,
        name: &str,
        payload: Option<Value>,
        signals: Option<&Value>,
    ) -> String {
        let current = self
            .page
            .client
            .signals()
            .as_object()
            .map(strip_local_map)
            .unwrap_or_default();
        let mut merged = Value::Object(current);
        if let Some(s) = signals {
            merge_patch(&mut merged, s);
        }
        let Value::Object(signals) = merged else {
            unreachable!("merging into an object keeps an object")
        };
        RendererMessage::Action(Action {
            name: name.to_string(),
            payload,
            signals,
        })
        .to_line()
    }

    /// Runs the handler in attribute `event` of element `id`, as a tap or
    /// an edit in a wallet would, and sends its action, if it has one.
    /// Returns whether an action was sent.
    pub fn trigger(&mut self, id: &str, event: &str) -> bool {
        let line = self.page.client.trigger(id, event);
        for url in self.page.client.take_opens() {
            eprintln!("cop: {id}.{event} asks to open {url}");
        }
        self.report();
        match line {
            Some(l) => {
                self.send(l);
                true
            }
            None => false,
        }
    }

    /// Answers an `invoicerequest` with an invoice.
    pub fn send_invoice(&mut self, invoice: Value) {
        let mut body = Map::new();
        body.insert("invoice".into(), invoice);
        self.send(hypereffect::message::encode_line("invoice", &body));
    }

    /// Prints the page's diagnostics, if asked to.
    pub fn report(&mut self) {
        let diags = self.page.take_diagnostics();
        if self.verbose {
            for d in diags {
                eprintln!("cop: {d}");
            }
        }
    }
}
