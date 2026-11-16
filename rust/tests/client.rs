//! The lower layer against the stand-in plane: sessions, exec, first attach, tokens.

mod common;

use std::io::Read as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chainguard_sandbox::Error;
use chainguard_sandbox::workspaces::{self, ExecOptions, ListOptions, TokenSource, pb};
use common::*;
use tonic::Status;

fn argv(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn list_and_resolve() {
    let (c, p) = plane().await;
    *p.sessions.lock().unwrap() = vec![
        session("aaaa1111", "app/quiet-harbor"),
        session("bbbb2222", "other/quiet-harbor"),
    ];
    let all = c.list(&ListOptions::default()).await.unwrap();
    assert_eq!(all.len(), 2, "follows the page token");
    assert!(
        p.auth
            .lock()
            .unwrap()
            .iter()
            .all(|a| a == "Bearer test-token")
    );
    assert!(p.user_agents.lock().unwrap()[0].starts_with("chainguard-sandbox-sdk-rs"));

    assert_eq!(c.resolve("bbbb2222").await.unwrap().short_id, "bbbb2222");
    assert_eq!(
        c.resolve("app/quiet-harbor").await.unwrap().short_id,
        "aaaa1111"
    );
    assert_eq!(
        c.resolve("acme/app/quiet-harbor").await.unwrap().short_id,
        "aaaa1111",
        "owner-qualified"
    );
    // Two suffix matches are ambiguous; an exact display name wins over them.
    assert!(matches!(
        c.resolve("quiet-harbor").await,
        Err(Error::Ambiguous { .. })
    ));
    p.sessions
        .lock()
        .unwrap()
        .push(session("cccc3333", "quiet-harbor"));
    assert_eq!(
        c.resolve("quiet-harbor").await.unwrap().short_id,
        "cccc3333"
    );
    assert!(matches!(
        c.resolve("nothing").await,
        Err(Error::NotFound { .. })
    ));
    let full = format!("instances/{TEST_ORG}/sessions/aaaa1111");
    assert_eq!(c.resolve(&full).await.unwrap().short_id, "aaaa1111");
}

#[tokio::test]
async fn exec_results_and_outcomes() {
    let (c, p) = plane().await;
    let mut out = Vec::new();
    let mut sink = |b: &[u8]| out.extend_from_slice(b);
    let o = ExecOptions {
        stdin: b"hello".to_vec(),
        stdout: Some(&mut sink),
        ..Default::default()
    };
    let r = c.exec("s", &argv(&["cat"]), o).await.unwrap();
    assert_eq!((r.exit_code, out.as_slice()), (0, &b"hello"[..]));
    // Without no_dir, argv runs in /work through chainctl's cd wrapper.
    let sent = p.execs.lock().unwrap()[0].argv.clone();
    assert_eq!(
        &sent[..5],
        ["sh", "-c", r#"cd "$1" && shift && exec "$@""#, "_", "/work"]
    );

    let r = c
        .exec(
            "s",
            &argv(&["false"]),
            ExecOptions {
                no_dir: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(r.exit_code, 1);
    match c
        .output(
            "s",
            &argv(&["false"]),
            ExecOptions {
                no_dir: true,
                ..Default::default()
            },
        )
        .await
    {
        Err(Error::Exit {
            exit_code: 1,
            stderr,
            ..
        }) => assert_eq!(stderr, b"nope\n"),
        other => panic!("{other:?}"),
    }
    // A stream that ends without an exit status: the outcome is unknown.
    assert!(matches!(
        c.exec(
            "s",
            &argv(&["vanish"]),
            ExecOptions {
                no_dir: true,
                ..Default::default()
            }
        )
        .await,
        Err(Error::OutcomeUnknown { cause: None, .. })
    ));
    // A transport-class error after the first event is an unknown outcome with the cause kept.
    *p.exec_fn.lock().unwrap() = Some(Box::new(|_, _| {
        vec![stdout("x"), Err(Status::unavailable("connection reset"))]
    }));
    let e = c
        .exec(
            "s",
            &argv(&["x"]),
            ExecOptions {
                no_dir: true,
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(e, Error::OutcomeUnknown { cause: Some(_), .. }));
    assert_eq!(e.grpc_code(), Some(tonic::Code::Unavailable));
    // An answer from the plane before anything ran is returned as it is.
    *p.exec_fn.lock().unwrap() = Some(Box::new(|_, _| vec![Err(Status::not_found("no session"))]));
    assert_eq!(
        c.exec("s", &argv(&["x"]), ExecOptions::default())
            .await
            .unwrap_err()
            .grpc_code(),
        Some(tonic::Code::NotFound)
    );
    // The stdin limit is checked before sending.
    let big = ExecOptions {
        stdin: vec![0; workspaces::MAX_EXEC_STDIN + 1],
        ..Default::default()
    };
    assert!(c.exec("s", &argv(&["cat"]), big).await.is_err());
    assert!(c.exec("s", &[], ExecOptions::default()).await.is_err());
}

#[tokio::test]
async fn exec_retries_while_another_controller_holds_the_guest() {
    let (c, p) = plane().await;
    let t0 = std::time::Instant::now();
    let r = c
        .exec(
            "s",
            &argv(&["busy-once"]),
            ExecOptions {
                no_dir: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(r.exit_code, 0);
    assert_eq!(p.exec_calls.load(Ordering::SeqCst), 2);
    assert!(
        t0.elapsed() >= Duration::from_secs(2),
        "waits before retrying"
    );
    // And it releases every stream.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(p.open_streams.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn is_guest_held_matches_the_plane_messages() {
    let held = |s: Status| workspaces::is_guest_held(&Error::Rpc(s));
    assert!(held(Status::aborted("instance is held by a shell")));
    assert!(held(Status::failed_precondition(
        "instance held by another controller"
    )));
    assert!(!held(Status::failed_precondition(
        "attach with a shell first"
    )));
    assert!(!held(Status::unavailable("held")));
}

#[tokio::test]
async fn park_heartbeat_and_delete() {
    let (c, p) = plane().await;
    c.park("instances/x/sessions/y").await.unwrap();
    let u = p.updates.lock().unwrap()[0].clone();
    assert_eq!(u.update_mask.unwrap().paths, ["park_request"]);
    assert_eq!(
        u.session.unwrap().park_request,
        pb::session::ParkRequest::Park as i32
    );
    c.heartbeat("n", Duration::ZERO).await.unwrap();
    c.heartbeat("n", Duration::from_secs(90)).await.unwrap();
    let hb = p.heartbeats.lock().unwrap().clone();
    assert!(hb[0].hold_for.is_none());
    assert_eq!(hb[1].hold_for.unwrap().seconds, 90);
}

#[tokio::test]
async fn first_attach_uploads_an_empty_tree() {
    let (c, p) = plane().await;
    c.complete_first_attach("instances/x/sessions/y")
        .await
        .unwrap();
    let open = p.opens.lock().unwrap()[0].clone();
    assert_eq!(open.name, "instances/x/sessions/y");
    assert!(open.resume.is_some(), "an empty Resume is sent");
    let chunk = p.chunks.lock().unwrap()[0].clone();
    assert!(chunk.last);
    assert_eq!(
        chunk.format,
        pb::attach_request::workspace_chunk::Format::FullTar as i32
    );
    assert_eq!(
        chunk.encoding,
        pb::attach_request::workspace_chunk::Encoding::Gzip as i32
    );
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(&chunk.data[..])
        .read_to_end(&mut raw)
        .unwrap();
    assert_eq!(raw, vec![0u8; 1024]);

    *p.attach_mode.lock().unwrap() = Some(AttachMode::NotPending);
    c.complete_first_attach("n").await.unwrap();
    assert_eq!(
        p.chunks.lock().unwrap().len(),
        1,
        "nothing uploaded when nothing is pending"
    );

    *p.attach_mode.lock().unwrap() = Some(AttachMode::Detach);
    assert!(matches!(
        c.complete_first_attach("n").await,
        Err(Error::Detached)
    ));
    *p.attach_err.lock().unwrap() = Some(Status::permission_denied("no"));
    assert_eq!(
        c.complete_first_attach("n").await.unwrap_err().grpc_code(),
        Some(tonic::Code::PermissionDenied)
    );
}

#[tokio::test]
async fn sessions_describe_what_chainctl_sends() {
    let s = workspaces::repository_session("git@github.com:o/r.git", "abc");
    let Some(pb::session::Environment::Source(src)) = s.environment else {
        panic!()
    };
    assert_eq!(
        (src.repository.as_str(), src.revision.as_str()),
        ("https://github.com/o/r", "abc")
    );
    assert_eq!(src.fallback_document, workspaces::DEFAULT_ENVIRONMENT);
    assert_eq!(s.idle, pb::IdlePolicy::AutoHold as i32);
    let s = workspaces::workspace_session("/l", "", "", None);
    let Some(pb::session::Environment::Workspace(w)) = s.environment else {
        panic!()
    };
    assert_eq!(w.provenance, pb::Provenance::Synthesized as i32);
    assert!(String::from_utf8_lossy(&w.document).contains("develop"));
    assert!(workspaces::is_session_name("instances/a/sessions/b"));
    assert!(!workspaces::is_session_name("instances/a/sessions/"));
    assert_eq!(workspaces::instance_name("x"), "instances/x");
}

#[tokio::test]
async fn cached_token_shares_one_fetch_and_invalidates() {
    let n = Arc::new(AtomicUsize::new(0));
    let n2 = n.clone();
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    let src = workspaces::token_fn(move || {
        let n = n2.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            n.fetch_add(1, Ordering::SeqCst);
            Ok(jwt(serde_json::json!({ "exp": exp })))
        }
    });
    let ts = workspaces::cached(src);
    let (a, b) = tokio::join!(ts.token(), ts.token());
    assert_eq!(a.unwrap(), b.unwrap());
    assert_eq!(
        n.load(Ordering::SeqCst),
        1,
        "concurrent callers share one fetch"
    );
    ts.token().await.unwrap();
    assert_eq!(n.load(Ordering::SeqCst), 1, "reused until near expiry");
    ts.invalidate();
    ts.token().await.unwrap();
    assert_eq!(
        n.load(Ordering::SeqCst),
        2,
        "fetched again after invalidate"
    );
}

#[tokio::test]
async fn unauthenticated_drops_the_cached_token() {
    let (c0, p) = plane().await;
    let _ = c0;
    let n = Arc::new(AtomicUsize::new(0));
    let n2 = n.clone();
    let ts = workspaces::cached(workspaces::token_fn(move || {
        let n = n2.clone();
        async move { Ok(format!("tok-{}", n.fetch_add(1, Ordering::SeqCst))) }
    }));
    // A second client on the same plane with the counting source.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let svc = pb::sessions_server::SessionsServer::new(PlaneService(p.clone()));
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(svc)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
    );
    let c = workspaces::Client::new(workspaces::Options {
        url: format!("http://{addr}"),
        instance: TEST_ORG.into(),
        token_source: Some(ts),
        ..Default::default()
    })
    .await
    .unwrap();
    *p.get_fn.lock().unwrap() = Some(Box::new(|_, _| Err(Status::unauthenticated("expired"))));
    let _ = c.get("x").await;
    let _ = c.get("x").await;
    let auths = p.auth.lock().unwrap().clone();
    assert_eq!(auths, ["Bearer tok-0", "Bearer tok-1"]);
}

#[tokio::test]
async fn write_file_refuses_empty_paths_and_reports_directories() {
    let (c, p) = plane().await;
    assert!(c.write_file("s", " ", b"x".to_vec(), 0o644).await.is_err());
    *p.exec_fn.lock().unwrap() = Some(Box::new(|_, _| {
        vec![stderr("/work/d: is a directory\n"), exited(21)]
    }));
    assert!(matches!(
        c.write_file("s", "d", b"x".to_vec(), 0o644).await,
        Err(Error::Exit { exit_code: 21, .. })
    ));
    let req = p.execs.lock().unwrap()[0].clone();
    assert_eq!(&req.argv[req.argv.len() - 2..], ["/work/d", "644"]);
}

#[cfg(unix)]
#[test]
fn tar_dir_is_root_owned_keeps_links_and_skips() {
    let d = temp_dir("tar");
    std::fs::create_dir_all(d.join("a/skipme")).unwrap();
    std::fs::write(d.join("a/f.txt"), "hello").unwrap();
    std::fs::write(d.join("a/skipme/x"), "x").unwrap();
    std::os::unix::fs::symlink("f.txt", d.join("a/ln")).unwrap();
    let skip = |rel: &str, _dir: bool| rel == "a/skipme";
    let tgz = workspaces::tar_gz_dir(&d, Some(&skip)).unwrap();
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(&tgz[..]));
    let mut names = Vec::new();
    for e in ar.entries().unwrap() {
        let e = e.unwrap();
        let h = e.header();
        assert_eq!((h.uid().unwrap(), h.gid().unwrap()), (0, 0));
        if h.entry_type().is_symlink() {
            assert_eq!(e.link_name().unwrap().unwrap().to_str(), Some("f.txt"));
        }
        names.push(e.path().unwrap().to_string_lossy().into_owned());
    }
    assert_eq!(names, ["a/", "a/f.txt", "a/ln"]);
    std::fs::remove_dir_all(&d).unwrap();
}

#[tokio::test]
async fn identity_exchange_sends_the_id_token_and_parameters() {
    let (url, seen) = http_server(Arc::new(|r| {
        if r.bearer() == "oidc-token" {
            (200, r#"{"token":"cg-token"}"#.into())
        } else {
            (403, r#"{"code":7,"message":"nope"}"#.into())
        }
    }))
    .await;
    let x = workspaces::IdentityExchange {
        identity: "id-1".into(),
        id_token: Arc::new(|| Box::pin(async { Ok("oidc-token".to_string()) })),
        issuer: url.clone(),
        audience: String::new(),
    };
    assert_eq!(x.token().await.unwrap(), "cg-token");
    let r = seen.lock().unwrap()[0].clone();
    assert_eq!(
        (r.method.as_str(), r.path.as_str()),
        ("POST", "/sts/exchange")
    );
    assert_eq!(r.query["aud"], workspaces::DEFAULT_AUDIENCE);
    assert_eq!(r.query["identity"], "id-1");
    assert_eq!(
        r.headers.get("content-length").map(String::as_str),
        Some("0")
    );
    let bad = workspaces::IdentityExchange {
        id_token: Arc::new(|| Box::pin(async { Ok("wrong".to_string()) })),
        ..x
    };
    let e = bad.token().await.unwrap_err().to_string();
    assert!(e.contains("HTTP 403: nope"), "{e}");
}
