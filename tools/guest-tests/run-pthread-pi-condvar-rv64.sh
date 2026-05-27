#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

SOURCE_DATA="${OSCOMP_SOURCE_DATA:-target/oscomp/futex-pthread-data}"
SUBMIT_DIR="${OSCOMP_SUBMIT:-target/oscomp/futex-pthread-submit}"
WORK_DATA="${OSCOMP_PI_CONDVAR_DATA:-target/oscomp/pi-condvar-data}"
OUT_LOG="${OSCOMP_OUT_RV:-target/oscomp/os_serial_out_pthread_pi_condvar_$(date +%Y%m%d%H%M%S).txt}"
GUEST_BUILD_DIR="${GUEST_BUILD_DIR:-target/guest-tests}"
TEST_SRC="${TEST_SRC:-tools/guest-tests/pthread_pi_condvar.c}"
TEST_BIN="${GUEST_BUILD_DIR}/pthread_pi_condvar.riscv64-musl"
JUDGE_SRC="${JUDGE_SRC:-tools/guest-tests/judge_pthread-pi-condvar.py}"
JUDGE_OUT="${GUEST_BUILD_DIR}/pthread_pi_condvar_judge.txt"
WRAPPER="${GUEST_BUILD_DIR}/pthread_pi_condvar_testcode.sh"

find_tool() {
    local explicit="$1"
    shift
    if [[ -n "${explicit}" ]]; then
        if [[ -x "${explicit}" ]]; then
            printf '%s\n' "${explicit}"
            return 0
        fi
        echo "missing executable: ${explicit}" >&2
        return 1
    fi

    local candidate
    for candidate in "$@"; do
        if [[ "${candidate}" = /* && -x "${candidate}" ]]; then
            printf '%s\n' "${candidate}"
            return 0
        fi
        if command -v "${candidate}" >/dev/null 2>&1; then
            command -v "${candidate}"
            return 0
        fi
    done
    echo "missing required tool; tried: $*" >&2
    return 1
}

ZIG="$(find_tool "${ZIG:-}" zig /opt/homebrew/bin/zig)"
DEBUGFS="$(find_tool "${DEBUGFS:-}" /opt/homebrew/Cellar/e2fsprogs/1.47.4/sbin/debugfs /opt/homebrew/sbin/debugfs debugfs)"
TIMEOUT="$(find_tool "${GTIMEOUT:-}" /opt/homebrew/bin/gtimeout gtimeout timeout)"

if [[ ! -f "${SOURCE_DATA}/sdcard-rv.img" ]]; then
    echo "missing source sdcard: ${SOURCE_DATA}/sdcard-rv.img" >&2
    echo "set OSCOMP_SOURCE_DATA to a prepared OSComp data directory" >&2
    exit 1
fi
if [[ ! -x "${SUBMIT_DIR}/kernel-rv" ]]; then
    echo "missing kernel artifact: ${SUBMIT_DIR}/kernel-rv" >&2
    echo "run make oscomp-submit-rv64 OSCOMP_SUBMIT=${SUBMIT_DIR}" >&2
    exit 1
fi
if [[ ! -f "${JUDGE_SRC}" ]]; then
    echo "missing judge script: ${JUDGE_SRC}" >&2
    exit 1
fi

mkdir -p "${GUEST_BUILD_DIR}" "${WORK_DATA}" "$(dirname "${OUT_LOG}")"

"${ZIG}" cc -target riscv64-linux-musl -static -O2 -pthread \
    "${TEST_SRC}" -o "${TEST_BIN}"

cp "${SOURCE_DATA}/sdcard-rv.img" "${WORK_DATA}/sdcard-rv.img"
if [[ -f "${SOURCE_DATA}/config.json" ]]; then
    cp "${SOURCE_DATA}/config.json" "${WORK_DATA}/config.json"
fi
cp "${JUDGE_SRC}" "${WORK_DATA}/judge_pthread-pi-condvar.py"

cat > "${WRAPPER}" <<'SH'
#!/bin/sh
echo "#### OS COMP TEST GROUP START pthread-pi-condvar ####"
echo "#### TX GUEST TEST WRAPPER START pthread-pi-condvar ####"
echo "TX guest wrapper: launching ./pthread_pi_condvar"
./pthread_pi_condvar
r=$?
echo "TX guest wrapper: ./pthread_pi_condvar returned $r"
echo "#### TX GUEST TEST WRAPPER END pthread-pi-condvar status=$r ####"
echo "#### OS COMP TEST GROUP END pthread-pi-condvar ####"
exit $r
SH
chmod +x "${WRAPPER}"

"${DEBUGFS}" -w -R "rm /musl/pthread_pi_condvar" "${WORK_DATA}/sdcard-rv.img" >/dev/null 2>&1 || true
"${DEBUGFS}" -w -R "rm /musl/pthread_pi_condvar_testcode.sh" "${WORK_DATA}/sdcard-rv.img" >/dev/null 2>&1 || true
"${DEBUGFS}" -w -R "write ${TEST_BIN} /musl/pthread_pi_condvar" "${WORK_DATA}/sdcard-rv.img" >/dev/null
"${DEBUGFS}" -w -R "write ${WRAPPER} /musl/pthread_pi_condvar_testcode.sh" "${WORK_DATA}/sdcard-rv.img" >/dev/null

"${TIMEOUT}" 120s make oscomp-qemu-rv64 \
    OSCOMP_DATA="${WORK_DATA}" \
    OSCOMP_SUBMIT="${SUBMIT_DIR}" \
    OSCOMP_OUT_RV="${OUT_LOG}" \
    OSCOMP_GROUPS=pthread-pi-condvar

rg -n "TX guest wrapper: ./pthread_pi_condvar returned 0" "${OUT_LOG}" >/dev/null
rg -n "PASS pthread-pi-condvar" "${OUT_LOG}" >/dev/null
rg -n "txkernel:qemu-riscv64-virt:userspace:exited:0" "${OUT_LOG}" >/dev/null

python3 tools/oscomp-judge.py "${OUT_LOG}" "${WORK_DATA}" | tee "${JUDGE_OUT}"
rg -n "\\[pthread-pi-condvar\\] 1/1" "${JUDGE_OUT}" >/dev/null

FAULT_OUT="${GUEST_BUILD_DIR}/pthread_pi_condvar_fault_decode.txt"
if cargo xtask fault-decode --target rv64-qemu --serial "${OUT_LOG}" --all --brief >"${FAULT_OUT}" 2>&1; then
    cat "${FAULT_OUT}"
    echo "unexpected trap lines decoded from ${OUT_LOG}" >&2
    exit 1
else
    cat "${FAULT_OUT}"
    rg -n "no scause/sepc/stval trap lines found" "${FAULT_OUT}" >/dev/null
    echo "fault-decode found no trap lines in ${OUT_LOG}"
fi

echo "pthread PI-condvar guest evidence: ${OUT_LOG}"
echo "pthread PI-condvar judge evidence: ${JUDGE_OUT}"
