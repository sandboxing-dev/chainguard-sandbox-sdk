//! Small HTTPS helpers shared by the login, STS and platform calls.

use std::time::Duration;

use crate::{Error, Result};

/// A client that never follows redirects (a token must not be re-sent to wherever one points)
/// and gives up after 30 seconds.
pub(crate) fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .user_agent("chainguard-sandbox-sdk-rs")
        .build()
        .map_err(|e| Error::transport("workspaces: building the HTTP client", e))
}

/// A response's status and up to `limit` bytes of its body.
pub(crate) struct Answer {
    pub status: u16,
    pub body: Vec<u8>,
}

pub(crate) async fn send(
    req: reqwest::RequestBuilder,
    limit: usize,
    context: &str,
) -> Result<Answer> {
    let mut resp = req
        .send()
        .await
        .map_err(|e| Error::transport(context, error_chain(&e)))?;
    let status = resp.status().as_u16();
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| Error::transport(context, error_chain(&e)))?
    {
        let room = limit.saturating_sub(body.len());
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if body.len() >= limit {
            break;
        }
    }
    Ok(Answer { status, body })
}

/// The `message` field of a JSON error body, or the body itself.
pub(crate) fn error_message(body: &[u8]) -> String {
    #[derive(serde::Deserialize)]
    struct St {
        #[serde(default)]
        message: String,
    }
    match serde_json::from_slice::<St>(body) {
        Ok(st) if !st.message.is_empty() => st.message,
        _ => String::from_utf8_lossy(body).trim().to_string(),
    }
}

/// reqwest's error with its causes ("error sending request: ...: connection refused").
pub(crate) fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        let m = c.to_string();
        if !s.contains(&m) {
            s.push_str(": ");
            s.push_str(&m);
        }
        cur = c.source();
    }
    s
}

/// `application/x-www-form-urlencoded` query string with sorted keys, as Go's `url.Values.Encode`.
pub(crate) fn query(pairs: &[(&str, &str)]) -> String {
    let mut sorted: Vec<&(&str, &str)> = pairs.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in sorted {
        s.append_pair(k, v);
    }
    s.finish()
}

/// Percent-escapes one URL path segment, as Go's `url.PathEscape`.
pub(crate) fn path_escape(s: &str) -> String {
    use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
    const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~')
        .remove(b'$')
        .remove(b'&')
        .remove(b'+')
        .remove(b',')
        .remove(b'=')
        .remove(b':')
        .remove(b'@');
    utf8_percent_encode(s, SEGMENT).to_string()
}
