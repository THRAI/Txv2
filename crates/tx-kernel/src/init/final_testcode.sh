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
