"""Unpacks a plugin job's result tar stream into a fresh directory (spec 18 §5.6).

    <tar stream> | python3 -I scripts/herdr-plugins/unpack_results.py <new-dir>

The stream comes from `tar -cf - .`, run as the boundary user over the results directory, so
its content is hostile. The trusted runner user runs this script, and it never trusts a name,
type or size from the stream:

- it creates <new-dir> itself and fails if it already exists;
- it accepts only regular files and directories. Symbolic links, hard links, devices and FIFOs
  are skipped, so nothing it writes can point elsewhere;
- names must be relative, use only `A-Za-z0-9_.-` in each component, contain no `.` or `..`
  components and stay short and shallow;
- it creates every file with O_CREAT | O_EXCL | O_NOFOLLOW and mode 0644, so an existing path is
  never overwritten or followed;
- it limits the size of each file, the number of entries, the total size and the raw stream.

Exit status: 0 when everything was unpacked, 1 when something was skipped or a limit stopped
the unpacking (what was unpacked so far stays), 2 on a usage error.
"""

import os
import re
import sys
import tarfile

MAX_FILE = 8 * 1024 * 1024
MAX_TOTAL = 256 * 1024 * 1024
MAX_ENTRIES = 2000
MAX_STREAM = MAX_TOTAL + 64 * 1024 * 1024
MAX_NAME = 256
MAX_DEPTH = 8
COMPONENT = re.compile(r"[A-Za-z0-9_.-]{1,100}")


class LimitedReader:
    """A read-only stream that fails after `limit` bytes."""

    def __init__(self, raw, limit):
        self.raw = raw
        self.left = limit

    def read(self, n=-1):
        if self.left <= 0:
            raise OSError("the tar stream is larger than the limit")
        if n is None or n < 0 or n > self.left:
            n = self.left
        data = self.raw.read(n)
        self.left -= len(data)
        return data


def safe_parts(name):
    """The path components of a member name, or None when the name is not acceptable."""
    if name.startswith("./"):
        name = name[2:]
    name = name.rstrip("/")
    if name in ("", "."):
        return []
    if len(name) > MAX_NAME or name.startswith("/"):
        return None
    parts = name.split("/")
    if len(parts) > MAX_DEPTH:
        return None
    for p in parts:
        if p in (".", "..") or not COMPONENT.fullmatch(p):
            return None
    return parts


def unpack(stream, dest, log):
    os.mkdir(dest, 0o755)
    ok = True
    entries = 0
    total = 0
    try:
        with tarfile.open(fileobj=LimitedReader(stream, MAX_STREAM), mode="r|") as tar:
            for m in tar:
                entries += 1
                if entries > MAX_ENTRIES:
                    log(f"stopped: more than {MAX_ENTRIES} entries")
                    return False
                parts = safe_parts(m.name)
                shown = repr(m.name[:120])
                if parts is None:
                    log(f"skipped {shown}: unsafe name")
                    ok = False
                    continue
                if not parts:
                    continue
                if m.isdir():
                    path = dest
                    for p in parts:
                        path = os.path.join(path, p)
                        if not os.path.lexists(path):
                            os.mkdir(path, 0o755)
                        elif os.path.islink(path) or not os.path.isdir(path):
                            log(f"skipped {shown}: path conflict")
                            ok = False
                            break
                    continue
                if not m.isreg():
                    log(f"skipped {shown}: not a regular file")
                    ok = False
                    continue
                if m.size > MAX_FILE:
                    log(f"skipped {shown}: larger than {MAX_FILE} bytes")
                    ok = False
                    continue
                if total + m.size > MAX_TOTAL:
                    log(f"stopped: more than {MAX_TOTAL} bytes in total")
                    return False
                parent = dest
                for p in parts[:-1]:
                    parent = os.path.join(parent, p)
                    if not os.path.lexists(parent):
                        os.mkdir(parent, 0o755)
                    elif os.path.islink(parent) or not os.path.isdir(parent):
                        parent = None
                        break
                if parent is None:
                    log(f"skipped {shown}: path conflict")
                    ok = False
                    continue
                path = os.path.join(parent, parts[-1])
                try:
                    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o644)
                except OSError as e:
                    log(f"skipped {shown}: {e.strerror}")
                    ok = False
                    continue
                src = tar.extractfile(m)
                written = 0
                with os.fdopen(fd, "wb") as out:
                    while written < m.size:
                        chunk = src.read(min(1024 * 1024, m.size - written))
                        if not chunk:
                            break
                        out.write(chunk)
                        written += len(chunk)
                total += written
    except (tarfile.TarError, OSError, EOFError) as e:
        log(f"stopped: {e}")
        return False
    return ok


def main(argv):
    if len(argv) != 2 or not os.path.isabs(argv[1]):
        print("usage: unpack_results.py <new absolute directory> < stream.tar", file=sys.stderr)
        return 2
    ok = unpack(sys.stdin.buffer, argv[1], lambda msg: print(f"unpack_results.py: {msg}", file=sys.stderr))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
