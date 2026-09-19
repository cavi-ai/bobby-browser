$ErrorActionPreference = "Stop"

$repo = if ($env:BOBBY_REPO) { $env:BOBBY_REPO } else { "cavi-ai/bobby-browser" }
$installDir = if ($env:INSTALL_DIR) { $env:INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "Programs\bobby-browser\bin" }
$shareDir = if ($env:BOBBY_SHARE_DIR) { $env:BOBBY_SHARE_DIR } else { Join-Path (Split-Path $installDir -Parent) "share\bobby-browser" }

if ($env:PROCESSOR_ARCHITECTURE -ne "AMD64" -and $env:PROCESSOR_ARCHITEW6432 -ne "AMD64") {
    throw "install.ps1: unsupported architecture; Windows releases require x64"
}

$tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("bobby-install-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $tempRoot | Out-Null

function Install-File([string]$source, [string]$destination) {
    New-Item -ItemType Directory -Force -Path (Split-Path $destination -Parent) | Out-Null
    $suffix = [guid]::NewGuid().ToString("N")
    $pending = "$destination.new.$suffix"
    $backup = "$destination.old.$suffix"
    Copy-Item -LiteralPath $source -Destination $pending -Force
    if (Test-Path -LiteralPath $destination) {
        [System.IO.File]::Replace($pending, $destination, $backup)
        Remove-Item -LiteralPath $backup -Force
    } else {
        [System.IO.File]::Move($pending, $destination)
    }
}

function Replace-Tree([string]$source, [string]$destination) {
    New-Item -ItemType Directory -Force -Path (Split-Path $destination -Parent) | Out-Null
    $suffix = [guid]::NewGuid().ToString("N")
    $pending = "$destination.new.$suffix"
    $previous = "$destination.old.$suffix"
    Copy-Item -LiteralPath $source -Destination $pending -Recurse
    if (Test-Path -LiteralPath $destination) {
        Move-Item -LiteralPath $destination -Destination $previous
    }
    try {
        Move-Item -LiteralPath $pending -Destination $destination
        if (Test-Path -LiteralPath $previous) {
            Remove-Item -LiteralPath $previous -Recurse -Force
        }
    } catch {
        if ((Test-Path -LiteralPath $previous) -and -not (Test-Path -LiteralPath $destination)) {
            Move-Item -LiteralPath $previous -Destination $destination
        }
        throw
    }
}

try {
    if ($env:BOBBY_VERSION) {
        $version = $env:BOBBY_VERSION.TrimStart("v")
        $tag = "v$version"
    } elseif ($env:BOBBY_ARCHIVE) {
        throw "install.ps1: BOBBY_VERSION is required with BOBBY_ARCHIVE"
    } else {
        $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/latest"
        $tag = $release.tag_name
        $version = $tag.TrimStart("v")
    }

    $asset = "bobby-browser-$version-windows-x64.zip"
    if ($env:BOBBY_ARCHIVE) {
        $archive = (Resolve-Path -LiteralPath $env:BOBBY_ARCHIVE).Path
    } else {
        $archive = Join-Path $tempRoot $asset
        $url = "https://github.com/$repo/releases/download/$tag/$asset"
        Write-Output "install.ps1: fetching $url"
        Invoke-WebRequest -Uri $url -OutFile $archive
    }

    Expand-Archive -LiteralPath $archive -DestinationPath $tempRoot
    $stage = Join-Path $tempRoot "bobby-browser-$version-windows-x64"
    foreach ($binary in @("bobby.exe", "mcp-gateway.exe", "acp-gateway.exe")) {
        $source = Join-Path $stage $binary
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            throw "install.ps1: archive missing $binary"
        }
        Install-File $source (Join-Path $installDir $binary)
        Write-Output "install.ps1: installed $(Join-Path $installDir $binary)"
    }

    $visionSource = Join-Path $stage "scripts\vision-mlx"
    $companionSource = Join-Path $stage "firefox-companion"
    if (-not (Test-Path -LiteralPath $visionSource -PathType Container)) {
        throw "install.ps1: archive missing scripts/vision-mlx"
    }
    if (-not (Test-Path -LiteralPath $companionSource -PathType Container)) {
        throw "install.ps1: archive missing firefox-companion"
    }
    Replace-Tree $visionSource (Join-Path $shareDir "scripts\vision-mlx")
    Replace-Tree $companionSource (Join-Path $shareDir "firefox-companion")
    Write-Output "install.ps1: next: bobby doctor"
} finally {
    Remove-Item -LiteralPath $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
}
