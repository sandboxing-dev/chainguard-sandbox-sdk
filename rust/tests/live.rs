//! Against the real service, opt in: CHAINGUARD_SANDBOX_LIVE=1 cargo test --test live -- --nocapture
//! Needs a saved login and an organization with Workspaces on. Creates one sandbox and deletes it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chainguard_sandbox::{
    CreateOptions, Error, FileType, RunOptions, Sandbox, WaitOptions, WriteEntry,
};

fn live() -> bool {
    std::env::var("CHAINGUARD_SANDBOX_LIVE").as_deref() == Ok("1")
}

#[tokio::test(flavor = "multi_thread")]
async fn live_sandbox() {
    if !live() {
        eprintln!("skipped (set CHAINGUARD_SANDBOX_LIVE=1)");
        return;
    }
    let opts = CreateOptions {
        packages: Some(vec!["bash".into(), "coreutils".into()]),
        name: "rust-live".into(),
        ..Default::default()
    };
    Sandbox::with(opts, async |sbx| {
        // Foreground commands, envs, callbacks, errors.
        let seen = Arc::new(Mutex::new(String::new()));
        let s2 = seen.clone();
        let r = sbx
            .commands
            .run_with("printf 'a€'; echo $X; echo err >&2", RunOptions {
                envs: [("X".to_string(), "y".to_string())].into(),
                on_stdout: Some(Box::new(move |s| s2.lock().unwrap().push_str(s))),
                ..Default::default()
            })
            .await?;
        assert_eq!(r.stdout, "a€y\n");
        assert_eq!(r.stderr, "err\n");
        assert_eq!(*seen.lock().unwrap(), "a€y\n");
        assert!(matches!(sbx.commands.run("exit 3").await, Err(Error::CommandExit(r)) if r.exit_code == 3));
        let quick = RunOptions { timeout: Some(Duration::from_secs(1)), ..Default::default() };
        assert!(matches!(sbx.commands.run_with("echo started; sleep 10", quick).await, Err(Error::Timeout { .. })));
        let nodir = RunOptions { cwd: "/nope".into(), ..Default::default() };
        assert!(matches!(sbx.commands.run_with("true", nodir).await, Err(Error::NotFound { what, .. }) if what == "directory"));
        let r = sbx.commands.run_with("cat", RunOptions { stdin: b"piped".to_vec(), ..Default::default() }).await?;
        assert_eq!(r.stdout, "piped");

        // Files: small, large (staged), several at once, metadata, missing.
        let big: Vec<u8> = (0..(17 << 20)).map(|i| (i % 251) as u8).collect();
        sbx.files.write("big.bin", big.clone()).await?;
        assert_eq!(sbx.files.read("big.bin").await?, big);
        sbx.files
            .write_files(vec![
                WriteEntry { path: "app/main.py".into(), data: b"print('hi')\n".to_vec(), ..Default::default() },
                WriteEntry { path: "app/run.sh".into(), data: b"echo run\n".to_vec(), mode: 0o755 },
            ])
            .await?;
        let es = sbx.files.list("app").await?;
        assert_eq!(es.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["main.py", "run.sh"]);
        assert_eq!(es[1].permissions, "rwxr-xr-x");
        assert!(sbx.files.exists("app/main.py").await?);
        assert!(sbx.files.make_dir("out/reports").await?);
        assert!(!sbx.files.make_dir("out/reports").await?);
        let moved = sbx.files.rename("app/main.py", "app2/main.py").await?;
        assert_eq!((moved.path.as_str(), moved.file_type), ("/work/app2/main.py", FileType::File));
        sbx.files.remove("out").await?;
        assert!(matches!(sbx.files.read("missing.txt").await, Err(Error::NotFound { .. })));
        assert!(matches!(sbx.files.write("app", "x").await, Err(Error::InvalidPath { .. })));

        // Directory transfer both ways, with a link inside the tree.
        let tmp = std::env::temp_dir().join(format!("cgs-live-{}", std::process::id()));
        let src = tmp.join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("sub/f.txt"), "deep").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("sub/f.txt", src.join("ln")).unwrap();
        sbx.files.upload_dir(&src, "proj").await?;
        assert_eq!(sbx.files.read_text("proj/sub/f.txt").await?, "deep");
        sbx.files.download_dir("proj", tmp.join("back")).await?;
        assert_eq!(std::fs::read_to_string(tmp.join("back/ln")).unwrap(), "deep");
        sbx.files.upload(src.join("sub/f.txt"), "one.txt").await?;
        sbx.files.download("one.txt", tmp.join("one/one.txt")).await?;
        assert_eq!(std::fs::read_to_string(tmp.join("one/one.txt")).unwrap(), "deep");
        std::fs::remove_dir_all(&tmp).unwrap();

        // Background commands.
        let h = sbx.commands.start("sleep 300").await?;
        assert_eq!(h.poll().await?, None);
        assert!(sbx.commands.list().await?.iter().any(|p| p.pid == h.pid && p.running));
        assert!(sbx.commands.connect(h.pid).await?.kill().await?);
        assert_eq!(h.poll().await?, Some(137));
        let job = sbx.commands.start("for i in 1 2 3; do echo $i; sleep 1; done").await?;
        let got = Arc::new(Mutex::new(String::new()));
        let g2 = got.clone();
        let r = job.wait_with(WaitOptions { on_stdout: Some(Box::new(move |s| g2.lock().unwrap().push_str(s))), ..Default::default() }).await?;
        assert_eq!(r.stdout, "1\n2\n3\n");
        assert_eq!(*got.lock().unwrap(), "1\n2\n3\n");

        // Lifecycle: info, keep-alive, pause and resume on the next command.
        let info = sbx.get_info().await?;
        assert_eq!(info.state, "running");
        assert!(info.cpus > 0);
        sbx.keep_alive(Duration::from_secs(60)).await?;
        sbx.pause().await?;
        let r = sbx.commands.run("cat /work/one.txt").await?;
        assert_eq!(r.stdout, "deep");
        assert_eq!(chainguard_sandbox::list(Default::default()).await?.iter().filter(|i| i.id == sbx.id).count(), 1);
        println!("live ok: sandbox {}", sbx.id);
        Ok(())
    })
    .await
    .unwrap();
}

#[test]
fn live_blocking() {
    if !live() {
        return;
    }
    use chainguard_sandbox::blocking;
    let id;
    {
        let sbx = blocking::Sandbox::create(CreateOptions {
            packages: Some(vec!["bash".into()]),
            ..Default::default()
        })
        .unwrap();
        id = sbx.id.clone();
        assert_eq!(
            sbx.commands.run("echo blocking").unwrap().stdout,
            "blocking\n"
        );
        sbx.files.write("x.txt", "y").unwrap();
        assert_eq!(sbx.files.read_text("x.txt").unwrap(), "y");
    } // deleted here
    assert!(
        !blocking::list(Default::default())
            .unwrap()
            .iter()
            .any(|i| i.id == id)
    );
    println!("live blocking ok");
}
