"""Bounded formats shared by the converter and sandbox programs."""
from __future__ import annotations

import hashlib
import json
import math
import os
from pathlib import Path, PurePosixPath
import re
import stat

ARMS = ("gptgrep", "raw_only")
PROFILE = {"model": "gpt-6-luna", "reasoning_effort": "max", "service_tier": "fast"}
MAX_JSON = 4 * 1024 * 1024
MAX_ANSWER = 1024 * 1024
# The product's positional multimodal question front door is narrower than the
# private row parser. Both reader arms share this UTF-8 byte bound.
MAX_READER_QUESTION_BYTES = 8192
MAX_NATIVE_PROMPT_BYTES = MAX_JSON


def require(condition, code):
    if not condition:
        raise ValueError(code)


def integer(value, low=0, high=2**63 - 1):
    require(type(value) is int and low <= value <= high, "integer_range")
    return value


def digest(value):
    require(isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value), "sha256")
    return value


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False,
                      allow_nan=False).encode("utf-8")


def sha(data):
    return hashlib.sha256(data).hexdigest()


def duplicate_free(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, "duplicate_json_key")
        result[key] = value
    return result


def parse(data):
    require(len(data) <= MAX_JSON, "json_size")
    return json.loads(data, object_pairs_hook=duplicate_free,
                      parse_constant=lambda _: (_ for _ in ()).throw(ValueError("nonfinite")))


def read(path, cap=MAX_JSON):
    path = Path(path)
    require(path.is_absolute() and path.resolve() == path, "physical_absolute_path")
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        before = os.fstat(fd)
        require(stat.S_ISREG(before.st_mode) and before.st_size <= cap, "regular_bounded_file")
        chunks, count = [], 0
        while True:
            part = os.read(fd, min(65536, cap + 1 - count))
            if not part:
                break
            count += len(part)
            require(count <= cap, "file_grew")
            chunks.append(part)
        after = os.fstat(fd)
        require((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) ==
                (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns), "file_changed")
        return b"".join(chunks)
    finally:
        os.close(fd)


def pinned(pin, cap=MAX_JSON):
    require(type(pin) is dict and set(pin) == {"path", "bytes", "sha256"}, "pin_shape")
    integer(pin["bytes"], 0, cap)
    data = read(pin["path"], cap)
    require(len(data) == pin["bytes"] and sha(data) == digest(pin["sha256"]), "pin_mismatch")
    return data


def copy_asset(pin, cap, destination=None):
    """Hash/copy one finite admitted asset with bounded memory and no symlinks."""
    require(type(pin) is dict and set(pin) == {"path", "bytes", "sha256"}, "pin_shape")
    integer(pin["bytes"], 0, cap)
    path = Path(pin["path"])
    require(path.is_absolute() and path.resolve() == path, "asset_physical_path")
    source = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    target = None
    try:
        before = os.fstat(source)
        require(stat.S_ISREG(before.st_mode) and before.st_size == pin["bytes"], "asset_size")
        if destination is not None:
            destination = Path(destination)
            destination.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            require(destination.parent.resolve() == destination.parent, "asset_destination")
            target = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o400)
        hasher, total = hashlib.sha256(), 0
        while True:
            block = os.read(source, 1048576)
            if not block:
                break
            total += len(block)
            require(total <= pin["bytes"], "asset_growth")
            hasher.update(block)
            if target is not None:
                view = memoryview(block)
                while view:
                    n = os.write(target, view)
                    require(n > 0, "asset_short_write")
                    view = view[n:]
        after = os.fstat(source)
        require((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) ==
                (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns), "asset_changed")
        require(total == pin["bytes"] and hasher.hexdigest() == digest(pin["sha256"]), "asset_digest")
        if target is not None:
            os.fsync(target)
    finally:
        os.close(source)
        if target is not None:
            os.close(target)


def relative(value):
    require(isinstance(value, str) and value and "\\" not in value, "relative_path")
    path = PurePosixPath(value)
    require(not path.is_absolute() and path.as_posix() == value and
            not any(p in ("", ".", "..") for p in path.parts), "relative_path")
    return value


def put(path, data):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    require(path.parent.resolve() == path.parent, "output_parent_symlink")
    if not isinstance(data, bytes):
        data = canonical(data) + b"\n"
    require(len(data) <= MAX_JSON, "output_size")
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        view = memoryview(data)
        while view:
            n = os.write(fd, view)
            require(n > 0, "short_write")
            view = view[n:]
        os.fsync(fd)
    finally:
        os.close(fd)


def finite_number(value):
    return type(value) in (int, float) and math.isfinite(value) and value >= 0


def binding_equal(left, right):
    keys = ("pair_id", "document_id", "cohort_sha256", "runtime_sha256",
            "judge_sha256", "question_sha256", "document_sha256")
    return all(left.get(k) == right.get(k) for k in keys)


def validate_binding(value):
    hashes = ("cohort_sha256", "runtime_sha256", "judge_sha256", "question_sha256", "document_sha256")
    require(type(value) is dict and set(value) == {"pair_id", "document_id", "arm", *hashes}, "binding_fields")
    require(value["arm"] in ARMS, "binding_arm")
    for key in ("pair_id", "document_id"):
        require(isinstance(value[key], str) and 0 < len(value[key].encode()) <= 1048576, "binding_id")
    for key in hashes:
        digest(value[key])
    return value
