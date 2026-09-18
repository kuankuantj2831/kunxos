#!/usr/bin/env python3
"""KunXOS 性能/稳定性技术债诊断（DMA 缓存行隔离 + 中断风暴 act0）。

技术债「DMA 64B 缓存行隔离（Pi4/KVM）」与「中断风暴 act0 待复现」在
QEMU TCG（x86 宿主无缓存仿真 / GIC SGI 锁存行为差异）下无法稳定复现
故障本身；本脚本采集的是**确定性观测点**（任何平台可跑）：

  1. 启动行 `[compositor] 缩放查表 N 列（=屏宽）`
     —— SX_TABLE 上限修复后表长跟随分辨率（旧实现固定 1024）；
  2. `netdiag` —— virtio-net 环布局 / DMA 缓冲的 64B 行共享事实：
     used_off % 64 != 0 ⟹ avail 尾与 used 头共享行（clean(avail) 回写
     可覆盖设备写入的 used ring，仅真机/KVM 有真实缓存时成灾）；
     RX 缓冲未 64B 对齐 ⟹ invalidate 可丢弃相邻所有者脏数据；
  3. `irqstat` —— 每 IRQ 分发计数 + 风暴 episode 历史（含 act0 置位位
     解码）。正常运行 episode 为空；一旦复现风暴，事后回溯不必盯串口。

用法：python3 tests/scripts/debt_diag.py [logfile]
退出码：0 = 观测点全部就位（诊断基础设施可用）；1 = 有缺失。
注意：本脚本**断言观测点存在**，不断言风险标记消失——风险标记是当前
债务的现状基线，修复后重跑应看到 [风险] → [安全]。
"""
import os
import subprocess
import sys
import tempfile
import threading
import time

KERNEL = os.path.abspath(os.environ.get("KERNEL", "kunxcore/kernel.img"))
LOGFILE = sys.argv[1] if len(sys.argv) > 1 else "/tmp/kx_debt_diag.log"
BOOT_TIMEOUT = 120

WORKDIR = tempfile.mkdtemp(prefix="kxdiag_")
PIPE = os.path.join(WORKDIR, "kx")
os.mkfifo(PIPE + ".in")
os.mkfifo(PIPE + ".out")

cmd = [
    "qemu-system-aarch64", "-M", "virt", "-cpu", "cortex-a72", "-m", "512M",
    # verbose：打开 kinfo 门控（compositor 缩放查表启动行需要上串口）；
    # 携带 virtio-net 使 netdiag 有设备可审计。
    "-kernel", KERNEL, "-append", "shell verbose",
    "-serial", "pipe:" + PIPE,
    "-no-reboot", "-display", "none", "-device", "ramfb",
    "-netdev", "user,id=n0", "-device", "virtio-net-device,netdev=n0",
    "-device", "virtio-rng-device",
]
logf = open(LOGFILE, "wb")
p = subprocess.Popen(cmd, cwd=WORKDIR, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
log = bytearray()
stop = False
results = []


def reader():
    try:
        outfd = os.open(PIPE + ".out", os.O_RDONLY)
    except OSError:
        return
    while not stop:
        try:
            data = os.read(outfd, 4096)
        except OSError:
            break
        if not data:
            time.sleep(0.02)
            continue
        log.extend(data)
        logf.write(data)
        logf.flush()


t = threading.Thread(target=reader, daemon=True)
t.start()


def send(cmdline: str):
    infd = os.open(PIPE + ".in", os.O_WRONLY)
    os.write(infd, cmdline.encode() + b"\n")
    os.close(infd)


def check(name: str, cond: bool):
    results.append((name, cond))
    print(f"[debt-diag] {'OK  ' if cond else 'FAIL'} {name}")


def run_cmd_and_wait(cmdline: str, marker: str, timeout: float = 15.0):
    """发命令并等待其输出特征；返回该命令输出是否出现。"""
    before = len(log)
    send(cmdline)
    deadline = time.time() + timeout
    while time.time() < deadline:
        if marker.encode() in log[before:]:
            return True
        time.sleep(0.05)
    return False


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
    # 当前布局的事实基线（USED_OFF=1156，非 64 对齐）：债务存在性证明
    check("netdiag used_off reported", b"used_off=1156" in seg)
    check("netdiag used/avail shared-line risk flagged",
          "[风险] used 头与 avail 尾共享行".encode() in seg)
    check("netdiag RX buffer alignment reported",
          "RX DMA 缓冲 64B 对齐".encode() in seg)
    check("netdiag TX buffer alignment reported",
          "TX DMA 缓冲 64B 对齐".encode() in seg)

    # ---- 3. 项4 观测点：irqstat 分发计数 + 风暴 episode ----
    # 先制造一点中断活动（ping 产生 virtio-net IRQ / 调度 tick 常驻）
    send("ping -c 1 10.0.2.2")
    time.sleep(3.0)
    before = len(log)
    ok = run_cmd_and_wait("irqstat", "IRQ 分发计数")
    check("irqstat counters header", ok)
    seg = log[before:]
    # 至少有两个中断源有非零计数（CNTV 调度 tick=27、virtio-net 等）
    lines = [l for l in seg.decode("utf-8", "replace").splitlines()
             if l.startswith("  ") and l[2:].split(" ")[0].isdigit()]
    check("irqstat has nonzero counters", len(lines) >= 2)
    check("irqstat storm episode section present", "风暴 episode".encode() in seg)

    # ---- 收尾 ----
    send("poweroff")
    deadline = time.time() + 30
    while time.time() < deadline and p.poll() is None:
        time.sleep(0.2)

finally:
    stop = True
    try:
        p.terminate()
    except OSError:
        pass
    try:
        p.wait(timeout=5)
    except subprocess.TimeoutExpired:
        p.kill()
    logf.close()

fails = [n for n, c in results if not c]
print(f"[debt-diag] {'PASS' if not fails else 'FAIL'}: "
      f"{len(results) - len(fails)}/{len(results)} 观测点就位")
if fails:
    print(f"[debt-diag] 缺失: {fails}（完整输出见 {LOGFILE}）")
    sys.exit(1)
sys.exit(0)
