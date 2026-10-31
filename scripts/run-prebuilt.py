"""运行 CI 已统一编译的目标，避免 cargo run 按单包特征重新编译；缺产物明确失败。"""
import argparse
import os
from pathlib import Path
import subprocess
import sys


def command(target_directory, kind, name, arguments):
    if not name or not all(c.isascii() and (c.isalnum() or c in '_-') for c in name):
        raise ValueError('invalid_target_name')
    if kind not in ('bin', 'example'):
        raise ValueError('invalid_target_kind')
    directory = Path(target_directory) / 'debug'
    if kind == 'example':
        directory /= 'examples'
    path = directory / (name + ('.exe' if os.name == 'nt' else ''))
    if not path.is_file():
        raise ValueError('prebuilt_target_missing:' + str(path))
    return [str(path.resolve()), *arguments]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument('--bin')
    group.add_argument('--example')
    parser.add_argument('arguments', nargs=argparse.REMAINDER)
    options = parser.parse_args()
    arguments = options.arguments
    if arguments[:1] == ['--']:
        arguments = arguments[1:]
    try:
        args = command(os.environ.get('CARGO_TARGET_DIR', 'target'),
                       'bin' if options.bin else 'example', options.bin or options.example, arguments)
        return subprocess.run(args, check=False).returncode
    except (ValueError, OSError) as error:
        print(str(error), file=sys.stderr)
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
