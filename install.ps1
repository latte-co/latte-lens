& {
$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$Version = $env:LATTE_LENS_VERSION
$InstallDir = $env:LATTE_LENS_INSTALL_DIR
$Repository = if ($env:LATTE_LENS_REPOSITORY) {
    $env:LATTE_LENS_REPOSITORY
} else {
    "latte-co/latte-lens"
}
$ApiBase = if ($env:LATTE_LENS_API_URL) {
    $env:LATTE_LENS_API_URL.TrimEnd("/")
} else {
    "https://api.github.com/repos/$Repository"
}
$DownloadBase = if ($env:LATTE_LENS_DOWNLOAD_URL) {
    $env:LATTE_LENS_DOWNLOAD_URL.TrimEnd("/")
} else {
    "https://github.com/$Repository/releases/download"
}

if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    if ($env:LOCALAPPDATA) {
        $InstallDir = Join-Path $env:LOCALAPPDATA "Programs\latte-lens\bin"
    } else {
        $InstallDir = Join-Path $HOME ".local\bin"
    }
}
$InstallDir = [IO.Path]::GetFullPath($InstallDir)

function Write-Step {
    param([string]$Message)
    Write-Host "[+] $Message" -ForegroundColor Green
}

function Write-WarningMessage {
    param([string]$Message)
    Write-Host "[!] $Message" -ForegroundColor Yellow
}

function Get-RequestHeaders {
    $headers = @{
        Accept = "application/vnd.github+json"
        "User-Agent" = "latte-lens-installer"
    }
    if ($env:GITHUB_TOKEN) {
        $headers.Authorization = "Bearer $($env:GITHUB_TOKEN)"
    }
    return $headers
}

function Invoke-ApiGet {
    param([string]$Uri)
    return Invoke-RestMethod -Uri $Uri -Headers (Get-RequestHeaders) -UseBasicParsing
}

function Receive-File {
    param(
        [string]$Uri,
        [string]$Destination
    )
    Invoke-WebRequest -Uri $Uri -OutFile $Destination -Headers @{
        "User-Agent" = "latte-lens-installer"
    } -UseBasicParsing
}

function Resolve-Release {
    if (-not [string]::IsNullOrWhiteSpace($Version)) {
        $requestedTag = if ($Version.StartsWith("v", [StringComparison]::Ordinal)) {
            $Version
        } else {
            "v$Version"
        }
        try {
            $manifest = Invoke-ApiGet "$ApiBase/releases/tags/$requestedTag"
        } catch {
            throw "release '$requestedTag' was not found"
        }
    } else {
        try {
            $manifest = Invoke-ApiGet "$ApiBase/releases/latest"
        } catch {
            Write-WarningMessage "no stable release found; falling back to the latest preview"
            $releases = @(Invoke-ApiGet "$ApiBase/releases?per_page=1")
            if ($releases.Count -eq 0) {
                throw "GitHub did not return any releases"
            }
            $manifest = $releases[0]
        }
    }

    $tag = [string]$manifest.tag_name
    if ([string]::IsNullOrWhiteSpace($tag)) {
        throw "release metadata did not include a tag"
    }
    if ($tag -notmatch '^[A-Za-z0-9._-]+$') {
        throw "release metadata included an unsafe tag: $tag"
    }

    $releaseVersion = if ($tag.StartsWith("v", [StringComparison]::Ordinal)) {
        $tag.Substring(1)
    } else {
        $tag
    }
    if ($releaseVersion.Contains("-")) {
        Write-WarningMessage "installing preview release $tag"
    }
    return @($tag, $releaseVersion)
}

function Resolve-Target {
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
        throw "install.ps1 supports Windows only"
    }

    $architecture = if ($env:PROCESSOR_ARCHITEW6432) {
        $env:PROCESSOR_ARCHITEW6432
    } else {
        $env:PROCESSOR_ARCHITECTURE
    }
    switch ($architecture.ToUpperInvariant()) {
        "AMD64" { return "x86_64-pc-windows-msvc" }
        "X86_64" { return "x86_64-pc-windows-msvc" }
        "ARM64" { throw "Windows ARM64 release packages are not available yet" }
        default { throw "unsupported Windows architecture: $architecture" }
    }
}

function Add-InstallDirToUserPath {
    if ($env:LATTE_LENS_NO_MODIFY_PATH -eq "1") {
        return
    }

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $entries = @($userPath -split ";" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
    $alreadyPresent = $false
    foreach ($entry in $entries) {
        if ([string]::Equals($entry.TrimEnd("\"), $InstallDir.TrimEnd("\"), [StringComparison]::OrdinalIgnoreCase)) {
            $alreadyPresent = $true
            break
        }
    }

    if (-not $alreadyPresent) {
        $updatedPath = if ([string]::IsNullOrWhiteSpace($userPath)) {
            $InstallDir
        } else {
            "$InstallDir;$userPath"
        }
        [Environment]::SetEnvironmentVariable("Path", $updatedPath, "User")
        Write-WarningMessage "added $InstallDir to your user PATH; open a new terminal to use latte-lens"
    }
    if (-not (($env:Path -split ";") -contains $InstallDir)) {
        $env:Path = "$InstallDir;$($env:Path)"
    }
}

function Install-LatteLens {
    if ($PSVersionTable.PSVersion.Major -lt 6) {
        [Net.ServicePointManager]::SecurityProtocol =
            [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    }

    $target = Resolve-Target
    Write-Step "detected windows/x86_64"
    $release = Resolve-Release
    $tag = $release[0]
    $releaseVersion = $release[1]
    $package = "latte-lens-$releaseVersion-$target"
    $archiveName = "$package.zip"
    $archiveUrl = "$DownloadBase/$tag/$archiveName"
    $checksumUrl = "$archiveUrl.sha256"
    $temporaryDirectory = Join-Path ([IO.Path]::GetTempPath()) "latte-lens-install-$([Guid]::NewGuid().ToString('N'))"
    $destinationTemporary = $null

    New-Item -ItemType Directory -Path $temporaryDirectory | Out-Null
    try {
        $archive = Join-Path $temporaryDirectory $archiveName
        $checksumFile = "$archive.sha256"
        Write-Step "downloading $tag"
        Receive-File $archiveUrl $archive
        Receive-File $checksumUrl $checksumFile

        $checksumText = (Get-Content -LiteralPath $checksumFile -Raw).Trim()
        $expected = @($checksumText -split '\s+')[0].ToLowerInvariant()
        if ($expected -notmatch '^[0-9a-f]{64}$') {
            throw "invalid checksum file for $archiveName"
        }
        $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash.ToLowerInvariant()
        if ($actual -ne $expected) {
            throw "checksum verification failed for $archiveName"
        }

        Expand-Archive -LiteralPath $archive -DestinationPath $temporaryDirectory
        $sourceBinary = Join-Path $temporaryDirectory "$package\latte-lens.exe"
        if (-not (Test-Path -LiteralPath $sourceBinary -PathType Leaf)) {
            throw "release archive did not contain latte-lens.exe"
        }
        $reportedVersion = (& $sourceBinary --version | Out-String).Trim()
        if ($LASTEXITCODE -ne 0) {
            throw "downloaded binary could not run on this system"
        }
        if ($reportedVersion -notlike "*$releaseVersion*") {
            throw "downloaded binary reported an unexpected version: $reportedVersion"
        }

        New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
        $destination = Join-Path $InstallDir "latte-lens.exe"
        $destinationTemporary = Join-Path $InstallDir ".latte-lens.tmp.$PID.exe"
        Copy-Item -LiteralPath $sourceBinary -Destination $destinationTemporary -Force
        Move-Item -LiteralPath $destinationTemporary -Destination $destination -Force
        $destinationTemporary = $null

        Add-InstallDirToUserPath
        Write-Step "installed $reportedVersion to $destination"
    } finally {
        if ($destinationTemporary -and (Test-Path -LiteralPath $destinationTemporary)) {
            Remove-Item -Force -LiteralPath $destinationTemporary
        }
        if (Test-Path -LiteralPath $temporaryDirectory) {
            Remove-Item -Recurse -Force -LiteralPath $temporaryDirectory
        }
    }
}

Install-LatteLens
}
