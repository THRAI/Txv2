#!/usr/bin/env bash
set -u
set -o pipefail

ROOT=/home/ljs/ljslll/os/Txv2
QEMU=/home/ljs/ljslll/os/qemu-local/install-9.2.1/bin/qemu-system-riscv64
KERNEL="$ROOT/target/oscomp/submit/kernel-rv"
IMAGE=/home/ljs/sdcards-final/sdcard-rv-pub.img
RUN_ROOT="$ROOT/target/final-rv-5x-$(date +%Y%m%d-%H%M%S)"
SUMMARY="$RUN_ROOT/summary.tsv"

mkdir -p "$RUN_ROOT"
printf 'run\trc\telapsed_s\tcagent_pass\tcagent_reject\tbuildstorm_ok\tlog\n' >"$SUMMARY"

for run in 1 2 3 4 5; do
    log="$RUN_ROOT/run-$run.log"
    start=$(date +%s)
    printf '[%s] run %d/5 start log=%s\n' "$(date '+%F %T')" "$run" "$log"

    timeout --signal=TERM --kill-after=20s 28m \
        "$QEMU" \
        -machine virt \
        -kernel "$KERNEL" \
        -m 4G \
        -smp 8 \
        -nographic \
        -drive "file=$IMAGE,if=none,format=raw,id=x0,file.locking=off" \
        -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
        -no-reboot \
        -device virtio-net-device,netdev=net0 \
        -netdev user,id=net0 \
        -rtc base=utc \
        -fw_cfg name=opt/tx.cmdline,string=tx.profile=final \
        >"$log" 2>&1
    rc=$?

    end=$(date +%s)
    elapsed=$((end - start))
    cagent_pass=$(grep -c 'testcase cagent .* pass ' "$log" 2>/dev/null || true)
    cagent_reject=$(grep -c 'testcase cagent .* reject ' "$log" 2>/dev/null || true)
    if grep -q 'BUILDSTORM_COMPILE .*ok=true' "$log"; then
        buildstorm_ok=yes
    else
        buildstorm_ok=no
    fi
    printf '%d\t%d\t%d\t%d\t%d\t%s\t%s\n' \
        "$run" "$rc" "$elapsed" "$cagent_pass" "$cagent_reject" "$buildstorm_ok" "$log" \
        >>"$SUMMARY"
    printf '[%s] run %d/5 end rc=%d elapsed=%ds cagent=%d/%d buildstorm=%s\n' \
        "$(date '+%F %T')" "$run" "$rc" "$elapsed" \
        "$cagent_pass" "$((cagent_pass + cagent_reject))" "$buildstorm_ok"
    sync
done

printf '[%s] all runs finished summary=%s\n' "$(date '+%F %T')" "$SUMMARY"
