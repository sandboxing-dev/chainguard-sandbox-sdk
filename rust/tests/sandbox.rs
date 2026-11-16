//! The sandbox API against the stand-in plane, and turning Workspaces on against a stand-in
//! platform API. The cases follow the Go SDK's tests.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chainguard_sandbox::workspaces::{self, pb};
use chainguard_sandbox::{ConnectOptions, CreateOptions, Error, RunOptions, Sandbox, SetupOptions};
use common::*;
use tonic::Status;

fn shorten() {
    // placement grace, start poll, kill wait, kill poll (ms)
    chainguard_sandbox::shorten_timings_for_tests(30_000, 10, 150, 10);
}

/// A handle to a sandbox on the stand-in plane, without creating one.
async fn handle(c: &workspaces::Client, p: &Plane) -> Sandbox {
    let mut s = session("01234567", "sandbox/quiet-harbor");
    s.environment = Some(pb::session::Environment::Workspace(pb::Workspace {
        label: "/chainguard-sandbox/sandbox".into(),
        ..Default::default()
    }));
    p.sessions.lock().unwrap().push(s.clone());
    Sandbox::connect(
        &s.name,
        ConnectOptions {
            client: Some(c.clone()),
            ..Default::default()
        },
    )
    .await
    .unwrap()
}

#[test]
fn scripts_are_byte_identical_to_the_go_source() {
    // The Go source is in the repository, not in the published crate.
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../go/sandbox/scripts.go");
    let Ok(go) = std::fs::read_to_string(path) else {
        eprintln!("skipped: {path} is not here (outside the repository)");
        return;
    };
    let stat_fn = between(&go, "const statFn = `", "`");
    for (name, text) in chainguard_sandbox::script_text_for_tests() {
        // "name = `...`" or "name    = statFn + `...`" (gofmt aligns the = signs)
        let rest = go
            .match_indices(name)
            .filter(|(i, _)| !go[..*i].ends_with(|c: char| c.is_alphanumeric()))
            .map(|(i, _)| go[i + name.len()..].trim_start_matches(' '))
            .find(|r| r.starts_with("= "))
            .unwrap_or_else(|| panic!("{name} not in scripts.go"));
        let rest = &rest[2..];
        let want = if let Some(r) = rest.strip_prefix("statFn + `") {
            format!("{stat_fn}{}", &r[..r.find('`').unwrap()])
        } else {
            let r = rest.strip_prefix('`').unwrap();
            r[..r.find('`').unwrap()].to_string()
        };
        assert_eq!(
            text, want,
            "{name} differs from go/sandbox/scripts.go: run python3 scripts/gen_sandbox_scripts.py"
        );
    }
}

fn between<'a>(s: &'a str, a: &str, b: &str) -> &'a str {
    let i = s.find(a).unwrap() + a.len();
    &s[i..i + s[i..].find(b).unwrap()]
}

#[test]
fn document() {
    let d = String::from_utf8(chainguard_sandbox::document(&CreateOptions {
        packages: Some(vec!["python-3.13".into(), r#"we"ird"#.into()]),
        egress: Some(vec!["example.com".into()]),
        egress_policy: "destroy".into(),
        ..Default::default()
    }))
    .unwrap();
    for want in [
        "version: 1\n",
        "  packages:\n    - \"python-3.13\"\n    - \"we\\\"ird\"\n",
        "      egress:\n        - \"example.com\"\n",
        "      policy: \"destroy\"\n",
    ] {
        assert!(d.contains(want), "document lacks {want:?}:\n{d}");
    }
    let def = String::from_utf8(chainguard_sandbox::document(&CreateOptions::default())).unwrap();
    assert!(
        def.contains(r#"- "python-3.13""#)
            && def.contains(r#"- "*""#)
            && def.contains(r#"policy: "deny""#),
        "{def}"
    );
    let empty = String::from_utf8(chainguard_sandbox::document(&CreateOptions {
        egress: Some(vec![]),
        ..Default::default()
    }))
    .unwrap();
    assert!(
        empty.contains("egress: []"),
        "an empty allowlist must be explicit"
    );
    let none = String::from_utf8(chainguard_sandbox::document(&CreateOptions {
        packages: Some(vec![]),
        ..Default::default()
    }))
    .unwrap();
    assert!(
        none.contains("  packages:\nchecks:"),
        "no packages is not the defaults:\n{none}"
    );
    assert_eq!(
        chainguard_sandbox::document(&CreateOptions {
            document: Some(b"custom".to_vec()),
            ..Default::default()
        }),
        b"custom"
    );
}

#[tokio::test]
async fn create_waits_uploads_and_returns_a_handle() {
    shorten();
    let (c, p) = plane().await;
    let n = std::sync::atomic::AtomicUsize::new(0);
    let _ = n;
    *p.get_fn.lock().unwrap() = Some(Box::new(|name, i| {
        let mut s = started(name.rsplit('/').next().unwrap());
        s.name = name.into();
        if i == 0 {
            s.guest_started_time = None; // not yet
        }
        Ok(s)
    }));
    let sbx = Sandbox::create(CreateOptions {
        client: Some(c.clone()),
        name: "job".into(),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(sbx.id, "00000001");
    let created = p.creates.lock().unwrap()[0].clone();
    let Some(pb::session::Environment::Workspace(w)) = created.environment else {
        panic!()
    };
    assert_eq!(w.label, "/chainguard-sandbox/job");
    assert_eq!(w.provenance, pb::Provenance::Authored as i32);
    assert_eq!(
        p.chunks.lock().unwrap().len(),
        1,
        "the first attach uploaded the empty tree"
    );
    assert!(
        Sandbox::create(CreateOptions {
            client: Some(c),
            name: "a/b".into(),
            ..Default::default()
        })
        .await
        .is_err()
    );
}

#[tokio::test]
async fn create_deletes_the_session_when_startup_fails() {
    shorten();
    let (c, p) = plane().await;
    *p.get_fn.lock().unwrap() = Some(Box::new(|_, _| Err(Status::internal("boom"))));
    assert!(
        Sandbox::create(CreateOptions {
            client: Some(c),
            ..Default::default()
        })
        .await
        .is_err()
    );
    assert_eq!(
        *p.deletes.lock().unwrap(),
        [format!("instances/{TEST_ORG}/sessions/00000001")]
    );
}

#[tokio::test]
async fn create_in_an_organization_without_workspaces() {
    let (c, p) = plane().await;
    *p.create_err.lock().unwrap() = Some(Status::permission_denied(format!(
        "this organization is not entitled to Chainguard Workspaces; an organization owner can turn it on with chainctl develop init --parent {TEST_ORG}, or contact Chainguard"
    )));
    let e = Sandbox::create(CreateOptions {
        client: Some(c.clone()),
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert!(matches!(e, Error::NotEnabled { .. }), "{e:?}");
    assert_eq!(e.grpc_code(), Some(tonic::Code::PermissionDenied));
    assert!(e.to_string().contains("not entitled"));
    *p.create_err.lock().unwrap() = Some(Status::permission_denied("caller lacks sessions.create"));
    let e = Sandbox::create(CreateOptions {
        client: Some(c.clone()),
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert!(
        matches!(e, Error::Rpc(_)),
        "an unrelated permission error stays as it is: {e:?}"
    );
    *p.create_err.lock().unwrap() = Some(Status::resource_exhausted("1 of 1 sessions in use"));
    let e = Sandbox::create(CreateOptions {
        client: Some(c),
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert!(matches!(e, Error::LimitReached { .. }), "{e:?}");
}

#[tokio::test]
async fn start_timeout_bounds_each_call() {
    shorten();
    let (c, p) = plane().await;
    *p.get_delay.lock().unwrap() = Some(Duration::from_secs(2));
    *p.get_fn.lock().unwrap() = Some(Box::new(|name, _| {
        let mut s = started("x");
        s.name = name.into();
        Ok(s) // "started", but only after the limit
    }));
    let t0 = std::time::Instant::now();
    let e = Sandbox::create(CreateOptions {
        client: Some(c),
        start_timeout: Some(Duration::from_millis(50)),
        ..Default::default()
    })
    .await
    .unwrap_err();
    assert!(
        matches!(&e, Error::Timeout { op, .. } if op == "start"),
        "{e:?}"
    );
    assert!(
        t0.elapsed() < Duration::from_secs(1),
        "took {:?}",
        t0.elapsed()
    );
    assert_eq!(
        p.deletes.lock().unwrap().len(),
        1,
        "the created session is deleted"
    );
}

#[tokio::test]
async fn kill_by_id_does_not_attach() {
    shorten();
    let (c, p) = plane().await;
    let mut s = session("01234567", "sandbox/x");
    s.workspace_pending = true;
    p.sessions.lock().unwrap().push(s.clone());
    let o = || ConnectOptions {
        client: Some(c.clone()),
        ..Default::default()
    };
    assert!(chainguard_sandbox::kill(&s.name, o()).await.unwrap());
    assert_eq!(*p.deletes.lock().unwrap(), [s.name.clone()]);
    assert_eq!(p.attaches.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(
        !chainguard_sandbox::kill(&s.name, o()).await.unwrap(),
        "a missing sandbox is false, not an error"
    );
    assert!(!chainguard_sandbox::kill("nothing", o()).await.unwrap());
}

#[tokio::test]
async fn kill_reports_unconfirmed_deletion() {
    shorten();
    let (c, p) = plane().await;
    let sbx = handle(&c, &p).await;
    // Never goes away.
    *p.get_fn.lock().unwrap() = Some(Box::new(|name, _| {
        Ok(pb::Session {
            name: name.into(),
            ..Default::default()
        })
    }));
    *p.delete_err.lock().unwrap() = None;
    let e = sbx.kill().await.unwrap_err();
    assert!(
        matches!(&e, Error::Timeout { op, .. } if op == "kill"),
        "{e:?}"
    );
    // Checks that fail are not taken as "gone" either.
    *p.get_fn.lock().unwrap() = Some(Box::new(|_, _| Err(Status::internal("boom"))));
    let e = sbx.kill().await.unwrap_err();
    assert!(
        matches!(e, Error::Timeout { .. }) && e.to_string().contains("boom"),
        "{e}"
    );
    // And it returns as soon as the session is gone.
    *p.get_fn.lock().unwrap() = Some(Box::new(|_, _| Err(Status::not_found("gone"))));
    assert!(sbx.kill().await.unwrap());
    *p.delete_err.lock().unwrap() = Some(Status::not_found("gone"));
    assert!(!sbx.kill().await.unwrap(), "already gone");
}

#[tokio::test]
async fn interrupted_command_is_outcome_unknown_with_partial_output() {
    let (c, p) = plane().await;
    let sbx = handle(&c, &p).await;
    *p.exec_fn.lock().unwrap() = Some(Box::new(|_, _| {
        vec![
            stdout("partial"),
            Err(Status::unavailable("connection lost")),
        ]
    }));
    let e = sbx.commands.run("do-something").await.unwrap_err();
    assert!(matches!(e, Error::OutcomeUnknown { .. }), "{e:?}");
    assert_eq!(e.grpc_code(), Some(tonic::Code::Unavailable));
    assert!(e.to_string().contains("connection lost"));
    assert_eq!(e.command_result().unwrap().stdout, "partial");
    // The plane refusing the command is an answer, not an unknown outcome.
    *p.exec_fn.lock().unwrap() = Some(Box::new(|_, _| {
        vec![Err(Status::invalid_argument("bad argv"))]
    }));
    let e = sbx.commands.run("x").await.unwrap_err();
    assert!(
        matches!(e, Error::Rpc(_)) && e.grpc_code() == Some(tonic::Code::InvalidArgument),
        "{e:?}"
    );
    // A held guest is Busy.
    *p.exec_fn.lock().unwrap() = Some(Box::new(|_, _| {
        vec![Err(Status::aborted(
            "instance is held by an attached shell",
        ))]
    }));
    assert!(matches!(
        sbx.commands.run("x").await,
        Err(Error::Busy { .. })
    ));
}

#[tokio::test]
async fn run_results_errors_and_the_argv_sent() {
    let (c, p) = plane().await;
    let mut sbx = handle(&c, &p).await;
    let _ = &mut sbx;
    *p.exec_fn.lock().unwrap() = Some(Box::new(|_, _| {
        vec![stdout("out"), stderr("err"), exited(3)]
    }));
    match sbx
        .commands
        .run_with(
            "x",
            RunOptions {
                cwd: "sub".into(),
                envs: [("K".to_string(), "v".to_string())].into(),
                ..Default::default()
            },
        )
        .await
    {
        Err(Error::CommandExit(r)) => assert_eq!(
            (r.exit_code, r.stdout.as_str(), r.stderr.as_str()),
            (3, "out", "err")
        ),
        other => panic!("{other:?}"),
    }
    let req = p.execs.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        &req.argv[3..],
        ["_", "/work/sub", "env", "K=v", "bash", "-lc", "x"]
    );
    assert_eq!(req.deadline.unwrap().seconds, 60, "the default timeout");
    *p.exec_fn.lock().unwrap() = Some(Box::new(|_, _| {
        vec![
            stderr("sandbox: no such directory: /work/nope\n"),
            exited(47),
        ]
    }));
    assert!(
        matches!(sbx.commands.run("x").await, Err(Error::NotFound { what, .. }) if what == "directory")
    );
    *p.exec_fn.lock().unwrap() = Some(Box::new(|_, _| {
        vec![
            stdout("so far"),
            Ok(pb::ExecResponse {
                event: Some(pb::exec_response::Event::Exited(
                    pb::exec_response::Exited {
                        code: 124,
                        timed_out: true,
                    },
                )),
            }),
        ]
    }));
    match sbx
        .commands
        .run_with(
            "x",
            RunOptions {
                timeout: Some(Duration::from_secs(1)),
                ..Default::default()
            },
        )
        .await
    {
        Err(Error::Timeout {
            op,
            result: Some(r),
            ..
        }) => assert_eq!((op.as_str(), r.stdout.as_str()), ("command", "so far")),
        other => panic!("{other:?}"),
    }
    *p.exec_fn.lock().unwrap() = Some(Box::new(|_, _| vec![exited(0)]));
    sbx.commands
        .run_with(
            "x",
            RunOptions {
                timeout: Some(Duration::ZERO),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        p.execs.lock().unwrap().last().unwrap().deadline.is_none(),
        "zero means no limit"
    );
}

#[tokio::test]
async fn callbacks_get_whole_characters() {
    let (c, p) = plane().await;
    let sbx = handle(&c, &p).await;
    let euro = "€".as_bytes().to_vec();
    let e2 = euro.clone();
    *p.exec_fn.lock().unwrap() = Some(Box::new(move |_, _| {
        let ev = |b: Vec<u8>| {
            Ok(pb::ExecResponse {
                event: Some(pb::exec_response::Event::Stdout(b)),
            })
        };
        vec![
            ev([b"a".to_vec(), e2[..1].to_vec()].concat()),
            ev(e2[1..2].to_vec()),
            ev([e2[2..].to_vec(), b"b".to_vec()].concat()),
            exited(0),
        ]
    }));
    let got = Arc::new(Mutex::new(Vec::<String>::new()));
    let g = got.clone();
    let r = sbx
        .commands
        .run_with(
            "x",
            RunOptions {
                on_stdout: Some(Box::new(move |s| g.lock().unwrap().push(s.into()))),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(r.stdout, "a€b");
    assert_eq!(got.lock().unwrap().join("|"), "a|€b");
}

#[tokio::test]
async fn concurrent_polls_do_not_duplicate_output() {
    let (c, p) = plane().await;
    let sbx = handle(&c, &p).await;
    *p.exec_fn.lock().unwrap() = Some(Box::new(|r, _| {
        // connect: resolve the pid's state directory
        if r.argv[2].contains("readlink") {
            return vec![stdout("/tmp/.chainguard-sandbox/cmd/abc\n"), exited(0)];
        }
        // sh -c <poll script> _ <dir> <outOff> <errOff> <wait> <max>
        let off: usize = r.argv[5].parse().unwrap();
        vec![stdout(&format!("- 3 0\n{}", &"abc"[off..])), exited(0)]
    }));
    let h = sbx.commands.connect(7).await.unwrap();
    assert_eq!(h.id, "abc");
    *p.exec_delay.lock().unwrap() = Some(Duration::from_millis(100)); // long enough for the polls to overlap
    let polls: Vec<_> = (0..4)
        .map(|_| {
            let h = h.clone();
            tokio::spawn(async move { h.poll().await })
        })
        .collect();
    for t in polls {
        assert_eq!(t.await.unwrap().unwrap(), None);
    }
    assert_eq!(h.stdout(), "abc");
}

#[tokio::test]
async fn background_commands_start_list_and_wait() {
    let (c, p) = plane().await;
    let sbx = handle(&c, &p).await;
    *p.exec_fn.lock().unwrap() = Some(Box::new(|r, _| {
        let script = &r.argv[2];
        if script.contains("setsid") {
            vec![stdout("4242\n"), exited(0)]
        } else if script.contains("for d in") {
            vec![
                stdout("abc\t4242\t-\tsleep 9\ndef\t17\t0\techo\n"),
                exited(0),
            ]
        } else if script.contains("tail -c") {
            let off: usize = r.argv[5].parse().unwrap();
            let body = "line1\nline2\n";
            vec![
                stdout(&format!("0 {} 0\n{}", body.len(), &body[off..])),
                exited(0),
            ]
        } else {
            vec![exited(0)]
        }
    }));
    let h = sbx.commands.start("sleep 9").await.unwrap();
    assert_eq!(h.pid, 4242);
    assert_eq!(h.id.len(), 16);
    let ps = sbx.commands.list().await.unwrap();
    assert_eq!(ps.len(), 2);
    assert!(ps[0].running && ps[0].exit_code.is_none());
    assert_eq!((ps[1].running, ps[1].exit_code), (false, Some(0)));
    let r = h.wait().await.unwrap();
    assert_eq!(r.stdout, "line1\nline2\n");
    assert_eq!(h.exit_code(), Some(0));
}

#[tokio::test]
async fn list_shows_only_this_apis_sandboxes() {
    let (c, p) = plane().await;
    let mut mine = session("aaaa1111", "sandbox/a");
    mine.environment = Some(pb::session::Environment::Workspace(pb::Workspace {
        label: "/chainguard-sandbox/sandbox".into(),
        ..Default::default()
    }));
    let other = session("bbbb2222", "repo/b");
    *p.sessions.lock().unwrap() = vec![mine, other];
    let l = chainguard_sandbox::list(ConnectOptions {
        client: Some(c),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(
        l.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
        ["aaaa1111"]
    );
    assert_eq!(l[0].state, "running");
}

// ---- turning Workspaces on ------------------------------------------------------------------

#[derive(Default)]
struct Platform {
    accepted: Vec<String>,
    enabled: bool,
    need_for_enable: Vec<String>,
    posts: Vec<String>,
    github: Vec<String>,
    app_installed: bool,
    link_body: serde_json::Value,
}

async fn platform() -> (Arc<Mutex<Platform>>, SetupOptions) {
    let f = Arc::new(Mutex::new(Platform {
        github: vec!["already-linked".into()],
        app_installed: true,
        ..Default::default()
    }));
    let f2 = f.clone();
    let (url, _) = http_server(Arc::new(move |r| {
        let mut f = f2.lock().unwrap();
        if r.bearer() != "platform-token" {
            return (401, r#"{"code":16,"message":"unauthenticated"}"#.into());
        }
        let terms = format!("/iam/v1/terms/{TEST_ORG}");
        let links = format!("/guardener/v1alpha1/githubAssociations/{TEST_ORG}");
        let link = format!("{links}:link");
        let enable = format!("/entitlements/v1alpha1/features/{TEST_ORG}:enableSandbox");
        match (r.method.as_str(), r.path.as_str()) {
            ("GET", p) if p == terms => {
                (200, serde_json::json!({"items": f.accepted.iter().map(|id| serde_json::json!({"docId": id})).collect::<Vec<_>>()}).to_string())
            }
            ("POST", p) if p == terms => {
                let ids: Vec<String> = serde_json::from_value(r.json()["documentIds"].clone()).unwrap();
                f.posts.push(format!("terms:{}", ids.join(",")));
                f.accepted.extend(ids);
                (200, "{}".into())
            }
            ("GET", p) if p == links => {
                (200, serde_json::json!({"organizations": f.github.iter().map(|g| serde_json::json!({"organization": g})).collect::<Vec<_>>()}).to_string())
            }
            ("GET", "/guardener/v1alpha1/githubAssociations:begin") => (200, r#"{"clientId":"client-1","state":"signed-state"}"#.into()),
            ("POST", p) if p == link => {
                f.link_body = r.json();
                f.posts.push("link".into());
                if !f.app_installed {
                    return (400, r#"{"code":9,"message":"Precondition failed: the guardener GitHub App must be installed on the GitHub account (organization or user) before it can be linked"}"#.into());
                }
                let login = f.link_body["organization"].as_str().unwrap_or("the-authorizing-user").to_string();
                f.github.push(login.clone());
                (200, serde_json::json!({"organization": login}).to_string())
            }
            ("POST", p) if p == enable => {
                f.posts.push("enable".into());
                if f.github.is_empty() {
                    // The service's real answer, as seen on 2026-11-15.
                    return (400, r#"{"code":9,"message":"Precondition failed: the organization must link a GitHub organization through the Chainguard GitHub App first"}"#.into());
                }
                let need: Vec<String> = workspaces::WORKSPACES_TERMS.iter().map(|s| s.to_string()).chain(f.need_for_enable.clone()).collect();
                let missing: Vec<_> = need
                    .iter()
                    .filter(|id| !f.accepted.contains(id))
                    .map(|id| serde_json::json!({"id": id, "label": format!("Label of {id}"), "url": format!("https://example.test/{id}")}))
                    .collect();
                if !missing.is_empty() {
                    return (400, serde_json::json!({"code": 9, "message": "terms not accepted", "details": [{"@type": "type.googleapis.com/chainguard.platform.iam.v2.TermsNotAcceptedDetail", "missing": missing}]}).to_string());
                }
                f.enabled = true;
                (200, "{}".into())
            }
            ("GET", "/entitlements/v2beta1/organizationEntitlements") if r.query.get("parent").map(String::as_str) == Some(TEST_ORG) => {
                let state = if f.enabled { "STATE_ACTIVE" } else { "STATE_NOT_INCLUDED" };
                (200, serde_json::json!({"plan": {"id": "starter", "displayName": "Starter"}, "products": [{
                    "product": "PRODUCT_SANDBOX_WORKSPACES", "state": state,
                    "limits": [{"displayName": "concurrent workspaces", "max": "3"}, {"displayName": "concurrent workspaces per user", "max": "1"}]
                }]}).to_string())
            }
            _ => (404, r#"{"code":5,"message":"no such route"}"#.into()),
        }
    }))
    .await;
    // SAFETY: the tests that set the environment hold ENV_LOCK.
    unsafe { std::env::set_var("CHAINGUARD_SANDBOX_PLATFORM_URL", url) };
    let o = SetupOptions {
        instance: TEST_ORG.into(),
        token_source: Some(Arc::new(workspaces::StaticToken("platform-token".into()))),
        out: Some(Arc::new(|_| {})),
        ..Default::default()
    };
    (f, o)
}

#[tokio::test]
async fn status_and_enable() {
    let _g = ENV_LOCK.lock().await;
    let (f, mut o) = platform().await;
    let st = chainguard_sandbox::workspaces_status(&o).await.unwrap();
    assert!(!st.enabled && !st.ready());
    assert_eq!(st.missing_terms.len(), workspaces::ENABLE_TERMS.len());
    assert_eq!(
        (
            st.plan.as_str(),
            st.max_sandboxes,
            st.max_sandboxes_per_user,
            st.github_accounts.len()
        ),
        ("Starter", 3, 1, 1)
    );
    assert!(!st.missing_terms[0].url.is_empty() && !st.missing_terms[0].label.is_empty());

    // Terms are never accepted without being asked to.
    match chainguard_sandbox::enable_workspaces(&o).await {
        Err(Error::TermsNotAccepted { missing }) => {
            assert_eq!(missing.len(), workspaces::ENABLE_TERMS.len())
        }
        other => panic!("{other:?}"),
    }
    assert!(f.lock().unwrap().posts.is_empty());

    o.accept_terms = true;
    let st = chainguard_sandbox::enable_workspaces(&o).await.unwrap();
    assert!(st.ready());
    assert_eq!(
        f.lock().unwrap().posts.join(" | "),
        format!("terms:{} | enable", workspaces::ENABLE_TERMS.join(","))
    );

    // Already on: nothing is sent.
    f.lock().unwrap().posts.clear();
    assert!(
        chainguard_sandbox::enable_workspaces(&o)
            .await
            .unwrap()
            .ready()
    );
    assert!(f.lock().unwrap().posts.is_empty());
}

#[tokio::test]
async fn enable_accepts_terms_the_switch_asks_for() {
    let _g = ENV_LOCK.lock().await;
    let (f, mut o) = platform().await;
    {
        let mut f = f.lock().unwrap();
        f.need_for_enable = vec!["some-new-terms.v1".into()]; // a document this package doesn't know yet
        f.accepted = workspaces::ENABLE_TERMS
            .iter()
            .map(|s| s.to_string())
            .collect();
    }
    match chainguard_sandbox::enable_workspaces(&o).await {
        Err(Error::TermsNotAccepted { missing }) => {
            assert_eq!(
                (missing.len(), missing[0].id.as_str()),
                (1, "some-new-terms.v1")
            );
            assert!(!missing[0].url.is_empty());
        }
        other => panic!("{other:?}"),
    }
    assert!(!f.lock().unwrap().enabled);
    o.accept_terms = true;
    f.lock().unwrap().posts.clear();
    chainguard_sandbox::enable_workspaces(&o).await.unwrap();
    assert_eq!(
        f.lock().unwrap().posts.join(" | "),
        "enable | terms:some-new-terms.v1 | enable"
    );
}

#[tokio::test]
async fn enable_links_github_first() {
    let _g = ENV_LOCK.lock().await;
    let (f, mut o) = platform().await;
    f.lock().unwrap().github.clear();
    o.accept_terms = true;

    // Without a browser it says what is needed instead of failing at the switch.
    o.no_browser = true;
    assert!(matches!(
        chainguard_sandbox::enable_workspaces(&o).await,
        Err(Error::GitHubLinkRequired { .. })
    ));
    assert!(!f.lock().unwrap().enabled);
    o.no_browser = false;

    // Stand in for the person and GitHub: follow the authorize URL's redirect_uri back.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    o.github_port = port;
    let authorize = Arc::new(Mutex::new(
        std::collections::HashMap::<String, String>::new(),
    ));
    let a2 = authorize.clone();
    o.open_url = Some(Arc::new(move |u: &str| {
        let q: std::collections::HashMap<String, String> = url::Url::parse(u)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        *a2.lock().unwrap() = q.clone();
        tokio::spawn(async move {
            let _ = reqwest::get(format!(
                "{}?code=gh-code&state={}",
                q["redirect_uri"], q["state"]
            ))
            .await;
        });
        true
    }));

    f.lock().unwrap().app_installed = false;
    match chainguard_sandbox::enable_workspaces(&o).await {
        Err(Error::GitHubAppNotInstalled { install_url, .. }) => {
            assert_eq!(install_url, workspaces::GITHUB_APP_INSTALL_URL)
        }
        other => panic!("{other:?}"),
    }
    assert!(!f.lock().unwrap().enabled);
    let q = authorize.lock().unwrap().clone();
    assert_eq!(
        (
            q["client_id"].as_str(),
            q["state"].as_str(),
            q["code_challenge_method"].as_str()
        ),
        ("client-1", "signed-state", "S256")
    );
    assert!(
        !q["code_challenge"].is_empty()
            && q["prompt"] == "select_account"
            && q["redirect_uri"] == format!("http://127.0.0.1:{port}/callback")
    );
    let body = f.lock().unwrap().link_body.clone();
    assert_eq!(
        (body["code"].as_str(), body["state"].as_str()),
        (Some("gh-code"), Some("signed-state"))
    );
    assert_eq!(
        body["redirectUri"].as_str(),
        Some(q["redirect_uri"].as_str())
    );
    assert!(body["codeVerifier"].as_str().is_some_and(|v| v.len() == 43));

    // With the app installed, the same port works again and Workspaces turns on.
    f.lock().unwrap().app_installed = true;
    o.github_account = "my-org".into();
    let st = chainguard_sandbox::enable_workspaces(&o).await.unwrap();
    assert!(st.ready());
    assert_eq!(st.github_accounts, ["my-org"]);
}

#[tokio::test]
async fn setup_errors_keep_the_service_message() {
    let _g = ENV_LOCK.lock().await;
    let (_f, mut o) = platform().await;
    o.token_source = Some(Arc::new(workspaces::StaticToken("wrong".into())));
    let e = chainguard_sandbox::workspaces_status(&o).await.unwrap_err();
    assert_eq!(e.platform_error().map(|p| p.http_status), Some(401));
    assert!(e.to_string().contains("unauthenticated"), "{e}");
}
