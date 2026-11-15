"""Verify fixes from the second audit against current storage sources.
Run: python docs/audit-2026-11-15-repro.py
Directly includes current shared/sync sources. Writes only build/ and temp files.
"""
from pathlib import Path
import os
import subprocess
ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / 'build/audit-2026-11-15/host'
OUT.mkdir(parents=True, exist_ok=True)
code = '#![allow(dead_code)]\n'
for name, path in [('sync', 'shared/sync/mod.rs'), ('host', 'sync_tests/src/host.rs'),
                   ('storage_state_regression', 'sync_tests/src/storage_state_regression.rs')]:
    code += '#[path="' + (ROOT / path).as_posix() + '"] mod ' + name + ';\n'
source = OUT / 'findings.rs'
source.write_text(code, encoding='utf-8')
exe = OUT / ('findings.exe' if os.name == 'nt' else 'findings')
subprocess.run(['rustc', '--edition=2021', '--test', str(source), '-o', str(exe)], cwd=ROOT, check=True)
subprocess.run([str(exe), 'audit2_', '--test-threads=1'], cwd=ROOT, check=True)
