# This script is embedded in the kernel and passed to `/bin/bash -c` as the
# PID 1 command.  The official disk remains the root filesystem; nothing is
# copied into or rewritten on that disk during bootstrap.

set +e

export PATH=/root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export HOME=/root
export TMPDIR=/tmp
export TERM=linux

find_test_script() {
    name=$1
    for base in / /work/testsuits-for-oskernel /testsuits-for-oskernel /work /glibc; do
        if [ -f "$base/scripts/$name" ]; then
            TEST_BASE=$base
            TEST_SCRIPT="./scripts/$name"
            return 0
        fi
        if [ -f "$base/$name" ]; then
            TEST_BASE=$base
            TEST_SCRIPT="./$name"
            return 0
        fi
    done
    return 1
}

run_cagent() {
    if ! find_test_script cagent_testcode.sh; then
        echo "TX_FINAL_INIT cagent=missing"
        return 127
    fi

    cd "$TEST_BASE" || return 1
    echo "TX_FINAL_INIT cagent=start base=$TEST_BASE script=$TEST_SCRIPT"

    # The current official script records the ten test-job PIDs, waits only
    # for those jobs, and then terminates simple_llm_server itself. Execute it
    # unchanged: the old watchdog workaround could outlive CAgent and kill a
    # PID reused by the following BuildStorm run.
    /bin/bash "$TEST_SCRIPT"
    cagent_rc=$?
    echo "TX_FINAL_INIT cagent=done rc=$cagent_rc"
    return "$cagent_rc"
}

run_cagent_diag() {
    if ! find_test_script cagent_testcode.sh; then
        echo "TX_FINAL_INIT cagent-diag=missing"
        return 127
    fi

    cd "$TEST_BASE" || return 1
    echo "TX_FINAL_INIT cagent-diag=start base=$TEST_BASE script=$TEST_SCRIPT"

    # Keep the official workload and timing unchanged.  The diagnostic copy
    # only exposes output which the official script normally deletes after a
    # rejected case.
    /bin/sed \
        -e 's@^[[:space:]]*timeout ${timeout}s@    echo "===== CAGENT_DIAG_CASE_START name=$test_name shell_pid=$BASHPID timeout_s=$timeout ====="; timeout ${timeout}s@' \
        -e 's@^[[:space:]]*local exit_code=$?@    local exit_code=$?; echo "===== CAGENT_DIAG_CASE_RETURN name=$test_name exit=$exit_code ====="@' \
        -e 's@^[[:space:]]*rm -f "$output_file"$@    if [ "$success" -ne 1 ]; then echo "===== CAGENT_DIAG_FAIL name=$test_name exit=$exit_code duration_ms=$duration ====="; cat "$output_file"; echo "===== CAGENT_DIAG_FAIL_END name=$test_name ====="; fi; rm -f "$output_file"@' \
        "$TEST_SCRIPT" > /tmp/cagent_testcode_diag.sh
    if ! /bin/grep -q CAGENT_DIAG_FAIL /tmp/cagent_testcode_diag.sh; then
        echo "TX_FINAL_INIT cagent-diag=instrumentation-failed"
        return 126
    fi

    /bin/bash /tmp/cagent_testcode_diag.sh &
    cagent_suite_pid=$!

    # The official cases have 20-35 second timeouts.  Do not start ps/tail/cat
    # while those cases are still active: that diagnostic process storm changes
    # the very clone/exit/wait4 concurrency being investigated.  After every
    # official timeout has had time to expire, emit one shell-builtin marker
    # only.  Per-case rejected output is already exposed by the instrumented
    # script above.
    (
        sleep 40
        if kill -0 "$cagent_suite_pid" 2>/dev/null; then
            echo "===== CAGENT_DIAG_STALL after_ms=40000 suite_pid=$cagent_suite_pid ====="
        fi
    ) &
    cagent_watchdog_pid=$!

    wait "$cagent_suite_pid"
    cagent_rc=$?
    kill "$cagent_watchdog_pid" 2>/dev/null
    wait "$cagent_watchdog_pid" 2>/dev/null
    echo "TX_FINAL_INIT cagent-diag=done rc=$cagent_rc"
    return "$cagent_rc"
}

run_buildstorm() {
    if ! find_test_script buildstorm_testcode.sh; then
        echo "TX_FINAL_INIT buildstorm=missing"
        return 127
    fi

    cd "$TEST_BASE" || return 1
    echo "TX_FINAL_INIT buildstorm=start base=$TEST_BASE script=$TEST_SCRIPT"
    /bin/sh "$TEST_SCRIPT"
    buildstorm_rc=$?
    echo "TX_FINAL_INIT buildstorm=done rc=$buildstorm_rc"
    return "$buildstorm_rc"
}

if [ "$TX_FINAL_MODE" = "cagent-diag" ]; then
    echo "TX_FINAL_INIT start mode=cagent-diag"
    run_cagent_diag
    cagent_rc=$?
    echo "TX_FINAL_INIT done mode=cagent-diag cagent_rc=$cagent_rc"
    exit "$cagent_rc"
fi

if [ "$TX_FINAL_MODE" = "buildstorm-only" ]; then
    echo "TX_FINAL_INIT start mode=buildstorm-only"
    run_buildstorm
    buildstorm_rc=$?
    sync
    echo "TX_FINAL_INIT done mode=buildstorm-only buildstorm_rc=$buildstorm_rc"
    exit "$buildstorm_rc"
fi

echo "TX_FINAL_INIT start mode=final-all"

run_cagent
cagent_rc=$?
sync

run_buildstorm
buildstorm_rc=$?
sync
echo "TX_FINAL_INIT done mode=final-all cagent_rc=$cagent_rc buildstorm_rc=$buildstorm_rc"

if [ "$cagent_rc" -ne 0 ] || [ "$buildstorm_rc" -ne 0 ]; then
    exit 1
fi
exit 0
