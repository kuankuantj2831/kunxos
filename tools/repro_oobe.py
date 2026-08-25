#!/usr/bin/env python3
"""Reproduce OOBE / launcher / app interaction issues under QEMU.

Boots KunxOS with ramfb + virtio input, captures serial log, takes screendumps
via the QEMU monitor, and injects touch taps over the serial UART protocol.
"""
import subprocess, sys, time, threading, os, tempfile, socket

KERNEL = os.path.abspath(os.environ.get("KERNEL", "kunxcore/kernel.img"))
LOGFILE = sys.argv[1] if len(sys.argv) > 1 else "/tmp/kx_repro.log"
SHOTS = sys.argv[2] if len(sys.argv) > 2 else "/tmp/kx_shots"
os.makedirs(SHOTS, exist_ok=True)
WORKDIR = tempfile.mkdtemp(prefix="kxr_")
PIPE = os.path.join(WORKDIR, "kx")
MON = os.path.join(WORKDIR, "mon")

FRAME_DOWN, FRAME_UP, FRAME_MOVE = 0x01, 0x00, 0x02

def frame(kind, x=0, y=0):
    return bytes([0xAA, kind, x & 0xFF, (x >> 8) & 0xFF, y & 0xFF, (y >> 8) & 0xFF])

os.mkfifo(PIPE + ".in")
os.mkfifo(PIPE + ".out")

cmd = ["qemu-system-aarch64", "-M", "virt", "-cpu", "cortex-a72", "-m", "512M",
       "-kernel", KERNEL, "-serial", "pipe:" + PIPE,
       "-device", "ramfb", "-no-reboot", "-display", "none",
       "-netdev", "user,id=n0", "-device", "virtio-net-device,netdev=n0",
       "-device", "virtio-keyboard-device", "-device", "virtio-tablet-device",
       "-monitor", "unix:" + MON + ",server,nowait"]

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

def send_frame(fr):
    infd = os.open(PIPE + ".in", os.O_WRONLY)
    os.write(infd, fr)
    os.close(infd)

def tap(x, y, hold=0.06):
    send_frame(frame(FRAME_DOWN, x, y))
    time.sleep(hold)
    send_frame(frame(FRAME_UP))
    time.sleep(0.05)

def shot(name):
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(MON)
        s.sendall(("screendump %s\n" % os.path.join(SHOTS, name)).encode())
        time.sleep(0.5)
        try:
            s.settimeout(0.5); s.recv(4096)
        except socket.timeout:
            pass
        s.close()
    except Exception as e:
        print("shot failed:", e)

def wait_for(text, timeout=60):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if text.encode() in log:
            return True
        time.sleep(0.2)
    print("TIMEOUT waiting for:", text)
    return False

try:
    wait_for("[oobe] welcome to KunxOS", 60)
    time.sleep(1.0)
    shot("01_oobe_welcome.ppm")

    # Tap the language pill "简体中文" (first pill, x=300 y=100) then a module
    # checkbox on later steps, to show taps outside next/back are ignored.
    tap(300, 100)
    time.sleep(0.4)
    shot("02_oobe_lang_pill_tap.ppm")

    # Tap Next (x=465, y=925)
    tap(465, 925)
    time.sleep(0.6)
    shot("03_oobe_modules.ppm")

    # Try toggling a module checkbox (e.g. browser row) - should be ignored
    tap(100, 350)
    time.sleep(0.4)
    shot("04_oobe_module_toggle.ppm")

    # Next through all steps quickly
    for i in range(8):
        tap(465, 925)
        time.sleep(0.6)
    time.sleep(1.0)
    shot("05_launcher.ppm")

    wait_for("[launcher]", 30)
    time.sleep(2.0)
    shot("06_launcher_desktop.ppm")

    # Tap the first grid icon (phone -> /bin/phone missing -> execve fail)
    tap(130, 100)
    time.sleep(2.5)
    shot("07_after_app_launch.ppm")

    # Tap settings icon (row1 col0)
    tap(130, 228)
    time.sleep(2.5)
    shot("08_settings_open.ppm")

    # Try tapping inside settings (a list row) to see if click works
    tap(270, 300)
    time.sleep(0.5)
    shot("09_settings_click.ppm")

    # Back to desktop (settings back button is at top-left)
    tap(60, 40)
    time.sleep(2.0)

    # Tap calculator (row2 col0)
    tap(130, 356)
    time.sleep(2.5)
    shot("10_calc_open.ppm")

    # Try tapping calculator buttons (row area y~700)
    tap(270, 700)
    time.sleep(0.5)
    shot("11_calc_click.ppm")

    time.sleep(3.0)
    print("=== LOG TAIL ===")
    print(log.decode(errors="replace")[-4000:])
finally:
    stop = True
    p.terminate()
    try:
        p.wait(timeout=5)
    except subprocess.TimeoutExpired:
        p.kill()
    logf.close()
