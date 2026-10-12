#!/usr/bin/env python3
"""coding_agent's terminal mode on a real (pseudo-)terminal, as a user runs it.

The headless `--demo` never touches the keyboard, so it cannot catch a
terminal UI that the OS stops: since rutis 0.9 runtimes run in process groups
of their own, and one that reads or sets up the terminal from there is
stopped (SIGTTIN / SIGTTOU). This drives the real mode: the screen must draw,
a typed prompt must run, and no process of the agent may be stopped.

Usage: terminal_pty.py <path to the built coding_agent example> (Unix).
"""
import os
import pty
import select
import signal
import subprocess
import sys
import time

exe = sys.argv[1]
pid, fd = pty.fork()
if pid == 0:
    os.execv(exe, [exe, "--no-dsh"])

screen = b""


def read_until(needle, seconds):
    """Read the terminal until `needle` shows (True) or time is up (False)."""
    global screen
    end = time.time() + seconds
    while time.time() < end:
        if needle in screen:
            return True
        ready, _, _ = select.select([fd], [], [], 0.2)
        if ready:
            try:
                screen += os.read(fd, 65536)
            except OSError:
                break
    return needle in screen


def tree():
    """(pid, stat, command) of the agent and everything under it."""
    out = subprocess.run(
        ["ps", "-A", "-o", "pid=,ppid=,stat=,command="], capture_output=True, text=True
    ).stdout
    rows = [line.split(None, 3) for line in out.splitlines() if line.strip()]
    pids, grew = {pid}, True
    while grew:
        grew = False
        for row in rows:
            if int(row[1]) in pids and int(row[0]) not in pids:
                pids.add(int(row[0]))
                grew = True
    return [(int(r[0]), r[2], r[3] if len(r) > 3 else "") for r in rows if int(r[0]) in pids]


def stopped():
    return [f"{p} {cmd[:100]}" for p, stat, cmd in tree() if stat.startswith("T")]


failures = []
if not read_until(b"terminal frontend", 90):
    failures.append("the terminal UI never drew its header")
time.sleep(1)
os.write(fd, b"list the files here\r")
if not read_until(b"list_files", 60):
    failures.append("the typed prompt did not run a tool")
if stopped():
    failures.append(f"stopped processes (SIGTTIN/SIGTTOU?): {stopped()}")

procs = tree()
os.kill(pid, signal.SIGTERM)
time.sleep(1)
for p, _, _ in procs:
    try:
        os.kill(p, signal.SIGKILL)
    except ProcessLookupError:
        pass

if failures:
    print("FAILED:", "; ".join(failures))
    print("screen (last 2000 bytes):", screen[-2000:].decode("utf-8", "replace"))
    sys.exit(1)
print("ok: the terminal UI drew, ran a typed prompt, and nothing was stopped")
