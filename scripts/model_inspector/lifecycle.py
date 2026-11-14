"""Shared native/qualification ownership interval; no process is created on import."""
import signal
import subprocess
import sys

HANDLED = (signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGALRM)


class Interrupted(RuntimeError):
    def __init__(self, signum):
        self.signum = signum
        super().__init__('controller signal ' + str(signum))


class OwnedChild:
    def __init__(self, materials):
        self.materials = materials
        self.child = None
        self.known = {}
        self.spawning = False
        self.pending_signal = None
        self.attempted = False
        self.cleanup_attempted = False
        self.previous = {}

    def interrupt(self, signum, frame):
        if self.spawning:
            self.pending_signal = signum
            return
        raise Interrupted(signum)

    def install(self):
        self.previous = {sig: signal.getsignal(sig) for sig in HANDLED}
        for sig in HANDLED:
            signal.signal(sig, self.interrupt)

    def arm(self, seconds):
        signal.setitimer(signal.ITIMER_REAL, seconds)

    def spawn(self, argv, *, factory=None, **kwargs):
        if self.attempted:
            raise RuntimeError('one spawn attempt per ownership interval')
        if kwargs.get('start_new_session') is not True:
            raise RuntimeError('new owned process session required')
        self.attempted = True
        # A handled signal must not unwind Popen before its returned handle is stored.
        # Keep it deferred through the first PID/birth snapshot as well. Do not poll/reap
        # the new child before that snapshot: its PID must not become reusable here.
        self.spawning = True
        try:
            self.child = (factory or subprocess.Popen)(argv, **kwargs)
            self.observe()
        finally:
            self.spawning = False
        if self.pending_signal is not None:
            raise Interrupted(self.pending_signal)
        return self.child

    def observe(self):
        if self.child is None:
            return {}, set()
        rows = self.materials.processes()
        owned = self.materials.family(rows, self.child.pid, self.known)
        self.known.update({pid: rows[pid]['birth'] for pid in owned})
        return rows, owned

    def mask_cleanup(self):
        self.arm(0)
        for sig in HANDLED:
            signal.signal(sig, signal.SIG_IGN)

    def cleanup(self):
        # No second signal or wall alarm may unwind cleanup and release the lease early.
        self.mask_cleanup()
        if self.cleanup_attempted:
            raise RuntimeError('cleanup must not be repeated after failure')
        self.cleanup_attempted = True
        if self.child is not None:
            self.materials.cleanup(self.child, self.known)
            _, remaining = self.observe()
            if remaining:
                raise RuntimeError('owned family remains after cleanup')
            for name in ('stdin', 'stdout', 'stderr'):
                stream = getattr(self.child, name, None)
                if stream is not None:
                    stream.close()

    def restore(self):
        for sig, previous in self.previous.items():
            signal.signal(sig, previous)

    def hold(self, write_receipt):
        self.mask_cleanup()
        try:
            write_receipt()
        except BaseException as exc:
            print('LEASE HELD: receipt failed: ' + str(exc), file=sys.stderr, flush=True)
        # No re-signalling or automatic release, even if receipt storage failed.
        while True:
            signal.pause()

    def description(self):
        return dict(pid=self.child.pid if self.child is not None else None,
                    known=self.known, pending_signal=self.pending_signal,
                    spawning=self.spawning, cleanup_attempted=self.cleanup_attempted)
