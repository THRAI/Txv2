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
    /bin/bash "$TEST_SCRIPT" &
    cagent_runner=$!

    # The published CAgent script starts simple_llm_server before its ten
    # background cases and then uses a bare `wait`.  Because the server is
    # persistent, that wait cannot finish by itself.  All cases have a maximum
    # timeout of 35 seconds, so stop only the server after 45 seconds; the
    # official script then prints its END marker and performs its own cleanup.
    (
        sleep 45
        if [ -x ./busybox ]; then
            ./busybox killall simple_llm_server 2>/dev/null
        else
            killall simple_llm_server 2>/dev/null
        fi
    ) &
    cagent_watchdog=$!

    wait "$cagent_runner"
    cagent_rc=$?
    kill "$cagent_watchdog" 2>/dev/null
    wait "$cagent_watchdog" 2>/dev/null
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

echo "TX_FINAL_INIT start"
run_cagent
cagent_rc=$?
run_buildstorm
buildstorm_rc=$?
sync
echo "TX_FINAL_INIT done cagent_rc=$cagent_rc buildstorm_rc=$buildstorm_rc"

if [ "$cagent_rc" -ne 0 ] || [ "$buildstorm_rc" -ne 0 ]; then
    exit 1
fi
exit 0
