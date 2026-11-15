#!/usr/bin/env python3
"""Build every native guest with kunxrt and test an isolated RISC-V image."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'tests/scripts'))
from qemu_test import Guest, qemu_command, TARGET


def run(command, **kwargs):
    subprocess.run([str(x) for x in command], cwd=ROOT, check=True, **kwargs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--build-dir', type=Path, default=ROOT / 'build/runtime-validation')
    parser.add_argument('--mode', choices=['shell', 'desktop', 'oobe'], default='shell')
    parser.add_argument('--reuse-binaries', action='store_true', help='reuse previously built guests; always rebuild the staged kernel')
    args = parser.parse_args()
    work = args.build_dir.resolve()
    work.mkdir(parents=True, exist_ok=True)
    target = work / 'target'
    manifests = sorted((ROOT / 'apps').glob('*/Cargo.toml')) + sorted((ROOT / 'kunxserv').glob('*/Cargo.toml'))
    manifests += [ROOT / 'kunxinit/Cargo.toml', ROOT / 'kunxpkg/Cargo.toml']
    if not args.reuse_binaries:
        for m in manifests:
            print('Building', m.parent.name, flush=True)
            with (work / (m.parent.name + '-build.log')).open('w', encoding='utf-8') as log:
                run(['cargo', 'build', '--release', '--target', TARGET, '--target-dir', target,
                     '--manifest-path', m], stdout=log, stderr=subprocess.STDOUT)
    stage = Path(tempfile.mkdtemp(prefix='rootfs-' + args.mode + '-', dir=work))
    shutil.copytree(ROOT / 'rootfs', stage, dirs_exist_ok=True,
                    ignore=shutil.ignore_patterns('data', 'sfsrace-test', 'audio-test'))
    for name in ('dev/input', 'etc/init.d', 'proc', 'tmp', 'var/run', 'var/log', 'data/oobe',
                 'data/photos', 'data/videos', 'data/app', 'var/lib/pkg/cache/index',
                 'var/lib/pkg/installed', 'var/lib/pkg/permissions', 'var/lib/pkg/reviewed', 'var/lib/pkg/backup'):
        (stage / name).mkdir(parents=True, exist_ok=True)
    for m in manifests:
        name = m.parent.name
        dest = 'init' if name == 'kunxinit' else 'bin/' + ('pkg' if name == 'kunxpkg' else name)
        shutil.copy2(target / TARGET / 'release' / name, stage / dest)
    if args.mode != 'oobe':
        (stage / 'data/oobe/.done').write_text('1\n')
    if args.mode == 'shell':
        (stage / 'etc/inittab').write_text('# Runtime test: shell only.\n')
    ramdisk = work / ('ramdisk-' + args.mode + '.img')
    run([sys.executable, ROOT / 'tools/mkfs_sfs.py', stage, ramdisk, '4096'])
    run(['cargo', 'build', '--release', '--target', TARGET, '--target-dir', target,
         '--manifest-path', ROOT / 'kunxcore/Cargo.toml'], env=dict(os.environ, KUNX_RAMDISK=str(ramdisk)))
    if args.mode != 'shell':
        command = [sys.executable, ROOT / 'tools/test_display.py', '--no-build', '--build-dir', work,
                   '--qemu', qemu_command()[0], '--gpu', 'ramfb']
        command += ['--oobe'] if args.mode == 'oobe' else ['--animate']
        run(command)
        for name in ('report.json', 'serial.log', 'screen.png'):
            if (work / name).exists():
                shutil.copy2(work / name, work / (args.mode + '-' + name))
        return
    guest = Guest(qemu_command() + ['-kernel', str(target / TARGET / 'release/rust-kernel'),
                  '-append', 'shell verbose', '-serial', 'stdio', '-display', 'none',
                  '-no-reboot', '-device', 'ramfb'], work / 'runtime-serial.log')
    try:
        assert guest.wait_for('kunxos$', 120), 'shell failed to start'
        results = {}
        for command, marker, timeout in [
            ('run /bin/rttest', b'[runtime-selftest] PASS', 180),
            ('run /bin/probe', '[syscall-test] 通过'.encode(), 90),
            ('run /bin/pkg list', None, 30),
        ]:
            output = guest.shell(command, timeout)
            assert b'PANIC' not in output and b'FAIL' not in output, output.decode('utf-8', 'replace')
            assert '退出码=0'.encode() in output, output.decode('utf-8', 'replace')
            if marker:
                assert marker in output, output.decode('utf-8', 'replace')
            results[command] = 'PASS'
        (work / 'runtime-report.json').write_text(json.dumps({'result': 'PASS', 'native_binaries': len(manifests), 'commands': results}, indent=2))
        print('Runtime guest tests PASS:', work, flush=True)
    finally:
        guest.close()


if __name__ == '__main__':
    main()
