// Quickstart: create a sandbox, run a command, write and read a file, delete it.
//
// Before anything else: your Chainguard organization must exist and have
// Workspaces turned on (one time, by an owner: `chainguard-sandbox enable`).
// Otherwise create fails with "PermissionDenied: this organization is not
// entitled to Chainguard Workspaces". See the Quickstart in the docs.
//
// Before you run it (from the rust/ directory of this repository):
//
//  1. Log in once. It opens your browser; the login is saved and shared by
//     the Go, Python, TypeScript and Rust SDKs:
//
//        cargo run --bin chainguard-sandbox -- login     # optional: add github, google-oauth2 or gitlab
//
//     A saved login lasts as long as it's used at least once every 20 hours.
//     If a run opens your browser, the login expired: approve it and the run
//     continues, or renew it ahead of time with the same login command.
//
//  2. Choose the organization (the "instance"). Any one of these works; the
//     first one set wins:
//
//     a. in code: CreateOptions::instance (commented out below);
//     b. an environment variable, for this terminal session only:
//          export CHAINCTL_SANDBOX_INSTANCE=XXXXXX    # your organization ID
//     c. nothing: if you belong to exactly one organization, it is detected
//        automatically (turn that off with CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE=1).
//
//     List your organization IDs with either of:
//
//        cargo run --bin chainguard-sandbox -- orgs
//        chainctl iam organizations list
//
//  3. Run it:
//
//        cargo run --example quickstart
// --8<-- [start:program]
use chainguard_sandbox::{CreateOptions, Sandbox};

#[tokio::main]
async fn main() -> chainguard_sandbox::Result<()> {
    // --8<-- [start:quickstart]
    let opts = CreateOptions {
        packages: Some(vec![
            "bash".into(),
            "coreutils".into(),
            "python-3.13".into(),
        ]),
        // instance: "XXXXXX".into(), // your organization ID; overrides $CHAINCTL_SANDBOX_INSTANCE and auto-detection
        ..Default::default()
    };
    // The sandbox is deleted when the closure returns, on success and on error.
    Sandbox::with(opts, async |sbx| {
        let res = sbx
            .commands
            .run(r#"python3 -c "print('hello from', __import__('platform').node())""#)
            .await?;
        print!("{}", res.stdout);

        sbx.files.write("notes/hello.txt", "hi").await?;
        println!("{}", sbx.files.read_text("notes/hello.txt").await?);
        Ok(())
    })
    .await
    // --8<-- [end:quickstart]
}
// --8<-- [end:program]
