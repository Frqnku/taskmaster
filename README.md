# Taskmaster

Taskmaster is a small Rust process supervisor inspired by `supervisord`. It loads a TOML configuration file, starts configured programs, watches them in a background supervision loop, restarts them when policy says to do so, and exposes an interactive shell for runtime control.

## What it does

Taskmaster can:

- launch one or more instances of a program
- autostart selected programs on launch
- restart processes based on `autorestart`, exit codes, and retry limits
- redirect stdout and stderr to files
- set environment variables and working directories
- apply a Unix umask for child processes
- stop processes gracefully with a configured signal and timeout
- provide an interactive shell with `start`, `stop`, `restart`, `status`, `help`, `quit`, and `exit`

## How it works

The project is organized into crates:

- `bin/main.rs` loads configuration, initializes logging, starts the supervisor, and runs the shell
- `crates/config` parses and validates the TOML configuration
- `crates/process` owns the runtime supervision logic
- `crates/logger` writes lifecycle events to per-program or global log files
- `crates/tui` implements the interactive shell and command parser

### Runtime flow

1. The binary reads a config file path from the command line.
2. The config is parsed and validated.
3. The logger is initialized.
4. The supervisor is built from the config.
5. Programs marked `autostart = true` are started.
6. The background supervisor thread reconciles running processes every 250 ms.
7. The shell accepts commands while the supervisor keeps running.
8. On exit, Taskmaster stops the supervisor and waits for children to terminate.

### Process supervision model

Each configured program becomes a `ProcessGroup`. A group manages multiple `ProcessInstance`s when `numprocs > 1`.

The supervisor checks each instance periodically and applies restart policy:

- `autorestart = "always"` restarts after any exit
- `autorestart = "never"` leaves the process stopped
- `autorestart = "unexpected"` restarts only when the exit code is not in `exitcodes`

If a process exits before `starttime` is reached, Taskmaster treats that as an early failure and applies `startretries`.

## Configuration

Taskmaster uses a TOML file with a `programs` table. See `config/example.toml` for a working sample.

Example:

```toml
[programs.web]
cmd = "cmd /C timeout /T 10 /NOBREAK >NUL"
numprocs = 2
autostart = true
autorestart = "always"
exitcodes = [0]
startretries = 3
starttime = 2
stopsignal = "TERM"
stoptime = 5
stdout = "./logs/web.out"
stderr = "./logs/web.err"
log = "./logs/web.log"

[programs.web.env]
PORT = "8080"
RUST_LOG = "debug"
```

### Supported fields

- `cmd`: command to run, required
- `numprocs`: number of instances to launch, default `1`
- `autostart`: start automatically when Taskmaster launches, default `false`
- `autorestart`: `always`, `never`, or `unexpected`, default `never`
- `exitcodes`: list of expected exit codes, default `[0]`
- `startretries`: retry count for early failures, default `0`
- `starttime`: seconds a process must stay alive to count as started, default `1`
- `stopsignal`: stop signal name or number, default `TERM`
- `stoptime`: graceful stop timeout in seconds, default `5`
- `stdout`: file path for stdout redirection
- `stderr`: file path for stderr redirection
- `log`: per-program lifecycle log path
- `env`: environment variables for the child process
- `workingdir`: working directory for the child process
- `umask`: Unix umask for the child process

### Validation rules

The config loader rejects:

- empty program lists
- empty `cmd`
- `numprocs = 0`
- `starttime = 0`
- `stoptime = 0`
- empty `exitcodes`
- invalid `stopsignal` values
- excessively large `startretries` values

## Command-line usage

Build and run:

```bash
cargo run -- config/example.toml
```

If you already built the project:

```bash
./target/debug/taskmaster config/example.toml
```

## Shell commands

Once Taskmaster is running, it shows a `taskmaster>` prompt.

- `start <program>` starts all instances for a program
- `stop <program>` stops all running instances for a program
- `restart <program>` stops and starts the program again
- `status` prints current state for every configured program
- `reload` is present in the shell, but in the current implementation it prints that reload is not yet implemented
- `help` shows available commands
- `quit` or `exit` shuts Taskmaster down cleanly

## Logging

Taskmaster writes lifecycle events such as `START`, `STOP`, `RESTART`, `CRASH`, and `CONFIG_RELOAD`.

- If a program has a `log` path, lifecycle events are written there
- Otherwise events fall back to `./logs/taskmaster.log`

Child stdout and stderr are separate from lifecycle logs when `stdout` or `stderr` are set in the config.

## How to test it

### 1. Basic startup

Use the sample config:

```bash
cargo run -- config/example.toml
```

Expected behavior:

- config is loaded successfully
- autostart programs begin running
- shell prompt appears
- `status` shows active programs

### 2. Start and stop a program

At the shell prompt:

```text
taskmaster> start web
taskmaster> status
taskmaster> stop web
taskmaster> status
```

Expected behavior:

- `start web` launches all configured instances of `web`
- `status` shows running instances with PIDs and uptime when available
- `stop web` stops every instance in the group

### 3. Restart behavior

Create a config where a program exits quickly:

```toml
[programs.flaky]
cmd = "sh -c 'exit 1'"
numprocs = 1
autostart = true
autorestart = "always"
exitcodes = [0]
startretries = 2
starttime = 3
log = "./logs/flaky.log"
```

Expected behavior:

- the process restarts after crashing
- after too many early failures, it stops retrying
- `flaky.log` shows `START`, `CRASH`, and `RESTART` events

### 4. Stop timeout escalation

Create a process that ignores the configured stop signal:

```toml
[programs.ignores_stop]
cmd = "sh -c 'trap \"\" TERM; sleep 60'"
numprocs = 1
autostart = true
stopsignal = "TERM"
stoptime = 2
log = "./logs/ignores_stop.log"
```

Expected behavior:

- Taskmaster sends the configured stop signal
- it waits up to `stoptime`
- if the process is still alive, it force-kills it

### 5. Invalid config handling

Test these cases:

```toml
[programs.bad]
cmd = ""
```

```toml
[programs.bad]
cmd = "sleep 1"
numprocs = 0
```

```toml
[programs.bad]
cmd = "sleep 1"
stopsignal = "NOT_A_SIGNAL"
```

Expected behavior:

- Taskmaster refuses to load the file
- it prints a validation error
- it exits without starting the supervisor

### 6. stdout and stderr redirection

Use a program that writes to both streams:

```toml
[programs.io_test]
cmd = "sh -c 'echo hello; echo error 1>&2'"
stdout = "./logs/io_test.out"
stderr = "./logs/io_test.err"
autostart = true
```

Expected behavior:

- stdout goes to `io_test.out`
- stderr goes to `io_test.err`
- lifecycle logs still go to the program log or global log

### 7. Process tree cleanup

Test with a child that spawns another process:

```toml
[programs.forking]
cmd = "sh -c 'sleep 100 & sleep 100; wait'"
autostart = true
stopsignal = "TERM"
stoptime = 2
```

Expected behavior:

- stopping the program should terminate the entire supervised tree
- if a descendant survives, that indicates a process-group cleanup problem

## Edge cases to check manually

1. Start a program that exits immediately and confirm restart policy matches `autorestart`.
2. Set `startretries = 1` and confirm early failures stop after the retry limit.
3. Use an invalid executable path and confirm spawn fails cleanly.
4. Send Ctrl+C while programs are running and confirm the supervisor shuts down.
5. Try `start` or `stop` with an unknown program name and confirm the shell prints a clear error.
6. Try extra whitespace or an empty line at the shell and confirm nothing crashes.

## Unix notes

- Taskmaster uses normal child process spawning, not a full double-fork daemon.
- Child stdout and stderr are redirected using file handles or `/dev/null`.
- On Unix, the child can apply a custom `umask`.
- The stop path respects a configured signal and falls back to force kill after a timeout.

## Known limitations

- `reload` is accepted by the shell, but the current implementation only prints that reload is not yet implemented.
- The supervisor uses periodic reconciliation rather than a signal-driven wakeup mechanism.
- The project does not currently expose a socket-based control API.

## Development

Run tests:

```bash
cargo test
```

Run a single crate’s tests if needed:

```bash
cargo test -p config
cargo test -p process
cargo test -p tui
```

## Recommended demo sequence

1. Show `config/example.toml`.
2. Run `cargo run -- config/example.toml`.
3. Use `status` to show running programs.
4. Use `stop <program>` and `restart <program>`.
5. Demonstrate invalid config rejection.
6. Demonstrate a crash-loop config and explain `starttime` and `startretries`.

## Repository layout

```text
bin/main.rs
config/example.toml
crates/config
crates/logger
crates/process
crates/signals
crates/tui
crates/utils
```

## License

No license file is present in the repository.