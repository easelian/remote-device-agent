# Install a self-contained Windows Agent without requiring Node.js or admin rights.
# Run the installed rdh.cmd in the foreground; no service is registered.
[CmdletBinding()]
param(
    [string]$Base = 'https://github.com/be-one-click/remote-device-agent/releases/latest/download',
    [string]$Version = '',
    [string]$Prefix = (Join-Path $env:LOCALAPPDATA 'RemoteDeviceHub'),
    [string]$BinDir = '',
    [string]$Server = '',
    [switch]$NoPair
)
$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT') { throw 'This installer requires Windows.' }
$cpu = $env:PROCESSOR_ARCHITEW6432
if (!$cpu) { $cpu = $env:PROCESSOR_ARCHITECTURE }
$arch = switch ($cpu.ToUpperInvariant()) {
    'AMD64' { 'x64' }
    'ARM64' { 'arm64' }
    default { throw "Unsupported Windows architecture: $cpu" }
}
$platform = "win32-$arch"
$Version = $Version -replace '^v', ''
if ($Version -and $Version -notmatch '^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$') {
    throw 'Invalid version.'
}
$Prefix = [IO.Path]::GetFullPath($Prefix)
if (!$BinDir) { $BinDir = Join-Path $Prefix 'bin' }
$BinDir = [IO.Path]::GetFullPath($BinDir)
$temp = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $temp | Out-Null
function Fetch([string]$Name, [string]$Destination) {
    if ($Base -match '^https?://') {
        Invoke-WebRequest -UseBasicParsing -Uri ($Base.TrimEnd('/') + '/' + $Name) -OutFile $Destination
    } else {
        Copy-Item -LiteralPath (Join-Path $Base $Name) -Destination $Destination
    }
}
try {
    $sums = Join-Path $temp 'SHA256SUMS'
    Fetch 'SHA256SUMS' $sums
    $entries = @(Get-Content -LiteralPath $sums | ForEach-Object {
        if ($_ -match '^([0-9a-f]{64})\s+rdh-(\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)-(win32-(?:x64|arm64))\.zip$') {
            if ($Matches[3] -eq $platform -and (!$Version -or $Matches[2] -eq $Version)) {
                [PSCustomObject]@{ Hash=$Matches[1]; Version=$Matches[2]; File="rdh-$($Matches[2])-$platform.zip" }
            }
        }
    })
    if ($entries.Count -ne 1) { throw "Expected one matching archive for $platform; found $($entries.Count)." }
    $entry = $entries[0]
    $archive = Join-Path $temp $entry.File
    Fetch $entry.File $archive
    if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ne $entry.Hash) {
        throw 'Archive checksum mismatch.'
    }
    $name = "rdh-$($entry.Version)-$platform"
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($archive)
    try {
        foreach ($item in $zip.Entries) {
            $parts = $item.FullName.Replace('\', '/').Split('/')
            if ($parts[0] -ne $name -or $parts -contains '..' -or $item.FullName.Contains(':')) {
                throw 'Unsafe archive entry.'
            }
        }
    } finally { $zip.Dispose() }
    Expand-Archive -LiteralPath $archive -DestinationPath $temp
    $tree = Join-Path $temp $name
    $node = Join-Path $tree 'node.exe'
    $cli = Join-Path $tree 'bin\remote-device-hub.mjs'
    $actual = & $node $cli --version
    if ($LASTEXITCODE -ne 0 -or $actual -ne $entry.Version) { throw 'Installed CLI version mismatch.' }
    $actualArch = & $node -p process.arch
    if ($LASTEXITCODE -ne 0 -or $actualArch -ne $arch) { throw 'Bundled Node architecture mismatch.' }
    $versions = Join-Path $Prefix 'versions'
    New-Item -ItemType Directory -Force -Path $versions, $BinDir | Out-Null
    $dest = Join-Path $versions $name
    if (Test-Path -LiteralPath $dest) { throw "Already installed: $dest" }
    Move-Item -LiteralPath $tree -Destination $dest
    $escaped = $dest.Replace('%', '%%')
    $shim = "@echo off`r`n`"$escaped\node.exe`" `"$escaped\bin\remote-device-hub.mjs`" %*`r`nexit /b %errorlevel%`r`n"
    [IO.File]::WriteAllText((Join-Path $BinDir 'rdh.cmd'), $shim, [Text.Encoding]::Default)
    Write-Host "Installed $platform with bundled Node.js: $BinDir\rdh.cmd"
    if (!$NoPair) {
        $pairArgs = @((Join-Path $dest 'bin\remote-device-hub.mjs'), '--pair-only')
        if ($Server) { $pairArgs += @('--server', $Server) }
        & (Join-Path $dest 'node.exe') @pairArgs
        if ($LASTEXITCODE -ne 0) { throw 'Pairing failed; rerun rdh.cmd --pair-only.' }
    }
    Write-Host 'Run rdh.cmd to start the Agent. No service or PATH changes were made.'
} finally {
    Remove-Item -LiteralPath $temp -Recurse -Force
}
