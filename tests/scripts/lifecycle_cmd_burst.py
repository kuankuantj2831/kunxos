#!/usr/bin/env python3
"""lifd 命令信箱协议回归：背靠背连发不丢命令。

round-004 实测旧协议（单文件 `echo > cmd_<pid>` 覆盖式）在 lifd 50ms 轮询
窗口内连发会静默丢失前面的命令（A2 被 A3 覆盖 → 上限/到期/挂起连锁假失败）。
生产写方已全部迁移 `cmd_<pid>_<seq>` 每消息一文件 + tmp→mv 原子发布
（shared/lifecycle.rs / launcher app_life.rs / pushd）。本脚本以同一形态
（shell echo + mv）背靠背连发 3 条 BTASK，断言 lifd 全部处理：
2 条「已授予」+ 1 条「被拒: 单应用数量已达上限」（每应用上限 2）。

用法：python3 tests/scripts/lifecycle_cmd_burst.py [logfile]
退出码：0 = 通过，1 = 失败。
"""
from qemu_test import Guest, kernel_path, temp_path, qemu_command
import sys
import tempfile
import time

KERNEL = kernel_path()
LOGFILE = sys.argv[1] if len(sys.argv) > 1 else temp_path("kx_cmd_burst.log")

WORKDIR = tempfile.mkdtemp(prefix="kxbs_")

cmd = qemu_command() + [

    "-kernel", KERNEL, "-append", "shell",
    "-serial", "stdio",
    "-no-reboot", "-display", "none", "-device", "ramfb",
    "-netdev", "user,id=n0", "-device", "virtio-net-device,netdev=n0",
    "-device", "virtio-rng-device",
    "-audiodev", "none,id=aud0", "-device", "virtio-sound-device,audiodev=aud0",
]
guest = Guest(cmd, LOGFILE, cwd=WORKDIR)
p = guest.process
log = guest.log


def send(line):
    guest.send(line.encode() + b"\n")


def wait_for(marker, timeout=90.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if marker.encode() in log:
            return True
        if p.poll() is not None:
            return False
        time.sleep(0.05)
    return False


def ps_pid(name):
    before = len(log)
    send("ps")
    time.sleep(1.0)
    for line in bytes(log[before:]).split(b"\n"):
        parts = line.split()
        if len(parts) >= 3 and parts[2] == name.encode():
            return parts[0].decode()
    return None


ok = True
try:
    if not wait_for("kunxos$ "):
        print("[burst] FAIL boot to shell")
        sys.exit(1)
    if not wait_for("初始内存压力=", 90.0):
        print("[burst] FAIL lifd ready")
        sys.exit(1)
    pid = ps_pid("audiod") or ps_pid("sensord")
    if not pid:
        print("[burst] FAIL no daemon pid")
        sys.exit(1)
    # 登记 appA（FG）
    send(f"echo {pid} FG > /data/life/reg_appA")
    if not wait_for(f"appA pid={pid} FG", 10.0):
        print("[burst] FAIL register")
        sys.exit(1)
    # ★ 背靠背三连发（与 round-004 丢命令同款节奏：不等任何回执）。
    # seq 协议下目标名互不相同 + mv 原子发布，lifd 应全部消费。
    for seq, (payload, _) in enumerate(
        [("BTASK 1 10000000", "grant1"), ("BTASK 2 10000000", "grant2"),
         ("BTASK 0 10000000", "deny3")], start=1):
        base = f"/data/life/cmd_{pid}_{seq}"
        send(f"echo {payload} > {base}.tmp")
        send(f"mv {base}.tmp {base}")
    time.sleep(8)  # lifd 50ms 轮询 ×3 消费 + 日志落串口
    body = bytes(log)
    grants = body.count("[lifd] 后台任务已授予".encode())
    denies = body.count("[lifd] 后台任务被拒: 单应用数量已达上限".encode())
    print(f"[burst] grants={grants} (期望2)  per-app-denies={denies} (期望1)", flush=True)
    if grants != 2 or denies != 1:
        ok = False
finally:
    guest.close()

print("[burst] PASS" if ok else "[burst] FAIL (see %s)" % LOGFILE, flush=True)
sys.exit(0 if ok else 1)
