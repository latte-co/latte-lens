[CmdletBinding()]
param(
    [string]$BuildTarget = $env:BUILD_TARGET
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Push-Location $root

try {
    $metadataJson = & cargo metadata --format-version 1 --no-deps --locked
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata --locked failed"
    }
    $metadata = ($metadataJson -join "`n") | ConvertFrom-Json
    $package = $metadata.packages | Where-Object { $_.name -eq "lattelens" } | Select-Object -First 1
    if ($null -eq $package) {
        throw "could not find the lattelens package in Cargo metadata"
    }

    $hostLine = & rustc -vV | Where-Object { $_ -like "host: *" } | Select-Object -First 1
    if ($LASTEXITCODE -ne 0 -or $null -eq $hostLine) {
        throw "could not determine the rustc host target"
    }
    $hostTarget = $hostLine.Substring("host: ".Length)
    $packageTarget = if ($BuildTarget) { $BuildTarget } else { $hostTarget }
    if ($packageTarget -notmatch "windows") {
        throw "build-release.ps1 packages Windows targets only (got '$packageTarget')"
    }

    $targetDir = if ($env:CARGO_TARGET_DIR) {
        if ([IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) {
            $env:CARGO_TARGET_DIR
        } else {
            Join-Path $root $env:CARGO_TARGET_DIR
        }
    } else {
        Join-Path $root "target"
    }

    $buildArgs = @("build", "--release", "--locked")
    if ($BuildTarget) {
        $buildArgs += @("--target", $BuildTarget)
        $binaryPath = Join-Path $targetDir "$BuildTarget/release/lattelens.exe"
    } else {
        $binaryPath = Join-Path $targetDir "release/lattelens.exe"
    }

    & cargo @buildArgs
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build --release --locked failed"
    }
    if (-not (Test-Path -LiteralPath $binaryPath -PathType Leaf)) {
        throw "release binary was not created at '$binaryPath'"
    }

    $packageName = "lattelens-$($package.version)-$packageTarget"
    $distDir = Join-Path $root "dist"
    $packageDir = Join-Path $distDir $packageName
    $archive = Join-Path $distDir "$packageName.zip"
    $checksum = "$archive.sha256"

    New-Item -ItemType Directory -Force -Path $distDir | Out-Null
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $packageDir
    Remove-Item -Force -ErrorAction SilentlyContinue $archive, $checksum
    New-Item -ItemType Directory -Path $packageDir | Out-Null

    Copy-Item -LiteralPath $binaryPath -Destination (Join-Path $packageDir "lattelens.exe")
    Copy-Item -LiteralPath (Join-Path $root "README.md") -Destination $packageDir
    Copy-Item -LiteralPath (Join-Path $root "LICENSE") -Destination $packageDir
    Compress-Archive -LiteralPath $packageDir -DestinationPath $archive -CompressionLevel Optimal
    Remove-Item -Recurse -Force $packageDir

    $digest = (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash.ToLowerInvariant()
    "$digest  $([IO.Path]::GetFileName($archive))" | Set-Content -LiteralPath $checksum -Encoding ascii

    Write-Output "Created $archive"
    Write-Output "Created $checksum"
} finally {
    Pop-Location
}
