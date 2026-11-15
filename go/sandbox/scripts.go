package sandbox

// Shell programs run in the sandbox over Exec. They are the same, byte for
// byte, in the Go, Python and TypeScript sandbox packages, and stick to POSIX
// sh plus busybox/coreutils tools, which every Workspaces environment has.

// stateDir holds background-command state in the guest. /tmp survives a park
// (sessions are memory snapshots).
const stateDir = "/tmp/.chainguard-sandbox"

// Exit codes the scripts use to report conditions the client maps to errors.
const (
	exitNotFound    = 44 // the path (or background command) does not exist
	exitIsDir       = 45 // a file was expected
	exitNotDir      = 46 // a directory was expected
	exitCantChdir   = 47 // the working directory does not exist
	maxPollChunk    = 4 << 20
	maxStdinPerExec = 15 << 20 // stay under the plane's 16 MiB exec stdin limit
)

// runScript: cd into $1, then exec the rest of argv (the env/shell wrapper).
const runScript = `cd -- "$1" 2>/dev/null || { echo "sandbox: no such directory: $1" >&2; exit 47; }; shift; exec "$@"`

// startScript starts argv in the background, detached from the exec, in its
// own session (process group), with output to files. Prints the pid.
// Args: dir cwd argv...
const startScript = `d=$1; c=$2; shift 2
mkdir -p "$d" || exit 1
printf '%s' "$*" > "$d/cmd"
setsid sh -c 'd=$1; c=$2; shift 2; echo $$ > "$d/pid.tmp"; mv "$d/pid.tmp" "$d/pid"
cd -- "$c" 2>/dev/null || { echo "sandbox: no such directory: $c" > "$d/stderr"; echo 47 > "$d/exit"; exit 47; }
"$@" >"$d/stdout" 2>"$d/stderr" </dev/null; echo $? > "$d/exit.tmp"; mv "$d/exit.tmp" "$d/exit"' _ "$d" "$c" "$@" </dev/null >/dev/null 2>&1 &
i=0; while [ ! -s "$d/pid" ] && [ $i -lt 100 ]; do sleep 0.1; i=$((i+1)); done
p=$(cat "$d/pid" 2>/dev/null) || exit 1
s=${d%/cmd/*}; mkdir -p "$s/pid" && ln -sfn "$d" "$s/pid/$p"
echo "$p"`

// pollScript waits up to $4 tenths of a second for a background command to
// exit, then prints "<exit|-> <stdout size> <stderr size>\n" followed by the
// new stdout bytes (from offset $2) and new stderr bytes (from offset $3), each
// capped at $5 bytes. Args: dir outOffset errOffset waitTenths maxBytes
const pollScript = `d=$1; o=$2; e=$3; w=$4; m=$5
[ -d "$d" ] || exit 44
i=0; while [ ! -f "$d/exit" ] && [ $i -lt $w ]; do sleep 0.1; i=$((i+1)); done
x=-; [ -f "$d/exit" ] && x=$(cat "$d/exit")
so=$(wc -c < "$d/stdout" 2>/dev/null) || so=0; se=$(wc -c < "$d/stderr" 2>/dev/null) || se=0
so=$((so)); se=$((se))
[ $((so-o)) -gt $m ] && so=$((o+m)); [ $((se-e)) -gt $m ] && se=$((e+m))
echo "$x $so $se"
[ $so -gt $o ] && tail -c +$((o+1)) "$d/stdout" | head -c $((so-o))
[ $se -gt $e ] && tail -c +$((e+1)) "$d/stderr" | head -c $((se-e))
exit 0`

// killScript signals a background command's process group and records the
// exit code (128+signal) if the command didn't. Prints 1 if it was running.
// Args: dir signal code
const killScript = `d=$1; s=$2; x=$3
[ -d "$d" ] || exit 44
[ -f "$d/exit" ] && { echo 0; exit 0; }
p=$(cat "$d/pid" 2>/dev/null) || exit 44
kill -s "$s" -- "-$p" 2>/dev/null || kill -s "$s" "-$p" 2>/dev/null || kill -s "$s" "$p" 2>/dev/null || { echo 0; exit 0; }
i=0; while kill -0 "$p" 2>/dev/null && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done
[ -f "$d/exit" ] || { echo "$x" > "$d/exit.tmp"; mv "$d/exit.tmp" "$d/exit"; }
echo 1`

// listScript prints one line per background command:
// "<id>\t<pid>\t<exit|->\t<command>". Args: stateDir
const listScript = `for d in "$1"/cmd/*; do [ -f "$d/pid" ] || continue
x=-; [ -f "$d/exit" ] && x=$(cat "$d/exit")
printf '%s\t%s\t%s\t%s\n' "${d##*/}" "$(cat "$d/pid")" "$x" "$(head -c 500 "$d/cmd" | tr '\n\t' '  ')"
done; exit 0`

// resolvePidScript prints the state directory of background pid $2. Args: stateDir pid
const resolvePidScript = `l="$1/pid/$2"; [ -L "$l" ] || exit 44; readlink -- "$l"`

// statFn prints "<type>\t<size> <mode> <user> <group> <mtime>\t<link target>\t<path>"
// for one path, without following a final symlink.
const statFn = `st() { if [ -L "$1" ]; then t=symlink; elif [ -d "$1" ]; then t=dir; else t=file; fi
s=$(stat -c '%s %a %U %G %Y' -- "$1") || return 1
l=; [ "$t" = symlink ] && l=$(readlink -- "$1")
printf '%s\t%s\t%s\t%s\n' "$t" "$s" "$l" "$1"; }
`

const (
	readScript    = `[ -e "$1" ] || exit 44; [ -d "$1" ] && exit 45; exec cat -- "$1"`
	existsScript  = `[ -e "$1" ] || [ -L "$1" ]`
	infoScript    = statFn + `[ -e "$1" ] || [ -L "$1" ] || exit 44; st "$1"`
	listDirScript = statFn + `[ -e "$1" ] || exit 44; [ -d "$1" ] || exit 46; cd -- "$1" || exit 1
for f in * .[!.]* ..?*; do [ -e "$f" ] || [ -L "$f" ] || continue; st "$f"; done; exit 0`
	makeDirScript = `if [ -d "$1" ]; then echo 0; elif [ -e "$1" ]; then exit 46; else mkdir -p -- "$1" && echo 1; fi`
	removeScript  = `[ -e "$1" ] || [ -L "$1" ] || exit 44; rm -rf -- "$1"`
	renameScript  = statFn + `[ -e "$1" ] || [ -L "$1" ] || exit 44; mkdir -p -- "$(dirname -- "$2")" && mv -f -- "$1" "$2" && st "$2"`
	// appendScript writes stdin to $1 ($2 = "trunc" for the first piece).
	appendScript = `mkdir -p -- "$(dirname -- "$1")" || exit 1; if [ "$2" = trunc ]; then cat > "$1"; else cat >> "$1"; fi`
	// finishScript moves a staged upload into place with mode $3. Args: tmp dest mode
	finishScript = `[ -d "$2" ] && { rm -f -- "$1"; echo "$2: is a directory" >&2; exit 45; }
mkdir -p -- "$(dirname -- "$2")" && chmod "$3" "$1" && mv -f -- "$1" "$2"`
	// untarScript extracts a gzip'd tar into $2 (creating it), from file $1 or stdin if $1 is "-".
	untarScript = `mkdir -p -- "$2" || exit 1; if [ "$1" = - ]; then tar -xzf - -C "$2"; else tar -xzf "$1" -C "$2"; s=$?; rm -f -- "$1"; exit $s; fi`
	// tarScript writes a gzip'd tar of directory $1 to stdout.
	tarScript = `[ -e "$1" ] || exit 44; [ -d "$1" ] || exit 46; exec tar -czf - -C "$1" .`
)
