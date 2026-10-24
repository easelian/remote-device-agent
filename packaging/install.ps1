# Install bundled Node.js and optionally register a logon task or boot service.
[CmdletBinding()]
param(
    [string]$Base = 'https://github.com/be-one-click/remote-device-agent/releases/latest/download',
    [string]$Version = '',
    [string]$Prefix = '',
    [string]$BinDir = '',
    [string]$Server = '',
    [switch]$NoPair,
    [ValidateSet('Choose', 'User', 'Service', 'None')]
    [string]$Startup = 'Choose',
    [string]$Name = '',
    [string]$Config = '',
    [string]$Root = ''
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
if ($Startup -eq 'Choose') {
    $Startup = 'None'
    if (!$NoPair -and ![Console]::IsInputRedirected) {
        Write-Host '1. This time only (default; no automatic startup)'
        Write-Host '2. Run in the background now and after this user logs on'
        switch (Read-Host 'Startup mode [1/2, Enter = 1]') {
            '' { }
            '1' { }
            '2' { $Startup = 'User' }
            default { throw 'Select 1 or 2. Advanced deployment: -Startup Service (administrator required).' }
        }
    }
}
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
if ($Startup -eq 'None') {
    $existingTask = Get-ScheduledTask -TaskName "RemoteDeviceHub-$($identity.User.Value)" -ErrorAction SilentlyContinue
    $existingService = Get-Service -Name 'RemoteDeviceHub' -ErrorAction SilentlyContinue
    if ($existingTask -or $existingService) {
        throw 'Automatic startup is already configured. Stop and remove the existing registration before selecting this time only.'
    }
}
if (!$Name) {
    $Name = if ($Startup -eq 'User') { "RemoteDeviceHub-$($identity.User.Value)" } else { 'RemoteDeviceHub' }
}
if ($Name -notmatch '^[A-Za-z0-9_-]{1,100}$') { throw 'Invalid task/service name.' }
if ($Startup -ne 'None' -and $NoPair -and !$Config) {
    throw 'Background startup requires pairing. With -NoPair, supply an existing paired -Config file.'
}
if ($Config) { $Config = (Resolve-Path -LiteralPath $Config).Path }
if ($Startup -eq 'Service') {
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (!$principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'Run PowerShell as administrator for -Startup Service, or choose -Startup User.'
    }
    if (Get-Service -Name $Name -ErrorAction SilentlyContinue) { throw "Service already exists: $Name" }
    $programFiles = [Environment]::GetFolderPath('ProgramFiles')
    if (!$Prefix) { $Prefix = Join-Path $programFiles $Name }
    if ([IO.Directory]::GetParent([IO.Path]::GetFullPath($Prefix)).FullName -ne $programFiles) {
        throw 'Service binaries must be installed below Program Files.'
    }
} else {
    if (!$Prefix) { $Prefix = Join-Path $env:LOCALAPPDATA $Name }
    if ($Startup -eq 'User' -and (Get-ScheduledTask -TaskName $Name -ErrorAction SilentlyContinue)) {
        throw "Logon task already exists: $Name"
    }
}
function Check-Ancestors([string]$Path) {
    $cursor = [IO.Path]::GetFullPath($Path)
    while ($cursor) {
        if (Test-Path -LiteralPath $cursor) {
            if ((Get-Item -LiteralPath $cursor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) {
                throw "Installation paths cannot traverse reparse points: $cursor"
            }
        }
        $parent = [IO.Directory]::GetParent($cursor)
        if (!$parent) { break }
        $cursor = $parent.FullName
    }
}
function Private-Directory([string]$Path) {
    Check-Ancestors $Path
    if (Test-Path -LiteralPath $Path) { throw "Data directory already exists: $Path" }
    $acl = [Security.AccessControl.DirectorySecurity]::new()
    $acl.SetOwner($identity.User)
    $acl.SetAccessRuleProtection($true, $false)
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
        $identity.User, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow'))
    [IO.DirectoryInfo]::new($Path).Create($acl)
}
function Quote-Argument([string]$Value) {
    # CommandLineToArgvW quoting, including trailing backslashes and embedded quotes.
    return '"' + ($Value -replace '(\\*)"', '$1$1\"' -replace '(\\+)$', '$1$1') + '"'
}
Check-Ancestors $Prefix
if ($Startup -eq 'Service' -and (Test-Path -LiteralPath $Prefix)) {
    throw 'Service installation requires a new directory under Program Files.'
}

$Version = $Version -replace '^v', ''
if ($Version -and $Version -notmatch '^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$') {
    throw 'Invalid version.'
}
$Prefix = [IO.Path]::GetFullPath($Prefix)
if (!$BinDir) { $BinDir = Join-Path $Prefix 'bin' }
$BinDir = [IO.Path]::GetFullPath($BinDir)
Check-Ancestors $BinDir
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
    $archiveName = "rdh-$($entry.Version)-$platform"
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($archive)
    try {
        foreach ($item in $zip.Entries) {
            $parts = $item.FullName.Replace('\', '/').Split('/')
            if ($parts[0] -ne $archiveName -or $parts -contains '..' -or $item.FullName.Contains(':')) {
                throw 'Unsafe archive entry.'
            }
        }
    } finally { $zip.Dispose() }
    Expand-Archive -LiteralPath $archive -DestinationPath $temp
    $tree = Join-Path $temp $archiveName
    $node = Join-Path $tree 'node.exe'
    $cli = Join-Path $tree 'bin\remote-device-hub.mjs'
    $actual = & $node $cli --version
    if ($LASTEXITCODE -ne 0 -or $actual -ne $entry.Version) { throw 'Installed CLI version mismatch.' }
    $actualArch = & $node -p process.arch
    if ($LASTEXITCODE -ne 0 -or $actualArch -ne $arch) { throw 'Bundled Node architecture mismatch.' }
    $versions = Join-Path $Prefix 'versions'
    New-Item -ItemType Directory -Force -Path $versions, $BinDir | Out-Null
    $dest = Join-Path $versions $archiveName
    if (Test-Path -LiteralPath $dest) { throw "Already installed: $dest" }
    Move-Item -LiteralPath $tree -Destination $dest
    if ($Startup -eq 'Service') {
        & "$env:SystemRoot\System32\icacls.exe" $dest /reset /t /q | Out-Null
        if ($LASTEXITCODE) { throw 'Cannot set service executable permissions.' }
    }
    $escaped = $dest.Replace('%', '%%')
    $shim = "@echo off`r`n`"$escaped\node.exe`" `"$escaped\bin\remote-device-hub.mjs`" %*`r`nexit /b %errorlevel%`r`n"
    [IO.File]::WriteAllText((Join-Path $BinDir 'rdh.cmd'), $shim, [Text.Encoding]::Default)
    Write-Host "Installed $platform with bundled Node.js: $BinDir\rdh.cmd"
    $node = Join-Path $dest 'node.exe'
    $cli = Join-Path $dest 'bin\remote-device-hub.mjs'
    $agentArgs = @($cli)
    if ($Startup -eq 'None' -and $Config) { $agentArgs += @('--config', $Config) }
    if ($Startup -eq 'None' -and $Root) { $agentArgs += @('--root', [IO.Path]::GetFullPath($Root)) }
    $data = ''
    if ($Startup -ne 'None') {
        $data = if ($Startup -eq 'Service') { Join-Path $env:ProgramData $Name } else { Join-Path $Prefix 'data' }
        Private-Directory $data
        $configPath = Join-Path $data 'agent.json'
        if ($Config) {
            $copies = ,@($Config, $configPath)
            $policy = if ($Config.EndsWith('.json')) { $Config.Substring(0, $Config.Length - 5) + '.policy.json' } else { $Config + '.policy.json' }
            if (Test-Path -LiteralPath $policy) { $copies += ,@($policy, (Join-Path $data 'agent.policy.json')) }
            foreach ($copy in $copies) {
                Copy-Item -LiteralPath $copy[0] -Destination $copy[1]
                # Preserve the paired profile's policy and protect every copied file.
                $acl = [Security.AccessControl.FileSecurity]::new()
                $acl.SetOwner($identity.User)
                $acl.SetAccessRuleProtection($true, $false)
                $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($identity.User, 'FullControl', 'Allow'))
                Set-Acl -LiteralPath $copy[1] -AclObject $acl
            }
        }
        $agentArgs += @('--config', $configPath)
        if (!$Config) {
            if (!$Root) {
                $Root = if ($Startup -eq 'Service') { Join-Path $data 'work' } else { (Get-Location).Path }
            }
            if ($Startup -eq 'Service' -and !(Test-Path -LiteralPath $Root)) {
                New-Item -ItemType Directory -Path $Root | Out-Null
            }
            $agentArgs += @('--root', [IO.Path]::GetFullPath($Root))
        }
    }
    if ($Server) { $agentArgs += @('--server', $Server) }
    if (!$NoPair -or $Startup -ne 'None') {
        $pairArgs = $agentArgs + @('--pair-only')
        & (Join-Path $dest 'node.exe') @pairArgs
        if ($LASTEXITCODE -ne 0) { throw 'Pairing failed. Background startup was not registered; inspect the configuration in the installation data directory.' }
    }
    if ($Startup -ne 'None') {
        $binding = @{ version = 1; binding = @{ mode = $Startup.ToLowerInvariant(); name = $Name }; pending = $null }
        [IO.File]::WriteAllText(($configPath + '.startup.json'), ($binding | ConvertTo-Json -Depth 4), [Text.UTF8Encoding]::new($false))
        # Elevated installers may otherwise create a file owned by Administrators.
        # User-mode Agents require the current user's SID, even with inherited private ACLs.
        $startupAcl = [Security.AccessControl.FileSecurity]::new()
        $startupAcl.SetOwner($identity.User)
        $startupAcl.SetAccessRuleProtection($true, $false)
        $startupAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($identity.User, 'FullControl', 'Allow'))
        Set-Acl -LiteralPath ($configPath + '.startup.json') -AclObject $startupAcl
    }
    $registered = $false
    try {
        $backgroundArgs = $agentArgs + @('--no-browser', '--recover-lock')
        if ($Startup -eq 'User') {
            $runner = "'" + (Join-Path $dest 'windows-logon.ps1').Replace("'", "''") + "'"
            $nodeLiteral = "'" + $node.Replace("'", "''") + "'"
            $logLiteral = "'" + (Join-Path $data 'agent.log').Replace("'", "''") + "'"
            $argumentLiterals = ($backgroundArgs | ForEach-Object { "'" + $_.Replace("'", "''") + "'" }) -join ','
            $command = "& $runner -Node $nodeLiteral -Log $logLiteral -AgentArguments @($argumentLiterals)"
            $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($command))
            $powershell = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
            $action = New-ScheduledTaskAction -Execute $powershell -Argument "-NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -EncodedCommand $encoded" -WorkingDirectory $Prefix
            $trigger = New-ScheduledTaskTrigger -AtLogOn -User $identity.Name
            $principal = New-ScheduledTaskPrincipal -UserId $identity.Name -LogonType Interactive -RunLevel Limited
            $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) -MultipleInstances IgnoreNew -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable
            Register-ScheduledTask -TaskName $Name -Action $action -Trigger $trigger -Principal $principal -Settings $settings | Out-Null
            $registered = $true
            Start-ScheduledTask -TaskName $Name
            Write-Host "Logon task registered and started: $Name"
            Write-Host "Stop: Stop-ScheduledTask -TaskName '$Name'"
            Write-Host "Start: Start-ScheduledTask -TaskName '$Name'"
            Write-Host "Remove startup: Unregister-ScheduledTask -TaskName '$Name' -Confirm:`$false"
        } elseif ($Startup -eq 'Service') {
            # Pair interactively, then transfer only the dedicated data tree to LocalService.
            # The Agent requires protected ACLs and an owner matching its runtime identity.
            $serviceSid = [Security.Principal.SecurityIdentifier]::new('S-1-5-19')
            $items = @(Get-ChildItem -LiteralPath $data -Recurse -Force) + @(Get-Item -LiteralPath $data)
            foreach ($item in $items) {
                Check-Ancestors $item.FullName
                $acl = if ($item.PSIsContainer) { [Security.AccessControl.DirectorySecurity]::new() } else { [Security.AccessControl.FileSecurity]::new() }
                $acl.SetAccessRuleProtection($true, $false)
                $inherit = if ($item.PSIsContainer) { 'ContainerInherit,ObjectInherit' } else { 'None' }
                foreach ($sid in @($serviceSid, [Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'))) {
                    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($sid, 'FullControl', $inherit, 'None', 'Allow'))
                }
                Set-Acl -LiteralPath $item.FullName -AclObject $acl
                & "$env:SystemRoot\System32\icacls.exe" $item.FullName /setowner '*S-1-5-19' /q | Out-Null
                if ($LASTEXITCODE) { throw 'Cannot assign service data ownership.' }
            }
            $hostPath = Join-Path $dest 'shawl.exe'
            $hostArgs = @('run', '--name', $Name, '--cwd', $data, '--log-dir', $data,
                '--env', "XDG_STATE_HOME=$data", '--stop-timeout', '15000', '--kill-process-tree',
                '--restart', '--restart-delay', '5000', '--', $node) + $backgroundArgs
            $binary = ((@($hostPath) + $hostArgs) | ForEach-Object { Quote-Argument $_ }) -join ' '
            New-Service -Name $Name -BinaryPathName $binary -StartupType Automatic -Credential ([PSCredential]::new('NT AUTHORITY\LocalService', [Security.SecureString]::new())) -DisplayName "Remote Device Hub ($Name)" | Out-Null
            $registered = $true
            Start-Service -Name $Name
            Write-Host "Boot service registered and started as LocalService: $Name"
            Write-Host "Service workspace/config/logs: $data"
            Write-Host 'LocalService must have access to any custom file roots; user desktop interaction is unavailable.'
            Write-Host "Stop/start: Stop-Service '$Name'; Start-Service '$Name'"
            Write-Host "Remove startup after stopping: sc.exe delete $Name"
        } else {
            if ($NoPair) {
                Write-Host 'Installed only. Run rdh.cmd to pair and start the Agent.'
            } else {
                Write-Host 'Running this time only. Press Ctrl+C to disconnect; automatic startup is disabled.'
                & $node @agentArgs
                if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne 130) { throw "Agent exited with code $LASTEXITCODE." }
            }
        }
    } catch {
        if ($registered -and $Startup -eq 'User') {
            Stop-ScheduledTask -TaskName $Name -ErrorAction SilentlyContinue
            Unregister-ScheduledTask -TaskName $Name -Confirm:$false
        } elseif ($registered -and $Startup -eq 'Service') {
            Stop-Service -Name $Name -ErrorAction SilentlyContinue
            & "$env:SystemRoot\System32\sc.exe" delete $Name | Out-Null
        }
        throw
    }
} finally {
    Remove-Item -LiteralPath $temp -Recurse -Force
}
