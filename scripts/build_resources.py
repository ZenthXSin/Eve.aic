"""只读设备资源探测与可替换的编译并发策略，不修改主机或 Eve 的资源限制。"""
from dataclasses import asdict, dataclass
import math
import os
from pathlib import Path

MIB = 1024 ** 2
GIB = 1024 ** 3


def number(path, allow_zero=False):
    try:
        value = int(path.read_text().strip())
        return value if (0 <= value if allow_zero else 0 < value) and value < 2 ** 60 else None
    except (OSError, ValueError):
        return None


def values(path):
    try:
        return {parts[0].rstrip(':'): int(parts[1]) for line in path.read_text().splitlines()
                if len(parts := line.split()) >= 2 and parts[1].isdigit()}
    except OSError:
        return {}


def cgroup_paths(proc, root, controller=None):
    base = root / controller if controller else root
    result = [base]
    try:
        for line in (proc / 'self/cgroup').read_text().splitlines():
            _, controllers, relative = line.split(':', 2)
            if (controller and controller not in controllers.split(',')) or (not controller and controllers):
                continue
            relative = Path(relative.lstrip('/'))
            if '..' in relative.parts: continue
            current = base / relative
            while current != base and base in current.parents:
                result.append(current); current = current.parent
    except (OSError, ValueError):
        pass
    return list(dict.fromkeys(result))


@dataclass(frozen=True)
class Resources:
    cpu_capacity: float
    total_memory: int | None
    available_memory: int | None
    load: float | None


class ResourceProbe:
    def __init__(self, proc=Path('/proc'), cgroup=Path('/sys/fs/cgroup'), windows=None):
        self.proc, self.cgroup = proc, cgroup
        self.windows = os.name == 'nt' if windows is None else windows

    def snapshot(self):
        cpus = float(os.cpu_count() or 1)
        if hasattr(os, 'sched_getaffinity'):
            try: cpus = min(cpus, len(os.sched_getaffinity(0)))
            except OSError: pass
        memory = values(self.proc / 'meminfo')
        total = memory.get('MemTotal', 0) * 1024 or None
        available = memory['MemAvailable'] * 1024 if 'MemAvailable' in memory else None
        if self.windows:
            import ctypes
            class Memory(ctypes.Structure):
                _fields_ = [('length', ctypes.c_ulong), ('load', ctypes.c_ulong)] + [
                    (name, ctypes.c_ulonglong) for name in
                    ['total', 'available', 'page_total', 'page_available', 'virtual_total', 'virtual_available', 'extended']]
            status = Memory(); status.length = ctypes.sizeof(status)
            if ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(status)):
                total, available = status.total, status.available
            process_mask, system_mask = ctypes.c_size_t(), ctypes.c_size_t()
            kernel = ctypes.windll.kernel32
            kernel.GetCurrentProcess.restype = ctypes.c_void_p
            if kernel.GetProcessAffinityMask(ctypes.c_void_p(kernel.GetCurrentProcess()), ctypes.byref(process_mask), ctypes.byref(system_mask)):
                cpus = min(cpus, process_mask.value.bit_count() or 1)
        for path in cgroup_paths(self.proc, self.cgroup):
            try:
                quota, period = (path / 'cpu.max').read_text().split()
                if quota != 'max' and int(quota) > 0 and int(period) > 0:
                    cpus = min(cpus, int(quota) / int(period))
            except (OSError, ValueError): pass
            limits = [v for name in ['memory.max', 'memory.high'] if (v := number(path / name))]
            used = number(path / 'memory.current', True)
            if limits:
                limit = min(limits); total = min(total or limit, limit)
                if used is not None:
                    reclaimable = values(path / 'memory.stat').get('inactive_file', 0)
                    remaining = max(0, limit - max(0, used - reclaimable))
                    available = min(available if available is not None else remaining, remaining)
        for path in cgroup_paths(self.proc, self.cgroup, 'cpu'):
            quota, period = number(path / 'cpu.cfs_quota_us'), number(path / 'cpu.cfs_period_us')
            if quota and period: cpus = min(cpus, quota / period)
        for path in cgroup_paths(self.proc, self.cgroup, 'memory'):
            limit, used = number(path / 'memory.limit_in_bytes'), number(path / 'memory.usage_in_bytes', True)
            if limit:
                total = min(total or limit, limit)
                if used is not None:
                    stats = values(path / 'memory.stat')
                    remaining = max(0, limit - max(0, used - stats.get('total_inactive_file', stats.get('inactive_file', 0))))
                    available = min(available if available is not None else remaining, remaining)
        try: load = os.getloadavg()[0]
        except (AttributeError, OSError): load = None
        return Resources(max(.01, cpus), total, available, load)


class BuildPlanner:
    """jobs='auto' 自动选择；正整数作为上限，不能绕过设备内存预算。"""
    def __init__(self, probe=None):
        self.probe = probe or ResourceProbe()

    def choose(self, stage, maximum='auto'):
        resource = self.probe.snapshot()
        jobs = max(1, math.floor(resource.cpu_capacity * .75))
        if resource.load is not None and resource.load >= resource.cpu_capacity:
            jobs = max(1, jobs // 2)
        reserve = max(512 * MIB, (resource.total_memory or 0) // 4)
        per_job = GIB if stage in ('test_compile', 'tests', 'binary') else 512 * MIB
        if resource.available_memory is None:
            jobs = 1
        else:
            jobs = min(jobs, max(0, resource.available_memory - reserve) // per_job)
        if maximum != 'auto': jobs = min(jobs, maximum)
        return dict(stage=stage, jobs=jobs, reserved_memory_bytes=reserve,
                    memory_per_job_bytes=per_job, **asdict(resource))

    def critical(self):
        available = self.probe.snapshot().available_memory
        return available is not None and available < 128 * MIB
