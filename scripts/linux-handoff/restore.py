#!/usr/bin/env python3
"""校验本会话分卷并还原到新目录；不自动启动、不覆盖已有安装。"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import threading


def verify(parts_dir, bundles):
    for bundle in bundles:
        for part in bundle['parts']:
            path = parts_dir / part['name']
            if not path.is_file() or path.stat().st_size != part['size']:
                raise RuntimeError(f'分卷缺失或大小不符：{part["name"]}，请下载齐全部分卷')
            with path.open('rb') as stream:
                value = hashlib.file_digest(stream, 'sha256').hexdigest()
            if value != part['sha256']:
                raise RuntimeError(f'SHA-256 不符：{part["name"]}，尚未解压')
            print('校验通过：', path.name, flush=True)


def unpack(parts_dir, bundle, target, decoder):
    errors = []
    process = subprocess.Popen([str(decoder), '-q', '-d', '-c'], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    tar = subprocess.Popen(['tar', '--no-same-owner', '-xpf', '-', '-C', str(target)], stdin=process.stdout)
    process.stdout.close()

    def feed():
        try:
            for part in bundle['parts']:
                with (parts_dir / part['name']).open('rb') as reader:
                    while data := reader.read(4 * 1024 * 1024):
                        process.stdin.write(data)
        except BaseException as error:
            errors.append(error)
        finally:
            process.stdin.close()

    writer = threading.Thread(target=feed)
    writer.start()
    try:
        writer.join()
        decoded = process.wait()
        extracted = tar.wait()
        if errors or decoded or extracted:
            raise RuntimeError('解压未完成；保留部分输出供检查，没有启动 Eve')
    finally:
        for child in [process, tar]:
            if child.poll() is None:
                child.terminate()
                child.wait(timeout=10)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--parts-dir', type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument('--destination', type=Path, default=Path.home() / 'Eve-local-20261020')
    parser.add_argument('--verify-only', action='store_true')
    args = parser.parse_args()
    os.umask(0o077)
    if platform.system() != 'Linux' or platform.machine() != 'x86_64':
        raise RuntimeError('本运行包需要 Linux x86_64（glibc 2.28 及以上），不支持 ARM 或 Alpine/musl')
    if os.getuid() == 0:
        raise RuntimeError('请不要以 root 或 sudo 解压运行含完整开发权限的 Eve')
    if shutil.which('tar') is None:
        raise RuntimeError('需要系统 tar 工具，请先安装 tar')
    manifest = json.loads((Path(__file__).parent / 'manifest.json').read_text(encoding='utf8'))
    verify(args.parts_dir, manifest['bundles'])
    if args.verify_only:
        print('全部分卷完整；未解压、未启动。')
        return
    target = args.destination.expanduser().resolve()
    if target.exists():
        raise RuntimeError('目标目录已存在；请选择一个新目录，避免覆盖旧状态')
    parent = target.parent
    while not parent.exists():
        parent = parent.parent
    required = manifest['required_free_bytes']
    if shutil.disk_usage(parent).free < required:
        raise RuntimeError(f'目标磁盘至少需要 {required / 2**30:.1f} GiB 可用空间（不含已下载分卷）')
    target.mkdir(mode=0o700, parents=True)
    decoder = Path(__file__).parent / 'bootstrap/zstd'
    for bundle in manifest['bundles']:
        print('正在还原：', bundle['label'], flush=True)
        unpack(args.parts_dir, bundle, target, decoder)
    print(f'完整还原到：{target}\n未启动 QQ。先执行：\n  cd "{target}"\n  ./eve doctor\n  ./eve start\n停止：./eve stop；状态：./eve status')


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, RuntimeError) as error:
        print('还原失败：', error, file=sys.stderr)
        raise SystemExit(1)
