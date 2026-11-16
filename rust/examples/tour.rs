// Tour: every part of the API, one snippet per docs section. It's type-checked
// but not meant to be run as-is (it refers to local files like ./my-project).
//
// Setup is the same as for the quickstart (see quickstart.rs); run the
// commands below from the rust/ directory of this repository:
//
//   - log in once:  cargo run --bin chainguard-sandbox -- login
//     (if a run opens your browser, the saved login expired: approve it);
//   - choose the organization: CreateOptions::instance in code, or
//     export CHAINCTL_SANDBOX_INSTANCE=XXXXXX for this terminal session, or
//     nothing if you belong to exactly one organization (auto-detected);
//   - list organization IDs:  cargo run --bin chainguard-sandbox -- orgs
//     or chainctl iam organizations list.
#![allow(unused_variables, unused_must_use)]

use std::collections::HashMap;
use std::time::Duration;

use chainguard_sandbox::{
    ConnectOptions, CreateOptions, Error, RunOptions, Sandbox, WaitOptions, WriteEntry,
};

#[tokio::main]
async fn main() -> chainguard_sandbox::Result<()> {
    // --8<-- [start:create]
    let sbx = Sandbox::create(CreateOptions {
        name: "data-job".into(), // display name "data-job/<word-word>"
        packages: Some(
            ["bash", "coreutils", "python-3.12"]
                .map(String::from)
                .to_vec(),
        ), // Chainguard package names
        envs: HashMap::from([("MODE".into(), "test".into())]), // for every command
        egress: Some(vec!["pypi.org".into(), "files.pythonhosted.org".into()]),
        // instance: "XXXXXX".into(), // organization ID; else $CHAINCTL_SANDBOX_INSTANCE, else auto-detected
        ..Default::default()
    })
    .await?;
    println!("sandbox {}", sbx.id);
    // ... and sbx.kill().await when done (or use Sandbox::with, which deletes it for you)
    // --8<-- [end:create]

    // --8<-- [start:run]
    let res = sbx
        .commands
        .run_with(
            "echo $MODE && ls /work",
            RunOptions {
                cwd: "/work".into(),
                envs: HashMap::from([("EXTRA".into(), "1".into())]),
                timeout: Some(Duration::from_secs(30)), // Some(Duration::ZERO) = no limit
                on_stdout: Some(Box::new(|s| print!("live: {s}"))),
                ..Default::default()
            },
        )
        .await?;
    println!("{} {}", res.exit_code, res.stdout);

    // No shell: argv runs as-is.
    let res = sbx
        .commands
        .run_args(["python3", "-c", "import sys; print(sys.version)"])
        .await?;
    // --8<-- [end:run]

    // --8<-- [start:errors]
    match sbx.commands.run("exit 3").await {
        Err(Error::CommandExit(r)) => println!("exit code {} stderr {}", r.exit_code, r.stderr),
        other => println!("{other:?}"),
    }
    let quick = RunOptions {
        timeout: Some(Duration::from_secs(1)),
        ..Default::default()
    };
    if let Err(Error::Timeout {
        result: Some(r), ..
    }) = sbx.commands.run_with("sleep 10", quick).await
    {
        println!("timed out; partial output: {}", r.stdout);
    }
    // --8<-- [end:errors]

    // --8<-- [start:background]
    let server = sbx.commands.start("python3 -m http.server 8000").await?;
    println!("pid {}", server.pid);
    println!("running: {}", server.poll().await?.is_none());
    println!("killed: {}", server.kill().await?);

    let job = sbx
        .commands
        .start("for i in 1 2 3; do echo $i; sleep 1; done")
        .await?;
    let result = job
        .wait_with(WaitOptions {
            on_stdout: Some(Box::new(|s| print!("{s}"))),
            ..Default::default()
        })
        .await?;
    // --8<-- [end:background]

    // --8<-- [start:files]
    sbx.files.write("data/input.csv", "a,b\n1,2\n").await?;
    sbx.files
        .write_files(vec![
            WriteEntry {
                path: "app/main.py".into(),
                data: b"print('hi')\n".to_vec(),
                ..Default::default()
            },
            WriteEntry {
                path: "app/run.sh".into(),
                data: b"python3 app/main.py\n".to_vec(),
                mode: 0o755,
            },
        ])
        .await?;
    let text = sbx.files.read_text("data/input.csv").await?; // String
    let raw = sbx.files.read("data/input.csv").await?; // Vec<u8>
    for e in sbx.files.list("app").await? {
        println!("{} {} {} {}", e.name, e.file_type, e.size, e.permissions);
    }
    let exists = sbx.files.exists("app/main.py").await?;
    let created = sbx.files.make_dir("out/reports").await?;
    sbx.files
        .rename("data/input.csv", "data/archive/input.csv")
        .await?;
    sbx.files.remove("out").await?;
    if let Err(Error::NotFound { name, .. }) = sbx.files.read("missing.txt").await {
        println!("not found: {name}");
    }
    // --8<-- [end:files]

    // --8<-- [start:transfer]
    sbx.files
        .upload("./local/model.bin", "models/model.bin")
        .await?;
    sbx.files
        .download("results/out.json", "./out/out.json")
        .await?;
    sbx.files
        .upload_dir("./my-project", "/work/project")
        .await?;
    sbx.files
        .download_dir("/work/project/build", "./build")
        .await?;
    // --8<-- [end:transfer]

    // --8<-- [start:lifecycle]
    let info = sbx.get_info().await?;
    println!(
        "{} {} {} {}",
        info.state, info.region, info.cpus, info.memory_mib
    );
    sbx.keep_alive(Duration::from_secs(30 * 60)).await?; // don't park while idle for 30 min
    sbx.pause().await?; // snapshot memory+disk; resumes on next call

    let again = Sandbox::connect(&sbx.id, ConnectOptions::default()).await?; // from anywhere, later
    println!(
        "{} sandboxes",
        chainguard_sandbox::list(ConnectOptions::default())
            .await?
            .len()
    );
    chainguard_sandbox::kill(&sbx.id, ConnectOptions::default()).await?; // by ID (or sbx.kill())
    // --8<-- [end:lifecycle]
    Ok(())
}
