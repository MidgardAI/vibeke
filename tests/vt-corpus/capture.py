#!/usr/bin/env python3
"""Record raw PTY output of a command at a fixed size (vt-corpus fixtures).
usage: capture.py OUT COLS ROWS [--keys 'bytes with \\n escapes' --delay S] -- CMD...
"""
import os, pty, sys, time, select, fcntl, termios, struct, signal
args = sys.argv[1:]
out, cols, rows = args[0], int(args[1]), int(args[2])
rest = args[3:]
keys, delay = b"", 0.5
if "--keys" in rest:
    i = rest.index("--keys"); keys = rest[i+1].encode().decode("unicode_escape").encode("latin1"); rest = rest[:i] + rest[i+2:]
if "--delay" in rest:
    i = rest.index("--delay"); delay = float(rest[i+1]); rest = rest[:i] + rest[i+2:]
cmd = rest[rest.index("--")+1:]
pid, fd = pty.fork()
if pid == 0:
    os.environ["TERM"] = "xterm-256color"; os.environ["COLUMNS"] = str(cols); os.environ["LINES"] = str(rows)
    os.execvp(cmd[0], cmd)
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
data = b""; start = time.time(); sent = False; ki = 0
while time.time() - start < 20:
    r, _, _ = select.select([fd], [], [], 0.05)
    if r:
        try: chunk = os.read(fd, 65536)
        except OSError: break
        if not chunk: break
        data += chunk
    elif keys and time.time() - start > delay and ki < len(keys):
        os.write(fd, keys[ki:ki+1]); ki += 1
        time.sleep(0.02)
try: os.kill(pid, signal.SIGKILL)
except Exception: pass
open(out, "wb").write(data)
print(out, len(data))
