#!/usr/bin/env python3
"""Soak test scaffold (spec 10 section 7): one isolated server, N panes of mixed workloads, a
poller and an event reader, for a fixed time, then the leak and integrity assertions that need
no fixture agents. The full 24 h / 50-pane run with recorded harness sessions, Vite previews and
a TUI client needs a dedicated host (spec 10 section 2.1); this is the part that runs anywhere,
and the structure the rest plugs into (add workloads in `start_workloads`, add checks in
`assertions`).

  scripts/soak.py --bin target/release/vibeke --seconds 600 --panes 50

Assertions (spec 10 section 7): server RSS growth <= 10% after warm-up, file-descriptor count
stable, event `seq` gapless as read by a poller, every pane's process still alive and the pane
list complete, zero panics in the server logs. Output is one JSON summary on stdout and, with
--report, in a file. Exit 0 pass, 1 an assertion failed, 2 setup error. Everything it starts is
stopped on exit (`server stop --kill-panes`).
"""
import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

WORKLOADS = [
    # (name, command): the mix scales to N panes round-robin.
    ("output", "i=0; while :; do i=$((i+1)); echo out-$i; sleep 0.05; done"),
    ("idle", "sleep 86400"),
    ("burst", "while :; do seq 1 500; sleep 1; done"),
    ("shell", "/bin/sh"),
]


class Soak:
    def __init__(self, a):
        self.a = a
        self.dir = tempfile.mkdtemp(prefix="vksoak", dir="/tmp")
        self.env = dict(os.environ)
        self.env.update(
            VIBEKE_RUNTIME_DIR=os.path.join(self.dir, "run"),
            VIBEKE_STATE_DIR=os.path.join(self.dir, "state"),
            VIBEKE_CONFIG=os.path.join(self.dir, "config.toml"),
        )
        for k in ("VIBEKE", "VIBEKE_SOCKET", "VIBEKE_SESSION", "VIBEKE_PANE_TOKEN"):
            self.env.pop(k, None)
        self.panes = []
        self.next_seq = 1
        self.seq_gaps = []
        self.rss, self.fds = [], []

    def vk(self, *args, check=True):
        p = subprocess.run([self.a.bin, "--json", *args], env=self.env, capture_output=True, text=True)
        if check and p.returncode != 0:
            raise RuntimeError(f"vibeke {' '.join(args)}: {p.stderr.strip()}")
        return json.loads(p.stdout) if p.stdout.strip() else {}

    def server_pid(self):
        try:
            return int(open(os.path.join(self.dir, "run/default/server.pid")).read().strip())
        except (OSError, ValueError):
            return 0

    def start_workloads(self):
        first = self.vk("workspace", "create", "--cwd", "/tmp", "--command", WORKLOADS[0][1])
        root = first["root_pane"]["id"]
        self.panes.append(root)
        for i in range(1, self.a.panes):
            _, cmd = WORKLOADS[i % len(WORKLOADS)]
            p = self.vk("pane", "split", root, "--direction", "right" if i % 2 else "down", "--command", cmd)
            self.panes.append(p["pane"]["id"])

    def poll_events(self):
        """Read new events; record any hole in the seq numbering."""
        while True:
            r = self.vk("events", "read", "--after", str(self.next_seq - 1), "--limit", "500")
            evs = r.get("events", [])
            for e in evs:
                if e["seq"] != self.next_seq:
                    self.seq_gaps.append((self.next_seq, e["seq"]))
                self.next_seq = e["seq"] + 1
            if len(evs) < 500:
                return

    def sample(self):
        pid = self.server_pid()
        if pid <= 0:
            return
        out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()
        if out:
            self.rss.append(int(out))
        fd_dir = f"/proc/{pid}/fd"
        if os.path.isdir(fd_dir):
            self.fds.append(len(os.listdir(fd_dir)))
        else:
            lsof = subprocess.run(["lsof", "-p", str(pid)], capture_output=True, text=True).stdout
            if lsof:
                self.fds.append(max(0, len(lsof.splitlines()) - 1))

    def assertions(self):
        a, fails = self.a, []
        listed = self.vk("pane", "list").get("panes", [])
        if len(listed) != len(self.panes):
            fails.append(f"pane list has {len(listed)} panes, expected {len(self.panes)}")
        for p in listed:
            pid = p.get("child_pid")
            if not pid or not alive(pid):
                fails.append(f"pane {p.get('id')} process {pid} is not alive")
        if self.seq_gaps:
            fails.append(f"event seq gaps: {self.seq_gaps[:5]}")
        warm = max(1, int(len(self.rss) * a.warmup))
        rss = self.rss[warm:]
        growth = None
        if len(rss) >= 6:
            head, tail = statistics.median(rss[:3]), statistics.median(rss[-3:])
            growth = (tail - head) / head
            if growth > a.rss_growth:
                fails.append(f"server RSS grew {growth * 100:.1f}% after warm-up (limit {a.rss_growth * 100:.0f}%)")
        fds = self.fds[warm:]
        if len(fds) >= 6 and max(fds[-3:]) > min(fds[:3]) + a.fd_slack:
            fails.append(f"fd count grew from {min(fds[:3])} to {max(fds[-3:])}")
        for root, _, files in os.walk(os.path.join(self.dir, "state")):
            for f in files:
                if f.endswith(".log") or ".log." in f:
                    text = open(os.path.join(root, f), errors="replace").read()
                    if "panicked at" in text:
                        fails.append(f"panic in {f}")
        return fails, growth

    def run(self):
        a = self.a
        self.start_workloads()
        end = time.time() + a.seconds
        last_sample = 0.0
        while time.time() < end:
            self.poll_events()
            if time.time() - last_sample >= a.sample_every:
                self.sample()
                last_sample = time.time()
            # The "phone-companion-like" poller: a snapshot-ish read every cycle.
            self.vk("pane", "list")
            time.sleep(a.poll_every)
        self.poll_events()
        self.sample()
        fails, growth = self.assertions()
        summary = {
            "panes": len(self.panes),
            "seconds": a.seconds,
            "events_seen": self.next_seq - 1,
            "rss_kb_first_last": [self.rss[0], self.rss[-1]] if self.rss else None,
            "rss_growth_after_warmup": None if growth is None else round(growth, 4),
            "fds_first_last": [self.fds[0], self.fds[-1]] if self.fds else None,
            "failures": fails,
        }
        return summary

    def cleanup(self):
        try:
            subprocess.run([self.a.bin, "--json", "server", "stop", "--kill-panes"], env=self.env,
                           capture_output=True, timeout=30)
        except (OSError, subprocess.SubprocessError):
            pass
        shutil.rmtree(self.dir, ignore_errors=True)


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--bin", default="target/release/vibeke")
    ap.add_argument("--panes", type=int, default=50)
    ap.add_argument("--seconds", type=int, default=600)
    ap.add_argument("--warmup", type=float, default=0.2, help="fraction of samples ignored")
    ap.add_argument("--rss-growth", type=float, default=0.10)
    ap.add_argument("--fd-slack", type=int, default=8)
    ap.add_argument("--poll-every", type=float, default=2.0)
    ap.add_argument("--sample-every", type=float, default=5.0)
    ap.add_argument("--report")
    a = ap.parse_args(argv)
    if not os.access(a.bin, os.X_OK):
        print(f"soak: {a.bin} is not executable (cargo build --release -p vibeke)", file=sys.stderr)
        return 2
    s = Soak(a)
    try:
        summary = s.run()
    except Exception as e:  # setup or API failure: report as an error, not an assertion
        print(f"soak: {e}", file=sys.stderr)
        return 2
    finally:
        s.cleanup()
    text = json.dumps(summary, indent=2)
    print(text)
    if a.report:
        with open(a.report, "w") as f:
            f.write(text + "\n")
    return 1 if summary["failures"] else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
