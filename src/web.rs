//! Web addresses that publish a `cop://` URI:
//! `<link rel="cassis-order" href="cop://...">` in the page's `<head>`.
//!
//! As a client must: HTTPS only (plain `http://` is upgraded), HTTPS
//! redirects only, bounded in time, size and redirects, the page never
//! rendered and its scripts never run.

use crate::uri::{CopUri, is_cop};
use std::time::Duration;

/// More than this is not the page of an order.
const MAX_BYTES: usize = 256 * 1024;
const MAX_REDIRECTS: usize = 3;
const TIMEOUT: Duration = Duration::from_secs(10);

/// How a web address became a `cop://` URI.
pub struct Resolution {
    /// The `https://` URL fetched first.
    pub url: String,
    /// The URL the page was finally read from, after redirects.
    pub final_url: String,
    pub status: u16,
    /// The `href` of the link, as written.
    pub href: String,
    pub uri: CopUri,
    /// `cassis-order` links skipped because their `href` is not a valid
    /// `cop://` URI, with the reason.
    pub skipped: Vec<(String, String)>,
}

/// The `https://` URL a web address is fetched from, or an error for an
/// address that is neither a web address nor a `cop://` URI.
pub fn https_url(input: &str) -> Result<String, String> {
    let input = input.trim();
    match input.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => {
            Ok(format!("https://{rest}"))
        }
        Some((scheme, _)) if scheme.eq_ignore_ascii_case("https") => Ok(input.to_owned()),
        Some((scheme, _)) => Err(format!(
            "{scheme}:// is neither a cop:// URI nor a web address"
        )),
        None if input.is_empty() => Err("the address is empty".into()),
        None => Ok(format!("https://{input}")),
    }
}

/// Fetches a web address and reads the `cop://` URI it publishes.
pub fn resolve(input: &str) -> Result<Resolution, String> {
    debug_assert!(!is_cop(input));
    let url = https_url(input)?;
    let (final_url, status, page) = crate::link::runtime().block_on(fetch(&url))?;
    let (found, skipped) = find_links(&page);
    let Some(href) = found else {
        let mut e = format!("{final_url} has no <link rel=\"cassis-order\"> in its <head>");
        for (href, why) in &skipped {
            e.push_str(&format!("\n  skipped href={href:?}: {why}"));
        }
        return Err(e);
    };
    let uri = CopUri::parse(&href).expect("find_links keeps valid URIs only");
    Ok(Resolution {
        url,
        final_url,
        status,
        href,
        uri,
        skipped,
    })
}

async fn fetch(url: &str) -> Result<(String, u16, String), String> {
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.error(format!("more than {MAX_REDIRECTS} redirects"))
            } else if attempt.url().scheme() != "https" {
                let url = attempt.url().to_string();
                attempt.error(format!("redirect to non-HTTPS {url}"))
            } else {
                attempt.follow()
            }
        }))
        .https_only(true)
        .build()
        .map_err(|e| e.to_string())?;
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("cannot fetch {url}: {}", chain(&e)))?;
    let final_url = response.url().to_string();
    let status = response.status();
    if !status.is_success() {
        return Err(format!("{final_url} answered {status}"));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !content_type.contains("html") {
        return Err(format!(
            "{final_url} is not an HTML page (Content-Type: {content_type:?})"
        ));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("reading {final_url}: {e}"))?
    {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_BYTES {
            // The link is in the head; what is past the limit does not matter.
            body.truncate(MAX_BYTES);
            break;
        }
    }
    Ok((
        final_url,
        status.as_u16(),
        String::from_utf8_lossy(&body).into_owned(),
    ))
}

/// An error with its sources, which is where reqwest says what went wrong.
fn chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut source = e.source();
    while let Some(e) = source {
        s.push_str(": ");
        s.push_str(&e.to_string());
        source = e.source();
    }
    s
}

/// The `href` of the first `<link rel="... cassis-order ..." href="cop://...">`
/// in the `<head>`, and the `cassis-order` links skipped before it.
pub fn find_links(html: &str) -> (Option<String>, Vec<(String, String)>) {
    let mut skipped = Vec::new();
    let lower = html.to_ascii_lowercase();
    let Some(start) = lower
        .find("<head")
        .and_then(|i| lower[i..].find('>').map(|j| i + j + 1))
    else {
        return (None, skipped);
    };
    let end = lower[start..]
        .find("</head")
        .map_or(html.len(), |i| start + i);
    let mut at = start;
    while let Some(i) = lower[at..end].find("<link") {
        let tag_start = at + i + "<link".len();
        // `<linkfoo` is another element.
        if !lower[tag_start..].starts_with(|c: char| c.is_ascii_whitespace() || c == '/') {
            at = tag_start;
            continue;
        }
        let tag_end = lower[tag_start..end]
            .find('>')
            .map_or(end, |j| tag_start + j);
        let attrs = attributes(&html[tag_start..tag_end]);
        let get = |name: &str| {
            attrs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        if let (Some(rel), Some(href)) = (get("rel"), get("href")) {
            let wanted = rel
                .split_ascii_whitespace()
                .any(|t| t.eq_ignore_ascii_case("cassis-order"));
            if wanted {
                match CopUri::parse(href) {
                    Ok(_) => return (Some(href.to_owned()), skipped),
                    Err(e) => skipped.push((href.to_owned(), e.0)),
                }
            }
        }
        at = tag_end;
    }
    (None, skipped)
}

/// The attributes of a tag, with lowercase names, in `name="value"`,
/// `name='value'` and `name=value` form.
fn attributes(tag: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = tag.trim_start_matches(['/', ' ', '\t', '\n', '\r']);
    while !rest.is_empty() {
        let name_end = rest
            .find(|c: char| c == '=' || c.is_ascii_whitespace() || c == '/')
            .unwrap_or(rest.len());
        let name = rest[..name_end].to_ascii_lowercase();
        rest = rest[name_end..].trim_start();
        let mut value = String::new();
        if let Some(after) = rest.strip_prefix('=') {
            let after = after.trim_start();
            let (v, remaining) = match after.chars().next() {
                Some(q @ ('"' | '\'')) => match after[1..].find(q) {
                    Some(i) => (&after[1..1 + i], &after[i + 2..]),
                    None => (&after[1..], ""),
                },
                _ => {
                    let i = after
                        .find(|c: char| c.is_ascii_whitespace())
                        .unwrap_or(after.len());
                    (&after[..i], &after[i..])
                }
            };
            value = v.to_owned();
            rest = remaining;
        }
        if !name.is_empty() {
            out.push((name, value));
        }
        rest = rest.trim_start_matches(['/', ' ', '\t', '\n', '\r']);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "5f1c9d0a27e4b6c38d2f71a09e4b5c6d8a3f2e1b0c9d8e7f6a5b4c3d2e1f0a9b";

    #[test]
    fn finds_the_link_in_the_head_only() {
        let page = format!(
            r#"<html><head><title>x</title>
            <link rel="stylesheet" href="a.css">
            <link rel="cassis-order" href="cop://nope">
            <LINK href='cop://{ID}#table=1' REL="alternate cassis-order"/>
            </head><body><link rel="cassis-order" href="cop://{}"></body></html>"#,
            "0".repeat(64)
        );
        let (found, skipped) = find_links(&page);
        assert_eq!(found.unwrap(), format!("cop://{ID}#table=1"));
        assert_eq!(skipped.len(), 1);
        let in_body = format!(r#"<head></head><body><link rel="cassis-order" href="cop://{ID}">"#);
        assert!(find_links(&in_body).0.is_none());
    }

    #[test]
    fn upgrades_to_https() {
        assert_eq!(
            https_url("example.com/o/1").unwrap(),
            "https://example.com/o/1"
        );
        assert_eq!(
            https_url("http://example.com").unwrap(),
            "https://example.com"
        );
        assert!(https_url("ftp://example.com").is_err());
    }
}
