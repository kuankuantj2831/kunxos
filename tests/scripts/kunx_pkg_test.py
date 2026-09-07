#!/usr/bin/env python3
"""KunxOS kunxpkg 端到端测试：QEMU 启动 + 串口 shell 驱动。

验证：keys / update（签名索引 + 坏源拒绝）/ search / info / review --approve /
install（验签 + 权限 + 沙箱 UID）/ list / perms / remove / install-file / verify。

用法：python3 tests/scripts/kunx_pkg_test.py [kernel.img] [logfile]
"""
import os
import subprocess
import sys
import tempfile
import threading
import time

KERNEL = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else "kunxcore/kernel.img")
LOGFILE = sys.argv[2] if len(sys.argv) > 2 else "/tmp/kx_pkg.log"
WORKDIR = tempfile.mkdtemp(prefix="kxpg_")
PIPE = os.path.join(WORKDIR, "kx")

os.mkfifo(PIPE + ".in")
os.mkfifo(PIPE + ".out")

cmd = [
    "qemu-system-aarch64", "-M", "virt", "-cpu", "cortex-a72", "-m", "512M",
    "-kernel", KERNEL, "-serial", "pipe:" + PIPE,
    "-device", "ramfb", "-no-reboot", "-display", "none",
    "-device", "virtio-rng-device", "-device", "AC97",
    "-append", "shell",
]
logf = open(LOGFILE, "wb")
p = subprocess.Popen(cmd, cwd=WORKDIR, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
log = bytearray()
stop = False

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

def send(s):
    infd = os.open(PIPE + ".in", os.O_WRONLY)
    os.write(infd, s.encode())
    os.close(infd)

def wait_for(needle, timeout=30.0):
    """等待日志中出现 needle（从当前末尾开始累积）。"""
    deadline = time.time() + timeout
    start = len(log)
    while time.time() < deadline:
        if needle.encode() in bytes(log[start:]):
            return True
        time.sleep(0.1)
    return False

results = []

def step(name, command, expect, timeout=180):
    send(command + "\n")
    ok = wait_for(expect, timeout)
    results.append((name, ok))
    print(f"[{'PASS' if ok else 'FAIL'}] {name}", flush=True)
    if not ok:
        print(f"      期望输出含: {expect!r}", flush=True)

# ---- 等待系统完全启动（launcher 是最后拉起的大服务）----
if not wait_for("launcher", 900):
    print("[FAIL] 系统未完成启动")
    p.kill()
    sys.exit(1)
time.sleep(6)  # 等启动期串口噪音结束；shell 空闲于首个提示符等待输入

step("keys: 显示信任锚", "run /bin/pkg keys", "api_level: 1")
step("update: 签名索引 + 3 包", "run /bin/pkg update", "updated 3 packages")
step("update: 坏源被拒绝", "run /bin/pkg update", "source 1 failed")
step("search: calculator", "run /bin/pkg search calculator", "calculator 1.0.0")
step("info: calculator", "run /bin/pkg info calculator", "Version: 1.0.0")
step("review: 验签+权限审查", "run /bin/pkg review /var/lib/pkg/repo/main/files/calculator-1.0.0.kunx", "通过")
step("review --approve", "run /bin/pkg review /var/lib/pkg/repo/main/files/calculator-1.0.0.kunx --approve", "approved calculator v1.0.0")

# 单次安装，检查完整流程的每个阶段输出
send("run /bin/pkg install --yes calculator\n")
ok1 = wait_for("verifying certificate chain... OK", 40)
ok2 = wait_for("verifying package signature... OK", 40)
ok3 = wait_for("sandbox uid=10000", 40)
ok4 = wait_for("installed calculator (uid=10000)", 40)
for name, ok in [("install: 证书链 OK", ok1), ("install: 签名 OK", ok2),
                 ("install: 沙箱 UID=10000", ok3), ("install: 完成", ok4)]:
    results.append((name, ok))
    print(f"[{'PASS' if ok else 'FAIL'}] {name}", flush=True)

step("list: 已安装", "run /bin/pkg list", "calculator 1.0.0 uid=10000")
step("perms: 授权记录", "run /bin/pkg perms calculator", "INTERNET: granted")
step("install-file: notes (uid=10001)", "run /bin/pkg install-file /var/lib/pkg/repo/main/files/notes-0.9.0.kunx --yes", "installed notes (uid=10001)")
step("verify: 签名有效", "run /bin/pkg verify /var/lib/pkg/repo/main/files/terminal-1.1.0.kunx", "signature: OK")
step("sources: 显示源", "run /bin/pkg sources", "file:///var/lib/pkg/repo/main")
step("remove: 卸载 calculator", "run /bin/pkg remove calculator", "removed calculator")
step("remove --keep-data", "run /bin/pkg remove --keep-data notes", "keeping user data in /data/app/notes")
step("rollback: 无备份提示", "run /bin/pkg rollback terminal", "no backup available")
step("install: 再装 calculator", "run /bin/pkg install --yes calculator", "installed calculator (uid=10002)")

send("poweroff\n")
time.sleep(1)
p.terminate()
try:
    p.wait(timeout=5)
except subprocess.TimeoutExpired:
    p.kill()
stop = True
t.join(timeout=2)

passed = sum(1 for _, ok in results if ok)
print(f"\n===== {passed}/{len(results)} 通过 =====", flush=True)
logf.close()
sys.exit(0 if passed == len(results) else 1)
