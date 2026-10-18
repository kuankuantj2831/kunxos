#!/usr/bin/env python3
"""KunxOS 生命周期回归测试（QEMU，无头模式，串口 shell 驱动）。

启动内核（`-append shell`，串口内置 shell 就绪）后按 docs/lifecycle-test.md
的阶段 A 依次验证（全部经 /data/life 文件信箱 + /proc/pressure 注入，确定性）：

  1. lifd/pushd 启动：/data/life 与 /data/push 目录可读
  2. 应用登记：写 reg_<name> 内容 "<pid> FG" → lifd 日志「已登记 ... FG」，
     最近任务文件出现 "name pid R"
  3. 多应用切换（状态机）：
     - 应用 A 后台化（SIGSTOP）→ lifd「已转入后台 appA」、/proc/processes
       中 A=T、recents 中 A 状态 B
     - 应用 B 恢复前台（SIGCONT）→ lifd「已回前台 appB」、/proc/processes
       中 B=R、recents 中 B 状态 R
     - 再切回 A →「已回前台 appA」
  4. 后台任务白名单：
     - BTASK 类型越界 →「后台任务被拒: 未知类型」
     - 每应用上限 2 → 第 3 个「后台任务被拒: 单应用数量已达上限」
     - BTASK 3 400（下载）→「后台任务已授予 type=3」；到期
       「后台任务已到期 pid=」，事件文件 evt_<pid> 含 BG_TASK_EXPIRED
  5. 超时挂起：SET_TIMEOUT 800 → 后台且无任务的应用到期待「已挂起」，
     recents 中状态 S
  6. 内存压力注入与 LMK：
     - `echo force 1 > /proc/pressure` → lifd「内存压力中等」
     - `echo force 2 > /proc/pressure` → lifd「内存压力严重」+「LMK: 正在击杀
       score=」，先杀最低分（Suspended）应用 →「已终止 appA」（Running 永不杀）
     - `echo clear > /proc/pressure` →「内存压力正常」
  7. 推送（本地，经 pushd → lifd）：
     - 对未登记 pid 投递 → lifd「通知已丢弃: 应用未登记 pid=」
     - 对后台应用投递 → lifd「通知已延迟 pid=」，/data/push/pending/<pid>
       落盘 NOTIFY；恢复前台时随 RESUME 补投 → evt_<pid> 含 "RESUME NOTIFY"
  8. 状态持久化：写 /data/state/sample.bin 再 cat 回读，内容一致

（阶段 B 的 GUI 端到端验证见 docs/lifecycle-test.md：经 launcher 真实启动 lcapp
演示应用，验证应用侧 on_pause/on_suspend/on_resume 回调与 Home/Recents 手势。）

用法：python3 tests/scripts/lifecycle_test.py [logfile]
退出码：0 = 全部通过，1 = 有失败。
"""
import os
import re
import subprocess
import sys
import tempfile
import threading
import time

KERNEL = os.path.abspath(os.environ.get("KERNEL", "kunxcore/kernel.img"))
LOGFILE = sys.argv[1] if len(sys.argv) > 1 and not sys.argv[1].startswith("--") else "/tmp/kx_life.log"
BOOT_TIMEOUT = 120

WORKDIR = tempfile.mkdtemp(prefix="kxl_")
PIPE = os.path.join(WORKDIR, "kx")
os.mkfifo(PIPE + ".in")
os.mkfifo(PIPE + ".out")

# RISC-V 移植后（2026-09 架构切换）重定向：QEMU/BIOS 与顶层 Makefile 同源，
# 可用环境变量 QEMU / BIOS 覆盖；AC97（aarch64 遗留）换 virtio-sound。
QEMU = os.environ.get(
    "QEMU", os.path.expanduser("~/qemu-riscv-pkgs/root/usr/bin/qemu-system-riscv64"))
BIOS = os.environ.get(
    "BIOS", os.path.expanduser(
        "~/qemu-riscv-pkgs/root/usr/share/qemu/opensbi-riscv64-generic-fw_dynamic.bin"))

cmd = [
    QEMU, "-M", "virt", "-accel", "tcg", "-cpu", "rv64", "-bios", BIOS, "-m", "512M",
    "-kernel", KERNEL, "-append", "shell",
    "-serial", "pipe:" + PIPE,
    "-no-reboot", "-display", "none",
    "-netdev", "user,id=n0", "-device", "virtio-net-device,netdev=n0",
    "-device", "virtio-rng-device",
    "-audiodev", "none,id=aud0", "-device", "virtio-sound-device,audiodev=aud0",
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
    print(f"[life] {'OK  ' if cond else 'FAIL'} {name}")
    logf.write(f"[check] {'OK' if cond else 'FAIL'} {name}\n".encode())


def wait_for(marker: str, timeout: float = BOOT_TIMEOUT):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if marker.encode() in log:
            return True
        time.sleep(0.05)
    return False


def run_cmd_and_wait(cmdline: str, marker: str, timeout: float = 8.0):
    before = len(log)
    send(cmdline)
    deadline = time.time() + timeout
    while time.time() < deadline:
        if marker.encode() in log[before:]:
            return True
        time.sleep(0.05)
    return False


def ps_pid(names):
    """从 ps 输出解析任一名字的 pid（names 为候选列表），返回第一个命中。"""
    before = len(log)
    send("ps")
    time.sleep(0.4)
    seg = bytes(log[before:])
    for line in seg.split(b"\n"):
        mm = re.match(rb"^(\d+)\s+\d+\s+(\w+)\s*$", line)
        if mm:
            name = mm.group(2).decode()
            if name in names:
                return mm.group(1).decode()
    return None


try:
    if not wait_for("kunxos$ "):
        check("boot to shell", False)
        sys.exit(1)
    check("boot to shell", True)
    time.sleep(0.5)

    # ---- 1. lifd / pushd 启动 ----
    check("lifd ready", wait_for("初始内存压力=", 15.0))
    check("pushd ready", wait_for("[pushd] pid=", 15.0))
    check("life dir", run_cmd_and_wait("ls /data/life", "", 3.0))
    check("push dir", run_cmd_and_wait("ls /data/push", "in", 3.0))

    # ---- 选取常驻 daemon 作为受控测试应用（可 SIGSTOP/SIGCONT 恢复） ----
    # 各选择列表之间互斥，保证受控 pid 两两不同（注册表 find_by_pid 不冲突）。
    pid_a = ps_pid(["audiod", "sensord", "positiond"])
    pid_b = ps_pid(["camerad", "networkd", "rild", "logd"])
    pid_c = ps_pid(["pushd", "logd", "lifd"])
    pid_d = ps_pid(["rild", "networkd", "sensord"])
    check("test pids", bool(pid_a and pid_b and pid_c))

    def register(name, pid, fg):
        # 内容 "<pid> <FG|BG>"（无引号，shell 按空白切词后 echo 拼回）
        return run_cmd_and_wait(f"echo {pid} {fg} > /data/life/reg_{name}", "", 2.0)

    def cmd_to(pid, text):
        return run_cmd_and_wait(f"echo {text} > /data/life/cmd_{pid}", "", 2.0)

    def cat_check(path, marker, timeout=3.0):
        return run_cmd_and_wait(f"cat {path}", marker, timeout)

    def cat_until(path, marker, timeout=8.0):
        # 反复 cat，直到给定标记出现在该次 cat 之后的新输出里
        deadline = time.time() + timeout
        while time.time() < deadline:
            before = len(log)
            send(f"cat {path}")
            time.sleep(0.3)
            if marker.encode() in log[before:]:
                return True
            time.sleep(0.15)
        return False

    # ---- 2. 登记与最近任务 ----
    check("reg A", register("appA", pid_a, "FG"))
    check("registered log A", wait_for(f"appA pid={pid_a} FG", 8.0))
    check("recents A R", cat_check("/data/life/recents", f"appA {pid_a} R", 3.0))
    check("reg B", register("appB", pid_b, "BG"))

    # ---- 3. 多应用切换（状态机）----
    #（2026-10-18 标记刷新：lifd 文案已全面中文化，旧英文标记自架构切换后未跟进而假失败）
    check("BG A", cmd_to(pid_a, "BG"))
    check("lifd background A", wait_for("已转入后台 appA", 8.0))
    check("recents A B-state", cat_check("/data/life/recents", f"appA {pid_a} B", 3.0))
    check("proc stopped A", cat_until("/proc/processes", f"{pid_a}     T", 8.0))
    check("FG B", cmd_to(pid_a, f"FG {pid_b}"))
    check("lifd foreground B", wait_for("已回前台 appB", 8.0))
    check("recents B R-state", cat_check("/data/life/recents", f"appB {pid_b} R", 3.0))
    check("proc running B", cat_until("/proc/processes", f"{pid_b}     R", 8.0))
    check("FG A", cmd_to(pid_a, f"FG {pid_a}"))
    check("lifd foreground A", wait_for("已回前台 appA", 8.0))

    # ---- 4. 后台任务白名单 ----
    # 时长 10s（值按 µs 计）：慢宿主（RISC-V TCG）下命令往返远超 0.6s，短任务
    # 在下一条提交前到期，凑不满每应用上限（与 lifecycle_core_test 同款修复）。
    # 每步等 lifd 的处理回执再发下一条：cmd_<pid> 是单文件信箱，连续 echo 会
    # 覆盖未消费的上一条（旧脚本 A2 被 A3 覆盖丢失 → 上限/到期/挂起连锁假失败）。
    check("BTASK bad type", run_cmd_and_wait(f"echo BTASK 9 100 > /data/life/cmd_{pid_c}", "后台任务被拒: 未知类型", 6.0))
    check("BTASK A1", run_cmd_and_wait(f"echo BTASK 1 10000000 > /data/life/cmd_{pid_a}", "后台任务已授予", 6.0))
    check("BTASK A2", run_cmd_and_wait(f"echo BTASK 2 10000000 > /data/life/cmd_{pid_a}", "后台任务已授予", 6.0))
    check("deny per-app limit", run_cmd_and_wait(f"echo BTASK 0 10000000 > /data/life/cmd_{pid_a}", "后台任务被拒: 单应用数量已达上限", 6.0))
    # 显式结束并等回执释放槽位，否则后续 BTASK 仍被上限拒绝
    check("BTASK_END A", run_cmd_and_wait(f"echo BTASK_END > /data/life/cmd_{pid_a}", "后台任务已结束", 6.0))
    check("BTASK A short", run_cmd_and_wait(f"echo BTASK 3 400 > /data/life/cmd_{pid_a}", "后台任务已授予 type=", 6.0))
    check("expired event", wait_for("后台任务已到期 pid=", 8.0))
    check("evt BG_TASK_EXPIRED", cat_check(f"/data/life/evt_{pid_a}", "BG_TASK_EXPIRED", 3.0))

    # ---- 5. 超时挂起 ----
    # appA 后台化（无任务）→ SET_TIMEOUT 800ms 后转 Suspended（lifd now_us 已
    # 改 CLOCK_MONOTONIC，墙钟回拨不再使超时永久失效）
    check("SET_TIMEOUT", run_cmd_and_wait(f"echo SET_TIMEOUT 800 > /data/life/cmd_{pid_c}", "bg->suspend 超时已设为 800", 6.0))
    time.sleep(0.3)
    check("BG A", cmd_to(pid_a, "BG"))
    check("suspend appA", wait_for("已挂起 appA", 15.0))
    check("recents A S-state", cat_check("/data/life/recents", f"appA {pid_a} S", 3.0))

    # ---- 6. 内存压力注入与 LMK ----
    check("force 1", run_cmd_and_wait("echo force 1 > /proc/pressure", "[mem] 强制压力 1", 6.0))
    check("Moderate response", wait_for("内存压力中等: 挂起空闲后台应用", 8.0))
    check("force 2", run_cmd_and_wait("echo force 2 > /proc/pressure", "[mem] 强制压力 2", 6.0))
    check("Critical response", wait_for("内存压力严重: 正在执行 LMK", 8.0))
    check("LMK killing", wait_for("LMK: 正在击杀 score=", 8.0))
    check("LMK killed appA", wait_for("已终止 appA", 8.0))
    check("clear", run_cmd_and_wait("echo clear > /proc/pressure", "[mem] 已清除强制", 6.0))
    check("Normal response", wait_for("内存压力正常", 8.0))

    # ---- 7. 推送（本地，pushd → lifd） ----
    # 未登记 pid：丢弃
    check("push to unknown", run_cmd_and_wait(f"echo hello world > /data/push/in/{pid_c}_1", "", 3.0))
    check("notify dropped", wait_for("通知已丢弃: 应用未登记 pid=", 8.0))
    # 后台应用：延迟补投 → 恢复时随 RESUME 送达
    pid_d = ps_pid(["rild", "networkd", "sensord"])
    check("reg D", register("appD", pid_d, "BG"))
    check("BG D", cmd_to(pid_d, "BG"))
    check("push to D", run_cmd_and_wait(f"echo hi there > /data/push/in/{pid_d}_2", "", 3.0))
    check("notify deferred", wait_for("通知已延迟 pid=", 8.0))
    check("pending file", cat_check(f"/data/push/pending/{pid_d}", "NOTIFY", 3.0))
    check("FG D", cmd_to(pid_c, f"FG {pid_d}"))
    check("foreground D", wait_for("已回前台 appD", 8.0))
    check("evt RESUME+NOTIFY", cat_check(f"/data/life/evt_{pid_d}", "RESUME NOTIFY", 3.0))

    # ---- 8. 状态持久化 ----
    check("state write", run_cmd_and_wait("echo save-test-123 > /data/state/sample.bin", "", 2.0))
    check("state read", run_cmd_and_wait("cat /data/state/sample.bin", "save-test-123", 3.0))

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
print(f"\n[life] {len(results) - len(fails)}/{len(results)} passed")
sys.exit(1 if fails else 0)