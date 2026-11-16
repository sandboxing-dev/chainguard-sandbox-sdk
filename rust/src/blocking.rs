//! The same API for programs without an async runtime.
//!
//! Every call blocks until it is done. The calls run on a small runtime this module starts on
//! first use and shares between threads. Calling them from inside an async runtime is refused
//! (use the async API there).
//!
//! Unlike the async [`crate::Sandbox`], a blocking [`Sandbox`] deletes its sandbox when it is
//! dropped, like Python's `with Sandbox.create() as sbx:`. Call [`Sandbox::detach`] to keep it.
//!
//! ```no_run
//! use chainguard_sandbox::blocking::Sandbox;
//! use chainguard_sandbox::CreateOptions;
//!
//! fn main() -> chainguard_sandbox::Result<()> {
//!     let sbx = Sandbox::create(CreateOptions::default())?; // deleted when sbx is dropped
//!     println!("{}", sbx.commands.run("echo hello")?.stdout);
//!     Ok(())
//! }
//! ```

use std::future::Future;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use crate::{
    CommandResult, ConnectOptions, CreateOptions, EntryInfo, Error, Info, Organization,
    ProcessInfo, Result, RunOptions, SetupOptions, StartOptions, WaitOptions, WorkspacesStatus,
    WriteEntry, WriteInfo,
};

crate::opaque_debug!(Commands, CommandHandle, Files);

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("chainguard-sandbox")
            .enable_all()
            .build()
            .expect("starting the chainguard-sandbox runtime")
    })
}

fn block_on<F: Future<Output = Result<T>>, T>(f: F) -> Result<T> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return Err(Error::other(
            "chainguard_sandbox::blocking was called from inside an async runtime; use the async API (chainguard_sandbox::Sandbox) there",
        ));
    }
    runtime().block_on(f)
}

/// A handle to one sandbox, deleted when dropped (see [`Sandbox::detach`]).
pub struct Sandbox {
    /// The short ID (8 hex characters).
    pub id: String,
    /// The full resource name.
    pub name: String,
    /// Runs commands in the sandbox.
    pub commands: Commands,
    /// Reads and writes the sandbox's filesystem.
    pub files: Files,
    inner: Option<crate::Sandbox>,
    kill_on_drop: bool,
}

impl std::fmt::Debug for Sandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sandbox")
            .field("id", &self.id)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Sandbox {
    fn wrap(s: crate::Sandbox, kill_on_drop: bool) -> Sandbox {
        Sandbox {
            id: s.id.clone(),
            name: s.name.clone(),
            commands: Commands(s.commands.clone()),
            files: Files(s.files.clone()),
            inner: Some(s),
            kill_on_drop,
        }
    }

    fn sbx(&self) -> &crate::Sandbox {
        self.inner.as_ref().expect("sandbox handle")
    }

    /// See [`crate::Sandbox::create`]. The sandbox is deleted when the handle is dropped.
    pub fn create(o: CreateOptions) -> Result<Sandbox> {
        block_on(crate::Sandbox::create(o)).map(|s| Sandbox::wrap(s, true))
    }

    /// See [`crate::Sandbox::connect`]. A connected sandbox is not deleted on drop.
    pub fn connect(id: &str, o: ConnectOptions) -> Result<Sandbox> {
        block_on(crate::Sandbox::connect(id, o)).map(|s| Sandbox::wrap(s, false))
    }

    /// Keeps the sandbox running after the handle is dropped. Returns the async handle.
    pub fn detach(mut self) -> crate::Sandbox {
        self.kill_on_drop = false;
        self.inner.take().expect("sandbox handle")
    }

    /// See [`crate::Sandbox::kill`].
    pub fn kill(&self) -> Result<bool> {
        block_on(self.sbx().kill())
    }

    /// See [`crate::Sandbox::pause`].
    pub fn pause(&self) -> Result<()> {
        block_on(self.sbx().pause())
    }

    /// See [`crate::Sandbox::keep_alive`].
    pub fn keep_alive(&self, d: Duration) -> Result<()> {
        block_on(self.sbx().keep_alive(d))
    }

    /// See [`crate::Sandbox::get_info`].
    pub fn get_info(&self) -> Result<Info> {
        block_on(self.sbx().get_info())
    }

    /// See [`crate::Sandbox::is_running`].
    pub fn is_running(&self) -> Result<bool> {
        block_on(self.sbx().is_running())
    }

    /// The async handle, for anything else.
    pub fn as_async(&self) -> &crate::Sandbox {
        self.sbx()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if self.kill_on_drop
            && let Some(s) = self.inner.take()
            && tokio::runtime::Handle::try_current().is_err()
        {
            let _ = runtime().block_on(s.kill());
        }
    }
}

/// Blocking [`crate::Commands`].
pub struct Commands(crate::Commands);

impl Commands {
    pub fn run(&self, cmd: &str) -> Result<CommandResult> {
        block_on(self.0.run(cmd))
    }
    pub fn run_with(&self, cmd: &str, o: RunOptions) -> Result<CommandResult> {
        block_on(self.0.run_with(cmd, o))
    }
    pub fn run_args<I>(&self, argv: I) -> Result<CommandResult>
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        let argv: Vec<String> = argv.into_iter().map(Into::into).collect();
        block_on(self.0.run_args(argv))
    }
    pub fn run_args_with<I>(&self, argv: I, o: RunOptions) -> Result<CommandResult>
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        let argv: Vec<String> = argv.into_iter().map(Into::into).collect();
        block_on(self.0.run_args_with(argv, o))
    }
    pub fn start(&self, cmd: &str) -> Result<CommandHandle> {
        block_on(self.0.start(cmd)).map(CommandHandle)
    }
    pub fn start_with(&self, cmd: &str, o: StartOptions) -> Result<CommandHandle> {
        block_on(self.0.start_with(cmd, o)).map(CommandHandle)
    }
    pub fn start_args_with<I>(&self, argv: I, o: StartOptions) -> Result<CommandHandle>
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        let argv: Vec<String> = argv.into_iter().map(Into::into).collect();
        block_on(self.0.start_args_with(argv, o)).map(CommandHandle)
    }
    pub fn list(&self) -> Result<Vec<ProcessInfo>> {
        block_on(self.0.list())
    }
    pub fn connect(&self, pid: i64) -> Result<CommandHandle> {
        block_on(self.0.connect(pid)).map(CommandHandle)
    }
    pub fn kill(&self, pid: i64) -> Result<bool> {
        block_on(self.0.kill(pid))
    }
}

/// Blocking [`crate::CommandHandle`].
pub struct CommandHandle(crate::CommandHandle);

impl CommandHandle {
    pub fn pid(&self) -> i64 {
        self.0.pid
    }
    pub fn id(&self) -> &str {
        &self.0.id
    }
    pub fn stdout(&self) -> String {
        self.0.stdout()
    }
    pub fn stderr(&self) -> String {
        self.0.stderr()
    }
    pub fn exit_code(&self) -> Option<i32> {
        self.0.exit_code()
    }
    pub fn poll(&self) -> Result<Option<i32>> {
        block_on(self.0.poll())
    }
    pub fn wait(&self) -> Result<CommandResult> {
        block_on(self.0.wait())
    }
    pub fn wait_with(&self, o: WaitOptions) -> Result<CommandResult> {
        block_on(self.0.wait_with(o))
    }
    pub fn kill(&self) -> Result<bool> {
        block_on(self.0.kill())
    }
}

/// Blocking [`crate::Files`].
pub struct Files(crate::Files);

impl Files {
    pub fn read(&self, p: &str) -> Result<Vec<u8>> {
        block_on(self.0.read(p))
    }
    pub fn read_text(&self, p: &str) -> Result<String> {
        block_on(self.0.read_text(p))
    }
    pub fn write(&self, p: &str, data: impl Into<Vec<u8>>) -> Result<WriteInfo> {
        block_on(self.0.write(p, data))
    }
    pub fn write_files(&self, files: Vec<WriteEntry>) -> Result<Vec<WriteInfo>> {
        block_on(self.0.write_files(files))
    }
    pub fn list(&self, p: &str) -> Result<Vec<EntryInfo>> {
        block_on(self.0.list(p))
    }
    pub fn exists(&self, p: &str) -> Result<bool> {
        block_on(self.0.exists(p))
    }
    pub fn get_info(&self, p: &str) -> Result<EntryInfo> {
        block_on(self.0.get_info(p))
    }
    pub fn make_dir(&self, p: &str) -> Result<bool> {
        block_on(self.0.make_dir(p))
    }
    pub fn remove(&self, p: &str) -> Result<()> {
        block_on(self.0.remove(p))
    }
    pub fn rename(&self, old_path: &str, new_path: &str) -> Result<EntryInfo> {
        block_on(self.0.rename(old_path, new_path))
    }
    pub fn upload(&self, local_path: impl AsRef<Path>, remote_path: &str) -> Result<()> {
        block_on(self.0.upload(local_path, remote_path))
    }
    pub fn download(&self, remote_path: &str, local_path: impl AsRef<Path>) -> Result<()> {
        block_on(self.0.download(remote_path, local_path))
    }
    pub fn upload_dir(&self, local_dir: impl AsRef<Path>, remote_dir: &str) -> Result<()> {
        block_on(self.0.upload_dir(local_dir, remote_dir))
    }
    pub fn download_dir(&self, remote_dir: &str, local_dir: impl AsRef<Path>) -> Result<()> {
        block_on(self.0.download_dir(remote_dir, local_dir))
    }
}

/// See [`crate::list`].
pub fn list(o: ConnectOptions) -> Result<Vec<Info>> {
    block_on(crate::list(o))
}

/// See [`crate::kill`].
pub fn kill(id: &str, o: ConnectOptions) -> Result<bool> {
    block_on(crate::kill(id, o))
}

/// See [`crate::list_organizations`].
pub fn list_organizations() -> Result<Vec<Organization>> {
    block_on(crate::list_organizations())
}

/// See [`crate::workspaces_status`].
pub fn workspaces_status(o: &SetupOptions) -> Result<WorkspacesStatus> {
    block_on(crate::workspaces_status(o))
}

/// See [`crate::enable_workspaces`].
pub fn enable_workspaces(o: &SetupOptions) -> Result<WorkspacesStatus> {
    block_on(crate::enable_workspaces(o))
}
