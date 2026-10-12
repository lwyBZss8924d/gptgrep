#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
    printf '%s\n' 'usage: sh stage.sh PACKAGE_ROOT EXISTING_TASK_CWD' >&2
    exit 2
fi
if ! command -v python3 >/dev/null 2>&1; then
    printf '%s\n' 'standalone staging requires Python 3 with POSIX dir_fd support' >&2
    exit 2
fi

# Use only the standard library; isolated mode excludes user import hooks.
exec python3 -I -B - "$@" <<'PY'
import contextlib
import hashlib
import os
import stat
import sys

ASSETS = {
    'runtime/codex/AGENTS.md': 'AGENTS.md',
    'runtime/codex/agents/gptgrep-document.toml': '.codex/agents/gptgrep-document.toml',
    'runtime/codex/agents/gptgrep-ocr.toml': '.codex/agents/gptgrep-ocr.toml',
    'runtime/codex/agents/gptgrep-retrieval.toml': '.codex/agents/gptgrep-retrieval.toml',
    'skills/gptgrep/SKILL.md': '.agents/skills/gptgrep/SKILL.md',
    'skills/gptgrep-document/SKILL.md': '.agents/skills/gptgrep-document/SKILL.md',
    'skills/gptgrep-ocr/SKILL.md': '.agents/skills/gptgrep-ocr/SKILL.md',
    'skills/gptgrep-retrieval/SKILL.md': '.agents/skills/gptgrep-retrieval/SKILL.md',
}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def main():
    require(os.name == 'posix' and {os.open, os.mkdir, os.stat} <= os.supports_dir_fd,
            'POSIX descriptor-relative filesystem operations are required')
    directory_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    file_flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC
    bindings = []
    with contextlib.ExitStack() as stack:
        def hold(fd):
            stack.callback(os.close, fd)
            return fd

        def open_directory(parent, name):
            child = hold(os.open(name, directory_flags, dir_fd=parent))
            bindings.append((parent, name, child))
            return child

        def root_directory(path):
            # Walk from / without following symlinks in any path component.
            absolute = os.path.abspath(path)
            fd = hold(os.open('/', directory_flags))
            for name in absolute.split('/')[1:]:
                if name:
                    fd = open_directory(fd, name)
            return fd

        def verify_bindings():
            for parent, name, child in bindings:
                current = os.stat(name, dir_fd=parent, follow_symlinks=False)
                held = os.fstat(child)
                require((current.st_dev, current.st_ino, stat.S_IFMT(current.st_mode)) ==
                        (held.st_dev, held.st_ino, stat.S_IFMT(held.st_mode)),
                        'path changed during staging: ' + name)

        def read_regular(parent, path, limit):
            parts = path.split('/')
            for name in parts[:-1]:
                parent = open_directory(parent, name)
            fd = hold(os.open(parts[-1], file_flags, dir_fd=parent))
            require(stat.S_ISREG(os.fstat(fd).st_mode), 'asset is not a regular file: ' + path)
            data = bytearray()
            while len(data) <= limit:
                chunk = os.read(fd, min(65536, limit + 1 - len(data)))
                if not chunk:
                    break
                data.extend(chunk)
            require(len(data) <= limit, 'asset exceeds size limit: ' + path)
            return bytes(data)

        package = root_directory(sys.argv[1])
        target = root_directory(sys.argv[2])
        require(os.fstat(target).st_uid == os.geteuid(), 'task cwd must be owned by the caller')
        manifest = read_regular(package, 'runtime/codex/assets.sha256', 8192).decode('ascii')
        verified = []
        seen = set()
        for line in manifest.splitlines():
            columns = line.split()
            require(len(columns) == 2, 'invalid asset manifest row')
            expected, source = columns
            require(source in ASSETS and source not in seen, 'unexpected or repeated asset: ' + source)
            require(len(expected) == 64 and all(c in '0123456789abcdef' for c in expected),
                    'invalid asset digest')
            data = read_regular(package, source, 1048576)
            require(hashlib.sha256(data).hexdigest() == expected, 'asset digest mismatch: ' + source)
            verified.append((ASSETS[source], expected, data))
            seen.add(source)
        require(seen == set(ASSETS), 'manifest must contain the exact eight assets')
        for name in ['AGENTS.md', '.agents', '.codex']:
            try:
                os.stat(name, dir_fd=target, follow_symlinks=False)
            except FileNotFoundError:
                continue
            raise ValueError('refusing existing destination: ' + name)
        verify_bindings()

        # No source paths are reopened after validation. Held target directory
        # descriptors prevent a renamed parent from redirecting a later write.
        directories = {'': target}
        for destination, _, _ in verified:
            parent = target
            prefix = ''
            for name in destination.split('/')[:-1]:
                prefix = prefix + '/' + name if prefix else name
                if prefix not in directories:
                    os.mkdir(name, 0o755, dir_fd=parent)
                    directories[prefix] = open_directory(parent, name)
                parent = directories[prefix]
        for destination, expected, data in verified:
            verify_bindings()
            parent_path, _, leaf = destination.rpartition('/')
            parent = directories[parent_path]
            fd = hold(os.open(leaf, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
                              0o644, dir_fd=parent))
            bindings.append((parent, leaf, fd))
            offset = 0
            while offset < len(data):
                written = os.write(fd, data[offset:])
                require(written > 0, 'short write: ' + destination)
                offset += written
            os.lseek(fd, 0, os.SEEK_SET)
            copied = bytearray()
            while len(copied) <= len(data):
                chunk = os.read(fd, min(65536, len(data) + 1 - len(copied)))
                if not chunk:
                    break
                copied.extend(chunk)
            require(hashlib.sha256(copied).hexdigest() == expected, 'staged digest mismatch: ' + destination)
        verify_bindings()
        for destination, expected, _ in verified:
            print(expected + '  ' + destination)


try:
    main()
except (OSError, ValueError) as error:
    print('staging refused: ' + str(error), file=sys.stderr)
    sys.exit(2)
PY
