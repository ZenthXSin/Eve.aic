"""设备配额、可回收缓存、负载与阶段内存预算的确定性验收。"""
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch, Mock
from types import SimpleNamespace
from build_resources import BuildPlanner, ResourceProbe, Resources, GIB, MIB


class ResourceTests(unittest.TestCase):
    def probe(self,cpu=8,total=16*GIB,available=12*GIB,load=0):
        class Probe:
            def snapshot(self):return Resources(cpu,total,available,load)
        return BuildPlanner(Probe())

    def test_cpu_memory_load_and_manual_ceiling(self):
        self.assertEqual(self.probe().choose('libraries')['jobs'],6)
        self.assertEqual(self.probe(available=6*GIB).choose('libraries')['jobs'],4)
        self.assertEqual(self.probe(available=6*GIB).choose('binary')['jobs'],2)
        self.assertEqual(self.probe(load=10).choose('libraries')['jobs'],3)
        self.assertEqual(self.probe().choose('libraries',2)['jobs'],2)
        self.assertEqual(self.probe(available=3*GIB).choose('libraries')['jobs'],0)
        self.assertEqual(self.probe(cpu=.5,total=2*GIB,available=2*GIB).choose('binary')['jobs'],1)
        self.assertEqual(self.probe(total=None,available=None).choose('libraries')['jobs'],1)

    def test_windows_uses_available_physical_memory_and_process_affinity(self):
        def memory(pointer):
            pointer._obj.total=8*GIB;pointer._obj.available=6*GIB;return 1
        def affinity(process,mask,system):
            mask._obj.value=3;system._obj.value=255;return 1
        kernel=SimpleNamespace(GlobalMemoryStatusEx=memory,GetProcessAffinityMask=affinity,GetCurrentProcess=Mock(return_value=123))
        with tempfile.TemporaryDirectory() as directory,patch('ctypes.windll',SimpleNamespace(kernel32=kernel),create=True):
            root=Path(directory)
            value=ResourceProbe(root/'proc',root/'cg',windows=True).snapshot()
        self.assertEqual((value.cpu_capacity,value.total_memory,value.available_memory),(2,8*GIB,6*GIB))

    def test_linux_nested_cgroups_bound_host_resources_and_reclaim_cache(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);proc=root/'proc';cg=root/'cg';(proc/'self').mkdir(parents=True);(cg/'nested').mkdir(parents=True)
            (proc/'meminfo').write_text(f'MemTotal: {64*GIB//1024} kB\nMemAvailable: {40*GIB//1024} kB\n')
            (proc/'self/cgroup').write_text('0::/nested\n')
            (cg/'cpu.max').write_text('600000 100000')
            (cg/'nested/cpu.max').write_text('250000 100000')
            (cg/'memory.max').write_text(str(16*GIB));(cg/'memory.current').write_text(str(12*GIB))
            (cg/'memory.stat').write_text(f'inactive_file {4*GIB}\n')
            (cg/'nested/memory.max').write_text(str(12*GIB));(cg/'nested/memory.current').write_text(str(8*GIB))
            (cg/'nested/memory.stat').write_text(f'inactive_file {3*GIB}\n')
            with patch('build_resources.os.cpu_count',return_value=32),patch('build_resources.os.sched_getaffinity',create=True,return_value=set(range(8))):
                value=ResourceProbe(proc,cg,windows=False).snapshot()
            self.assertEqual((value.cpu_capacity,value.total_memory,value.available_memory),(2.5,12*GIB,7*GIB))

    def test_linux_v1_limits_zero_usage_and_critical_pressure(self):
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp);proc=root/'proc';cg=root/'cg';(proc/'self').mkdir(parents=True)
            for name in ['cpu','memory']:(cg/name).mkdir(parents=True)
            (proc/'meminfo').write_text(f'MemTotal: {16*GIB//1024} kB\nMemAvailable: {12*GIB//1024} kB\n')
            (proc/'self/cgroup').write_text('2:cpu,cpuacct:/\n3:memory:/\n')
            (cg/'cpu/cpu.cfs_quota_us').write_text('50000');(cg/'cpu/cpu.cfs_period_us').write_text('100000')
            (cg/'memory/memory.limit_in_bytes').write_text(str(GIB));(cg/'memory/memory.usage_in_bytes').write_text('0')
            probe=ResourceProbe(proc,cg,windows=False);value=probe.snapshot()
            self.assertEqual((value.cpu_capacity,value.total_memory,value.available_memory),(.5,GIB,GIB))
            (cg/'memory/memory.usage_in_bytes').write_text(str(GIB-64*MIB))
            self.assertTrue(BuildPlanner(probe).critical())


if __name__=='__main__':unittest.main()
