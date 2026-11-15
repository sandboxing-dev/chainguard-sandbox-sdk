"""Tour: every part of the API, one snippet per docs section. It's type-checked
but not meant to be run as-is (it refers to local files like ./my-project).

Setup is the same as for the quickstart (see quickstart.py):

- log in once:  uv run chainguard-sandbox login
  (if a run opens your browser, the saved login expired: approve it);
- choose the organization: Sandbox.create(instance="XXXXXX") in code, or
  export CHAINCTL_SANDBOX_INSTANCE=XXXXXX for this terminal session, or
  nothing if you belong to exactly one organization (auto-detected);
- list organization IDs:  uv run chainguard-sandbox orgs  or  chainctl iam organizations list.
"""
from chainguard_sandbox import CommandExitError, FileNotFoundError, Sandbox, TimeoutError

# --8<-- [start:create]
sbx = Sandbox.create(
    name="data-job",                                # display name "data-job/<word-word>"
    packages=["bash", "coreutils", "python-3.12"],  # Chainguard package names
    envs={"MODE": "test"},                          # for every command
    egress=["pypi.org", "files.pythonhosted.org"],
    # instance="XXXXXX",  # organization ID; else $CHAINCTL_SANDBOX_INSTANCE, else auto-detected
)
print("sandbox", sbx.sandbox_id)
# ... and sbx.kill() when done (or use `with Sandbox.create(...) as sbx:`)
# --8<-- [end:create]

# --8<-- [start:run]
r = sbx.commands.run(
    "echo $MODE && ls /work",
    cwd="/work",
    envs={"EXTRA": "1"},
    timeout=30,                                   # seconds; None or 0 = no limit
    on_stdout=lambda s: print("live:", s, end=""),
)
print(r.exit_code, r.stdout)

# A list runs without a shell.
r = sbx.commands.run(["python3", "-c", "import sys; print(sys.version)"])
# --8<-- [end:run]

# --8<-- [start:errors]
try:
    sbx.commands.run("exit 3")
except CommandExitError as e:
    print("exit code", e.exit_code, "stderr", e.stderr)

try:
    sbx.commands.run("sleep 10", timeout=1)
except TimeoutError as e:
    print("timed out; partial output:", e.result.stdout if e.result else "")
# --8<-- [end:errors]

# --8<-- [start:background]
server = sbx.commands.run("python3 -m http.server 8000", background=True)
print("pid", server.pid)
print("running:", server.poll() is None)
print("killed:", server.kill())

job = sbx.commands.run("for i in 1 2 3; do echo $i; sleep 1; done", background=True)
result = job.wait(on_stdout=lambda s: print(s, end=""))

# Or iterate over output chunks as they arrive:
job = sbx.commands.run("for i in 1 2 3; do echo $i; sleep 1; done", background=True)
for stdout, stderr in job:
    print(stdout or "", end="")
# --8<-- [end:background]

# --8<-- [start:files]
sbx.files.write("data/input.csv", "a,b\n1,2\n")
sbx.files.write_files([
    {"path": "app/main.py", "data": "print('hi')\n"},
    {"path": "app/run.sh", "data": "python3 app/main.py\n", "mode": 0o755},
])
text = sbx.files.read("data/input.csv")                  # str
raw = sbx.files.read("data/input.csv", format="bytes")   # bytes
for e in sbx.files.list("app"):
    print(e.name, e.type, e.size, e.permissions)
exists = sbx.files.exists("app/main.py")
created = sbx.files.make_dir("out/reports")
sbx.files.rename("data/input.csv", "data/archive/input.csv")
sbx.files.remove("out")
try:
    sbx.files.read("missing.txt")
except FileNotFoundError as e:
    print("not found:", e.name)
# --8<-- [end:files]

# --8<-- [start:transfer]
sbx.files.upload("./local/model.bin", "models/model.bin")
sbx.files.download("results/out.json", "./out/out.json")
sbx.files.upload_dir("./my-project", "/work/project")
sbx.files.download_dir("/work/project/build", "./build")
# --8<-- [end:transfer]

# --8<-- [start:lifecycle]
info = sbx.get_info()
print(info.state, info.region, info.cpu_count, info.memory_mb)
sbx.keep_alive(30 * 60)          # don't park while idle for 30 min
sbx.pause()                      # snapshot memory+disk; resumes on next call

again = Sandbox.connect(sbx.sandbox_id)   # from anywhere, later
print(len(Sandbox.list()), "sandboxes")
Sandbox.kill(sbx.sandbox_id)              # by ID (or sbx.kill())
# --8<-- [end:lifecycle]
