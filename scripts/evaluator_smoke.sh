#!/usr/bin/env bash
set -euo pipefail

project_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
binary=${1:-"$project_root/target/debug/taskmaster"}

if [[ ! -x "$binary" ]]; then
    cargo build --manifest-path "$project_root/Cargo.toml"
fi

test_dir=$(mktemp -d)
supervisor_pid=""
cleanup() {
    exec 3>&- 2>/dev/null || true
    if [[ -n "$supervisor_pid" ]] && kill -0 "$supervisor_pid" 2>/dev/null; then
        kill -TERM "$supervisor_pid" 2>/dev/null || true
        wait "$supervisor_pid" 2>/dev/null || true
    fi
    rm -rf -- "$test_dir"
}
trap cleanup EXIT

write_initial_config() {
    cat >"$test_dir/taskmaster.toml" <<EOF
[programs.keep]
cmd = "sleep 30"
autostart = true
starttime = 0
stoptime = 1
stdout = "$test_dir/keep.out"
stderr = "$test_dir/keep.err"
EOF
}

write_added_config() {
    cat >"$test_dir/taskmaster.toml" <<EOF
[programs.keep]
cmd = "sleep 30"
autostart = true
starttime = 0
stoptime = 1
stdout = "$test_dir/keep.out"
stderr = "$test_dir/keep.err"

[programs.added]
cmd = "sleep 30"
autostart = true
starttime = 0
stoptime = 1
stdout = "$test_dir/added.out"
stderr = "$test_dir/added.err"
EOF
}

child_pids() {
    pgrep -P "$supervisor_pid" 2>/dev/null || true
}

wait_for_child_count() {
    local expected=$1
    for _ in $(seq 1 100); do
        local count
        count=$(child_pids | wc -l | tr -d ' ')
        if [[ "$count" == "$expected" ]]; then
            return 0
        fi
        sleep 0.05
    done
    echo "Expected $expected child process(es), found: $(child_pids | tr '\n' ' ')" >&2
    return 1
}

write_initial_config
mkfifo "$test_dir/input"
exec 3<>"$test_dir/input"
"$binary" "$test_dir/taskmaster.toml" <"$test_dir/input" >"$test_dir/output" 2>&1 &
supervisor_pid=$!

wait_for_child_count 1
unchanged_pid=$(child_pids)

write_added_config
kill -HUP "$supervisor_pid"
wait_for_child_count 2
kill -0 "$unchanged_pid"
all_child_pids=$(child_pids)

write_initial_config
printf 'reload\n' >&3
wait_for_child_count 1
kill -0 "$unchanged_pid"

printf 'status\nexit\n' >&3
for _ in $(seq 1 100); do
    if ! kill -0 "$supervisor_pid" 2>/dev/null; then
        break
    fi
    sleep 0.05
done
if kill -0 "$supervisor_pid" 2>/dev/null; then
    echo "Taskmaster did not exit after the exit command" >&2
    exit 1
fi
wait "$supervisor_pid"
supervisor_pid=""

for pid in $all_child_pids; do
    if kill -0 "$pid" 2>/dev/null; then
        echo "Supervised child $pid survived Taskmaster shutdown" >&2
        exit 1
    fi
done

cat "$test_dir/output"
echo "Evaluator smoke test: PASS"