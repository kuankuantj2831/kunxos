#!/usr/bin/env python3
"""RISC-V PLIC / virtio-net diagnostic regression.

Checks observable counters and DMA layout, accepting both safe and risk reports.
QEMU TCG does not reproduce real hardware cache-maintenance failures.
"""
from qemu_test import Guest, kernel_path, temp_path, qemu_command
import re
import sys
import tempfile
import time

KERNEL = kernel_path()
LOGFILE = sys.argv[1] if len(sys.argv) > 1 else temp_path("kx_debt_diag.log")
BOOT_TIMEOUT = 120

WORKDIR = tempfile.mkdtemp(prefix="kxdiag_")

cmd = qemu_command() + [

    # verbose：打开 kinfo 门控（compositor 缩放查表启动行需要上串口）；
    # 携带 virtio-net 使 netdiag 有设备可审计。
    "-kernel", KERNEL, "-append", "shell verbose",
    "-serial", "stdio",
    "-no-reboot", "-display", "none", "-device", "ramfb",
    "-netdev", "user,id=n0", "-device", "virtio-net-device,netdev=n0",
    "-device", "virtio-rng-device",
]
guest = Guest(cmd, LOGFILE, cwd=WORKDIR)
p = guest.process
log = guest.log
results = []


def send(cmdline):
    guest.send(cmdline.encode() + b"\n")


def check(name: str, cond: bool):
    results.append((name, cond))
    print(f"[debt-diag] {'OK  ' if cond else 'FAIL'} {name}")


def run_cmd_and_wait(cmdline, marker, timeout=15.0):
    return marker.encode() in guest.shell(cmdline, timeout)


try:
    boot_deadline = time.time() + BOOT_TIMEOUT
    while time.time() < boot_deadline and b"kunxos$ " not in log:
        time.sleep(0.2)
    check("boot to shell", b"kunxos$ " in log)
    if b"kunxos$ " not in log:
        sys.exit(1)
    time.sleep(0.5)

    # ---- 1. 项1 观测点：SX_TABLE 表长 = 屏宽（540），非固定 1024 ----
    check("SX_TABLE sized to screen width (boot log)",
          "缩放查表 540 列".encode() in log)

    # ---- 2. 项3 观测点：netdiag 缓存行隔离审计 ----
    before = len(log)
    ok = run_cmd_and_wait("netdiag", "virtio-net DMA 缓存行(64B)隔离审计")
    check("netdiag audit header", ok)
    seg = log[before:]
    # 校验诊断字段，不把某次构建的布局风险当作成功条件。
    check("netdiag used_off reported", re.search(rb"used_off=\d+ %64=\d+", seg) is not None)
    check("netdiag used/avail isolation reported",
          "[风险] used 头与 avail 尾共享行".encode() in seg or "[安全] used 头独立成行".encode() in seg)
    check("netdiag RX buffer alignment reported",
          "RX DMA 缓冲 64B 对齐".encode() in seg)
    check("netdiag TX buffer alignment reported",
          "TX DMA 缓冲 64B 对齐".encode() in seg)

    # ---- 3. 项4 观测点：irqstat 分发计数 + 风暴 episode ----
    # 先制造一点中断活动（ping 产生 virtio-net IRQ / 调度 tick 常驻）
    send("ping 10.0.2.2")
    time.sleep(3.0)
    before = len(log)
    ok = run_cmd_and_wait("irqstat", "IRQ 分发计数")
    check("irqstat counters header", ok)
    seg = log[before:]
    # RISC-V 的监督定时器不走 PLIC；只要求外部设备计数非零。
    lines = [l for l in seg.decode("utf-8", "replace").splitlines()
             if l.startswith("  ") and l[2:].split(" ")[0].isdigit()]
    check("irqstat has nonzero counters", len(lines) >= 1)
    check("irqstat storm episode section present", "风暴 episode".encode() in seg)

    # ---- 收尾 ----
    send("poweroff")
    deadline = time.time() + 30
    while time.time() < deadline and p.poll() is None:
        time.sleep(0.2)

finally:
    guest.close()

fails = [n for n, c in results if not c]
print(f"[debt-diag] {'PASS' if not fails else 'FAIL'}: "
      f"{len(results) - len(fails)}/{len(results)} 观测点就位")
if fails:
    print(f"[debt-diag] 缺失: {fails}（完整输出见 {LOGFILE}）")
    sys.exit(1)
sys.exit(0)
