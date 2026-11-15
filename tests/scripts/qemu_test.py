"""Shared RISC-V QEMU configuration and portable serial transport for tests."""
import atexit
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[2]
TARGET = "riscv64gc-unknown-none-elf"


def kernel_path():
    return str(Path(os.environ.get("KERNEL", ROOT / "kunxcore/kernel.img")).resolve())


def build_kernel(work, apps=()):
    """Build an isolated regression image without changing normal rootfs/images."""
    work = Path(work).resolve()
    work.mkdir(parents=True, exist_ok=True)
    stage = Path(tempfile.mkdtemp(prefix="rootfs-", dir=work))
    shutil.copytree(ROOT / "rootfs", stage, dirs_exist_ok=True,
                    ignore=shutil.ignore_patterns("data", "sfsrace-test", "audio-test"))
    for name in ("dev/input", "proc", "tmp", "var/run", "var/log", "data/oobe",
                 "data/photos", "data/videos", "data/app"):
        (stage / name).mkdir(parents=True, exist_ok=True)
    (stage / "etc/inittab").write_text("# Isolated shell regression image.\n")
    (stage / "data/oobe/.done").write_text("1\n")
    target_dir = work / "target"
    for app in ("kunxinit", *apps):
        manifest = ROOT / ("kunxinit" if app == "kunxinit" else f"apps/{app}") / "Cargo.toml"
        subprocess.run(["cargo", "build", "--release", "--target", TARGET,
                        "--target-dir", str(target_dir), "--manifest-path", str(manifest)],
                       cwd=ROOT, check=True)
        shutil.copy2(target_dir / TARGET / "release" / app,
                     stage / ("init" if app == "kunxinit" else f"bin/{app}"))
    subprocess.run(["rustc", "--edition=2021", "--target", TARGET,
                    "-C", "panic=abort", "-C", "relocation-model=static",
                    "-C", f"link-arg=-T{ROOT / 'kunxcore/apps/hello.ld'}",
                    str(ROOT / "tests/hello.rs"), "-o", str(stage / "bin/hello")],
                   cwd=ROOT, check=True)
    for path in (stage / "bin").iterdir():
        if path.is_file():
            with path.open("rb") as stream:
                header = stream.read(20)
            if header[:4] == b"\x7fELF" and int.from_bytes(header[18:20], "little") != 243:
                raise ValueError(f"Non-RISC-V guest executable: {path}")
    ramdisk = work / "ramdisk.img"
    subprocess.run([os.sys.executable, str(ROOT / "tools/mkfs_sfs.py"), str(stage),
                    str(ramdisk), "4096"], cwd=ROOT, check=True)
    subprocess.run(["cargo", "build", "--release", "--target", TARGET,
                    "--target-dir", str(target_dir), "--manifest-path",
                    str(ROOT / "kunxcore/Cargo.toml")], cwd=ROOT, check=True,
                   env=dict(os.environ, KUNX_RAMDISK=str(ramdisk)))
    return str(target_dir / TARGET / "release/rust-kernel")


def temp_path(name):
    return str(Path(tempfile.gettempdir()) / name)


def qemu_command():
    executable = os.environ.get("QEMU") or shutil.which("qemu-system-riscv64")
    if not executable:
        candidates = [Path(r"C:\Program Files\qemu\qemu-system-riscv64.exe"),
                      Path.home() / "qemu-riscv-pkgs/root/usr/bin/qemu-system-riscv64"]
        executable = next((str(p) for p in candidates if p.is_file()), "qemu-system-riscv64")
    return [executable, "-M", "virt", "-accel", os.environ.get("QEMU_ACCEL", "tcg"),
            "-cpu", "rv64", "-bios", os.environ.get("BIOS", os.environ.get("OPENBI", "default")),
            "-m", "512M"]


def monitor_address():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()


def monitor_option(address):
    return f"tcp:{address[0]}:{address[1]},server=on,wait=off"


def launcher_layout(log):
    tiles, docks, folders, contents = {}, {}, {}, {}
    for line in bytes(log).decode("utf-8", "replace").splitlines():
        fields = line.split()
        if len(fields) < 5 or fields[0] != "[launcher]":
            continue
        kind, key = fields[1:3]
        if kind not in ("tile", "dock", "folder", "folder-app"):
            continue
        values = dict(part.split("=", 1) for part in fields[3:] if "=" in part)
        if not {"cx", "cy"} <= values.keys():
            continue
        point = (int(values["cx"]), int(values["cy"]))
        if kind in ("tile", "folder"):
            (tiles if kind == "tile" else folders)[key] = (int(values["page"]), *point)
        else:
            (docks if kind == "dock" else contents)[key] = point
    return tiles, docks, folders, contents


def monitor_command(address, command):
    with socket.create_connection(address, timeout=5) as connection:
        def response():
            data = bytearray()
            while b"(qemu)" not in data:
                block = connection.recv(4096)
                if not block:
                    raise ConnectionError("QEMU monitor closed")
                data.extend(block)
            return bytes(data)
        response()
        connection.sendall(command.encode() + b"\n")
        return response()


def screenshot(address, directory, name):
    path = (Path(directory) / name).resolve()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.unlink(missing_ok=True)
    result = monitor_command(address, f'screendump "{path.as_posix()}"')
    if not path.is_file() or path.stat().st_size == 0:
        raise RuntimeError(f"QEMU did not save {path}: {result!r}")


def desktop_ready(guest, tap, timeout=180):
    """Handle both a completed desktop fixture and the current OOBE skip flow."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if "[launcher] 桌面就绪".encode() in guest.log:
            return
        if guest.process.poll() is not None:
            break
        if b"[oobe]" in guest.log:
            tap(495, 30)  # OOBE skip, then confirm its completion page.
            time.sleep(1)
            tap(465, 925)
        time.sleep(2)
    raise TimeoutError(f"Desktop did not become ready; inspect {guest.error_path}")


class Guest:
    """Capture UART from the first byte, with bounded connect/write and cleanup.

    Callers pass '-serial stdio'; it is replaced by a TCP client connected to
    an already-listening host socket. This works on Windows and Unix and avoids
    FIFO opens hanging forever when QEMU fails to start.
    """

    def __init__(self, command, logfile, cwd=ROOT):
        self.log = bytearray()
        self.closed = False
        self.serial = None
        self.thread = None
        self.process = None
        logpath = Path(logfile).resolve()
        logpath.parent.mkdir(parents=True, exist_ok=True)
        self.log_file = logpath.open("wb")
        self.error_path = logpath.with_suffix(".qemu-error.log")
        self.errors = self.error_path.open("wb")
        command = list(command)
        if "-append" not in command:
            command += ["-append", "verbose"]
        else:
            index = command.index("-append") + 1
            if "verbose" not in command[index].split():
                command[index] += " verbose"
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen(1)
            listener.settimeout(0.1)
            index = command.index("-serial") + 1
            command[index] = f"tcp:127.0.0.1:{listener.getsockname()[1]}"
            try:
                self.process = subprocess.Popen(command, cwd=cwd, stdout=subprocess.DEVNULL,
                                                stderr=self.errors)
                deadline = time.monotonic() + 15
                while self.serial is None:
                    if self.process.poll() is not None:
                        raise RuntimeError(f"QEMU exited; inspect {self.error_path}")
                    if time.monotonic() >= deadline:
                        raise TimeoutError(f"QEMU UART did not connect; inspect {self.error_path}")
                    try:
                        self.serial, _ = listener.accept()
                    except socket.timeout:
                        continue
                self.serial.settimeout(0.2)
                self.thread = threading.Thread(target=self._read, daemon=True)
                self.thread.start()
                atexit.register(self.close)
            except BaseException:
                self.close()
                raise

    def _read(self):
        while not self.closed:
            try:
                data = self.serial.recv(65536)
                if not data:
                    break
                self.log.extend(data)
                self.log_file.write(data)
                self.log_file.flush()
            except socket.timeout:
                continue
            except OSError:
                break

    def send(self, data):
        if self.closed or self.process.poll() is not None:
            raise BrokenPipeError(f"QEMU stopped; inspect {self.error_path}")
        self.serial.sendall(data)

    def wait_for(self, marker, timeout=90, start=0):
        marker = marker.encode() if isinstance(marker, str) else marker
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if marker in self.log[start:]:
                return True
            if self.process.poll() is not None:
                break
            time.sleep(0.05)
        return marker in self.log[start:]

    def shell(self, command, timeout=15):
        start = len(self.log)
        self.send(command.encode() + b"\n")
        if not self.wait_for("kunxos$", timeout, start):
            raise TimeoutError(f"Shell command did not finish: {command}; inspect {self.error_path}")
        return bytes(self.log[start:])

    def close(self):
        if self.closed:
            return
        if self.process is not None and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self.closed = True
        if self.thread is not None:
            self.thread.join(timeout=2)
        if self.serial is not None:
            self.serial.close()
        self.log_file.close()
        self.errors.close()
        atexit.unregister(self.close)
