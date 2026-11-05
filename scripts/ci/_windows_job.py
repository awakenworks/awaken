"""Windows deadline process ownership; imported only on native Windows.

The deadline owner holds the sole lasting Job handle. A Python trampoline
joins that Job before launching any requested command, then closes its handle.
This avoids the child-spawn race of assigning an already-running command.
https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects
"""
from __future__ import annotations

import ctypes
from ctypes import wintypes
import time
import uuid


class _BasicLimits(ctypes.Structure):
    _fields_ = [
        ("PerProcessUserTimeLimit", ctypes.c_int64), ("PerJobUserTimeLimit", ctypes.c_int64),
        ("LimitFlags", wintypes.DWORD), ("MinimumWorkingSetSize", ctypes.c_size_t),
        ("MaximumWorkingSetSize", ctypes.c_size_t), ("ActiveProcessLimit", wintypes.DWORD),
        ("Affinity", ctypes.c_size_t), ("PriorityClass", wintypes.DWORD),
        ("SchedulingClass", wintypes.DWORD),
    ]


class _IoCounters(ctypes.Structure):
    _fields_ = [(name, ctypes.c_uint64) for name in (
        "ReadOperationCount", "WriteOperationCount", "OtherOperationCount",
        "ReadTransferCount", "WriteTransferCount", "OtherTransferCount",
    )]


class _ExtendedLimits(ctypes.Structure):
    _fields_ = [
        ("BasicLimitInformation", _BasicLimits), ("IoInfo", _IoCounters),
        ("ProcessMemoryLimit", ctypes.c_size_t), ("JobMemoryLimit", ctypes.c_size_t),
        ("PeakProcessMemoryUsed", ctypes.c_size_t), ("PeakJobMemoryUsed", ctypes.c_size_t),
    ]


class _BasicAccounting(ctypes.Structure):
    _fields_ = [(name, ctypes.c_int64) for name in (
        'TotalUserTime', 'TotalKernelTime', 'ThisPeriodTotalUserTime', 'ThisPeriodTotalKernelTime')]
    _fields_ += [(name, wintypes.DWORD) for name in (
        'TotalPageFaultCount', 'TotalProcesses', 'ActiveProcesses', 'TotalTerminatedProcesses')]


_kernel = ctypes.WinDLL("kernel32", use_last_error=True)
_kernel.CreateJobObjectW.argtypes = (ctypes.c_void_p, wintypes.LPCWSTR)
_kernel.CreateJobObjectW.restype = wintypes.HANDLE
_kernel.SetInformationJobObject.argtypes = (wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD)
_kernel.SetInformationJobObject.restype = wintypes.BOOL
_kernel.OpenJobObjectW.argtypes = (wintypes.DWORD, wintypes.BOOL, wintypes.LPCWSTR)
_kernel.OpenJobObjectW.restype = wintypes.HANDLE
_kernel.GetCurrentProcess.argtypes = ()
_kernel.GetCurrentProcess.restype = wintypes.HANDLE
_kernel.AssignProcessToJobObject.argtypes = (wintypes.HANDLE, wintypes.HANDLE)
_kernel.AssignProcessToJobObject.restype = wintypes.BOOL
_kernel.CloseHandle.argtypes = (wintypes.HANDLE,)
_kernel.CloseHandle.restype = wintypes.BOOL
_kernel.TerminateJobObject.argtypes = (wintypes.HANDLE, wintypes.UINT)
_kernel.TerminateJobObject.restype = wintypes.BOOL
_kernel.QueryInformationJobObject.argtypes = (
    wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD, ctypes.c_void_p)
_kernel.QueryInformationJobObject.restype = wintypes.BOOL
_kernel.OpenProcess.argtypes = (wintypes.DWORD, wintypes.BOOL, wintypes.DWORD)
_kernel.OpenProcess.restype = wintypes.HANDLE
_kernel.IsProcessInJob.argtypes = (wintypes.HANDLE, wintypes.HANDLE, ctypes.POINTER(wintypes.BOOL))
_kernel.IsProcessInJob.restype = wintypes.BOOL
_kernel.WaitForSingleObject.argtypes = (wintypes.HANDLE, wintypes.DWORD)
_kernel.WaitForSingleObject.restype = wintypes.DWORD


class WindowsJob:
    def __init__(self) -> None:
        self.name = f"awaken-ci-deadline-{uuid.uuid4().hex}"
        self.handle = _kernel.CreateJobObjectW(None, self.name)
        if not self.handle:
            raise ctypes.WinError(ctypes.get_last_error())
        limits = _ExtendedLimits()
        limits.BasicLimitInformation.LimitFlags = 0x2000  # KILL_ON_JOB_CLOSE; no breakaway
        if not _kernel.SetInformationJobObject(self.handle, 9, ctypes.byref(limits), ctypes.sizeof(limits)):
            error = ctypes.WinError(ctypes.get_last_error())
            self.close()
            raise error

    def close(self) -> None:
        if self.handle:
            handle, self.handle = self.handle, None
            if not _kernel.CloseHandle(handle):
                raise ctypes.WinError(ctypes.get_last_error())

    def terminate_and_wait(self, timeout: float = 5) -> None:
        """Drain only this owned Job before a caller removes its fixture files."""
        if not self.handle:
            return
        until = time.monotonic() + timeout
        capacity = 64
        handles = []
        try:
            while True:
                buffer = ctypes.create_string_buffer(8 + ctypes.sizeof(ctypes.c_size_t) * capacity)
                counts = (wintypes.DWORD * 2).from_buffer(buffer)
                if _kernel.QueryInformationJobObject(self.handle, 3, buffer, len(buffer), None):
                    if counts[1] == counts[0]:
                        break
                elif ctypes.get_last_error() != 234:  # ERROR_MORE_DATA
                    raise ctypes.WinError(ctypes.get_last_error())
                if time.monotonic() >= until:
                    raise TimeoutError('Cannot capture the owned Windows Job process list')
                capacity = max(capacity * 2, counts[0])
            for pid in (ctypes.c_size_t * counts[1]).from_buffer(buffer, 8):
                # Retain process objects, so PID reuse cannot change the wait's
                # subject. No process outside this exact Job is ever stopped.
                handle = _kernel.OpenProcess(0x00101000, False, pid)  # SYNCHRONIZE | QUERY_LIMITED_INFORMATION
                if not handle:
                    if ctypes.get_last_error() == 87:  # already exited
                        continue
                    raise ctypes.WinError(ctypes.get_last_error())
                handles.append(handle)
                member = wintypes.BOOL()
                if not _kernel.IsProcessInJob(handle, self.handle, ctypes.byref(member)):
                    raise ctypes.WinError(ctypes.get_last_error())
                if not member.value:
                    raise RuntimeError('Owned Windows Job membership changed before termination')
            if not _kernel.TerminateJobObject(self.handle, 1):
                raise ctypes.WinError(ctypes.get_last_error())
            for handle in handles:
                result = _kernel.WaitForSingleObject(handle, max(0, int((until - time.monotonic()) * 1000)))
                if result == 0xFFFFFFFF:
                    raise ctypes.WinError(ctypes.get_last_error())
                if result != 0:
                    raise TimeoutError('Owned Windows process did not finish termination')
        finally:
            for handle in handles:
                _kernel.CloseHandle(handle)
        while True:
            accounting = _BasicAccounting()
            if not _kernel.QueryInformationJobObject(
                    self.handle, 1, ctypes.byref(accounting), ctypes.sizeof(accounting), None):
                raise ctypes.WinError(ctypes.get_last_error())
            if accounting.ActiveProcesses == 0:
                return
            remaining = until - time.monotonic()
            if remaining <= 0:
                raise TimeoutError('Owned Windows Job did not drain before fixture cleanup')
            # awaken-allow(wait): actual owned Job accounting acknowledges exit.
            time.sleep(min(.01, remaining))

    def __enter__(self) -> WindowsJob:
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


def attach_current_process(name: str) -> None:
    handle = _kernel.OpenJobObjectW(0x0001, False, name)  # JOB_OBJECT_ASSIGN_PROCESS
    if not handle:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        if not _kernel.AssignProcessToJobObject(handle, _kernel.GetCurrentProcess()):
            raise ctypes.WinError(ctypes.get_last_error())
    finally:
        # The owner retains the only lasting handle. Owner death also closes it
        # and kills the entire Job, including descendants after leader exit.
        if not _kernel.CloseHandle(handle):
            raise ctypes.WinError(ctypes.get_last_error())
