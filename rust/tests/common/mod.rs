//! Stand-ins for the sandbox plane (an in-process gRPC server) and the HTTP services (issuer,
//! platform API), for the tests. Nothing here talks to the real service.
#![allow(dead_code)]

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chainguard_sandbox::workspaces::pb::{self, sessions_server::Sessions};
use chainguard_sandbox::workspaces::{Client, Options, StaticToken};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_stream::Stream;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status, Streaming};

pub const TEST_ORG: &str = "0123456789abcdef0123456789abcdef01234567";

/// Environment variables are process-wide: tests that set them take this lock.
pub static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub type ExecFn =
    Box<dyn Fn(&pb::ExecRequest, usize) -> Vec<Result<pb::ExecResponse, Status>> + Send + Sync>;
pub type GetFn = Box<dyn Fn(&str, usize) -> Result<pb::Session, Status> + Send + Sync>;

#[derive(Clone, Copy, PartialEq)]
pub enum AttachMode {
    /// Opened with workspace_pending, then WorkspaceReady once a chunk arrives.
    Pending,
    /// Opened without anything pending.
    NotPending,
    /// Detached right away.
    Detach,
}

#[derive(Default)]
pub struct Plane {
    pub sessions: Mutex<Vec<pb::Session>>,
    pub auth: Mutex<Vec<String>>,
    pub user_agents: Mutex<Vec<String>>,
    pub deletes: Mutex<Vec<String>>,
    pub creates: Mutex<Vec<pb::Session>>,
    pub updates: Mutex<Vec<pb::UpdateSessionRequest>>,
    pub heartbeats: Mutex<Vec<pb::HeartbeatSessionRequest>>,
    pub chunks: Mutex<Vec<pb::attach_request::WorkspaceChunk>>,
    pub opens: Mutex<Vec<pb::attach_request::Open>>,
    pub execs: Mutex<Vec<pb::ExecRequest>>,
    pub exec_fn: Mutex<Option<ExecFn>>,
    pub get_fn: Mutex<Option<GetFn>>,
    pub create_err: Mutex<Option<Status>>,
    pub delete_err: Mutex<Option<Status>>,
    pub attach_mode: Mutex<Option<AttachMode>>,
    pub attach_err: Mutex<Option<Status>>,
    pub get_delay: Mutex<Option<std::time::Duration>>,
    pub exec_delay: Mutex<Option<std::time::Duration>>,
    pub attaches: AtomicUsize,
    pub exec_calls: AtomicUsize,
    pub get_calls: AtomicUsize,
    pub open_streams: Arc<AtomicUsize>,
}

pub fn session(short: &str, display: &str) -> pb::Session {
    pb::Session {
        name: format!("instances/{TEST_ORG}/sessions/{short}"),
        short_id: short.into(),
        display_name: display.into(),
        owner: "users/me".into(),
        phase: pb::Phase::Idle as i32,
        ..Default::default()
    }
}

pub fn started(short: &str) -> pb::Session {
    let mut s = session(short, "sandbox/quiet-harbor");
    s.guest_started_time = Some(prost_types::Timestamp {
        seconds: 1,
        nanos: 0,
    });
    s.workspace_pending = true;
    s.environment = Some(pb::session::Environment::Workspace(pb::Workspace {
        label: "/chainguard-sandbox/sandbox".into(),
        ..Default::default()
    }));
    s
}

pub fn stdout(s: &str) -> Result<pb::ExecResponse, Status> {
    Ok(pb::ExecResponse {
        event: Some(pb::exec_response::Event::Stdout(s.as_bytes().to_vec())),
    })
}

pub fn stderr(s: &str) -> Result<pb::ExecResponse, Status> {
    Ok(pb::ExecResponse {
        event: Some(pb::exec_response::Event::Stderr(s.as_bytes().to_vec())),
    })
}

pub fn exited(code: i32) -> Result<pb::ExecResponse, Status> {
    Ok(pb::ExecResponse {
        event: Some(pb::exec_response::Event::Exited(
            pb::exec_response::Exited {
                code,
                timed_out: false,
            },
        )),
    })
}

/// Counts open exec streams, so a test can check they are released.
struct Counted<S> {
    inner: S,
    n: Arc<AtomicUsize>,
}

impl<S: Stream + Unpin> Stream for Counted<S> {
    type Item = S::Item;
    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

impl<S> Drop for Counted<S> {
    fn drop(&mut self) {
        self.n.fetch_sub(1, Ordering::SeqCst);
    }
}

fn record<T>(p: &Plane, r: &Request<T>) {
    let h = |k: &str| {
        r.metadata()
            .get(k)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    };
    p.auth.lock().unwrap().push(h("authorization"));
    p.user_agents.lock().unwrap().push(h("user-agent"));
}

#[derive(Clone)]
pub struct PlaneService(pub Arc<Plane>);

#[tonic::async_trait]
impl Sessions for PlaneService {
    async fn create_session(
        &self,
        r: Request<pb::CreateSessionRequest>,
    ) -> Result<Response<pb::Session>, Status> {
        record(&self.0, &r);
        if let Some(e) = self.0.create_err.lock().unwrap().clone() {
            return Err(e);
        }
        let mut s = r.into_inner().session.unwrap_or_default();
        self.0.creates.lock().unwrap().push(s.clone());
        let n = self.0.creates.lock().unwrap().len();
        s.name = format!("instances/{TEST_ORG}/sessions/0000000{n}");
        s.short_id = format!("0000000{n}");
        Ok(Response::new(s))
    }

    async fn get_session(
        &self,
        r: Request<pb::GetSessionRequest>,
    ) -> Result<Response<pb::Session>, Status> {
        record(&self.0, &r);
        let n = self.0.get_calls.fetch_add(1, Ordering::SeqCst);
        let name = r.into_inner().name;
        let delay = *self.0.get_delay.lock().unwrap();
        if let Some(d) = delay {
            tokio::time::sleep(d).await;
        }
        if let Some(f) = self.0.get_fn.lock().unwrap().as_ref() {
            return f(&name, n).map(Response::new);
        }
        let ss = self.0.sessions.lock().unwrap();
        ss.iter()
            .find(|s| s.name == name)
            .cloned()
            .map(Response::new)
            .ok_or_else(|| Status::not_found("no such session"))
    }

    async fn list_sessions(
        &self,
        r: Request<pb::ListSessionsRequest>,
    ) -> Result<Response<pb::ListSessionsResponse>, Status> {
        record(&self.0, &r);
        let req = r.into_inner();
        let ss = self.0.sessions.lock().unwrap().clone();
        // Two pages, to exercise the page tokens.
        let (page, next) = if req.page_token.is_empty() && ss.len() > 1 {
            (ss[..1].to_vec(), "page-2".to_string())
        } else if req.page_token == "page-2" {
            (ss[1..].to_vec(), String::new())
        } else {
            (ss, String::new())
        };
        Ok(Response::new(pb::ListSessionsResponse {
            sessions: page,
            next_page_token: next,
        }))
    }

    async fn update_session(
        &self,
        r: Request<pb::UpdateSessionRequest>,
    ) -> Result<Response<pb::Session>, Status> {
        record(&self.0, &r);
        let req = r.into_inner();
        self.0.updates.lock().unwrap().push(req.clone());
        Ok(Response::new(req.session.unwrap_or_default()))
    }

    async fn delete_session(
        &self,
        r: Request<pb::DeleteSessionRequest>,
    ) -> Result<Response<()>, Status> {
        record(&self.0, &r);
        let name = r.into_inner().name;
        self.0.deletes.lock().unwrap().push(name.clone());
        if let Some(e) = self.0.delete_err.lock().unwrap().clone() {
            return Err(e);
        }
        self.0.sessions.lock().unwrap().retain(|s| s.name != name);
        Ok(Response::new(()))
    }

    async fn heartbeat_session(
        &self,
        r: Request<pb::HeartbeatSessionRequest>,
    ) -> Result<Response<pb::Session>, Status> {
        record(&self.0, &r);
        let req = r.into_inner();
        self.0.heartbeats.lock().unwrap().push(req.clone());
        Ok(Response::new(pb::Session {
            name: req.name,
            ..Default::default()
        }))
    }

    type AttachSessionStream = ReceiverStream<Result<pb::AttachResponse, Status>>;

    async fn attach_session(
        &self,
        r: Request<Streaming<pb::AttachRequest>>,
    ) -> Result<Response<Self::AttachSessionStream>, Status> {
        record(&self.0, &r);
        self.0.attaches.fetch_add(1, Ordering::SeqCst);
        if let Some(e) = self.0.attach_err.lock().unwrap().clone() {
            return Err(e);
        }
        let mode = self
            .0
            .attach_mode
            .lock()
            .unwrap()
            .unwrap_or(AttachMode::Pending);
        let mut inbound = r.into_inner();
        let (tx, rx) = mpsc::channel(8);
        let p = self.0.clone();
        tokio::spawn(async move {
            while let Ok(Some(m)) = inbound.message().await {
                match m.input {
                    Some(pb::attach_request::Input::Open(o)) => {
                        p.opens.lock().unwrap().push(o);
                        let out = match mode {
                            AttachMode::Detach => {
                                pb::attach_response::Output::Detached(Default::default())
                            }
                            m => pb::attach_response::Output::Opened(pb::attach_response::Opened {
                                workspace_pending: m == AttachMode::Pending,
                                workspace_encodings: vec![
                                    pb::attach_request::workspace_chunk::Encoding::Gzip as i32,
                                ],
                                ..Default::default()
                            }),
                        };
                        // Something the SDK's subset doesn't know comes first, as on a real shell attach.
                        let _ = tx.send(Ok(pb::AttachResponse { output: None })).await;
                        let _ = tx.send(Ok(pb::AttachResponse { output: Some(out) })).await;
                    }
                    Some(pb::attach_request::Input::WorkspaceChunk(c)) => {
                        p.chunks.lock().unwrap().push(c);
                        let _ = tx
                            .send(Ok(pb::AttachResponse {
                                output: Some(pb::attach_response::Output::WorkspaceReady(
                                    Default::default(),
                                )),
                            }))
                            .await;
                    }
                    None => {}
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    type ExecStream = Pin<Box<dyn Stream<Item = Result<pb::ExecResponse, Status>> + Send>>;

    async fn exec(
        &self,
        r: Request<pb::ExecRequest>,
    ) -> Result<Response<Self::ExecStream>, Status> {
        record(&self.0, &r);
        let n = self.0.exec_calls.fetch_add(1, Ordering::SeqCst);
        let req = r.into_inner();
        self.0.execs.lock().unwrap().push(req.clone());
        let delay = *self.0.exec_delay.lock().unwrap();
        if let Some(d) = delay {
            tokio::time::sleep(d).await;
        }
        let events = match self.0.exec_fn.lock().unwrap().as_ref() {
            Some(f) => f(&req, n),
            None => default_exec(&req, n),
        };
        self.0.open_streams.fetch_add(1, Ordering::SeqCst);
        let s = Counted {
            inner: tokio_stream::iter(events),
            n: self.0.open_streams.clone(),
        };
        Ok(Response::new(Box::pin(s)))
    }
}

/// cat echoes stdin; false exits 1; vanish ends without an exit status; busy-once is refused
/// the way the plane refuses an exec while another controller holds the guest, then runs.
fn default_exec(req: &pb::ExecRequest, n: usize) -> Vec<Result<pb::ExecResponse, Status>> {
    // Look past chainctl's cd wrapper.
    let argv = if req.argv.len() > 5 && req.argv[2].starts_with("cd ") {
        &req.argv[5..]
    } else {
        &req.argv[..]
    };
    match argv.first().map(String::as_str) {
        Some("cat") => vec![
            Ok(pb::ExecResponse {
                event: Some(pb::exec_response::Event::Stdout(req.stdin.clone())),
            }),
            exited(0),
        ],
        Some("false") => vec![stderr("nope\n"), exited(1)],
        Some("vanish") => vec![stdout("partial")],
        Some("busy-once") if n == 0 => vec![Err(Status::failed_precondition(
            "instance held by another controller (request a takeover to preempt): Channel already in use",
        ))],
        _ => vec![stdout("ok\n"), exited(0)],
    }
}

/// Starts the stand-in plane and a client for it (token "test-token", organization TEST_ORG).
pub async fn plane() -> (Client, Arc<Plane>) {
    let p = Arc::new(Plane::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let svc = pb::sessions_server::SessionsServer::new(PlaneService(p.clone()));
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(svc)
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    let c = Client::new(Options {
        url: format!("http://127.0.0.1:{port}"),
        instance: TEST_ORG.into(),
        token_source: Some(Arc::new(StaticToken("test-token".into()))),
        ..Default::default()
    })
    .await
    .unwrap();
    (c, p)
}

// ---- HTTP -----------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl HttpRequest {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_default()
    }
    pub fn bearer(&self) -> String {
        self.headers
            .get("authorization")
            .and_then(|a| a.strip_prefix("Bearer "))
            .unwrap_or_default()
            .to_string()
    }
}

pub type Handler = Arc<dyn Fn(&HttpRequest) -> (u16, String) + Send + Sync>;

/// A small HTTP/1.1 server: one request per connection.
pub async fn http_server(handler: Handler) -> (String, Arc<Mutex<Vec<HttpRequest>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            let h = handler.clone();
            let seen = s2.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    let n = conn.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
                let mut lines = head.split("\r\n");
                let first = lines.next().unwrap_or_default();
                let mut parts = first.split(' ');
                let method = parts.next().unwrap_or_default().to_string();
                let target = parts.next().unwrap_or("/").to_string();
                let headers: HashMap<String, String> = lines
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
                    .collect();
                let len: usize = headers
                    .get("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let mut body = buf[head_end + 4..].to_vec();
                while body.len() < len {
                    let n = conn.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&chunk[..n]);
                }
                let (path, q) = target.split_once('?').unwrap_or((&target, ""));
                let query = url::form_urlencoded::parse(q.as_bytes())
                    .into_owned()
                    .collect();
                let req = HttpRequest {
                    method,
                    path: path.to_string(),
                    query,
                    headers,
                    body,
                };
                seen.lock().unwrap().push(req.clone());
                let (status, body) = h(&req);
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = conn.write_all(resp.as_bytes()).await;
                let _ = conn.shutdown().await;
            });
        }
    });
    (url, seen)
}

/// An unsigned JWT with these claims.
pub fn jwt(claims: serde_json::Value) -> String {
    use base64::Engine as _;
    let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        "{}.{}.sig",
        e.encode(r#"{"alg":"none"}"#),
        e.encode(claims.to_string())
    )
}

pub fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "cgs-test-{tag}-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn rand_suffix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    static N: AtomicUsize = AtomicUsize::new(0);
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u64
        + N.fetch_add(1, Ordering::SeqCst) as u64
}
