# Install a SHA-256-verified NovaGPT release binary for Windows x64 (current user).
# Example: powershell -ExecutionPolicy Bypass -File .\script\install.ps1 -Version v0.1.0
[CmdletBinding()]
param(
    [ValidatePattern('^(latest|v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?)$')]
    [string]$Version = 'latest',

    [string]$InstallDir = (Join-Path $env:LOCALAPPDATA 'Programs\NovaGPT\bin')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not [System.IO.Path]::IsPathRooted($InstallDir)) {
    throw 'InstallDir must be an absolute path.'
}
if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne
    [System.Runtime.InteropServices.Architecture]::X64) {
    throw 'This installer currently supports Windows x64. No Windows ARM64 release binary is published.'
}

$repository = 'aintDatCap/novagpt'
$asset = 'novagpt-windows-x86_64.zip'
if ($Version -eq 'latest') {
    $baseUrl = "https://github.com/$repository/releases/latest/download"
} else {
    $baseUrl = "https://github.com/$repository/releases/download/$Version"
}

# Windows PowerShell 5.1 may otherwise negotiate a TLS version rejected by GitHub.
[Net.ServicePointManager]::SecurityProtocol =
    [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("novagpt-install-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $tempDir | Out-Null

try {
    $archive = Join-Path $tempDir $asset
    $checksum = "$archive.sha256"
    Write-Host "Downloading $asset from $Version..."
    Invoke-WebRequest -Uri "$baseUrl/$asset" -OutFile $archive -UseBasicParsing
    Invoke-WebRequest -Uri "$baseUrl/$asset.sha256" -OutFile $checksum -UseBasicParsing

    $expected = (Get-Content -LiteralPath $checksum -Raw).Trim()
    if ($expected -notmatch '^[a-fA-F0-9]{64}$') {
        throw 'The release SHA-256 checksum is invalid.'
    }
    $actual = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash
    if ($actual -ine $expected) {
        throw 'SHA-256 verification failed; NovaGPT was not installed.'
    }

    $extractDir = Join-Path $tempDir 'extracted'
    Expand-Archive -LiteralPath $archive -DestinationPath $extractDir
    $executable = Join-Path $extractDir 'novagpt.exe'
    if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
        throw 'Release archive does not contain novagpt.exe.'
    }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    $destination = Join-Path $InstallDir 'novagpt.exe'
    Copy-Item -LiteralPath $executable -Destination $destination -Force

    # Persist PATH for the user; also update this PowerShell process.
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $normalizedDir = $InstallDir.TrimEnd('\', '/')
    $existing = @(($userPath -split ';') | ForEach-Object { $_.Trim().TrimEnd('\', '/') })
    if ($existing -notcontains $normalizedDir) {
        $newPath = if ([string]::IsNullOrWhiteSpace($userPath)) {
            $InstallDir
        } else {
            "$userPath;$InstallDir"
        }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        Write-Host "Added $InstallDir to your user PATH."
    }
    $processPath = @( ($env:Path -split ';') | ForEach-Object { $_.Trim().TrimEnd('\', '/') } )
    if ($processPath -notcontains $normalizedDir) {
        $env:Path = "$env:Path;$InstallDir"
    }

    Write-Host "Installed: $destination"
    Write-Host 'Open a new terminal to use novagpt from anywhere.'
}
finally {
    Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
}
