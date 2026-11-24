<#
.SYNOPSIS
  Give a Windows CI runner a fixed-size pagefile, at once and with no reboot,
  and fail within the step when it does not take.
.DESCRIPTION
  A cold test build runs out of commit charge (os error 1455) on the
  windows-2025 image unless the pagefile is enlarged. This creates one with
  NtCreatePagingFile on the first drive that takes it, reports the NTSTATUS of
  any that refuse, and then checks the pagefiles Windows has: a build that
  would die of it hours later fails here instead.
.EXAMPLE
  ./scripts/windows-pagefile.ps1 -SizeGB 16 -Drives D:,C:
#>
param(
    [UInt64] $SizeGB = 16,
    [string[]] $Drives = @("D:", "C:")
)

$ErrorActionPreference = "Stop"

Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

namespace HotCi
{
    public static class PageFile
    {
        [StructLayout(LayoutKind.Sequential)]
        struct LUID { public uint LowPart; public int HighPart; }

        [StructLayout(LayoutKind.Sequential)]
        struct TOKEN_PRIVILEGES { public uint PrivilegeCount; public LUID Luid; public uint Attributes; }

        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
        struct UNICODE_STRING { public ushort Length; public ushort MaximumLength; public string Buffer; }

        [DllImport("advapi32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
        static extern bool LookupPrivilegeValueW(string system, string name, out LUID luid);

        [DllImport("advapi32.dll", SetLastError = true)]
        static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);

        [DllImport("advapi32.dll", SetLastError = true)]
        static extern bool AdjustTokenPrivileges(IntPtr token, bool disableAll, ref TOKEN_PRIVILEGES state,
                                                 uint length, IntPtr previous, IntPtr returned);

        [DllImport("kernel32.dll", SetLastError = true)]
        static extern bool CloseHandle(IntPtr handle);

        [DllImport("kernel32.dll")]
        static extern IntPtr GetCurrentProcess();

        [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
        static extern uint QueryDosDeviceW(string device, StringBuilder target, int size);

        [DllImport("ntdll.dll", CharSet = CharSet.Unicode)]
        static extern int NtCreatePagingFile(ref UNICODE_STRING name, ref long minimum, ref long maximum, uint priority);

        [DllImport("ntdll.dll")]
        static extern int RtlNtStatusToDosError(int status);

        // Enables SeCreatePagefilePrivilege; "" when it is held, else why not.
        public static string EnablePrivilege()
        {
            LUID luid;
            if (!LookupPrivilegeValueW(null, "SeCreatePagefilePrivilege", out luid))
                return new Win32Exception(Marshal.GetLastWin32Error()).Message;
            IntPtr token;
            if (!OpenProcessToken(GetCurrentProcess(), 0x28 /* ADJUST_PRIVILEGES | QUERY */, out token))
                return new Win32Exception(Marshal.GetLastWin32Error()).Message;
            try
            {
                var state = new TOKEN_PRIVILEGES { PrivilegeCount = 1, Luid = luid, Attributes = 2 /* ENABLED */ };
                bool adjusted = AdjustTokenPrivileges(token, false, ref state, 0, IntPtr.Zero, IntPtr.Zero);
                int error = Marshal.GetLastWin32Error();  // ERROR_NOT_ALL_ASSIGNED comes with true
                return adjusted && error == 0 ? "" : new Win32Exception(error).Message;
            }
            finally { CloseHandle(token); }
        }

        // Creates <drive>\pagefile.sys of exactly `bytes`. Returns "" or what refused it.
        public static string Create(string drive, long bytes)
        {
            var target = new StringBuilder(260);
            if (QueryDosDeviceW(drive, target, target.Capacity) == 0)
                return "QueryDosDevice: " + new Win32Exception(Marshal.GetLastWin32Error()).Message;
            string path = target.ToString() + "\\pagefile.sys";
            var name = new UNICODE_STRING
            {
                Length = (ushort)(path.Length * 2),
                MaximumLength = (ushort)((path.Length + 1) * 2),
                Buffer = path
            };
            long minimum = bytes, maximum = bytes;
            int status = NtCreatePagingFile(ref name, ref minimum, ref maximum, 0);
            if (status == 0) return "";
            return String.Format("NtCreatePagingFile({0}): NTSTATUS 0x{1:X8}, {2}", path, status,
                                 new Win32Exception(RtlNtStatusToDosError(status)).Message);
        }
    }
}
'@

$wantedMB = $SizeGB * 1024

function Show-Memory([string] $when) {
    $os = Get-CimInstance Win32_OperatingSystem
    $files = @(Get-CimInstance Win32_PageFileUsage)
    $pagedMB = ($files | Measure-Object -Property AllocatedBaseSize -Sum).Sum
    if (-not $pagedMB) { $pagedMB = 0 }
    Write-Host ("{0}: RAM {1:N0} MB, commit limit {2:N0} MB, pagefiles {3:N0} MB" -f $when,
        ($os.TotalVisibleMemorySize / 1KB), ($os.TotalVirtualMemorySize / 1KB), $pagedMB)
    foreach ($file in $files) {
        Write-Host ("  {0}: {1:N0} MB allocated, {2:N0} MB in use" -f $file.Name, $file.AllocatedBaseSize, $file.CurrentUsage)
    }
    # The commit limit is RAM plus every pagefile, whatever WMI lists.
    [pscustomobject]@{
        PagedMB = [math]::Max([double]$pagedMB, ($os.TotalVirtualMemorySize - $os.TotalVisibleMemorySize) / 1KB)
    }
}

$before = Show-Memory "Before"
foreach ($drive in $Drives) {
    $disk = Get-PSDrive -Name $drive.TrimEnd(":") -ErrorAction SilentlyContinue
    if ($disk) { Write-Host ("{0} has {1:N1} GB free" -f $drive, ($disk.Free / 1GB)) }
}
if ($before.PagedMB -ge 0.95 * $wantedMB) {
    Write-Host "The pagefiles already hold $SizeGB GB."
    exit 0
}

$privilege = [HotCi.PageFile]::EnablePrivilege()
if ($privilege) { Write-Host "::warning::SeCreatePagefilePrivilege: $privilege" }

$refusals = @()
foreach ($drive in $Drives) {
    $disk = Get-PSDrive -Name $drive.TrimEnd(":") -ErrorAction SilentlyContinue
    if (-not $disk) { $refusals += "${drive}: no such drive"; continue }
    # Leave room for the build itself on the drive it shares.
    if ($disk.Free -lt ($SizeGB + 8) * 1GB) {
        $refusals += ("{0}: {1:N1} GB free, under {2} GB" -f $drive, ($disk.Free / 1GB), ($SizeGB + 8))
        continue
    }
    $refused = [HotCi.PageFile]::Create($drive, [long]($SizeGB * 1GB))
    if (-not $refused) {
        Write-Host "Created a $SizeGB GB pagefile on $drive"
        $after = Show-Memory "After"
        if ($after.PagedMB -lt 0.95 * $wantedMB) {
            Write-Host ("::warning::Windows took the pagefile on {0}, but reports {1:N0} MB of pagefiles" -f $drive, $after.PagedMB)
        }
        exit 0
    }
    $refusals += "${drive}: $refused"
    Write-Host $refused
}

Show-Memory "After" | Out-Null
Write-Host ("::error::No drive took a {0} GB pagefile ({1}). The build would run out of commit charge (os error 1455); rerun the job for another runner." -f $SizeGB, ($refusals -join "; "))
exit 1
