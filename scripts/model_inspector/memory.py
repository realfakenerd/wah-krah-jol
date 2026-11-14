"""Darwin per-process physical-footprint sampler."""
import ctypes
import os
import sys

class Refused(RuntimeError):
    pass

def require(condition, message):
    if not condition:
        raise Refused(message)

class RusageV0(ctypes.Structure):
    _fields_ = [('uuid', ctypes.c_uint8 * 16)] + [(name, ctypes.c_uint64) for name in (
        'user_time', 'system_time', 'pkg_idle_wkups', 'interrupt_wkups', 'pageins',
        'wired_size', 'resident_size', 'phys_footprint', 'proc_start_abstime', 'proc_exit_abstime')]


class Darwin:
    def __init__(self):
        require(sys.platform == 'darwin', 'Darwin sampler required')
        self.lib = ctypes.CDLL('/usr/lib/libproc.dylib', use_errno=True)
        self.lib.proc_pid_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
        self.lib.proc_pid_rusage.restype = ctypes.c_int
        require(ctypes.sizeof(RusageV0) == 96, 'unexpected rusage_info_v0 ABI')
        self.memory(os.getpid())  # Verify availability before any fixture process exists.

    def memory(self, pid):
        info = RusageV0()
        if self.lib.proc_pid_rusage(pid, 0, ctypes.byref(info)):
            raise Refused('proc_pid_rusage failed for ' + str(pid))
        return dict(rss=info.resident_size, footprint=info.phys_footprint)
