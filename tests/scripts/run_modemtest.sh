#!/bin/bash
# KunxOS 电话/短信子系统 QEMU 自动化测试
# 用法: tests/scripts/run_modemtest.sh [timeout_s]
set -u
cd "$(dirname "$0")/../.."
TIMEOUT="${1:-240}"

echo "== build rild + modemtest =="
make rild modemtest || exit 1

echo "== build ramdisk with modemtest.flag =="
touch rootfs/data/modemtest.flag
python3 tools/mkfs_sfs.py rootfs kunxcore/ramdisk.img 4096 || exit 1
# 强制内核重编（include_bytes! 跟踪 ramdisk.img，保险起见 touch 源）
touch kunxcore/src/main.rs
cargo build --release --target aarch64-unknown-none --manifest-path kunxcore/Cargo.toml --features qemu || exit 1
llvm-objcopy -O binary kunxcore/target/aarch64-unknown-none/release/rust-kernel kunxcore/kernel.img || exit 1
rm -f rootfs/data/modemtest.flag

LOG=/tmp/kunxos-modemtest.log
rm -f "$LOG"
echo "== running QEMU (headless, timeout ${TIMEOUT}s) =="
timeout "$TIMEOUT" qemu-system-aarch64 -M virt -cpu cortex-a72 -m 512M \
    -kernel kunxcore/kernel.img -nographic -no-reboot \
    -device virtio-rng-device > "$LOG" 2>&1

echo "== results =="
grep -E "\[MODEMTEST\]" "$LOG" || true
if grep -q "\[MODEMTEST\] summary: PASS" "$LOG"; then
    echo "MODEM TEST: PASS"
    exit 0
else
    echo "MODEM TEST: FAIL (see $LOG)"
    exit 1
fi
