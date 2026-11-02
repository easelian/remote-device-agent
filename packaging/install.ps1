# Download and verify one native executable; Rust owns installation.
[CmdletBinding()]
param(
    [string]$Base = '',
    [string]$Version = '',
    [string]$Offline = '',
    [switch]$Service,
    [switch]$NoStart,
    [switch]$NoModifyPath,
    [switch]$Help,
    [string[]]$NativeArguments = @()
)
$ErrorActionPreference = 'Stop'
if ($Help) {
    Write-Output @'
usage: install.ps1 [-Version VERSION] [-Base HTTPS_URL|DIRECTORY] [-Service] [-NativeArguments @(...)]
                  [-NoStart] [-NoModifyPath]
       install.ps1 -Offline NATIVE_BINARY [-Service] -NativeArguments @(...)

The native binary must have a matching SHA256SUMS entry or per-binary .sha256 file.
No Node/npm is used.
With no arguments, install the newest published GitHub release (including prereleases) into
%LOCALAPPDATA%\RemoteDeviceAgent. RDH_VERSION, RDH_BASE and RDH_INSTALL_DIR
override these defaults. Installation saves the directory in the user PATH,
updates this PowerShell process's PATH and starts the Agent in the foreground.
-NoStart or RDH_NO_START=1 disables automatic launch.
-NoModifyPath or RDH_NO_MODIFY_PATH=1 leaves PATH unchanged.
NativeArguments go to rdh install-native, or install-service with -Service.
The Rust command owns all profile, ownership, existing-install and recovery checks.
Service installation does not change PATH or start the service. Existing
installations are not overwritten. Device pairing requires browser approval.

Examples (replace the version and absolute paths):
  .\install.ps1 -Version VERSION -NativeArguments @('--prefix', 'C:\Path\NewInstall')
  .\install.ps1 -Offline 'C:\Downloads\rdh-VERSION-win32-x64.exe' -NativeArguments @('--prefix', 'C:\Path\NewInstall', '--config', 'C:\Path\paired.json')
  .\install.ps1 -Version VERSION -Service -NativeArguments @('--prefix', 'C:\Program Files\RDH-new', '--data', 'C:\ProgramData\RDH-new', '--config', 'C:\Path\paired.json', '--name', 'RemoteDeviceHub')
'@
    exit 0
}
if ($env:OS -ne 'Windows_NT') { throw 'Use install.sh on Linux and macOS.' }
foreach ($rdhOption in @('RDH_NO_START', 'RDH_NO_MODIFY_PATH')) {
    $rdhValue = [Environment]::GetEnvironmentVariable($rdhOption)
    if ($rdhValue -and $rdhValue -notin @('0', '1')) { throw "$rdhOption must be 0 or 1." }
}
$NoStart = $NoStart -or $env:RDH_NO_START -eq '1'
$NoModifyPath = $NoModifyPath -or $env:RDH_NO_MODIFY_PATH -eq '1'
$rdhDefaultPrefix = ''
if (!$NativeArguments -or $NativeArguments.Count -eq 0) {
    if ($Service) { throw 'Supply service installation arguments in -NativeArguments.' }
    $rdhDefaultPrefix = $env:RDH_INSTALL_DIR
    if (!$rdhDefaultPrefix) {
        if (!$env:LOCALAPPDATA) { throw 'LOCALAPPDATA is unset; set RDH_INSTALL_DIR or pass --prefix in -NativeArguments.' }
        $rdhDefaultPrefix = Join-Path $env:LOCALAPPDATA 'RemoteDeviceAgent'
    }
    $NativeArguments = @('--prefix', $rdhDefaultPrefix)
}
$rdhInstallPrefix = ''
$rdhConfig = ''
for ($rdhIndex = 0; $rdhIndex -lt $NativeArguments.Count - 1; $rdhIndex++) {
    if ($NativeArguments[$rdhIndex] -eq '--prefix') { $rdhInstallPrefix = $NativeArguments[$rdhIndex + 1] }
    if ($NativeArguments[$rdhIndex] -eq '--config') { $rdhConfig = $NativeArguments[$rdhIndex + 1] }
}
if (!$Base) { $Base = $env:RDH_BASE }
if (!$Version) { $Version = $env:RDH_VERSION }
$rdhCpu = $env:PROCESSOR_ARCHITEW6432
if (!$rdhCpu) { $rdhCpu = $env:PROCESSOR_ARCHITECTURE }
$rdhArch = switch ($rdhCpu.ToUpperInvariant()) {
    'AMD64' { 'x64' }
    'ARM64' { 'arm64' }
    default { throw 'Unsupported Windows architecture.' }
}
$rdhPlatform = "win32-$rdhArch"
$Version = $Version -replace '^v', ''
if ($Offline) {
    if (![IO.Path]::IsPathRooted($Offline)) { throw '-Offline requires an absolute path.' }
    $rdhName = [IO.Path]::GetFileName($Offline)
    $rdhMatch = [regex]::Match($rdhName, ('^rdh-(.+)-' + [regex]::Escape($rdhPlatform) + '\.exe$'))
    if (!$rdhMatch.Success) { throw 'Offline binary name does not match this platform.' }
    $rdhDetected = $rdhMatch.Groups[1].Value
    if ($Version -and $Version -ne $rdhDetected) { throw 'Offline version differs from -Version.' }
    $Version = $rdhDetected
}
if (!$Version) {
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    $rdhReleases = @(Invoke-RestMethod -Uri 'https://api.github.com/repos/easelian/remote-device-agent/releases?per_page=1' `
        -Headers @{Accept = 'application/vnd.github+json'} -TimeoutSec 30)
    if ($rdhReleases.Count -ne 1 -or $rdhReleases[0].draft -ne $false -or $rdhReleases[0].tag_name -notmatch '^v(.+)$') {
        throw 'The latest release has no supported version tag. Retry or specify -Version.'
    }
    $Version = $Matches[1]
}
if ($Version -notmatch '\A[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?\z') {
    throw 'Supply a valid -Version when downloading.'
}
if (!$Base) { $Base = "https://github.com/easelian/remote-device-agent/releases/download/v$Version" }
$rdhName = "rdh-$Version-$rdhPlatform.exe"
$rdhTemporary = Join-Path ([IO.Path]::GetTempPath()) ('rdh-install-' + [Guid]::NewGuid().ToString('N'))
$rdhIdentity = [Security.Principal.WindowsIdentity]::GetCurrent()
$rdhAcl = [Security.AccessControl.DirectorySecurity]::new()
$rdhAcl.SetOwner($rdhIdentity.User)
$rdhAcl.SetAccessRuleProtection($true, $false)
$rdhAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
    $rdhIdentity.User, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow'))
$rdhDirectory = [IO.DirectoryInfo]::new($rdhTemporary)
if ($PSVersionTable.PSEdition -eq 'Core') {
    [IO.FileSystemAclExtensions]::Create($rdhDirectory, $rdhAcl)
} else {
    $rdhDirectory.Create($rdhAcl)
}
function Fetch-Native([string]$Name, [string]$Destination) {
    if ($Base -match '^https://') {
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
        $rdhUri = [Uri]::new($Base.TrimEnd('/') + '/' + $Name)
        for ($rdhRedirect = 0; $rdhRedirect -le 8; $rdhRedirect++) {
            if ($rdhUri.Scheme -cne 'https') { throw 'Download redirects must use HTTPS.' }
            $rdhRequest = [Net.HttpWebRequest]::Create($rdhUri)
            $rdhRequest.AllowAutoRedirect = $false
            $rdhRequest.Timeout = 300000
            $rdhRequest.ReadWriteTimeout = 300000
            $rdhResponse = $rdhRequest.GetResponse()
            try {
                $rdhStatus = [int]$rdhResponse.StatusCode
                if ($rdhStatus -in @(301, 302, 303, 307, 308)) {
                    $rdhLocation = $rdhResponse.Headers['Location']
                    if (!$rdhLocation) { throw 'Download redirect has no Location.' }
                    $rdhUri = [Uri]::new($rdhUri, $rdhLocation)
                    continue
                }
                if ($rdhStatus -ne 200) { throw "Download returned HTTP $rdhStatus." }
                $rdhInput = $rdhResponse.GetResponseStream()
                $rdhOutput = [IO.File]::Open($Destination, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write)
                $rdhShowProgress = $Name -eq $rdhName
                $rdhProgressClock = [Diagnostics.Stopwatch]::StartNew()
                $rdhProgressAt = -200L
                if ($rdhShowProgress) { Write-Host "Downloading Remote Device Hub $Version ($rdhPlatform)..." }
                try {
                    $rdhBuffer = [byte[]]::new(65536)
                    $rdhTotal = 0L
                    while (($rdhCount = $rdhInput.Read($rdhBuffer, 0, $rdhBuffer.Length)) -gt 0) {
                        $rdhTotal += $rdhCount
                        if ($rdhTotal -gt 268435456) { throw 'Download is too large.' }
                        $rdhOutput.Write($rdhBuffer, 0, $rdhCount)
                        if ($rdhShowProgress -and $rdhProgressClock.ElapsedMilliseconds - $rdhProgressAt -ge 200) {
                            $rdhProgressAt = $rdhProgressClock.ElapsedMilliseconds
                            $rdhStatusText = ('{0:N1} MiB downloaded' -f ($rdhTotal / 1MB))
                            $rdhPercent = -1
                            if ($rdhResponse.ContentLength -gt 0) {
                                $rdhStatusText += (' of {0:N1} MiB' -f ($rdhResponse.ContentLength / 1MB))
                                $rdhPercent = [Math]::Min(100, [int](100.0 * $rdhTotal / $rdhResponse.ContentLength))
                            }
                            Write-Progress -Id 1 -Activity "Downloading $Name" -Status $rdhStatusText -PercentComplete $rdhPercent
                        }
                    }
                } finally {
                    $rdhOutput.Dispose()
                    $rdhInput.Dispose()
                    if ($rdhShowProgress) { Write-Progress -Id 1 -Activity "Downloading $Name" -Completed }
                }
                return
            } finally {
                $rdhResponse.Dispose()
            }
        }
        throw 'Too many download redirects.'
    } elseif ($Base -match '://') {
        throw '-Base must use HTTPS or a local directory.'
    } else {
        # Treat local bases as filesystem paths, including verbatim \\?\ paths,
        # rather than asking the PowerShell provider to resolve a drive.
        Copy-Item -LiteralPath ([IO.Path]::Combine($Base, $Name)) -Destination $Destination -ErrorAction Stop
        if (![IO.File]::Exists($Destination)) { throw "Local source did not provide $Name." }
    }
}
try {
    $rdhBinary = Join-Path $rdhTemporary $rdhName
    $rdhChecksum = Join-Path $rdhTemporary 'checksum'
    if ($Offline) {
        $rdhSums = [IO.Path]::Combine([IO.Path]::GetDirectoryName($Offline), 'SHA256SUMS')
        if ([IO.File]::Exists($rdhSums)) {
            Copy-Item -LiteralPath $rdhSums -Destination $rdhChecksum
        } else {
            Copy-Item -LiteralPath ($Offline + '.sha256') -Destination $rdhChecksum
        }
        Copy-Item -LiteralPath $Offline -Destination $rdhBinary
    } else {
        try {
            Fetch-Native 'SHA256SUMS' $rdhChecksum
        } catch {
            Remove-Item -LiteralPath $rdhChecksum -Force -ErrorAction SilentlyContinue
            Fetch-Native ($rdhName + '.sha256') $rdhChecksum
        }
        Fetch-Native $rdhName $rdhBinary
    }
    if ((Get-Item -LiteralPath $rdhChecksum).Length -gt 65536) { throw 'Checksum record is too large.' }
    if ((Get-Item -LiteralPath $rdhBinary).Length -gt 268435456) { throw 'Native executable is too large.' }
    $rdhExpected = $null
    foreach ($rdhLine in [IO.File]::ReadAllLines($rdhChecksum)) {
        $rdhMatch = [regex]::Match($rdhLine, '\A([0-9a-f]{64})  ([^\r\n]+)\z')
        if (!$rdhMatch.Success) { throw 'Invalid checksum record.' }
        if ($rdhMatch.Groups[2].Value -ceq $rdhName) {
            if ($null -ne $rdhExpected) { throw 'Duplicate checksum record.' }
            $rdhExpected = $rdhMatch.Groups[1].Value
        }
    }
    if ($null -eq $rdhExpected) { throw 'Invalid checksum record.' }
    if ((Get-FileHash -LiteralPath $rdhBinary -Algorithm SHA256).Hash.ToLowerInvariant() -ne $rdhExpected) {
        throw 'Native executable checksum mismatch.'
    }
    $rdhReported = & $rdhBinary --version
    if ($LASTEXITCODE -ne 0 -or $rdhReported -cne $Version) { throw 'Native executable version mismatch or self-check failed.' }
    $rdhCommand = if ($Service) { 'install-service' } else { 'install-native' }
    & $rdhBinary $rdhCommand @NativeArguments
    $rdhExit = $LASTEXITCODE
    if ($rdhExit -ne 0) { throw "Native installation failed with exit code $rdhExit. Inspect its diagnostic before retrying." }
} finally {
    Remove-Item -LiteralPath $rdhTemporary -Recurse -Force
}
if (!$Service -and $rdhInstallPrefix) {
    $rdhInstallPrefix = [IO.Path]::GetFullPath($rdhInstallPrefix).TrimEnd('\', '/')
    if (!$NoModifyPath) {
        if ($rdhInstallPrefix.Contains(';')) { throw 'The installation path cannot contain a semicolon when updating PATH.' }
        $rdhEnvironment = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
        try {
            $rdhUserPath = $rdhEnvironment.GetValue('Path', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            $rdhPathKind = [Microsoft.Win32.RegistryValueKind]::ExpandString
            if ($null -ne $rdhUserPath) {
                $rdhPathKind = $rdhEnvironment.GetValueKind('Path')
                if ($rdhPathKind -notin @([Microsoft.Win32.RegistryValueKind]::String, [Microsoft.Win32.RegistryValueKind]::ExpandString)) {
                    throw 'The user PATH registry value is not a string.'
                }
            }
            $rdhPresent = @($rdhUserPath -split ';' | Where-Object {
                [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\', '/') -ieq $rdhInstallPrefix
            }).Count -gt 0
            if (!$rdhPresent) {
                $rdhNewPath = if ($rdhUserPath) { "$rdhInstallPrefix;$rdhUserPath" } else { $rdhInstallPrefix }
                $rdhEnvironment.SetValue('Path', $rdhNewPath, $rdhPathKind)
            }
        } finally {
            $rdhEnvironment.Dispose()
        }
        if (!(@($env:PATH -split ';' | Where-Object { $_.TrimEnd('\', '/') -ieq $rdhInstallPrefix }).Count)) {
            $env:PATH = "$rdhInstallPrefix;$env:PATH"
        }
        if (!$rdhPresent) {
            try {
                if (!('RdhInstaller.EnvironmentNotification' -as [type])) {
                    Add-Type -Namespace RdhInstaller -Name EnvironmentNotification -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("user32.dll", CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
public static extern System.IntPtr SendMessageTimeout(System.IntPtr window, uint message, System.UIntPtr parameter, string area, uint flags, uint timeout, out System.UIntPtr result);
'@
                }
                $rdhNotificationResult = [UIntPtr]::Zero
                [void][RdhInstaller.EnvironmentNotification]::SendMessageTimeout([IntPtr]0xffff, 0x1a, [UIntPtr]::Zero, 'Environment', 2, 1000, [ref]$rdhNotificationResult)
            } catch {
                Write-Verbose 'PATH is saved; desktop environment notification was unavailable.'
            }
        }
        Write-Output 'User PATH updated. This PowerShell session can run rdh immediately.'
    }
    if (!$NoStart) {
        Write-Output 'Starting Remote Device Hub. Press Ctrl+C to stop.'
        $rdhLaunchArguments = @()
        if ($rdhConfig) { $rdhLaunchArguments = @('--config', $rdhConfig) }
        & (Join-Path $rdhInstallPrefix 'rdh.exe') @rdhLaunchArguments
        if ($LASTEXITCODE -ne 0) { throw "The Agent exited with code $LASTEXITCODE." }
    }
}
