# Run once per Windows runner: installs VB-CABLE as the WASAPI render endpoint the
# IDE worker tests open, and fails unless it is active at 48 kHz stereo.
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 3.0

$packageUrl = 'https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack43.zip'
$packageSha256 = '66fd0a4d9f4896ff41632b7e3d53892c085c4561f53e8ae8d0f0bc10eedd1cdd'
$hardwareId = 'VBAudioVACWDM'
$renderKey = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render'
$friendlyNameValue = '{a45c254e-df1c-4efd-8020-67d146a850e0},2'
$deviceFormatValue = '{f19f064d-082c-4e27-bc73-6882a1bb8e4c},0'

$devcon = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\Tools\*\x64\devcon.exe" -ErrorAction SilentlyContinue |
    Sort-Object FullName | Select-Object -Last 1
if ($null -eq $devcon) {
    throw 'devcon.exe from the Windows Driver Kit is not installed on this runner'
}

$work = Join-Path $env:RUNNER_TEMP 'vbcable'
New-Item -ItemType Directory -Force -Path $work | Out-Null
$package = Join-Path $work 'VBCABLE_Driver_Pack43.zip'
Invoke-WebRequest -Uri $packageUrl -OutFile $package -MaximumRetryCount 3 -RetryIntervalSec 5
$actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $package).Hash.ToLowerInvariant()
if ($actual -ne $packageSha256) {
    throw "VB-CABLE package hash is $actual, expected $packageSha256"
}
$driver = Join-Path $work 'driver'
Expand-Archive -LiteralPath $package -DestinationPath $driver -Force

$signature = Get-AuthenticodeSignature -LiteralPath (Join-Path $driver 'vbaudio_cable64_win7.cat')
if ($null -eq $signature.SignerCertificate) {
    throw 'VB-CABLE driver catalog carries no signer certificate'
}
$certificate = Join-Path $work 'vbcable.cer'
[IO.File]::WriteAllBytes($certificate, $signature.SignerCertificate.Export('Cert'))
& certutil.exe -addstore -f TrustedPublisher $certificate
if ($LASTEXITCODE -ne 0) { throw "certutil exited with $LASTEXITCODE" }

& $devcon.FullName install (Join-Path $driver 'vbMmeCable64_win7.inf') $hardwareId
if ($LASTEXITCODE -ne 0) { throw "devcon exited with $LASTEXITCODE" }

$audio = Get-Service -Name Audiosrv
Write-Host "Windows Audio service: $($audio.Status), $($audio.StartType)"
if ($audio.Status -ne 'Running') {
    Set-Service -Name Audiosrv -StartupType Automatic
    Start-Service -Name Audiosrv
}

function Get-RegistryValue([string] $path, [string] $name) {
    # Get-ItemPropertyValue throws on a missing value even with SilentlyContinue.
    $item = Get-ItemProperty -LiteralPath $path -ErrorAction SilentlyContinue
    if ($null -eq $item) { return $null }
    $property = $item.PSObject.Properties[$name]
    if ($null -eq $property) { return $null }
    return , $property.Value
}

# Windows publishes an endpoint as active before it writes the endpoint's default format.
$deadline = (Get-Date).AddSeconds(60)
$ready = $false
$seen = @{}
$polls = 0
do {
    $polls += 1
    foreach ($endpoint in @(Get-ChildItem $renderKey -ErrorAction SilentlyContinue)) {
        if ((Get-RegistryValue $endpoint.PSPath 'DeviceState') -ne 1) { continue }
        $properties = Join-Path $endpoint.PSPath 'Properties'
        $name = Get-RegistryValue $properties $friendlyNameValue
        $format = Get-RegistryValue $properties $deviceFormatValue
        if ($null -eq $format -or $format.Length -lt 16) {
            $seen[$endpoint.PSChildName] = "Render endpoint '$name' reports no default format"
            continue
        }
        $channels = [BitConverter]::ToUInt16([byte[]] $format, 10)
        $rate = [BitConverter]::ToUInt32([byte[]] $format, 12)
        $seen[$endpoint.PSChildName] = "Render endpoint '$name': $channels channels at $rate Hz"
        if ($channels -eq 2 -and $rate -eq 48000) { $ready = $true }
    }
    if ($ready) { break }
    Start-Sleep -Seconds 1
} while ((Get-Date) -lt $deadline)
$seen.Values | ForEach-Object { Write-Host $_ }
Write-Host "Render endpoints polled $polls times"
if ($seen.Count -eq 0) {
    throw 'no active WASAPI render endpoint appeared'
}
if (-not $ready) {
    throw 'no active render endpoint has a 48 kHz stereo default format'
}
