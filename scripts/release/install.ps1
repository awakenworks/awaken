# Install one exact, checksum-verified Awaken release on Windows x86-64.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidatePattern('^v[0-9]+\.[0-9]+\.[0-9]+$')]
    [string] $Version,

    [string] $InstallDir = (Join-Path $env:LOCALAPPDATA 'Awaken\bin'),

    [string] $ReleaseBaseUrl = '',

    [switch] $AllowInsecure
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not [Environment]::Is64BitOperatingSystem) {
    throw 'unsupported platform: Awaken requires 64-bit Windows'
}

$target = 'x86_64-pc-windows-msvc'
$archive = "awaken-$Version-$target.zip"
if ([string]::IsNullOrWhiteSpace($ReleaseBaseUrl)) {
    $ReleaseBaseUrl = "https://github.com/awakenworks/awaken/releases/download/$Version"
}
$ReleaseBaseUrl = $ReleaseBaseUrl.TrimEnd('/')
if (-not $AllowInsecure -and -not $ReleaseBaseUrl.StartsWith('https://', [StringComparison]::OrdinalIgnoreCase)) {
    throw "refusing non-HTTPS release URL: $ReleaseBaseUrl"
}

$temporary = Join-Path ([IO.Path]::GetTempPath()) ("awaken-install-" + [Guid]::NewGuid().ToString('N'))
$stagedAwaken = $null
$stagedCompanion = $null
New-Item -ItemType Directory -Path $temporary | Out-Null

try {
    $archivePath = Join-Path $temporary $archive
    $checksumPath = "$archivePath.sha256"
    Invoke-WebRequest -UseBasicParsing -Uri "$ReleaseBaseUrl/$archive" -OutFile $archivePath
    Invoke-WebRequest -UseBasicParsing -Uri "$ReleaseBaseUrl/$archive.sha256" -OutFile $checksumPath

    $checksumLine = (Get-Content -Raw -LiteralPath $checksumPath).Trim()
    if ($checksumLine -notmatch '^([0-9a-fA-F]{64})\s+\*?(.+)$') {
        throw 'release checksum file has an invalid format'
    }
    if ($Matches[2] -ne $archive) {
        throw "release checksum names '$($Matches[2])', expected '$archive'"
    }
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $archivePath).Hash
    if (-not $actual.Equals($Matches[1], [StringComparison]::OrdinalIgnoreCase)) {
        throw 'release archive SHA-256 does not match'
    }

    Expand-Archive -LiteralPath $archivePath -DestinationPath $temporary
    $packageRoot = Join-Path $temporary $archive.Substring(0, $archive.Length - 4)
    $candidate = Join-Path $packageRoot 'awaken.exe'
    $candidateCompanion = Join-Path $packageRoot 'awaken-sandbox.exe'
    if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) {
        throw 'release archive does not contain the expected awaken.exe binary'
    }
    if (-not (Test-Path -LiteralPath $candidateCompanion -PathType Leaf)) {
        throw 'release archive does not contain the expected awaken-sandbox.exe companion'
    }

    $reported = (& $candidate --version | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $reported -ne "awaken $($Version.Substring(1))") {
        throw "release binary reports '$reported', expected 'awaken $($Version.Substring(1))'"
    }
    & $candidateCompanion hand --check
    if ($LASTEXITCODE -ne 0) {
        throw 'release companion failed its hand capability check'
    }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $nonce = [Guid]::NewGuid().ToString('N')
    $stagedAwaken = Join-Path $InstallDir ".awaken.$nonce.tmp"
    $stagedCompanion = Join-Path $InstallDir ".awaken-sandbox.$nonce.tmp"
    Copy-Item -LiteralPath $candidate -Destination $stagedAwaken
    Copy-Item -LiteralPath $candidateCompanion -Destination $stagedCompanion

    # Install the companion first. Moving the user-facing entrypoint last is the
    # activation fence after both release payloads passed validation and staging.
    Move-Item -Force -LiteralPath $stagedCompanion -Destination (Join-Path $InstallDir 'awaken-sandbox.exe')
    $stagedCompanion = $null
    Move-Item -Force -LiteralPath $stagedAwaken -Destination (Join-Path $InstallDir 'awaken.exe')
    $stagedAwaken = $null

    Write-Host "installed awaken $($Version.Substring(1)) and its sandbox companion at $InstallDir"
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (($userPath -split ';') -notcontains $InstallDir) {
        Write-Host "add $InstallDir to your user PATH to run awaken from any directory"
    }
}
finally {
    if ($null -ne $stagedAwaken -and (Test-Path -LiteralPath $stagedAwaken)) {
        Remove-Item -Force -LiteralPath $stagedAwaken
    }
    if ($null -ne $stagedCompanion -and (Test-Path -LiteralPath $stagedCompanion)) {
        Remove-Item -Force -LiteralPath $stagedCompanion
    }
    if (Test-Path -LiteralPath $temporary) {
        Remove-Item -Recurse -Force -LiteralPath $temporary
    }
}
