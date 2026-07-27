# Taskmaster

Rust process supervisor for the 42 Taskmaster project. It runs in the foreground, keeps configured jobs under supervision, logs lifecycle events, and provides an interactive control shell.

## Evaluation status

The mandatory marking-sheet behaviors are implemented:

| Requirement | Implementation / proof |
|---|---|
| Control shell | `status`, `start`, `stop`, `restart`, `reload`, `exit` |
| Config at launch | TOML loader with validation |
| Hot reload | Shell `reload` and external `SIGHUP` |
| Selective reload | Unchanged program groups keep their existing PIDs |
| Logging | START, STOP, RESTART, CRASH, FATAL, CONFIG_RELOAD |
| Required options | All options from the subject are represented below |
| Graceful stop | Configured signal, timeout, then SIGKILL escalation |
| Clean shutdown | Every attached process group is stopped before exit |
| Retry abort | Early failures become `FATAL` after `startretries` |
| Output handling | Omitted output paths discard; configured paths append |

Only `serde` and `toml` are third-party dependencies. They are used exclusively for configuration parsing, as allowed by the subject. Signals, process groups, timestamps, shell parsing, and logging use the Rust standard library and small operating-system FFI declarations.

## Build and run

```sh
cargo build --locked
cargo run -- config/evaluation.toml
```

The binary remains in the foreground:

```sh
./target/debug/taskmaster config/evaluation.toml
```

## Control shell

```text
status
start <program|all>
stop <program|all>
restart <program|all>
reload
history
help
quit
exit
```

`status` shows one row per configured instance with its PID, `STARTING`, `RUNNING`, `STOPPED`, or `FATAL` state, and uptime.

`quit`, `exit`, Ctrl-C, and SIGTERM stop the supervisor. Taskmaster sends each configured stop signal to the whole child process group, waits `stoptime`, then sends SIGKILL to any survivors. Descendants do not remain orphaned.

## Hot reload

Both interfaces read the original config path again:

```text
taskmaster> reload
```

```sh
kill -HUP <taskmaster-pid>
```

Reload is diff-based:

- unchanged programs and their PIDs are preserved;
- removed programs are stopped and removed;
- added programs are registered and started when `autostart = true`;
- changed programs are gracefully stopped, replaced, and restarted if they were active or are configured for autostart;
- invalid replacement files are rejected before runtime state changes.

## Configuration

```toml
[programs.worker]
cmd = "while true; do echo $MESSAGE; sleep 2; done"
numprocs = 2
autostart = true
autorestart = "unexpected"
exitcodes = [0]
startretries = 3
starttime = 1
stopsignal = "TERM"
stoptime = 2
stdout = "/tmp/worker.out"
stderr = "/tmp/worker.err"
log = "/tmp/worker.lifecycle.log"
workingdir = "/tmp"
umask = 0o027

[programs.worker.env]
MESSAGE = "managed by taskmaster"
```

| Field | Meaning | Default |
|---|---|---|
| `cmd` | Shell command used to launch the program | required |
| `numprocs` | Instances to create and supervise | `1` |
| `autostart` | Start when Taskmaster launches | `false` |
| `autorestart` | `always`, `never`, or `unexpected` | `never` |
| `exitcodes` | Expected exit codes | `[0]` |
| `startretries` | Retries after early startup failures | `0` |
| `starttime` | Seconds required to reach `RUNNING`; `0` accepts immediate exit | `1` |
| `stopsignal` | Graceful stop signal name or number | `TERM` |
| `stoptime` | Seconds before SIGKILL; `0` escalates immediately | `5` |
| `stdout` | Append stdout to this file; omit to discard | discard |
| `stderr` | Append stderr to this file; omit to discard | discard |
| `log` | Per-program lifecycle log; omit for global log | global |
| `env` | Environment variables for the child | inherited environment |
| `workingdir` | Child working directory | Taskmaster directory |
| `umask` | Child Unix umask | inherited umask |

Commands are executed through `/bin/sh -c` on Unix and `cmd.exe /S /C` on Windows. Shell built-ins such as `exit 42`, quoting, environment expansion, and redirections therefore work as configuration commands.

### Short-lived commands

A command such as `ls` exits before the default one-second `starttime`, so it is correctly treated as a startup failure. Configure `starttime = 0` for intentional one-shot commands:

```toml
[programs.oneshot]
cmd = "ls -la"
starttime = 0
autorestart = "never"
exitcodes = [0]
```

### `NUL` is not a typo

`NUL` is the Windows null device, equivalent to `/dev/null` on Unix. Taskmaster selects the correct name at compile time.

## Logging

The fallback log is `./logs/taskmaster.log`. A program-specific `log` path overrides it for that program.

Logged events:

- process start and PID;
- graceful/manual stop;
- unexpected exit or signal death;
- automatic restart and reason;
- retry exhaustion (`FATAL` and attempt count);
- configuration reload and source path.

## Evaluator proof

Run all unit tests:

```sh
cargo test --workspace
```

On Linux, run the end-to-end marking-sheet smoke test:

```sh
bash scripts/evaluator_smoke.sh
```

It proves:

1. initial autostart;
2. external SIGHUP reload;
3. unchanged PID preservation;
4. autostart of an added program;
5. shell-command reload;
6. removal of a program;
7. accurate status output;
8. shutdown through `exit`;
9. no surviving supervised children.

Use `config/evaluation.toml` during the defense to demonstrate every required configuration option, retry exhaustion, and a short-lived command.

## Project layout

- `bin/main.rs`: startup, signal installation, shell, shutdown
- `crates/config`: TOML model and validation
- `crates/process`: spawning, process groups, lifecycle reconciliation, selective reload
- `crates/signals`: SIGINT/SIGTERM/SIGHUP flags
- `crates/logger`: lifecycle logging
- `crates/tui`: interactive control shell
- `scripts/evaluator_smoke.sh`: end-to-end evaluator proof