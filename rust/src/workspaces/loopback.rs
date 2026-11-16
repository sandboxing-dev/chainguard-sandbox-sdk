//! A small HTTP/1.1 server on 127.0.0.1 for browser redirects (the login callback and the GitHub
//! authorization). It answers one request per connection and closes it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// One request, as the handler sees it.
pub(crate) struct LoopbackRequest {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
}

/// The handler's answer.
pub(crate) struct LoopbackReply {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: String,
}

impl LoopbackReply {
    pub fn text(status: u16, headers: &[(&'static str, &str)], body: impl Into<String>) -> Self {
        let mut h: Vec<(&'static str, String)> =
            headers.iter().map(|(k, v)| (*k, v.to_string())).collect();
        if !h
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        {
            h.push(("Content-Type", "text/plain; charset=utf-8".into()));
        }
        LoopbackReply {
            status,
            headers: h,
            body: body.into(),
        }
    }
}

pub(crate) struct LoopbackServer {
    pub port: u16,
    accept: JoinHandle<()>,
}

impl LoopbackServer {
    /// Listens on 127.0.0.1:port (0 picks a free port).
    pub async fn bind<F>(port: u16, handler: F) -> std::io::Result<LoopbackServer>
    where
        F: Fn(LoopbackRequest) -> LoopbackReply + Send + Sync + 'static,
    {
        let listener = TcpListener::bind(("127.0.0.1", port)).await?;
        let port = listener.local_addr()?.port();
        let handler = Arc::new(handler);
        let accept = tokio::spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                let h = handler.clone();
                tokio::spawn(async move {
                    let _ =
                        tokio::time::timeout(Duration::from_secs(10), serve_one(conn, &*h)).await;
                });
            }
        });
        Ok(LoopbackServer { port, accept })
    }

    /// Stops accepting connections. A response already being written finishes.
    pub fn stop(&self) {
        self.accept.abort();
    }
}

impl Drop for LoopbackServer {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

async fn serve_one<F>(mut conn: TcpStream, handler: &F) -> std::io::Result<()>
where
    F: Fn(LoopbackRequest) -> LoopbackReply,
{
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = conn.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > 64 * 1024 {
            return Ok(());
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let line = head.lines().next().unwrap_or_default();
    let mut parts = line.split(' ');
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or("/");
    let (path, q) = target.split_once('?').unwrap_or((target, ""));
    let query = url::form_urlencoded::parse(q.as_bytes())
        .into_owned()
        .collect();
    let reply = handler(LoopbackRequest {
        method,
        path: path.to_string(),
        query,
    });
    let mut out = format!("HTTP/1.1 {} {}\r\n", reply.status, reason(reply.status));
    for (k, v) in &reply.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        reply.body.len()
    ));
    out.push_str(&reply.body);
    conn.write_all(out.as_bytes()).await?;
    conn.shutdown().await
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        303 => "See Other",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "",
    }
}
