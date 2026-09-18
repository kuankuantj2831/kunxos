#!/usr/bin/env python3
"""KunxOS kunxpkg 端到端测试：QEMU 启动 + 串口 shell 驱动。

验证：keys / update（签名索引 + 坏源拒绝）/ search / info / review --approve /
install（验签 + 权限 + 沙箱 UID）/ list / perms / remove / install-file / verify，
以及依赖治理：
  - 递归依赖：install depdemo 自动按序安装 libbase → libgfx → depdemo；
  - 目标自检：run /bin/depdemo keys（多级依赖装完后目标应用可运行）；
  - 依赖环：cyc-a ↔ cyc-b 报「检测到依赖环」，不无限递归；
  - 缺失依赖：missingdep 依赖的 ghost-lib 不在软件源 → 明确报错；
  - 签名失败：badsig 包签名被篡改 → 拒绝安装。

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
    # kvm:tcg —— 仓库惯例加速（Makefile QEMU_ACCEL 同款）：Ed25519 验签在
    # 纯 TCG 下需数分钟/包，依赖链安装会超时。
    "qemu-system-aarch64", "-M", "virt,accel=kvm:tcg",
    "-cpu", "cortex-a72", "-m", "512M",
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

def record(name, ok, expect=None):
    results.append((name, ok))
    print(f"[{'PASS' if ok else 'FAIL'}] {name}", flush=True)
    if not ok and expect:
        print(f"      期望输出含: {expect!r}", flush=True)

def step(name, command, expect, timeout=600):
    send(command + "\n")
    ok = wait_for(expect, timeout)
    record(name, ok, expect)

# ---- 等待系统完全启动（launcher 是最后拉起的大服务）----
if not wait_for("launcher", 900):
    print("[FAIL] 系统未完成启动")
    p.kill()
    sys.exit(1)
time.sleep(6)  # 等启动期串口噪音结束；shell 空闲于首个提示符等待输入

step("keys: 显示信任锚", "run /bin/pkg keys", "api_level: 1")
step("update: 签名索引 + 10 包", "run /bin/pkg update", "已更新 10 个软件包")
step("update: 坏源被拒绝", "run /bin/pkg update", "软件源 1 获取失败")
step("search: calculator", "run /bin/pkg search calculator", "calculator 1.0.0")
step("info: calculator", "run /bin/pkg info calculator", "Version: 1.0.0")
step("review: 验签+权限审查", "run /bin/pkg review /var/lib/pkg/repo/main/files/calculator-1.0.0.kunx", "通过")
step("review --approve", "run /bin/pkg review /var/lib/pkg/repo/main/files/calculator-1.0.0.kunx --approve", "已批准 calculator v1.0.0")

# 单次安装，检查完整流程的每个阶段输出
send("run /bin/pkg install calculator --yes\n")
ok1 = wait_for("正在验证证书链... OK", 600)
ok2 = wait_for("正在验证软件包签名... OK", 600)
ok3 = wait_for("沙箱 uid=10000", 600)
ok4 = wait_for("已安装 calculator (uid=10000)", 600)
for name, ok, exp in [
    ("install: 证书链 OK", ok1, "正在验证证书链... OK"),
    ("install: 签名 OK", ok2, "正在验证软件包签名... OK"),
    ("install: 沙箱 UID=10000", ok3, "沙箱 uid=10000"),
    ("install: 完成", ok4, "已安装 calculator (uid=10000)"),
]:
    record(name, ok, exp)

step("list: 已安装", "run /bin/pkg list", "calculator 1.0.0 uid=10000")
step("perms: 授权记录", "run /bin/pkg perms calculator", "INTERNET: granted")
step("install-file: notes (uid=10001)", "run /bin/pkg install-file /var/lib/pkg/repo/main/files/notes-0.9.0.kunx --yes", "已安装 notes (uid=10001)")
step("verify: 签名有效", "run /bin/pkg verify /var/lib/pkg/repo/main/files/terminal-1.1.0.kunx", "签名: OK")
step("sources: 显示源", "run /bin/pkg sources", "file:///var/lib/pkg/repo/main")

# ---- 递归依赖：depdemo → libgfx → libbase（多级 + 菱形），依赖在前安装 ----
send("run /bin/pkg install depdemo --yes\n")
ok = wait_for("安装计划: 3 个软件包", 600)
ok = ok and wait_for("已安装 libbase (uid=10002)", 600)
ok = ok and wait_for("已安装 libgfx (uid=10003)", 600)
ok = ok and wait_for("已安装 depdemo (uid=10004)", 600)
record("递归依赖: 安装计划 3 包", ok, "安装计划: 3 个软件包 … libbase/libgfx/depdemo 依次安装")

step("递归依赖: 目标应用自检", "run /bin/depdemo keys", "api_level: 1")
step("环检测: cyc-a <-> cyc-b", "run /bin/pkg install cyc-a", "检测到依赖环: cyc-a -> cyc-b -> cyc-a")
step("缺失依赖: ghost-lib", "run /bin/pkg install missingdep", "ghost-lib 无法满足依赖")

# 签名失败：badsig 证书链有效但签名被篡改 → 拒绝且不落盘
send("run /bin/pkg install badsig\n")
start = len(log)
ok = wait_for("正在验证软件包签名... FAILED", 600)
time.sleep(2)  # 等失败路径输出完整
not_installed = "已安装 badsig".encode() not in bytes(log[start:])
record("签名失败: badsig 拒绝", ok, "正在验证软件包签名... FAILED")
record("签名失败: badsig 未落盘", ok and not_installed, "输出中不得出现 已安装 badsig")

step("remove: 卸载 calculator", "run /bin/pkg remove calculator", "已卸载 calculator")
step("remove --keep-data", "run /bin/pkg remove --keep-data notes", "用户数据保留在 /data/app/notes")
step("rollback: 未安装提示", "run /bin/pkg rollback terminal", "未安装: terminal")
step("install: 再装 calculator", "run /bin/pkg install calculator --yes", "已安装 calculator (uid=10005)")

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
