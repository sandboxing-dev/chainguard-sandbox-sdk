//! Browser login, the saved login, refresh-token rotation, organizations.

mod common;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use chainguard_sandbox::Error;
use chainguard_sandbox::workspaces::{
    self, BrowserLoginToken, FileLoginStore, LoginOptions, LoginStore, TokenSource,
};
use common::*;

fn set_env(k: &str, v: &str) {
    // SAFETY: the tests that touch the environment hold ENV_LOCK.
    unsafe { std::env::set_var(k, v) }
}

fn unset_env(k: &str) {
    // SAFETY: as above.
    unsafe { std::env::remove_var(k) }
}

/// A refresh token the way Chainguard's look: base64 JSON with exp and aud.
fn refresh_token(tag: &str, aud: &[&str]) -> String {
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 3600;
    base64::engine::general_purpose::STANDARD
        .encode(serde_json::json!({ "exp": exp, "aud": aud, "tag": tag }).to_string())
}

/// An issuer that spends each refresh token once and rotates it.
async fn issuer() -> (String, Arc<Mutex<Vec<HttpRequest>>>) {
    let spent: Arc<Mutex<HashSet<String>>> = Default::default();
    let n = Arc::new(Mutex::new(0));
    http_server(Arc::new(move |r| {
        if r.path != "/sts/exchange_refresh_token" || r.method != "POST" {
            return (404, "{}".into());
        }
        let Some(aud) = r.query.get("aud").cloned() else { return (400, r#"{"message":"Audience must be specified"}"#.into()) };
        if r.headers.get("content-length").map(String::as_str) != Some("0") {
            return (411, "Length Required".into());
        }
        let rt = r.bearer();
        if rt.starts_with("expired") {
            return (400, r#"{"code":3,"message":"invalid refresh token: refresh token expired at 2026-11-11"}"#.into());
        }
        // A token that says which audiences it covers is refused for any other.
        if let Ok(b) = base64::engine::general_purpose::STANDARD.decode(&rt)
            && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b)
            && !v["aud"].as_array().is_some_and(|a| a.iter().any(|x| x == aud.as_str()))
        {
            return (401, r#"{"message":"refresh token not valid for audience"}"#.into());
        }
        if !spent.lock().unwrap().insert(rt.clone()) || rt.is_empty() || rt.starts_with("unknown") {
            return (401, r#"{"message":"invalid refresh token"}"#.into());
        }
        let mut n = n.lock().unwrap();
        *n += 1;
        let tok = jwt(serde_json::json!({ "sub": "me", "iss": "issuer", "aud": [aud], "exp": 9999999999u64 }));
        (200, serde_json::json!({ "token": { "token": tok }, "refreshToken": { "token": format!("rt-{n}") } }).to_string())
    }))
    .await
}

#[tokio::test]
async fn browser_login_sends_chainctl_parameters_and_returns_tokens() {
    let seen_url = Arc::new(Mutex::new(String::new()));
    let su = seen_url.clone();
    let o = LoginOptions {
        issuer: "https://issuer.example/".into(),
        connection: "github".into(),
        out: Some(Arc::new(|_| {})),
        open_url: Some(Arc::new(move |u: &str| {
            *su.lock().unwrap() = u.to_string();
            // Play the browser: follow the redirect parameter back with tokens.
            let q: std::collections::HashMap<String, String> = url::Url::parse(u)
                .unwrap()
                .query_pairs()
                .into_owned()
                .collect();
            let cb = q["redirect"].replace("localhost", "127.0.0.1");
            tokio::spawn(async move {
                let c = reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .unwrap();
                let r = c
                    .get(format!("{cb}&token=tok-1&refresh_token=rt-1"))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(r.status().as_u16(), 303);
                let base = cb.split("/callback").next().unwrap().to_string();
                let page = c
                    .get(format!("{base}/"))
                    .send()
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap();
                assert!(page.contains("login complete"));
            });
            true
        })),
        ..Default::default()
    };
    let res = workspaces::browser_login(o).await.unwrap();
    assert_eq!(
        (res.token.as_str(), res.refresh_token.as_str()),
        ("tok-1", "rt-1")
    );
    let u = url::Url::parse(&seen_url.lock().unwrap()).unwrap();
    assert_eq!(u.path(), "/oauth");
    let q: std::collections::HashMap<String, String> = u.query_pairs().into_owned().collect();
    assert_eq!(q["client_id"], "auth0");
    assert_eq!(q["connection"], "github");
    assert_eq!(
        q["audience"],
        format!(
            "{},{}",
            workspaces::DEFAULT_AUDIENCE,
            workspaces::PLATFORM_AUDIENCE
        )
    );
    assert_eq!(
        (
            q["create_refresh_token"].as_str(),
            q["refresh_token_rotation"].as_str()
        ),
        ("true", "true")
    );
    assert_eq!(
        (q["skip_registration"].as_str(), q["exit"].as_str()),
        ("true", "redirect")
    );
    assert!(
        q["redirect"].starts_with("http://localhost:")
            && q["redirect"].ends_with("/callback?token=true&error=true")
    );
}

async fn login_with(query: &str) -> Result<workspaces::LoginResult, Error> {
    let q = query.to_string();
    let o = LoginOptions {
        out: Some(Arc::new(|_| {})),
        open_url: Some(Arc::new(move |u: &str| {
            let qs: std::collections::HashMap<String, String> = url::Url::parse(u)
                .unwrap()
                .query_pairs()
                .into_owned()
                .collect();
            let cb = qs["redirect"]
                .replace("localhost", "127.0.0.1")
                .replace("?token=true&error=true", "");
            let q = q.clone();
            tokio::spawn(async move {
                let _ = reqwest::get(format!("{cb}?{q}")).await;
            });
            true
        })),
        ..Default::default()
    };
    workspaces::browser_login(o).await
}

#[tokio::test]
async fn browser_login_errors() {
    assert!(matches!(
        login_with("status_code=404").await,
        Err(Error::AccountNotFound)
    ));
    let e = login_with("status_code=500&error=boom")
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("(status 500): boom"), "{e}");
}

#[tokio::test]
async fn login_server_ignores_stray_requests() {
    let (cb, result, _server) = workspaces::login_callback_server().await.unwrap();
    let base = cb
        .split("/callback")
        .next()
        .unwrap()
        .replace("localhost", "127.0.0.1");
    let c = reqwest::Client::new();
    assert_eq!(
        c.get(format!("{base}/"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        404,
        "no page before the login"
    );
    assert_eq!(
        c.post(format!("{base}/callback?token=x"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        405
    );
    let r = c
        .get(format!("{base}/callback?token=t&refresh_token=r"))
        .send()
        .await
        .unwrap();
    assert!(
        r.headers()["cache-control"]
            .to_str()
            .unwrap()
            .contains("no-store")
    );
    // The first answer wins.
    let _ = c.get(format!("{base}/callback?token=second")).send().await;
    assert_eq!(result.await.unwrap().token, "t");
}

#[tokio::test]
async fn browser_login_token_refreshes_rotates_and_needs_a_login_when_expired() {
    let (iss, seen) = issuer().await;
    let dir = temp_dir("login");
    let store = Arc::new(FileLoginStore {
        path: dir.join("login.json"),
    });
    store.save(&iss, "start-token").unwrap();
    let ts = BrowserLoginToken {
        login: LoginOptions {
            issuer: iss.clone(),
            ..Default::default()
        },
        store: Some(store.clone()),
        no_browser: true,
        ..Default::default()
    };
    let tok = ts.token().await.unwrap();
    assert_eq!(
        workspaces::token_claims(&tok).unwrap().audience,
        [workspaces::DEFAULT_AUDIENCE]
    );
    assert_eq!(
        store.load(&iss).unwrap(),
        "rt-1",
        "the rotated token is saved"
    );
    ts.token().await.unwrap();
    assert_eq!(store.load(&iss).unwrap(), "rt-2");
    let r = seen.lock().unwrap()[0].clone();
    assert_eq!(
        r.query["aud"],
        workspaces::DEFAULT_AUDIENCE,
        "the audience goes in the query"
    );
    assert!(r.body.is_empty());

    // The file is the shared format: version 1, keyed by issuer.
    let v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("login.json")).unwrap()).unwrap();
    assert_eq!(v["version"], 1);
    assert_eq!(
        v["logins"][iss.trim_end_matches('/')]["refresh_token"],
        "rt-2"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(dir.join("login.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    // An expired refresh token (answered 400) is forgotten; without a browser that is LoginRequired.
    store.save(&iss, "expired-1").unwrap();
    assert!(matches!(ts.token().await, Err(Error::LoginRequired { .. })));
    assert_eq!(
        store.load(&iss).unwrap(),
        "",
        "the expired login was deleted"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn saved_login_is_kept_when_it_only_lacks_an_audience() {
    let (iss, _) = issuer().await;
    let dir = temp_dir("aud");
    let store = Arc::new(FileLoginStore {
        path: dir.join("login.json"),
    });
    // An unexpired login made for the sessions service only.
    let rt = refresh_token("x", &[workspaces::DEFAULT_AUDIENCE]);
    store.save(&iss, &rt).unwrap();
    let ts = BrowserLoginToken {
        login: LoginOptions {
            issuer: iss.clone(),
            ..Default::default()
        },
        store: Some(store.clone()),
        audience: workspaces::PLATFORM_AUDIENCE.into(),
        no_browser: true,
        ..Default::default()
    };
    match ts.token().await {
        Err(Error::LoginRequired { detail: Some(d) }) => {
            assert!(d.contains("doesn't cover"), "{d}")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        store.load(&iss).unwrap(),
        rt,
        "still good for its own audience, so it is kept"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn login_store_stale_lock_and_logout() {
    let dir = temp_dir("lock");
    let store = FileLoginStore {
        path: dir.join("login.json"),
    };
    {
        let _l = store.lock().await.unwrap();
        assert!(dir.join("login.json.lock").exists());
        // A second taker waits; it gets the lock once the first lets go.
        let s2 = store.clone();
        let waiter = tokio::spawn(async move { s2.lock().await.map(|_| ()) });
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(!waiter.is_finished());
        drop(_l);
        waiter.await.unwrap().unwrap();
    }
    assert!(!dir.join("login.json.lock").exists());
    // A lock left by a crashed process (older than two minutes) is taken over.
    std::fs::write(dir.join("login.json.lock"), "123\n").unwrap();
    let old = std::time::SystemTime::now() - Duration::from_secs(600);
    std::fs::File::options()
        .write(true)
        .open(dir.join("login.json.lock"))
        .unwrap()
        .set_modified(old)
        .unwrap();
    let l = tokio::time::timeout(Duration::from_secs(2), store.lock())
        .await
        .unwrap()
        .unwrap();
    drop(l);
    store.save("", "rt").unwrap();
    assert_eq!(store.load("").unwrap(), "rt");
    workspaces::logout(Some(&store), "").unwrap();
    assert_eq!(store.load("").unwrap(), "");
    // A corrupt file is replaced on save.
    std::fs::write(dir.join("login.json"), "{not json").unwrap();
    assert!(store.load("").is_err());
    store.save("", "again").unwrap();
    assert_eq!(store.load("").unwrap(), "again");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn default_login_file_and_audiences_follow_the_environment() {
    let _g = ENV_LOCK.lock().await;
    set_env("CHAINGUARD_SANDBOX_LOGIN_FILE", "/tmp/x/login.json");
    assert_eq!(
        workspaces::default_login_file(),
        std::path::PathBuf::from("/tmp/x/login.json")
    );
    unset_env("CHAINGUARD_SANDBOX_LOGIN_FILE");
    let p = workspaces::default_login_file();
    assert!(p.ends_with("chainguard-sandbox/login.json"));
    if cfg!(target_os = "macos") {
        assert!(p.to_string_lossy().contains("Library/Application Support"));
    }
    set_env("CHAINGUARD_SANDBOX_LOGIN_AUDIENCES", " a, ,b ");
    assert_eq!(workspaces::default_login_audiences(), ["a", "b"]);
    unset_env("CHAINGUARD_SANDBOX_LOGIN_AUDIENCES");
    assert_eq!(workspaces::default_login_audiences().len(), 2);
}

#[tokio::test]
async fn list_and_detect_organizations() {
    let _g = ENV_LOCK.lock().await;
    let orgs = Arc::new(Mutex::new(serde_json::json!({"items": [
        {"id": "b".repeat(40), "name": "zeta"}, {"id": "a".repeat(40), "name": "alpha"}, {"id": format!("{}/child", "a".repeat(40)), "name": "sub"}
    ]})));
    let o2 = orgs.clone();
    let (url, seen) = http_server(Arc::new(move |r| {
        if r.bearer() != "plat" {
            return (401, r#"{"message":"unauthenticated"}"#.into());
        }
        assert_eq!(
            (r.path.as_str(), r.query["uidp.in_root"].as_str()),
            ("/iam/v1/groups", "true")
        );
        (200, o2.lock().unwrap().to_string())
    }))
    .await;
    set_env("CHAINGUARD_SANDBOX_PLATFORM_URL", &url);
    let ts: workspaces::SharedTokenSource = Arc::new(workspaces::StaticToken("plat".into()));
    let got = workspaces::list_organizations(Some(ts.clone()))
        .await
        .unwrap();
    assert_eq!(
        got.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(),
        ["alpha", "zeta"],
        "root groups, sorted by name"
    );
    assert!(matches!(
        workspaces::detect_instance(Some(ts.clone())).await,
        Err(Error::AmbiguousOrganization { .. })
    ));
    *orgs.lock().unwrap() = serde_json::json!({"items": [{"id": "c".repeat(40), "name": "only"}]});
    assert_eq!(
        workspaces::detect_instance(Some(ts.clone())).await.unwrap(),
        "c".repeat(40)
    );
    *orgs.lock().unwrap() = serde_json::json!({"items": []});
    assert!(matches!(
        workspaces::detect_instance(Some(ts.clone())).await,
        Err(Error::NoOrganization)
    ));

    // A client without an organization detects it; with detection off it explains every option.
    *orgs.lock().unwrap() = serde_json::json!({"items": [{"id": "c".repeat(40), "name": "only"}]});
    let c = workspaces::Client::new(workspaces::Options {
        url: "http://127.0.0.1:1".into(),
        token_source: Some(Arc::new(workspaces::StaticToken("s".into()))),
        platform_token_source: Some(ts.clone()),
        ..Default::default()
    })
    .await
    .unwrap();
    assert_eq!(c.instance(), format!("instances/{}", "c".repeat(40)));
    let c = workspaces::Client::new(workspaces::Options {
        url: "http://127.0.0.1:1".into(),
        token_source: Some(Arc::new(workspaces::StaticToken("s".into()))),
        no_detect_instance: true,
        ..Default::default()
    })
    .await
    .unwrap();
    let e = c.list(&Default::default()).await.unwrap_err();
    assert!(matches!(e, Error::NoInstance { .. }));
    assert!(e.to_string().contains("CHAINCTL_SANDBOX_INSTANCE"), "{e}");
    assert_eq!(seen.lock().unwrap().len(), 5);
    unset_env("CHAINGUARD_SANDBOX_PLATFORM_URL");
}
