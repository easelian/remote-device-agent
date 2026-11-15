# Disposable CI-only volume. Never select or format a physical disk.
$ErrorActionPreference = 'Stop'
$rdhVhd = Join-Path $env:RUNNER_TEMP ('rdh-exfat-' + [Guid]::NewGuid().ToString('N') + '.vhdx')
$rdhDiskScript = $rdhVhd + '.txt'
$rdhLetter = @('R','S','T','U','V','W','X','Y','Z') | Where-Object { !(Test-Path ($_ + ':\')) } | Select-Object -First 1
if (!$rdhLetter) { throw 'No spare drive letter for the disposable exFAT test.' }
try {
    @"
create vdisk file="$rdhVhd" maximum=128 type=expandable
select vdisk file="$rdhVhd"
attach vdisk
create partition primary
format fs=exfat quick label=RDH_EXFAT_TEST
assign letter=$rdhLetter
"@ | Set-Content -LiteralPath $rdhDiskScript -Encoding ascii
    & diskpart.exe /s $rdhDiskScript
    if ($LASTEXITCODE -ne 0) { throw 'Disposable exFAT volume creation failed.' }
    $rdhVolume = Get-Volume -DriveLetter $rdhLetter
    if ($rdhVolume.FileSystem -ne 'exFAT' -or $rdhVolume.FileSystemLabel -ne 'RDH_EXFAT_TEST') { throw 'Unexpected test volume.' }
    $env:RDH_TEST_EXFAT_ROOT = $rdhLetter + ':\'
    cargo +1.93.0 test --profile ci --locked -p rdh-agent --lib --all-features --target $env:RDH_TARGET native_windows_exfat_ -- --ignored --nocapture --test-threads=1
    if ($LASTEXITCODE -ne 0) { throw 'exFAT file operation regression failed.' }
    cargo +1.93.0 test --profile ci --locked -p rdh-agent --all-features --target $env:RDH_TARGET --test windows_pairing native_windows_cli_pairs_with_exfat_shared_root -- --ignored --exact --nocapture
    if ($LASTEXITCODE -ne 0) { throw 'exFAT post-binding startup regression failed.' }
} finally {
    Remove-Item Env:RDH_TEST_EXFAT_ROOT -ErrorAction SilentlyContinue
    @"
select vdisk file="$rdhVhd"
detach vdisk
"@ | Set-Content -LiteralPath $rdhDiskScript -Encoding ascii
    & diskpart.exe /s $rdhDiskScript
    Remove-Item -LiteralPath $rdhVhd -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath $rdhDiskScript -Force -ErrorAction SilentlyContinue
}
