#!/usr/bin/env python3
"""KunxOS 生命周期核心回归测试（QEMU 无头：-append shell，仅串口驱动）。

与 tests/scripts/lifecycle_test.py（GUI 端到端）互补：不依赖 OOBE/桌面/触摸，
确定性验证 lifd/pushd 的文件信箱协议与内核信号协作：
  1. 守护进程就绪（[lifd]/[pushd] starting、/proc/pressure 可读）
  2. 应用登记（reg_<key>）与 recents 状态表（R/B/S）
  3. 状态机：Running -> Background（SIGSTOP：/proc/processes 显示 T）
     -> 恢复（FG + SIGCONT -> R）-> Suspended（SET_TIMEOUT 超时）-> 
     Terminated（/proc/pressure force 2 触发 LMK）
  4. 多应用切换（appA/appB 交替前台与后台互不影响）
  5. 后台任务白名单：每应用 2 上限、第 3 拒绝、下载 400ms 到期
  6. 本地推送：前台立即投递 evt；后台延迟落 pending；恢复时随 RESUME 补投

进程载体：用真实守护进程（audiod/sensord）的 pid 走信号路径（SIGSTOP/
SIGCONT/SIGKILL 由内核处理），测试结束前全部恢复，不影响系统。

用法：python3 tests/scripts/lifecycle_core_test.py [logfile]
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
LOGFILE = sys.argv[1] if len(sys.argv) > 1 else "/tmp/kx_life_core.log"
BOOT_TIMEOUT = 90

WORKDIR = tempfile.mkdtemp(prefix="kxlc_")
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


def send(s):
    infd = os.open(PIPE + ".in", os.O_WRONLY)
    os.write(infd, s.encode() + b"\n")
    os.close(infd)


def check(name, cond):
    results.append((name, bool(cond)))
    print("[core] %s %s" % ("OK  " if cond else "FAIL", name))
    sys.stdout.flush()


def wait_for(marker, timeout=BOOT_TIMEOUT):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if marker.encode() in log:
            return True
        if p.poll() is not None:
            return False
        time.sleep(0.05)
    return False


def wait_since(before, marker, timeout=6.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if marker.encode() in log[before:]:
            return True
        time.sleep(0.05)
    return False


def run_wait(cli, marker, timeout=6.0):
    before = len(log)
    send(cli)
    return wait_since(before, marker, timeout)


# ---- lifd 命令发布：与生产同款协议 ----
# 生产写方（shared/lifecycle.rs lc_write_cmd / launcher app_life.rs / pushd）
# 均为 cmd_<pid>_<seq> 每消息一文件 + 写 .tmp 后 mv（rename）原子发布。
# 旧「echo > cmd_<pid>」单文件覆盖式写法在 lifd 消费（50ms 轮询）前就会被
# 下一条覆盖（round-004 连锁假失败根因）；脚本迁移到真实 IPC 路径后，
# 回归门覆盖的才是生产实际使用的协议。
CMD_SEQ = [0]


def run_cmd(pid, text, marker, timeout=6.0):
    CMD_SEQ[0] += 1
    base = "/data/life/cmd_%s_%d" % (pid, CMD_SEQ[0])
    before = len(log)
    send("echo %s > %s.tmp" % (text, base))
    send("mv %s.tmp %s" % (base, base))
    return wait_since(before, marker, timeout)


def output_seg(cli, timeout=1.2):
    before = len(log)
    send(cli)
    time.sleep(timeout)
    return log[before:].decode(errors="replace")


def pid_of(name):
    seg = output_seg("ps", 1.0)
    for line in seg.splitlines():
        m = re.match(r"(\d+)\s+\d+\s+(\S+)", line.strip())
        if m and m.group(2) == name:
            return m.group(1)
    return None


def kernel_state(pid):
    """/proc/processes 中该 pid 的状态字母（T/R/None）。"""
    seg = output_seg("cat /proc/processes", 1.0)
    for line in seg.splitlines():
        m = re.match(r"\s*(\d+)\s+([TRDZ])\s+(\d+)\s+(\S+)", line.strip())
        if m and m.group(1) == pid:
            return m.group(2)
    return None


def reg(key, pid, mode):
    return run_wait("echo %s %s > /data/life/reg_%s" % (pid, mode, key),
                    "已登记 %s pid=%s" % (key, pid), 8.0)


def recents_frag(frag):
    return run_wait("cat /data/life/recents", frag, 5.0)


try:
    if not wait_for("kunxos$ "):
        check("boot to shell", False)
        sys.exit(1)
    check("boot to shell", True)
    time.sleep(0.4)

    # 内核/守护进程启动语已中文化，按实际输出匹配。
    check("lifd starting", wait_for("[lifd] KunxOS 生命周期守护进程启动中", 60))
    check("pushd starting", wait_for("[pushd] KunxOS 推送守护进程启动中", 60))
    check("pressure readable", run_wait("cat /proc/pressure", "total_kB", 10.0))

    # 守护进程 pid 解析：等待并重试（daemon 逐一 fork，启动慢于首个提示符）
    aud = None
    sen = None
    for _ in range(12):
        aud = pid_of("audiod")
        sen = pid_of("sensord")
        if aud and sen:
            break
        time.sleep(1.0)
    check("audiod pid", bool(aud))
    check("sensord pid", bool(sen))
    if not aud or not sen:
        sys.exit(1)

    # ---- 登记与 recents ----
    check("reg appA(FG)", reg("appA", aud, "FG"))
    check("reg appB(BG)", reg("appB", sen, "BG"))
    check("recents appA R", recents_frag("appA %s R" % aud))
    check("recents appB B", recents_frag("appB %s B" % sen))

    # ---- 多应用切换 + SIGSTOP/SIGCONT ----
    check("bg appA", run_cmd(aud, "BG", "已转入后台 appA", 5.0))
    time.sleep(0.4)
    check("appA T (SIGSTOP)", kernel_state(aud) == "T")
    check("fg appB", run_cmd(0, "FG %s" % sen, "已回前台 appB", 5.0))
    check("appB R", kernel_state(sen) == "R")
    check("fg appA", run_cmd(0, "FG %s" % aud, "已回前台 appA", 5.0))
    check("appA R (SIGCONT)", kernel_state(aud) == "R")

    # ---- 后台任务白名单 ----
    # 时长 10s（值按 µs 计）：慢宿主（RISC-V TCG）下命令往返 >0.5s，短任务
    # 在下一条提交前就到期，凑不满「每应用 2 个」上限（假失败）；10s 保证
    # 三个请求必然重叠，随后 BTASK_END 显式释放。
    check("btask0 granted", run_cmd(sen, "BTASK 0 10000000", "后台任务已授予", 5.0))
    check("btask1 granted", run_cmd(sen, "BTASK 1 10000000", "后台任务已授予", 5.0))
    check("btask2 denied", run_cmd(sen, "BTASK 2 10000000", "后台任务被拒: 单应用数量已达上限", 5.0))
    # 每应用上限 2 已满：先结束任务释放槽位，再请求 400ms 短任务测到期路径
    # （docs/lifecycle-test.md A7 的前置步骤 BTASK_END 在此脚本中缺失，
    #  此前 BTASK 3 恒被「单应用数量已达上限」拒绝）。
    check("btask end", run_cmd(sen, "BTASK_END", "后台任务已结束", 5.0))
    check("btask3 expiring", run_cmd(sen, "BTASK 3 400", "后台任务已授予", 5.0))
    check("btask expired", wait_for("后台任务已到期 pid=", 4.0))

    # ---- 超时挂起 ----
    check("set timeout 1500", run_cmd(0, "SET_TIMEOUT 1500", "bg->suspend 超时已设为 1500", 5.0))
    check("bg appA(2)", run_cmd(aud, "BG", "已转入后台 appA", 5.0))
    # 慢宿主余量：50ms 轮询 + 150ms 宽限 + TCG 调度抖动，15s 足以区分
    # 「超时挂起正常」与「真不挂起」（旧 4s 在 RISC-V TCG 下偏紧）。
    check("appA suspended", wait_for("已挂起 appA", 15.0))
    time.sleep(0.3)
    check("recents appA S", recents_frag("appA %s S" % aud))

    # ---- 内存压力 + LMK ----
    check("force 2", run_wait("echo force 2 > /proc/pressure", "内存压力严重", 5.0))
    check("LMK terminated appA", wait_for("已终止 appA", 5.0))
    time.sleep(0.3)
    check("recents cleared", not recents_frag("appA %s" % aud))
    check("clear pressure", run_wait("echo clear > /proc/pressure", "内存压力正常", 5.0))

    # ---- 本地推送 ----
    check("notify forwarded", run_wait("echo Hello World > /data/push/in/%s_1" % sen,
                                       "通知已投递", 5.0))
    check("evt NOTIFY", run_wait("cat /data/life/evt_%s" % sen, "NOTIFY", 5.0))
    check("bg appB(2)", run_cmd(sen, "BG", "已转入后台 appB", 5.0))
    check("通知已延迟", run_wait("echo Deferred msg > /data/push/in/%s_2" % sen,
                                      "通知已延迟", 5.0))
    check("pending NOTIFY", run_wait("cat /data/push/pending/%s" % sen, "NOTIFY", 5.0))
    check("fg appB(2)", run_cmd(0, "FG %s" % sen, "已回前台 appB", 5.0))
    check("evt RESUME+NOTIFY", run_wait("cat /data/life/evt_%s" % sen, "RESUME NOTIFY", 5.0))
    check("pending removed", not run_wait("cat /data/push/pending/%s" % sen, "NOTIFY", 2.5))

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
print("\n[lifecycle-core] %d/%d passed" % (len(results) - len(fails), len(results)))
sys.exit(1 if fails else 0)