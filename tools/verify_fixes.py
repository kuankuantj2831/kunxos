#!/usr/bin/env python3
"""Verify all fixes: OOBE option taps + input boxes, stub apps, cursor in apps."""
import subprocess, sys, time, threading, os, tempfile, socket

KERNEL = os.path.abspath(os.environ.get("KERNEL", "kunxcore/kernel.img"))
LOGFILE = sys.argv[1] if len(sys.argv) > 1 else "/tmp/kx_verify.log"
SHOTS = sys.argv[2] if len(sys.argv) > 2 else "/tmp/kx_verify_shots"
os.makedirs(SHOTS, exist_ok=True)
WORKDIR = tempfile.mkdtemp(prefix="kxv_")
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

def sendkey(key):
    """Send a key via QEMU monitor (PS/2 path; may not reach virtio-keyboard)."""
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(MON)
        s.sendall(("sendkey %s\n" % key).encode())
        time.sleep(0.2)
        try:
            s.settimeout(0.3); s.recv(4096)
        except socket.timeout:
            pass
        s.close()
    except Exception:
        pass

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

def next_step(n=1):
    for _ in range(n):
        tap(465, 925)
        time.sleep(0.5)

results = []

def check(name, cond):
    results.append((name, bool(cond)))
    print(("PASS " if cond else "FAIL ") + name)

try:
    wait_for("[oobe] welcome to KunxOS", 60)
    time.sleep(0.8)
    shot("01_welcome.ppm")

    # --- 0. Back button regression: next -> back should return to welcome ---
    next_step(1)               # welcome -> language
    time.sleep(0.3)
    tap(60, 925)               # back arrow (y>880, x<120)
    time.sleep(0.4)
    check("back button returns to welcome",
          b"back button hit" in log and log.count(b"welcome to KunxOS") >= 2)
    shot("01b_back_to_welcome.ppm")

    # --- 1. advance to language step, then tap the English pill ---
    next_step(1)               # welcome -> language (step 1)
    tap(300, 190)              # English pill: y in 168..216
    time.sleep(0.4)
    check("lang pill consumed", b"option consumed" in log)
    check("still on language step", b"step 3/10" not in log)
    shot("02_lang_english.ppm")

    # --- 2. Next to modules; toggle a module checkbox (calc row, y=268) ---
    next_step(1)               # language -> modules (step 2)
    tap(100, 278)              # calc checkbox row (i=6, y=268..290)
    time.sleep(0.4)
    check("module checkbox consumed", b"option consumed" in log)
    shot("03_modules_toggle.ppm")
    tap(100, 278)              # toggle back on so the desktop keeps 9 apps
    time.sleep(0.4)
    shot("03b_modules_toggle_back.ppm")

    # --- 3. Next through timezone/network/agreement to account ---
    next_step(3)               # modules -> timezone -> network -> agreement
    next_step(1)               # agreement -> account (step 6)
    time.sleep(0.5)
    shot("04_account_step.ppm")

    # --- 4. Account input boxes: tap username field, try typing via sendkey ---
    tap(250, 140)              # username field (FIELD_Y0=118..162)
    time.sleep(0.3)
    sendkey("a"); sendkey("b"); sendkey("c")
    time.sleep(0.5)
    shot("05_account_typed.ppm")
    tap(250, 236)              # password field
    time.sleep(0.2)
    tap(250, 332)              # confirm field
    time.sleep(0.2)
    next_step(1)               # account -> security (saves entered values)
    time.sleep(0.3)
    next_step(1)               # security -> theme
    time.sleep(0.3)
    next_step(1)               # theme -> complete (auto-finishes)
    time.sleep(1.0)
    wait_for("[launcher] desktop ready", 30)
    time.sleep(1.0)
    shot("06_launcher.ppm")
    check("launcher 9 apps", b"desktop ready, 9 apps" in log)

    # --- 6. Open phone (stub, previously execve failed) ---
    tap(130, 100)
    time.sleep(1.5)
    check("phone stub launched", b"launch Phone" in log and b"[stub] ready" in log)
    check("phone no longer execve-fails", b"execve /bin/phone failed" not in log)
    shot("07_phone_stub.ppm")

    # back from phone
    tap(24, 24)
    time.sleep(2.0)

    # --- 7. Open calculator (cursor should be visible now) ---
    tap(130, 356)
    wait_for("[calculator] ready", 20)
    time.sleep(1.0)
    shot("08_calc_open.ppm")
    # move the tablet pointer (inject ABS via UART is touch; use virtio tablet not
    # reachable headless, so instead just tap a digit)
    tap(204, 570)
    time.sleep(0.5)
    check("calc digit tap", b"[calculator] tap (204,570)" in log)
    tap(24, 24)            # back
    time.sleep(2.0)

    # --- 8. Open store (was a dead-end; now has back button) ---
    tap(410, 356)          # store icon: row2 col2 center
    time.sleep(2.0)
    check("store opened", b"[store]" in log)
    shot("09_store_open.ppm")
    tap(24, 24)            # back from store
    time.sleep(2.0)
    check("store back to desktop", b"app 'Store' exited" in log)

    time.sleep(1.0)
    print("=== VERIFY SUMMARY ===")
    for n, ok in results:
        print(("PASS " if ok else "FAIL ") + n)
    print("=== LOG TAIL ===")
    print(log.decode(errors="replace")[-3000:])
finally:
    stop = True
    p.terminate()
    try:
        p.wait(timeout=5)
    except subprocess.TimeoutExpired:
        p.kill()
    logf.close()
