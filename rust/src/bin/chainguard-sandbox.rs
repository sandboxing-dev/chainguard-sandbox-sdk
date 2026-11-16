//! chainguard-sandbox: manages the login and the sandboxes of the Simple Chainguard Sandbox SDK.
//!
//! ```text
//! chainguard-sandbox login [github|google-oauth2|gitlab]   # browser login, saved for later runs
//! chainguard-sandbox whoami                                # who the saved login is
//! chainguard-sandbox logout                                # forget the saved login
//! chainguard-sandbox orgs                                  # your organization IDs
//! chainguard-sandbox status [ORG]                          # is Workspaces turned on?
//! chainguard-sandbox enable [--accept-terms] [ORG]         # turn it on (asks before accepting terms)
//! chainguard-sandbox ls                                    # your sandboxes
//! chainguard-sandbox rm ID                                 # delete a sandbox
//! ```
//!
//! Run it from a checkout (cd rust && cargo run --bin chainguard-sandbox -- login), or in a project
//! that depends on the crate (cargo run --bin chainguard-sandbox -- login).

use std::process::exit;
use std::time::{Duration, SystemTime};

use chainguard_sandbox::workspaces::{
    self, BrowserLoginToken, FileLoginStore, LoginOptions, LoginStore as _, TokenSource as _,
};
use chainguard_sandbox::{ConnectOptions, Error, SetupOptions, WorkspacesStatus};

const USAGE: &str = "usage: chainguard-sandbox login [github|google-oauth2|gitlab] | logout | whoami | orgs | status [ORG] | enable [--accept-terms] [--github ACCOUNT] [--port N] [ORG] | ls | rm ID";

fn usage() -> ! {
    eprintln!("{USAGE}");
    exit(2)
}

fn fail(e: impl std::fmt::Display) -> ! {
    eprintln!("chainguard-sandbox: {e}");
    exit(1)
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().cloned() else {
        usage()
    };
    let rest = args[1..].to_vec();
    tokio::select! {
        r = run(&cmd, rest) => if let Err(e) = r { fail(e) },
        _ = tokio::signal::ctrl_c() => fail("interrupted"),
    }
}

async fn run(cmd: &str, args: Vec<String>) -> Result<(), Error> {
    match cmd {
        "login" => login(args).await,
        "logout" => {
            workspaces::logout(None, "")?;
            println!("logged out: {}", workspaces::default_login_file().display());
            Ok(())
        }
        "whoami" => whoami().await,
        "orgs" => {
            let orgs = workspaces::list_organizations(None).await?;
            let mut rows = vec![vec!["ID".to_string(), "NAME".to_string()]];
            rows.extend(orgs.into_iter().map(|o| vec![o.id, o.name]));
            table(&rows);
            Ok(())
        }
        "status" => {
            let st = chainguard_sandbox::workspaces_status(&setup_args(&args)).await?;
            print_status(&st);
            if !st.ready() {
                println!(
                    "\nSandboxes can't be created yet. An organization owner can turn Workspaces on with: chainguard-sandbox enable"
                );
                exit(1);
            }
            Ok(())
        }
        "enable" => enable(args).await,
        "ls" => {
            let infos = chainguard_sandbox::list(ConnectOptions::default()).await?;
            let mut rows = vec![
                ["ID", "NAME", "STATE", "CREATED"]
                    .map(String::from)
                    .to_vec(),
            ];
            rows.extend(
                infos
                    .into_iter()
                    .map(|i| vec![i.id, i.display_name, i.state, local_time(i.created_at)]),
            );
            table(&rows);
            Ok(())
        }
        "rm" => {
            if args.len() != 1 {
                usage();
            }
            if !chainguard_sandbox::kill(&args[0], ConnectOptions::default()).await? {
                fail(format!("no sandbox {:?}", args[0]));
            }
            println!("deleted {}", args[0]);
            Ok(())
        }
        _ => usage(),
    }
}

/// Reads [--accept-terms] [--github ACCOUNT] [--port N] [ORG].
fn setup_args(args: &[String]) -> SetupOptions {
    let mut o = SetupOptions::default();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--accept-terms" | "--yes" => o.accept_terms = true,
            "--github" if i + 1 < args.len() => {
                i += 1;
                o.github_account = args[i].clone();
            }
            "--port" if i + 1 < args.len() => {
                i += 1;
                o.github_port = args[i].parse().unwrap_or_else(|_| usage());
            }
            _ if a.starts_with('-') || !o.instance.is_empty() => usage(),
            _ => o.instance = a.clone(),
        }
        i += 1;
    }
    o
}

fn print_status(st: &WorkspacesStatus) {
    println!(
        "organization: {}\nplan:         {}",
        st.organization, st.plan
    );
    println!("workspaces:   {}", if st.enabled { "on" } else { "off" });
    if st.max_sandboxes > 0 || st.max_sandboxes_per_user > 0 {
        println!(
            "sandboxes:    up to {} per user, {} in the organization",
            st.max_sandboxes_per_user, st.max_sandboxes
        );
    }
    if !st.github_accounts.is_empty() {
        println!("github:       linked to {}", st.github_accounts.join(", "));
    } else if !st.enabled {
        println!("github:       no account linked yet");
    }
    if st.missing_terms.is_empty() {
        println!("terms:        accepted");
    } else {
        println!("terms:        not accepted yet:");
        for d in &st.missing_terms {
            println!("  - {}: {}", d.label, d.url);
        }
    }
}

/// Reads one line from the terminal.
async fn read_line(prompt: &str) -> String {
    use std::io::Write as _;
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    tokio::task::spawn_blocking(|| {
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
        s
    })
    .await
    .unwrap_or_default()
}

/// Turns Workspaces on: shows the terms the organization still has to accept and asks before
/// accepting them (or takes --accept-terms as the answer).
async fn enable(args: Vec<String>) -> Result<(), Error> {
    let mut o = setup_args(&args);
    let st = chainguard_sandbox::workspaces_status(&o).await?;
    if st.ready() {
        print_status(&st);
        println!("\nWorkspaces is already on.");
        return Ok(());
    }
    println!(
        "Turning on Chainguard Workspaces (and Checks) for organization {}.",
        st.organization
    );
    if !st.missing_terms.is_empty() && !o.accept_terms {
        println!("\nThe organization has to accept these documents first:");
        for d in &st.missing_terms {
            println!("  - {}: {}", d.label, d.url);
        }
        let answer = read_line(
            "\nDo you accept them on behalf of the organization? Type \"yes\" to accept: ",
        )
        .await;
        if answer.trim().to_lowercase() != "yes" {
            fail("not accepted; nothing was changed");
        }
        o.accept_terms = true;
    }
    if st.github_accounts.is_empty() {
        println!(
            "\nNext, a GitHub account is linked to the organization: Chainguard requires one before it turns"
        );
        println!(
            "Workspaces on. Your browser opens GitHub, where you choose the account (your own user, unless you"
        );
        println!(
            "passed --github ORG). The Chainguard GitHub App has to be installed on that account:"
        );
        println!("  {}", workspaces::GITHUB_APP_INSTALL_URL);
    }
    let mut attempt = 1;
    let st = loop {
        match chainguard_sandbox::enable_workspaces(&o).await {
            Err(e @ Error::TermsNotAccepted { .. }) => fail(format!(
                "{e}\nRe-run with --accept-terms once you have read them"
            )),
            // The app isn't on the account yet: install it, then authorize again.
            Err(Error::GitHubAppNotInstalled { install_url, .. }) if attempt < 3 => {
                println!(
                    "\nThe Chainguard GitHub App is not installed on that GitHub account yet."
                );
                println!("Install it here (opening it in your browser), then come back:");
                println!("  {install_url}");
                let _ = workspaces::open_browser(&install_url);
                read_line("\nPress Enter once the app is installed, to authorize again: ").await;
                attempt += 1;
            }
            Err(e) => return Err(e),
            Ok(st) => break st,
        }
    };
    println!();
    print_status(&st);
    println!(
        "\nWorkspaces is on. Create a sandbox with the SDK, or list them with: chainguard-sandbox ls"
    );
    Ok(())
}

/// Runs the browser login and saves it (replacing any saved login).
async fn login(args: Vec<String>) -> Result<(), Error> {
    let mut o = LoginOptions {
        connection: std::env::var("CHAINGUARD_SANDBOX_SOCIAL_LOGIN").unwrap_or_default(),
        ..Default::default()
    };
    if args.len() > 1 {
        usage();
    }
    if let Some(c) = args.first() {
        o.connection = c.clone();
    }
    let res = tokio::time::timeout(Duration::from_secs(600), workspaces::browser_login(o))
        .await
        .map_err(|_| {
            Error::Other("workspaces: login: no answer from the browser within 10 minutes".into())
        })??;
    if res.refresh_token.is_empty() {
        fail("the issuer returned no refresh token, so the login can't be saved");
    }
    FileLoginStore::default().save("", &res.refresh_token)?;
    println!(
        "logged in; saved to {}",
        workspaces::default_login_file().display()
    );
    whoami().await
}

/// Mints a sessions token from the saved login and prints its claims (not the token).
async fn whoami() -> Result<(), Error> {
    let ts = BrowserLoginToken {
        no_browser: true,
        ..Default::default()
    };
    let tok = ts.token().await?;
    let c = workspaces::token_claims(&tok)?;
    println!(
        "subject:  {}\nissuer:   {}\naudience: [{}]\nexpires:  {}",
        c.subject,
        c.issuer,
        c.audience.join(" "),
        local_time(c.expiry)
    );
    Ok(())
}

fn local_time(t: Option<SystemTime>) -> String {
    match t {
        Some(t) => chrono::DateTime::<chrono::Local>::from(t)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        None => String::new(),
    }
}

/// Prints rows as columns separated by two spaces (like Go's tabwriter).
fn table(rows: &[Vec<String>]) {
    let n = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    let widths: Vec<usize> = (0..n)
        .map(|i| {
            rows.iter()
                .map(|r| r.get(i).map_or(0, |c| c.chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    for r in rows {
        let mut line = String::new();
        for (i, c) in r.iter().enumerate() {
            if i + 1 < r.len() {
                line.push_str(&format!("{c:<w$}  ", w = widths[i]));
            } else {
                line.push_str(c);
            }
        }
        println!("{}", line.trim_end());
    }
}
