#!/usr/bin/env python3
"""浏览器 TLS 栈来宾侧自测：QEMU 启动 → 内核 shell → run /bin/tlstest。

用法：python3 tests/scripts/tlstest.py [host] [logfile]
断言串口出现 "[tls-selftest] PASS"。
"""
from qemu_test import Guest, kernel_path, temp_path, qemu_command
import sys, time, os, tempfile

HOST = sys.argv[1] if len(sys.argv) > 1 else "www.baidu.com"
LOGFILE = sys.argv[2] if len(sys.argv) > 2 else temp_path("kx_tls.log")
KERNEL = kernel_path()
DISK = os.environ.get("DISK")
TOP = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
os.chdir(TOP)

WORKDIR = tempfile.mkdtemp(prefix="kxt_")

cmd = qemu_command() + [
       "-kernel", KERNEL,
       "-serial", "stdio", "-no-reboot", "-display", "none", "-device", "ramfb",
       "-netdev", "user,id=n0", "-device", "virtio-net-device,netdev=n0",
       "-device", "virtio-rng-device",
       "-append", "shell"]

if DISK:
    cmd += ["-drive", f"file={os.path.abspath(DISK)},format=raw,if=none,id=disk0,snapshot=on",
            "-device", "virtio-blk-device,drive=disk0"]
guest = Guest(cmd, LOGFILE, cwd=WORKDIR)
p = guest.process
log = guest.log


def send(s):
    guest.send(s.encode())


def wait_for(needle, timeout, from_idx=0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        text = log.decode("utf-8", "replace")
        idx = text.find(needle, from_idx)
        if idx >= 0:
            return idx
        if p.poll() is not None:
            break
        time.sleep(0.5)
    return -1


rc = 1
try:
    boot = wait_for("kunxos$", 180)
    print(f"[driver] shell 就绪 ({boot >= 0})", flush=True)
    if boot < 0:
        raise RuntimeError("RISC-V guest shell did not boot")
    time.sleep(2)
    send(f"run /bin/tlstest {HOST}\n")
    res = wait_for("[tls-selftest]", 420, boot)
    if res < 0:
        raise RuntimeError("No TLS selftest result; inspect " + LOGFILE)
    deadline = time.time() + 60
    while time.time() < deadline:
        text = log.decode("utf-8", "replace")
        if "[tls-selftest] PASS" in text[res:]:
            rc = 0
            break
        if "[tls-selftest] FAIL" in text[res:] or "PANIC" in text[res:]:
            rc = 1
            break
        time.sleep(0.5)
finally:
    guest.close()

print("TLS GUEST SELFTEST:", "PASS" if rc == 0 else "FAIL")
sys.exit(rc)
