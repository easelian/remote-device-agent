# Download and verify one native executable; Rust owns installation.
[CmdletBinding()]
param(
    [string]$Base = 'https://github.com/easelian/remote-device-agent/releases/latest/download',
    [string]$Version = '',
    [string]$Offline = '',
    [switch]$Service,
    [switch]$Help,
    [string[]]$NativeArguments = @()
)
$ErrorActionPreference = 'Stop'
if ($Help) {
    Write-Output @'
usage: install.ps1 -Version VERSION [-Base HTTPS_URL|DIRECTORY] [-Service] -NativeArguments @(...)
       install.ps1 -Offline NATIVE_BINARY [-Service] -NativeArguments @(...)

The native binary must have a matching .sha256 file. No Node/npm is used.
NativeArguments go to rdh install-native, or install-service with -Service.
The Rust command owns all profile, ownership, existing-install and recovery checks.
It does not pair a device, start a service or overwrite an installation.

Examples (replace the version and absolute paths):
  .\install.ps1 -Version VERSION -NativeArguments @('--prefix', 'C:\Path\NewInstall')
  .\install.ps1 -Offline 'C:\Downloads\rdh-VERSION-win32-x64.exe' -NativeArguments @('--prefix', 'C:\Path\NewInstall', '--config', 'C:\Path\paired.json')
  .\install.ps1 -Version VERSION -Service -NativeArguments @('--prefix', 'C:\Program Files\RDH-new', '--data', 'C:\ProgramData\RDH-new', '--config', 'C:\Path\paired.json', '--name', 'RemoteDeviceHub')
'@
    exit 0
}
if ($env:OS -ne 'Windows_NT') { throw 'Use install.sh on Linux and macOS.' }
if (!$NativeArguments -or $NativeArguments.Count -eq 0) { throw 'Supply native installation arguments such as --prefix and --config.' }
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
if ($Version -notmatch '\A[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?\z') {
    throw 'Supply a valid -Version when downloading.'
}
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
                try {
                    $rdhBuffer = [byte[]]::new(65536)
                    $rdhTotal = 0L
                    while (($rdhCount = $rdhInput.Read($rdhBuffer, 0, $rdhBuffer.Length)) -gt 0) {
                        $rdhTotal += $rdhCount
                        if ($rdhTotal -gt 268435456) { throw 'Download is too large.' }
                        $rdhOutput.Write($rdhBuffer, 0, $rdhCount)
                    }
                } finally {
                    $rdhOutput.Dispose()
                    $rdhInput.Dispose()
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
        Copy-Item -LiteralPath (Join-Path $Base $Name) -Destination $Destination
    }
}
try {
    $rdhBinary = Join-Path $rdhTemporary $rdhName
    $rdhChecksum = Join-Path $rdhTemporary 'checksum'
    if ($Offline) {
        Copy-Item -LiteralPath ($Offline + '.sha256') -Destination $rdhChecksum
        Copy-Item -LiteralPath $Offline -Destination $rdhBinary
    } else {
        Fetch-Native ($rdhName + '.sha256') $rdhChecksum
        Fetch-Native $rdhName $rdhBinary
    }
    if ((Get-Item -LiteralPath $rdhChecksum).Length -gt 1024) { throw 'Checksum record is too large.' }
    if ((Get-Item -LiteralPath $rdhBinary).Length -gt 268435456) { throw 'Native executable is too large.' }
    $rdhRecord = [IO.File]::ReadAllText($rdhChecksum)
    $rdhPattern = '\A([0-9a-f]{64})  ' + [regex]::Escape($rdhName) + '\r?\n\z'
    $rdhMatch = [regex]::Match($rdhRecord, $rdhPattern)
    if (!$rdhMatch.Success) { throw 'Invalid checksum record.' }
    if ((Get-FileHash -LiteralPath $rdhBinary -Algorithm SHA256).Hash.ToLowerInvariant() -ne $rdhMatch.Groups[1].Value) {
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
