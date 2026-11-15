// Tour: every part of the API, one snippet per docs section. It's type-checked
// but not meant to be run as-is (it refers to local files like ./my-project).
//
// Setup is the same as for the quickstart (see ../quickstart/main.go); run the
// commands below from the go/ directory of this repository:
//
//   - log in once:  go run ./cmd/chainguard-sandbox login
//     (if a run opens your browser, the saved login expired: approve it);
//   - choose the organization: CreateOptions.Instance in code, or
//     export CHAINCTL_SANDBOX_INSTANCE=XXXXXX for this terminal session, or
//     nothing if you belong to exactly one organization (auto-detected);
//   - list organization IDs:  go run ./cmd/chainguard-sandbox orgs
//     or chainctl iam organizations list.
package main

import (
	"context"
	"errors"
	"fmt"
	"log"
	"time"

	"github.com/sandboxing-dev/chainguard-sandbox-sdk/go/sandbox"
)

func main() {
	// log.Fatal exits without running deferred calls, so the work is in run:
	// its deferred Kill has already deleted the sandbox when an error gets here.
	if err := run(context.Background()); err != nil {
		log.Fatal(err)
	}
}

func run(ctx context.Context) error {
	// --8<-- [start:create]
	sb, err := sandbox.Create(ctx, &sandbox.CreateOptions{
		Name:     "data-job",                                   // display name "data-job/<word-word>"
		Packages: []string{"bash", "coreutils", "python-3.12"}, // Chainguard package names
		Envs:     map[string]string{"MODE": "test"},            // for every command
		Egress:   []string{"pypi.org", "files.pythonhosted.org"},
		// Instance: "XXXXXX", // organization ID; else $CHAINCTL_SANDBOX_INSTANCE, else auto-detected
	})
	if err != nil {
		return err
	}
	defer sb.Kill(context.Background())
	fmt.Println("sandbox", sb.ID)
	// --8<-- [end:create]

	// --8<-- [start:run]
	res, err := sb.Commands.Run(ctx, "echo $MODE && ls /work", &sandbox.RunOptions{
		Cwd:      "/work",
		Envs:     map[string]string{"EXTRA": "1"},
		Timeout:  30 * time.Second,
		OnStdout: func(s string) { fmt.Print("live: ", s) },
	})
	if err != nil {
		return err
	}
	fmt.Println(res.ExitCode, res.Stdout)

	// No shell: argv runs as-is.
	res, err = sb.Commands.RunArgs(ctx, []string{"python3", "-c", "import sys; print(sys.version)"}, nil)
	// --8<-- [end:run]

	// --8<-- [start:errors]
	_, err = sb.Commands.Run(ctx, "exit 3", nil)
	var exit *sandbox.CommandExitError
	if errors.As(err, &exit) {
		fmt.Println("exit code", exit.ExitCode, "stderr", exit.Stderr)
	}
	var timeout *sandbox.TimeoutError
	if _, err := sb.Commands.Run(ctx, "sleep 10", &sandbox.RunOptions{Timeout: time.Second}); errors.As(err, &timeout) {
		fmt.Println("timed out; partial output:", timeout.Result.Stdout)
	}
	// --8<-- [end:errors]

	// --8<-- [start:background]
	h, err := sb.Commands.Start(ctx, "python3 -m http.server 8000", nil)
	if err != nil {
		return err
	}
	fmt.Println("pid", h.PID)
	code, _ := h.Poll(ctx) // nil while running
	fmt.Println("running:", code == nil)
	killed, _ := h.Kill(ctx)
	fmt.Println("killed:", killed)

	job, _ := sb.Commands.Start(ctx, "for i in 1 2 3; do echo $i; sleep 1; done", nil)
	result, err := job.Wait(ctx, &sandbox.WaitOptions{OnStdout: func(s string) { fmt.Print(s) }})
	// --8<-- [end:background]
	_ = result

	// --8<-- [start:files]
	sb.Files.Write(ctx, "data/input.csv", []byte("a,b\n1,2\n"))
	sb.Files.WriteFiles(ctx, []sandbox.WriteEntry{
		{Path: "app/main.py", Data: []byte("print('hi')\n")},
		{Path: "app/run.sh", Data: []byte("python3 app/main.py\n"), Mode: 0o755},
	})
	data, _ := sb.Files.Read(ctx, "data/input.csv")
	entries, _ := sb.Files.List(ctx, "app")
	for _, e := range entries {
		fmt.Println(e.Name, e.Type, e.Size, e.Permissions)
	}
	exists, _ := sb.Files.Exists(ctx, "app/main.py")
	created, _ := sb.Files.MakeDir(ctx, "out/reports")
	sb.Files.Rename(ctx, "data/input.csv", "data/archive/input.csv")
	sb.Files.Remove(ctx, "out")
	var nf *sandbox.NotFoundError
	if _, err := sb.Files.Read(ctx, "missing.txt"); errors.As(err, &nf) {
		fmt.Println("not found:", nf.Name)
	}
	// --8<-- [end:files]
	_, _, _ = data, exists, created

	// --8<-- [start:transfer]
	sb.Files.Upload(ctx, "./local/model.bin", "models/model.bin")
	sb.Files.Download(ctx, "results/out.json", "./out/out.json")
	sb.Files.UploadDir(ctx, "./my-project", "/work/project")
	sb.Files.DownloadDir(ctx, "/work/project/build", "./build")
	// --8<-- [end:transfer]

	// --8<-- [start:lifecycle]
	info, _ := sb.GetInfo(ctx)
	fmt.Println(info.State, info.Region, info.CPUs, info.MemoryMiB)
	sb.KeepAlive(ctx, 30*time.Minute) // don't park while idle for 30 min
	sb.Pause(ctx)                     // snapshot memory+disk; resumes on next call

	again, _ := sandbox.Connect(ctx, sb.ID, nil) // from anywhere, later
	defer again.Close()
	list, _ := sandbox.List(ctx, nil)
	fmt.Println(len(list), "sandboxes")
	sandbox.Kill(ctx, sb.ID, nil) // by ID
	// --8<-- [end:lifecycle]
	return nil
}
