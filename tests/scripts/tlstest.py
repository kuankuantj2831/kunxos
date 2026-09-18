#!/usr/bin/env python3
"""浏览器 TLS 栈来宾侧自测：QEMU 启动 → 内核 shell → run /bin/tlstest。

用法：python3 tests/scripts/tlstest.py [host] [logfile]
断言串口出现 "[tls-selftest] PASS"。
"""
import subprocess, sys, time, os, tempfile, threading

HOST = sys.argv[1] if len(sys.argv) > 1 else "www.baidu.com"
LOGFILE = sys.argv[2] if len(sys.argv) > 2 else "/tmp/kx_tls.log"
KERNEL = os.path.abspath(os.environ.get("KERNEL", "kunxcore/kernel.img"))
DISK = os.path.abspath(os.environ.get("DISK", "disk.img"))
TOP = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
os.chdir(TOP)

WORKDIR = tempfile.mkdtemp(prefix="kxt_")
PIPE = os.path.join(WORKDIR, "kx")
os.mkfifo(PIPE + ".in")
os.mkfifo(PIPE + ".out")

cmd = ["qemu-system-aarch64", "-M", "virt", "-cpu", "cortex-a72", "-m", "512M",
       "-kernel", KERNEL,
       "-drive", f"file={DISK},format=raw,if=none,id=disk0",
       "-device", "virtio-blk-device,drive=disk0",
       "-serial", "pipe:" + PIPE, "-no-reboot", "-display", "none",
       "-netdev", "user,id=n0", "-device", "virtio-net-device,netdev=n0",
       "-device", "virtio-rng-device",
       "-append", "shell"]

logf = open(LOGFILE, "wb")
p = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
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


threading.Thread(target=reader, daemon=True).start()
infd = None


def send(s):
    global infd
    if infd is None:
        infd = os.open(PIPE + ".in", os.O_WRONLY)
    os.write(infd, s.encode())


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
    time.sleep(2)
    send(f"run /bin/tlstest {HOST}\n")
    res = wait_for("[tls-selftest]", 420, boot)
    if res < 0:
        print("[driver] 未看到 [tls-selftest] 输出")
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
    stop = True
    time.sleep(0.3)
    try:
        p.terminate()
        p.wait(5)
    except Exception:
        p.kill()
    logf.close()

print("TLS GUEST SELFTEST:", "PASS" if rc == 0 else "FAIL")
sys.exit(rc)
