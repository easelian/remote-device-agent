# Own the Agent's process tree even when Task Scheduler forcibly ends this host.
param([string]$Node, [string]$Log, [string[]]$AgentArguments)
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;

public static class RdhTaskJob {
    [StructLayout(LayoutKind.Sequential)]
    struct BasicLimits {
        public long ProcessTime, JobTime;
        public uint Flags;
        public UIntPtr MinimumWorkingSet, MaximumWorkingSet;
        public uint ActiveProcesses;
        public UIntPtr Affinity;
        public uint Priority, Scheduling;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct IoCounters {
        public ulong ReadOperations, WriteOperations, OtherOperations;
        public ulong ReadBytes, WriteBytes, OtherBytes;
    }
    [StructLayout(LayoutKind.Sequential)]
    struct ExtendedLimits {
        public BasicLimits Basic;
        public IoCounters Io;
        public UIntPtr ProcessMemory, JobMemory, PeakProcessMemory, PeakJobMemory;
    }
    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    static extern IntPtr CreateJobObject(IntPtr attributes, string name);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool SetInformationJobObject(IntPtr job, int kind, ref ExtendedLimits limits, uint size);
    [DllImport("kernel32.dll", SetLastError = true)]
    static extern bool AssignProcessToJobObject(IntPtr job, IntPtr process);
    [DllImport("kernel32.dll")]
    static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32.dll")]
    static extern bool CloseHandle(IntPtr handle);

    // Intentionally held for the lifetime of this PowerShell process. The handle
    // is non-inheritable; killing this host closes the last handle and kills all
    // children. Assign the host BEFORE spawning any child to avoid an escape race.
    static IntPtr job;
    public static void Attach() {
        job = CreateJobObject(IntPtr.Zero, null);
        if (job == IntPtr.Zero) throw new Win32Exception(Marshal.GetLastWin32Error());
        var limits = new ExtendedLimits();
        limits.Basic.Flags = 0x2000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if (!SetInformationJobObject(job, 9, ref limits, (uint)Marshal.SizeOf(limits)) ||
            !AssignProcessToJobObject(job, GetCurrentProcess())) {
            int error = Marshal.GetLastWin32Error();
            CloseHandle(job);
            job = IntPtr.Zero;
            throw new Win32Exception(error);
        }
    }
}
'@
[RdhTaskJob]::Attach()
# Keep at most two roughly 2 MiB logs, including native stderr. A failed Agent
# retains its exit code so Task Scheduler's configured restart policy applies.
$ErrorActionPreference = 'Continue'
& $Node @AgentArguments 2>&1 | ForEach-Object {
    if ((Get-Item -LiteralPath $Log -ErrorAction SilentlyContinue).Length -ge 2MB) {
        Move-Item -LiteralPath $Log -Destination ($Log + '.previous') -Force
    }
    $_ | Out-File -LiteralPath $Log -Encoding utf8 -Append
}
exit $LASTEXITCODE
