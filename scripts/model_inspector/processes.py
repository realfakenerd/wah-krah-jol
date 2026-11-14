"""Birth-bound process family observation and owned cleanup; no work on import."""
import os
import signal
import subprocess
import time

def require(condition, message):
    if not condition:
        raise RuntimeError(message)

def command(argv):
    return subprocess.check_output(argv, text=True, timeout=5)

def parse_processes(text):
    rows = {}
    # Four integers, five lstart words, then the command path (which can contain spaces).
    for line in text.splitlines():
        fields = line.split(None, 9)
        require(len(fields) == 10, "cannot parse process snapshot")
        pid, ppid, pgid, rss = map(int, fields[:4])
        rows[pid] = {"ppid": ppid, "pgid": pgid, "rss_kib": rss,
                     "birth": " ".join(fields[4:9]), "command": fields[9]}
    return rows

def processes():
    return parse_processes(command(["ps", "-axo", "pid=,ppid=,pgid=,rss=,lstart=,comm="]))

def family(rows, leader, known):
    owned = {pid for pid, row in rows.items() if pid in known and known[pid] == row["birth"]}
    if not known and leader in rows:
        owned.add(leader)
    # A numeric process-group ID alone is insufficient after the leader PID is reused.
    if any(rows[pid]["pgid"] == leader for pid in owned):
        owned.update(pid for pid, row in rows.items() if row["pgid"] == leader)
    changed = True
    while changed:
        extra = {pid for pid, row in rows.items() if row["ppid"] in owned}
        changed = not extra.issubset(owned)
        owned.update(extra)
    return owned

def cleanup(child, known):
    # Only signal the group we created and descendants whose PID/start identity still matches.
    for sig in [signal.SIGTERM, signal.SIGKILL]:
        rows = processes()
        owned = family(rows, child.pid, known)
        for pid in owned:
            known[pid] = rows[pid]["birth"]
        if any(rows[pid]["pgid"] == child.pid for pid in owned):
            try:
                os.killpg(child.pid, sig)
            except ProcessLookupError:
                pass
        for pid in owned:
            if rows[pid]["pgid"] != child.pid:
                try:
                    os.kill(pid, sig)
                except ProcessLookupError:
                    pass
        try:
            child.wait(timeout=1)
        except subprocess.TimeoutExpired:
            pass
        time.sleep(0.1)
    rows = processes()
    require(not family(rows, child.pid, known), "owned native descendants remain; keep workspace lease until resolved")
