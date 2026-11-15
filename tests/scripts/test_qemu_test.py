"""Host regressions for early UART capture, failed launches and guest ISA drift."""
from pathlib import Path
import sys
import tempfile
import time
import unittest

from qemu_test import Guest, ROOT, launcher_layout


class HarnessTests(unittest.TestCase):
    def test_uart_first_bytes_and_bounded_cleanup(self):
        child = """
import socket, sys
address = sys.argv[sys.argv.index('-serial') + 1].split(':')
with socket.create_connection((address[1], int(address[2]))) as connection:
    connection.sendall(b'BOOT-FIRST-BYTES\\n')
    while True:
        data = connection.recv(4096)
        if not data:
            break
        connection.sendall(data)
"""
        with tempfile.TemporaryDirectory() as directory:
            guest = Guest([sys.executable, "-u", "-c", child, "-serial", "stdio"],
                          Path(directory) / "serial.log")
            try:
                self.assertTrue(guest.wait_for("BOOT-FIRST-BYTES", 5))
                guest.send(b"echo-request\n")
                self.assertTrue(guest.wait_for("echo-request", 5))
            finally:
                guest.close()
                guest.close()
            self.assertIsNotNone(guest.process.poll())
            self.assertFalse(guest.thread.is_alive())
            self.assertIn(b"BOOT-FIRST-BYTES", (Path(directory) / "serial.log").read_bytes())

    def test_failed_qemu_exits_promptly_and_preserves_stderr(self):
        with tempfile.TemporaryDirectory() as directory:
            start = time.monotonic()
            with self.assertRaises(RuntimeError):
                Guest([sys.executable, "-c", "import sys; sys.stderr.write('bad-machine'); sys.exit(7)",
                       "-serial", "stdio"], Path(directory) / "serial.log")
            self.assertLess(time.monotonic() - start, 5)
            self.assertIn("bad-machine", (Path(directory) / "serial.qemu-error.log").read_text())

    def test_rootfs_elfs_are_riscv(self):
        for path in (ROOT / "rootfs").rglob("*"):
            if path.is_file():
                with path.open("rb") as stream:
                    header = stream.read(20)
                if header[:4] == b"\x7fELF":
                    self.assertEqual(int.from_bytes(header[18:20], "little"), 243, str(path))

    def test_layout_uses_reported_folder_positions(self):
        log = b"""[launcher] tile notes page=1 cx=300 cy=500
[launcher] dock settings cx=400 cy=900
[launcher] folder tools page=1 cx=150 cy=360
[launcher] folder-app files cx=270 cy=314
"""
        tiles, docks, folders, apps = launcher_layout(log)
        self.assertEqual(tiles["notes"], (1, 300, 500))
        self.assertEqual(docks["settings"], (400, 900))
        self.assertEqual(folders["tools"], (1, 150, 360))
        self.assertEqual(apps["files"], (270, 314))


if __name__ == "__main__":
    unittest.main()
