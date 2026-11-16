// Checks a packaged chainguard-sandbox-sdk crate: it builds in an empty project and its API is there.
// With --live it also creates a sandbox, runs a command, moves a file and deletes the sandbox
// (needs a saved login and an organization with Workspaces on).
use chainguard_sandbox::{CreateOptions, Sandbox};

#[tokio::main]
async fn main() -> chainguard_sandbox::Result<()> {
    let _ = chainguard_sandbox::document(&CreateOptions::default());
    println!("build ok (chainguard-sandbox-sdk)");
    if std::env::args().any(|a| a == "--live") {
        let opts = CreateOptions { packages: Some(vec!["bash".into(), "coreutils".into()]), ..Default::default() };
        Sandbox::with(opts, async |sbx| {
            let r = sbx.commands.run("echo hello from $(uname -s)").await?;
            assert_eq!(r.stdout.trim(), "hello from Linux", "unexpected output");
            sbx.files.write("notes/hello.txt", "hi").await?;
            assert_eq!(sbx.files.read_text("notes/hello.txt").await?, "hi", "file round trip failed");
            Ok(())
        })
        .await?;
        println!("live ok: created a sandbox, ran a command, wrote and read a file");
    }
    Ok(())
}
