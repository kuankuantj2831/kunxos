#!/usr/bin/env python3
"""Focused repro: boot -> OOBE (auto) -> desktop -> open calculator -> tap buttons."""
import subprocess, sys, time, threading, os, tempfile, socket

KERNEL = os.path.abspath(os.environ.get("KERNEL", "kunxcore/kernel.img"))
LOGFILE = sys.argv[1] if len(sys.argv) > 1 else "/tmp/kx_calc.log"
SHOTS = sys.argv[2] if len(sys.argv) > 2 else "/tmp/kx_calc_shots"
os.makedirs(SHOTS, exist_ok=True)
WORKDIR = tempfile.mkdtemp(prefix="kxc_")
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
        log.extend(data); logf.write(data); logf.flush()

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
    except Exception:
        pass

def wait_for(text, timeout=60):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if text.encode() in log:
            return True
        time.sleep(0.2)
    return False

try:
    wait_for("[oobe] welcome to KunxOS", 60)
    time.sleep(0.5)
    # Fast-forward OOBE: tap Next 9 times (step 0..8), then final step auto-finishes
    for i in range(9):
        tap(465, 925)
        time.sleep(0.5)
    wait_for("[launcher] desktop ready", 60)
    time.sleep(1.0)
    # Tap calculator icon (row2 col0): icon center (130, 356)
    tap(130, 356)
    wait_for("[calculator] ready", 20)
    time.sleep(1.0)
    shot("calc_open.ppm")
    # Tap digit '5' button (idx 9: col1 row2 -> x=142..266, y=498..643, center (204,570))
    tap(204, 570)
    time.sleep(0.3)
    shot("calc_tap1.ppm")
    tap(204, 570)
    time.sleep(0.3)
    shot("calc_tap2.ppm")
    # Back button top-left (4..48, 4..48) center (24, 24)
    tap(24, 24)
    time.sleep(1.5)
    shot("back_to_desktop.ppm")
    time.sleep(2.0)
    print("=== CALC LOG ===")
    print(log.decode(errors="replace")[-3000:])
finally:
    stop = True
    p.terminate()
    try:
        p.wait(timeout=5)
    except subprocess.TimeoutExpired:
        p.kill()
    logf.close()
