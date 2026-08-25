#!/usr/bin/env python3
"""Verify QWERTY key mapping in the OOBE account input boxes."""
import subprocess, sys, time, threading, os, tempfile, socket

KERNEL = os.path.abspath(os.environ.get("KERNEL", "kunxcore/kernel.img"))
LOGFILE = sys.argv[1] if len(sys.argv) > 1 else "/tmp/kx_keys.log"
WORKDIR = tempfile.mkdtemp(prefix="kxk_")
PIPE = os.path.join(WORKDIR, "kx")
MON = os.path.join(WORKDIR, "mon")

FRAME_DOWN, FRAME_UP = 0x01, 0x00

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
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect(MON)
        s.sendall(("sendkey %s\n" % key).encode())
        time.sleep(0.15)
        try:
            s.settimeout(0.3); s.recv(4096)
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

try:
    wait_for("[oobe] welcome to KunxOS", 60)
    time.sleep(0.6)
    # welcome -> language -> modules -> timezone -> network -> agreement -> account
    next_step(6)
    time.sleep(0.5)
    # focus the username field
    tap(250, 140)
    time.sleep(0.3)
    # type QWERTY top row, home row, bottom row, digits, shift combo
    for k in ["q","w","e","r","t","y","u","i","o","p"]:
        sendkey(k)
    for k in ["a","s","d","f","g","h","j","k","l"]:
        sendkey(k)
    for k in ["z","x","c","v","b","n","m"]:
        sendkey(k)
    for k in ["1","2","3","4","5","6","7","8","9","0"]:
        sendkey(k)
    sendkey("shift-a")   # should map to 'A'
    sendkey("shift-1")   # should map to '!'
    time.sleep(1.0)

    # read the received (code, ch) pairs from the log
    import re
    text = log.decode(errors="replace")
    pairs = re.findall(r"\[oobe\] key code=(\d+) ch='(.)'", text)
    print("received key pairs:", pairs)
    expected = ("qwertyuiop" "asdfghjkl" "zxcvbnm" "1234567890" "A" "!")
    got = "".join(ch for _, ch in pairs)
    print("expected:", expected)
    print("got     :", got)
    print("MATCH:", "PASS" if got == expected else "FAIL")

    time.sleep(1.0)
    print("=== TAIL ===")
    print(text[-1500:])
finally:
    stop = True
    p.terminate()
    try:
        p.wait(timeout=5)
    except subprocess.TimeoutExpired:
        p.kill()
    logf.close()
