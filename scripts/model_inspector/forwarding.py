"""Defer outer-wrapper signals until its managed worker has cleaned up."""
import json
import os
import signal

from . import processes

HANDLED = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)


class ForwardSignals:
    """Forward only to the current direct worker with its recorded birth identity."""
    def __init__(self, output):
        self.output = output
        self.previous = {}

    def forward(self, signum, frame):
        # A request before worker publication is observed before that worker spawns anything.
        request = self.output / 'stop-request.json'
        request.write_text(json.dumps({'signal': signum}) + '\n')
        identity = self.output / 'worker.json'
        if not identity.exists():
            return
        worker = json.loads(identity.read_text())
        row = processes.processes().get(worker['pid'])
        if row and row['birth'] == worker['birth'] and row['ppid'] == os.getpid():
            try:
                os.kill(worker['pid'], signum)
            except ProcessLookupError:
                pass

    def __enter__(self):
        self.previous = {sig: signal.getsignal(sig) for sig in HANDLED}
        for sig in HANDLED:
            signal.signal(sig, self.forward)
        return self

    def __exit__(self, *args):
        for sig, previous in self.previous.items():
            signal.signal(sig, previous)
