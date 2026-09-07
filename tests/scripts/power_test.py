#!/usr/bin/env python3
"""KunxOS 电源管理回归测试（QEMU）。

启动内核 → shell 就绪后依次执行：
  1. `power selftest`                    电源逻辑自检（纯函数，0 失败）
  2. `power status`                      状态展示字段齐全
  3. `power thermal force 95` + status   热注入 → 级别 critical/emergency、频率被压
  4. `power thermal clear` + status      清除注入 → 恢复
  5. `power governor powersave` + status 调速器切换 → 频率 600MHz
  6. `power battery 10` + status         低电量注入 → LOW 告警
  7. `power plug 1` + status             充电状态机 → charging
  8. `cat /proc/power`                   可读 procfs
  9. 检查 UART 日志含 `[power] dvfs`（切换真实发生）

用法：python3 tests/scripts/power_test.py [logfile]
退出码：0 = 全部通过，1 = 有失败。
"""
import os
import subprocess
import sys
import tempfile
import threading
import time

KERNEL = os.path.abspath(os.environ.get("KERNEL", "kunxcore/kernel.img"))
LOGFILE = sys.argv[1] if len(sys.argv) > 1 else "/tmp/kx_power.log"
BOOT_TIMEOUT = 120  # GUI（OOBE/launcher）+ shell 并起，启动较慢

WORKDIR = tempfile.mkdtemp(prefix="kxpow_")
PIPE = os.path.join(WORKDIR, "kx")
os.mkfifo(PIPE + ".in")
os.mkfifo(PIPE + ".out")

cmd = [
    "qemu-system-aarch64", "-M", "virt", "-cpu", "cortex-a72", "-m", "512M",
    "-kernel", KERNEL, "-append", "shell",
    "-serial", "pipe:" + PIPE,
    "-no-reboot", "-display", "none",
    "-netdev", "user,id=n0", "-device", "virtio-net-device,netdev=n0",
    "-device", "virtio-rng-device", "-device", "AC97",
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
    print(f"[power-test] {'OK  ' if cond else 'FAIL'} {name}")


def wait_for(marker: str, timeout: float = BOOT_TIMEOUT):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if marker.encode() in log:
            return True
        time.sleep(0.05)
    return False


def run_cmd_and_wait(cmdline: str, marker: str, timeout: float = 8.0):
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
    if not wait_for("kunxos$ "):
        check("boot to shell", False)
        sys.exit(1)
    check("boot to shell", True)
    time.sleep(0.5)

    # 1. 自检：boot 期与 shell 期均应 0 failure
    check("selftest runs", run_cmd_and_wait("power selftest", "处失败"))
    check("selftest zero fails (boot)", b"[kernel] \xe7\x94\xb5\xe6\xba\x90\xe8\x87\xaa\xe6\xa3\x80\xe5\xae\x8c\xe6\x88\x90\xef\xbc\x9a0 \xe4\xb8\xaa\xe5\xa4\xb1\xe8\xb4\xa5" in log)
    check("selftest zero fails (shell)", b"[power] \xe8\x87\xaa\xe6\xa3\x80\xe5\xae\x8c\xe6\x88\x90\xef\xbc\x9a0 \xe4\xb8\xaa\xe5\xa4\xb1\xe8\xb4\xa5" in log)

    # 2. 状态字段
    ok = run_cmd_and_wait("power status", "KunxOS power manager", 8.0)
    check("status header", ok)
    for field in [b"governor", b"load", b"freq", b"thermal", b"battery", b"idle", b"events"]:
        check(f"status field {field.decode()}", field in log)

    # 3. 热注入 → 节流（检查 force 命令之后的新日志段）
    before = len(log)
    run_cmd_and_wait("power thermal force 95", "", 2.0)
    time.sleep(1.2)  # 等 200ms 热采样生效
    run_cmd_and_wait("power status", "KunxOS power manager", 8.0)
    seg = log[before:]
    check("thermal forced hot level", b"level=emergency" in seg or b"level=critical" in seg)
    check("thermal forced freq drop", b"freq        : 600 MHz" in seg)
    # 日志应出现节流降频
    check("dvfs switch logged", b"[power] dvfs" in seg)

    # 4. 清除注入
    before = len(log)
    run_cmd_and_wait("power thermal clear", "", 2.0)
    time.sleep(1.2)
    run_cmd_and_wait("power status", "KunxOS power manager", 8.0)
    seg = log[before:]
    check("thermal clear recovers", b"level=normal" in seg or b"level=warm" in seg)

    # 5. governor powersave → 600MHz
    run_cmd_and_wait("power governor powersave", "", 2.0)
    time.sleep(0.5)
    run_cmd_and_wait("power status", "KunxOS power manager", 8.0)
    check("powersave freq 600", b"freq        : 600 MHz" in log)

    # 6. 低电量
    run_cmd_and_wait("power battery 10", "", 2.0)
    time.sleep(0.5)
    run_cmd_and_wait("power status", "KunxOS power manager", 8.0)
    check("battery low shows", b"LOW" in log or b"battery-low" in log)

    # 7. 充电状态机
    run_cmd_and_wait("power plug 1", "", 2.0)
    time.sleep(1.2)  # 等 1s 电池采样
    run_cmd_and_wait("power status", "KunxOS power manager", 8.0)
    check("charging state", b"charging" in log)
    check("charging event", b"charging-start" in log)

    # 8. procfs
    run_cmd_and_wait("cat /proc/power", "KunxOS power manager", 8.0)
    check("procfs power readable", b"KunxOS power manager" in log)

finally:
    send("poweroff")
    time.sleep(0.5)
    stop = True
    try:
        p.wait(timeout=5)
    except Exception:
        p.kill()
    logf.close()

fails = [n for n, ok in results if not ok]
print(f"\n[power-test] {len(results) - len(fails)}/{len(results)} passed")
sys.exit(1 if fails else 0)
