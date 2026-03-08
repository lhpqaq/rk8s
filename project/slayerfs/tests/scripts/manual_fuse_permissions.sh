#!/usr/bin/env bash

set -euo pipefail

current_dir=$(dirname "$(realpath "$0")")
workspace_dir=$(realpath "$current_dir/../../..")

profile="${SLAYERFS_PROFILE:-debug}"
target_dir="${CARGO_TARGET_DIR:-$workspace_dir/target}"
slayerfs_bin="${SLAYERFS_BIN:-$target_dir/$profile/slayerfs}"
build_threads=6

work_dir="${SLAYERFS_WORKDIR:-$(mktemp -d /tmp/slayerfs-fuse-perm.XXXXXX)}"
data_dir="$work_dir/data"
meta_dir="$work_dir/meta"
mount_dir="$work_dir/mount"
log_dir="$work_dir/logs"
daemon_log="$log_dir/slayerfs.log"
meta_url="${SLAYERFS_META_URL:-sqlite://$meta_dir/metadata.db?mode=rwc}"

force_build="${FORCE_BUILD:-0}"
keep_workdir="${KEEP_WORKDIR:-1}"

test_total=0
test_passed=0
test_failed=0
test_skipped=0
mount_pid=""

usage() {
    cat <<'EOF'
Usage: manual_fuse_permissions.sh

Automates the Linux FUSE permission checks for SlayerFS.

Environment variables:
  SLAYERFS_PROFILE   Build profile to use: debug (default) or release
  SLAYERFS_BIN       Use an existing slayerfs binary instead of target/<profile>/slayerfs
  CARGO_TARGET_DIR   Override Cargo target directory lookup
  FORCE_BUILD=1      Rebuild slayerfs even if the binary already exists
  KEEP_WORKDIR=0     Remove the temporary work directory after the run
  SLAYERFS_WORKDIR   Reuse a fixed work directory instead of mktemp
  SLAYERFS_META_URL  Override the SQLite metadata URL used for the mount

Notes:
  - If Cargo is needed, the script always builds with at most 6 jobs: cargo -j 6.
  - The mount flow uses `slayerfs mount ... --meta-backend sqlx --meta-url ...`.
  - The optional chown -> ENOSYS check needs root or passwordless sudo.
EOF
}

log_info() {
    printf '[INFO] %s\n' "$1"
}

log_warn() {
    printf '[WARN] %s\n' "$1"
}

log_pass() {
    printf '[PASS] %s\n' "$1"
}

log_fail() {
    printf '[FAIL] %s\n' "$1"
}

log_skip() {
    printf '[SKIP] %s\n' "$1"
}

die() {
    printf '[ERROR] %s\n' "$1" >&2
    exit 1
}

require_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        die "Missing required command: $1"
    fi
}

capture_cmd() {
    local __out_var=$1
    shift

    local cmd_output
    local status

    set +e
    cmd_output="$("$@" 2>&1)"
    status=$?
    set -e

    printf -v "$__out_var" '%s' "$cmd_output"
    return "$status"
}

record_pass() {
    test_total=$((test_total + 1))
    test_passed=$((test_passed + 1))
    log_pass "$1"
}

record_fail() {
    test_total=$((test_total + 1))
    test_failed=$((test_failed + 1))
    log_fail "$1"
}

record_skip() {
    test_total=$((test_total + 1))
    test_skipped=$((test_skipped + 1))
    log_skip "$1"
}

show_daemon_log_tail() {
    if [[ -f "$daemon_log" ]]; then
        printf '\n----- slayerfs log tail -----\n'
        tail -n 80 "$daemon_log" || true
        printf '%s\n\n' '-----------------------------'
    fi
}

warn_about_allow_other() {
    if [[ $EUID -eq 0 ]]; then
        return
    fi

    if ! grep -Eq '^[[:space:]]*user_allow_other([[:space:]]|$)' /etc/fuse.conf 2>/dev/null; then
        log_warn "Non-root FUSE mounts may fail because /etc/fuse.conf does not enable user_allow_other."
    fi
}

build_binary_if_needed() {
    local -a cargo_cmd

    if [[ "$force_build" != "1" && -x "$slayerfs_bin" ]]; then
        log_info "Reusing existing slayerfs binary: $slayerfs_bin"
        return
    fi

    require_cmd cargo

    cargo_cmd=(cargo build -p slayerfs -j "$build_threads")
    if [[ "$profile" == "release" ]]; then
        cargo_cmd+=(--release)
    fi

    log_info "Building slayerfs with: ${cargo_cmd[*]}"
    (
        cd "$workspace_dir"
        "${cargo_cmd[@]}"
    )

    if [[ ! -x "$slayerfs_bin" ]]; then
        die "slayerfs binary not found at $slayerfs_bin after build. Set SLAYERFS_BIN if you use a custom target layout."
    fi
}

wait_for_mount() {
    local attempt

    for attempt in $(seq 1 100); do
        if mountpoint -q "$mount_dir"; then
            return 0
        fi

        if [[ -n "$mount_pid" ]] && ! kill -0 "$mount_pid" 2>/dev/null; then
            show_daemon_log_tail
            die "slayerfs mount process exited before the mount became ready"
        fi

        sleep 0.2
    done

    show_daemon_log_tail
    die "Timed out waiting for $mount_dir to become a mountpoint"
}

start_mount() {
    mkdir -p "$data_dir" "$meta_dir" "$mount_dir" "$log_dir"
    : >"$daemon_log"

    if mountpoint -q "$mount_dir"; then
        die "$mount_dir is already mounted"
    fi

    log_info "Starting SlayerFS FUSE mount at $mount_dir"
    (
        export RUST_LOG="${RUST_LOG:-slayerfs=info}"
        export SLAYERFS_FUSE_OP_LOG="${SLAYERFS_FUSE_OP_LOG:-0}"
        exec "$slayerfs_bin" mount "$mount_dir" \
            --data-dir "$data_dir" \
            --meta-backend sqlx \
            --meta-url "$meta_url"
    ) >"$daemon_log" 2>&1 &

    mount_pid=$!
    wait_for_mount
    log_info "Mounted successfully (pid=$mount_pid)"
}

stop_mount() {
    if [[ -n "$mount_pid" ]] && kill -0 "$mount_pid" 2>/dev/null; then
        kill -INT "$mount_pid" 2>/dev/null || true
    fi

    for _ in $(seq 1 50); do
        if ! mountpoint -q "$mount_dir"; then
            break
        fi
        sleep 0.2
    done

    if mountpoint -q "$mount_dir"; then
        if command -v fusermount3 >/dev/null 2>&1; then
            fusermount3 -u "$mount_dir" >/dev/null 2>&1 || true
        fi

        if mountpoint -q "$mount_dir"; then
            umount "$mount_dir" >/dev/null 2>&1 || umount -l "$mount_dir" >/dev/null 2>&1 || true
        fi
    fi

    if [[ -n "$mount_pid" ]] && kill -0 "$mount_pid" 2>/dev/null; then
        kill -TERM "$mount_pid" 2>/dev/null || true
    fi

    if [[ -n "$mount_pid" ]]; then
        wait "$mount_pid" 2>/dev/null || true
    fi

    mount_pid=""
}

cleanup() {
    stop_mount

    if [[ "$keep_workdir" == "0" ]]; then
        rm -rf "$work_dir"
    else
        log_info "Artifacts kept in: $work_dir"
        log_info "SlayerFS daemon log: $daemon_log"
    fi
}

assert_mode() {
    local label=$1
    local path=$2
    local expected=$3
    local actual

    actual=$(stat -c '%a' "$path")
    if [[ "$actual" == "$expected" ]]; then
        record_pass "$label -> $actual"
    else
        record_fail "$label -> expected $expected, got $actual"
    fi
}

assert_regular_file() {
    local label=$1
    local path=$2

    if [[ -f "$path" ]]; then
        record_pass "$label"
    else
        record_fail "$label"
    fi
}

assert_failure_contains() {
    local label=$1
    local expected_text=$2
    shift 2

    local output
    if capture_cmd output "$@"; then
        record_fail "$label -> command unexpectedly succeeded"
        return
    fi

    if grep -Fqi "$expected_text" <<<"$output"; then
        record_pass "$label"
    else
        record_fail "$label -> missing expected text: $expected_text; got: $output"
    fi
}

run_optional_chown_check() {
    local test_file=$1
    local output

    if [[ $EUID -eq 0 ]]; then
        if capture_cmd output chown 123:123 "$test_file"; then
            record_fail "chown should return ENOSYS -> command unexpectedly succeeded"
        elif grep -Eqi 'function not implemented|not implemented|enosys' <<<"$output"; then
            record_pass "chown returns ENOSYS/Function not implemented"
        else
            record_fail "chown should return ENOSYS -> got: $output"
        fi
        return
    fi

    if command -v sudo >/dev/null 2>&1 && sudo -n true >/dev/null 2>&1; then
        if capture_cmd output sudo -n chown 123:123 "$test_file"; then
            record_fail "sudo chown should return ENOSYS -> command unexpectedly succeeded"
        elif grep -Eqi 'function not implemented|not implemented|enosys' <<<"$output"; then
            record_pass "chown returns ENOSYS/Function not implemented"
        else
            record_fail "sudo chown should return ENOSYS -> got: $output"
        fi
        return
    fi

    record_skip "chown -> ENOSYS check skipped (needs root or passwordless sudo)"
}

print_summary() {
    printf '\n==========================================\n'
    printf ' SlayerFS FUSE Permission Checks\n'
    printf '==========================================\n'
    printf 'Total:   %s\n' "$test_total"
    printf 'Passed:  %s\n' "$test_passed"
    printf 'Failed:  %s\n' "$test_failed"
    printf 'Skipped: %s\n' "$test_skipped"
    printf 'Workdir: %s\n' "$work_dir"
    printf 'Mount:   %s\n' "$mount_dir"
    printf 'Meta:    %s\n' "$meta_url"
    printf 'Log:     %s\n' "$daemon_log"
    printf '==========================================\n'
}

main() {
    if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
        usage
        return 0
    fi

    require_cmd mountpoint
    require_cmd stat
    require_cmd chmod
    require_cmd chown
    require_cmd mkdir
    require_cmd touch
    require_cmd grep
    require_cmd tail
    require_cmd umount

    warn_about_allow_other
    build_binary_if_needed
    start_mount

    log_info "Running SlayerFS Linux FUSE permission checks"

    umask 022
    touch "$mount_dir/default_file"
    assert_mode "default file mode with umask 022" "$mount_dir/default_file" "644"

    mkdir "$mount_dir/default_dir"
    assert_mode "default directory mode with umask 022" "$mount_dir/default_dir" "755"

    touch "$mount_dir/persist_file"
    chmod 640 "$mount_dir/persist_file"
    assert_mode "chmod updates mode before remount" "$mount_dir/persist_file" "640"

    stop_mount
    start_mount
    assert_mode "mode persists after remount" "$mount_dir/persist_file" "640"

    touch "$mount_dir/special_file"
    chmod 4755 "$mount_dir/special_file"
    assert_mode "chmod strips setuid/setgid/sticky bits" "$mount_dir/special_file" "755"
    assert_regular_file "chmod keeps the inode as a regular file" "$mount_dir/special_file"

    assert_failure_contains \
        "chmod on a missing path returns ENOENT" \
        "No such file or directory" \
        chmod 644 "$mount_dir/no_such_file"

    umask 000
    mkdir -m 1777 "$mount_dir/special_dir"
    umask 022
    assert_mode \
        "mkdir -m 1777 strips special bits on the create path" \
        "$mount_dir/special_dir" \
        "777"

    run_optional_chown_check "$mount_dir/special_file"

    print_summary

    if [[ "$test_failed" -gt 0 ]]; then
        show_daemon_log_tail
        return 1
    fi

    log_info "Optional next step: run project/slayerfs/tests/scripts/xfstests_slayer.sh for broader FUSE coverage."
}

trap cleanup EXIT

main "$@"
