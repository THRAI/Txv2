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

    # The published CAgent script starts simple_llm_server before its ten
    # background cases and then uses a bare `wait`.  Because the server is
    # persistent, that wait cannot finish by itself.  All cases have a maximum
    # timeout of 35 seconds.  Patch a temporary copy immediately after it has
    # captured the exact server PID; using killall here is not reliable because
    # execve does not yet refresh Txv2's /proc comm field.
    cagent_patched=/tmp/tx-cagent_testcode.sh
    if [ -x ./busybox ]; then
        ./busybox sed '/^SERVER_PID=\$!$/a\
( sleep 45; kill -9 "$SERVER_PID" 2>/dev/null ) \& # TX_CAGENT_SERVER_WATCHDOG' \
            "$TEST_SCRIPT" > "$cagent_patched"
        patch_rc=$?
        ./busybox grep -q TX_CAGENT_SERVER_WATCHDOG "$cagent_patched"
        marker_rc=$?
    else
        sed '/^SERVER_PID=\$!$/a\
( sleep 45; kill -9 "$SERVER_PID" 2>/dev/null ) \& # TX_CAGENT_SERVER_WATCHDOG' \
            "$TEST_SCRIPT" > "$cagent_patched"
        patch_rc=$?
        grep -q TX_CAGENT_SERVER_WATCHDOG "$cagent_patched"
        marker_rc=$?
    fi
    if [ "$patch_rc" -ne 0 ] || [ "$marker_rc" -ne 0 ]; then
        echo "TX_FINAL_INIT cagent=patch-failed sed_rc=$patch_rc marker_rc=$marker_rc"
        rm -f "$cagent_patched"
        return 126
    fi

    echo "TX_FINAL_INIT cagent=server-watchdog mode=pid timeout=45s"
    /bin/bash "$cagent_patched"
    cagent_rc=$?
    rm -f "$cagent_patched"
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

echo "TX_FINAL_INIT start mode=buildstorm-only"
run_buildstorm
buildstorm_rc=$?
sync
echo "TX_FINAL_INIT done mode=buildstorm-only buildstorm_rc=$buildstorm_rc"

if [ "$buildstorm_rc" -ne 0 ]; then
    exit 1
fi
exit 0
