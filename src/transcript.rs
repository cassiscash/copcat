//! Transcripts: a flow written down, one message a line, as `cop stream`
//! prints it and `--record` saves it.
//!
//! ```text
//! # Luigi's Pizza, pay at the till
//! > hello {"params":{"till":"2"},"session":"f1K9xQ2v8Tn4LmZp7Rb3wq"}
//! < metadata {"name":"Luigi's Pizza","description":"Till 2"}
//! @sleep 500
//! < invoice {"invoice":{...},"memo":"1 × flat white"}
//! ```
//!
//! `> ` is the client, `< ` the server. Without a prefix the direction is
//! the tag's: `hello` and `action` are the client's, anything else the
//! server's. Blank lines and `#` comments are skipped, and `@sleep MS` is a
//! pause for `cop serve`.

use hypereffect::message::parse_line;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// From the client (wallet) to the server.
    Client,
    /// From the server (merchant) to the client.
    Server,
}

#[derive(Clone, Debug)]
pub enum Step {
    Message { dir: Dir, line: String },
    Sleep(u64),
}

#[derive(Clone, Debug)]
pub struct Entry {
    /// The line number, from 1.
    pub no: usize,
    pub step: Step,
}

pub fn parse(text: &str) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let no = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(d) = line.strip_prefix('@') {
            let mut words = d.split_whitespace();
            let step = match (words.next(), words.next(), words.next()) {
                (Some("sleep"), Some(ms), None) => Step::Sleep(
                    ms.parse()
                        .map_err(|_| format!("line {no}: @sleep takes milliseconds, not {ms:?}"))?,
                ),
                _ => {
                    return Err(format!(
                        "line {no}: unknown directive {line:?} (try @sleep MS)"
                    ));
                }
            };
            out.push(Entry { no, step });
            continue;
        }
        let (dir, msg) = if let Some(m) = line.strip_prefix("> ") {
            (Some(Dir::Client), m)
        } else if let Some(m) = line.strip_prefix("< ") {
            (Some(Dir::Server), m)
        } else {
            (None, line)
        };
        let msg = msg.trim();
        let dir = dir.unwrap_or_else(|| match tag(msg) {
            Some("hello" | "action") => Dir::Client,
            _ => Dir::Server,
        });
        out.push(Entry {
            no,
            step: Step::Message {
                dir,
                line: msg.to_string(),
            },
        });
    }
    Ok(out)
}

/// The tag of a message line, if it has one.
pub fn tag(line: &str) -> Option<&str> {
    parse_line(line).ok().map(|(t, _)| t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_directions() {
        let t =
            parse("# x\n\nhello {}\n< invoice {}\n> invoice {}\nelement {}\n@sleep 20\n").unwrap();
        let dirs: Vec<Option<Dir>> = t
            .iter()
            .map(|e| match &e.step {
                Step::Message { dir, .. } => Some(*dir),
                Step::Sleep(_) => None,
            })
            .collect();
        assert_eq!(
            dirs,
            vec![
                Some(Dir::Client),
                Some(Dir::Server),
                Some(Dir::Client),
                Some(Dir::Server),
                None
            ]
        );
        assert_eq!(t[0].no, 3);
        assert!(parse("@nap 1").is_err());
    }
}
