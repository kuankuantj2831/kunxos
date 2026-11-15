#!/usr/bin/env python3
"""KunxOS 开发者工具链 QEMU 验证驱动。

用法: python3 tests/scripts/devtool_verify.py [--timeout N] [--port P]
通过 QEMU 串口 TCP 交互，验证 shell 工具 / crash / perf / log。
"""
import argparse
import socket
import sys
import time

PORT = 1234
BUF = []

def connect(port, tries=60):
    for i in range(tries):
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=2)
            return s
        except OSError:
            time.sleep(0.5)
    raise SystemExit("cannot connect to QEMU serial on port %d" % port)

def drain(s, timeout=1.0):
    """读取直到静默 timeout 秒，返回累计文本。"""
    s.settimeout(timeout)
    out = b""
    try:
        while True:
            chunk = s.recv(4096)
            if not chunk:
                break
            out += chunk
    except socket.timeout:
        pass
    return out

def send(s, cmd, wait=0.8):
    s.sendall(cmd.encode() + b"\n")
    time.sleep(wait)
    return drain(s, 1.0).decode("utf-8", "replace")

def check(name, out, *needles):
    ok = all(n in out for n in needles)
    print(("[PASS] " if ok else "[FAIL] ") + name + ("  <= " + " | ".join(needles)))
    if not ok:
        print("---- output ----")
        print(out[-3000:])
        print("----------------")
    return ok

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=PORT)
    parser.add_argument("--timeout", type=int, default=30)
    args = parser.parse_args()
    s = connect(args.port, tries=max(1, args.timeout * 2))
    boot = drain(s, 3.0).decode("utf-8", "replace")
    print("=== boot output (tail) ===")
    print(boot[-2000:])
    print("==========================")

    results = []

    # 等待 shell 提示符（最多 40s，覆盖完整启动）
    out = b""
    for _ in range(40):
        out += drain(s, 1.0)
        if b"kunxos$" in out:
            break
    out = out.decode("utf-8", "replace")
    results.append(check("boot to shell", boot + out, "kunxos$"))

    # 1. perf
    out = send(s, "perf stats")
    results.append(check("perf stats", out, "FPS:", "CPU:", "MEM:"))
    out = send(s, "perf on")
    results.append(check("perf on", out, "overlay"))
    out = send(s, "perf off")

    # 2. top
    out = send(s, "top -n 1")
    results.append(check("top -n 1", out, "PID", "TICKS", "idle", "shell"))

    # 3. mount
    out = send(s, "mount")
    results.append(check("mount list", out, "Mounts:", "Block devices:"))

    # 4. ifconfig
    out = send(s, "ifconfig")
    results.append(check("ifconfig", out, "eth0", "inet", "netmask"))

    # 5. log / dmesg
    out = send(s, "log")
    results.append(check("log", out, "<6>", "kernel", "boot"))
    out = send(s, "dmesg")
    results.append(check("dmesg", out, "Hello from Rust kernel"))

    # 6. 注入用户态日志 → log 可见
    out = send(s, "echo '<6>[app] hello-from-test' > /dev/kmsg")
    time.sleep(0.3)
    out = send(s, "log app")
    results.append(check("log inject + filter", out, "hello-from-test"))

    # 7. crash（初始应为空或 None）
    out = send(s, "crash")
    results.append(check("crash list", out, "Crash reports"))

    # 8. crashme segv → tombstone
    out = send(s, "run /bin/crashme segv", wait=2.0)
    results.append(check("crashme segv", out, "SIGSEGV", "tombstone", "killing process"))
    time.sleep(0.5)
    out = send(s, "crash")
    results.append(check("crash list after segv", out, "tombstone_"))
    ts = first_tombstone(out)
    if ts:
        out = send(s, "crash show " + ts)
        results.append(check("crash show", out, "KunxOS tombstone", "registers:", "memory map", "backtrace:"))
    else:
        results.append(("crash show", False))

    # 9. crashme ill → SIGILL
    out = send(s, "run /bin/crashme ill", wait=2.0)
    results.append(check("crashme ill", out, "SIGILL", "tombstone"))

    print("\n==== SUMMARY ====")
    for name, ok in results:
        print(("PASS" if ok else "FAIL") + " | " + name)
    print("%d/%d passed" % (sum(1 for _, ok in results if ok), len(results)))
    s.close()
    return 0 if all(ok for _, ok in results) else 1

def first_tombstone(out):
    import re
    m = re.search(r"tombstone_[^\s]+\.txt", out)
    return m.group(0) if m else ""

if __name__ == "__main__":
    sys.exit(main())
