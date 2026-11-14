# Mudcrab Installer for Windows
# Usage in PowerShell:
#   irm https://raw.githubusercontent.com/Mudcrab-Team/mudcrab/main/scripts/install.ps1 | iex
#
# "Hey you, you're finally awake."

$ErrorActionPreference = 'Stop'
# Windows PowerShell 5.1 renders a progress bar that slows Invoke-WebRequest by an order of magnitude.
$ProgressPreference = 'SilentlyContinue'

$Repo = "Mudcrab-Team/mudcrab"
$Target = "x86_64-pc-windows-msvc"

# 1. Architecture Check
if ($env:PROCESSOR_ARCHITECTURE -ne "AMD64" -and $env:PROCESSOR_ARCHITEW6432 -ne "AMD64") {
    throw "Mudcrab currently requires 64-bit Windows (x86_64 / AMD64)."
}

Write-Host "==> Detected Windows 64-bit ($Target)" -ForegroundColor Cyan

# 2. Resolve Version
$Tag = $env:MUDCRAB_VERSION

if (-not $Tag -or $Tag -eq "latest") {
    Write-Host "==> Resolving latest Mudcrab release..." -ForegroundColor Cyan
    # /releases/latest redirects to /releases/tag/<tag>; with no published release it
    # redirects to /releases instead, which must not be mistaken for a tag.
    $Response = $null
    try {
        $Request = [System.Net.WebRequest]::Create("https://github.com/$Repo/releases/latest")
        $Request.AllowAutoRedirect = $true
        $Response = $Request.GetResponse()
        $FinalPath = $Response.ResponseUri.AbsolutePath
    } catch {
        throw "Could not resolve the latest Mudcrab release: $_"
    } finally {
        if ($Response) { $Response.Close() }
    }
    if ($FinalPath -notmatch '/releases/tag/([^/]+)/?$') {
        throw "No published release found for $Repo. Set MUDCRAB_VERSION to install a specific tag."
    }
    $Tag = $Matches[1]
} else {
    if (-not $Tag.StartsWith("v")) {
        $Tag = "v$Tag"
    }
}

$ArchiveFileName = "mudcrab-$Tag-$Target.zip"
$DownloadUrl = "https://github.com/$Repo/releases/download/$Tag/$ArchiveFileName"
$ChecksumUrl = "https://github.com/$Repo/releases/download/$Tag/SHA256SUMS.txt"

# 3. Setup Install Directories
$InstallDir = if ($env:MUDCRAB_INSTALL_DIR) { $env:MUDCRAB_INSTALL_DIR } else { "$env:LOCALAPPDATA\Mudcrab" }
$BinDir = "$InstallDir\bin"

if (-not (Test-Path $BinDir)) {
    New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
}

$TempDir = [System.IO.Path]::GetTempPath()
$TempZip = Join-Path $TempDir $ArchiveFileName
$TempChecksums = Join-Path $TempDir "SHA256SUMS-$Tag.txt"

# 4. Download Release Archive
Write-Host "==> Downloading Mudcrab $Tag..." -ForegroundColor Cyan
try {
    [System.Net.ServicePointManager]::SecurityProtocol = [System.Net.ServicePointManager]::SecurityProtocol -bor [System.Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -Uri $DownloadUrl -OutFile $TempZip -UseBasicParsing
} catch {
    throw "Failed to download Mudcrab from: $DownloadUrl`nVerify that release $Tag exists."
}

# 4.1 Verify Checksum
Write-Host "==> Verifying SHA256 checksum..." -ForegroundColor Cyan
try {
    # Do not accept a checksum file left behind by a previous installation attempt.
    if (Test-Path $TempChecksums) {
        Remove-Item -Path $TempChecksums -Force
    }
    try {
        Invoke-WebRequest -Uri $ChecksumUrl -OutFile $TempChecksums -UseBasicParsing -ErrorAction Stop
    } catch {
        throw "Checksum verification failed: $_"
    }
    if (-not (Test-Path $TempChecksums)) {
        throw "Checksum verification failed: SHA256SUMS.txt was not downloaded."
    }

    $ExpectedHash = $null
    foreach ($Line in Get-Content $TempChecksums) {
        $Parts = $Line -split '\s+'
        if ($Parts.Length -ge 2 -and $Parts[1].TrimStart('*') -eq $ArchiveFileName) {
            $ExpectedHash = $Parts[0].Trim().ToLower()
        }
    }
    if (-not $ExpectedHash) {
        throw "Checksum verification failed: No entry for $ArchiveFileName in SHA256SUMS.txt."
    }

    $ActualHash = (Get-FileHash -Path $TempZip -Algorithm SHA256).Hash.ToLower()
    if ($ActualHash -ne $ExpectedHash) {
        throw "Checksum verification failed!`nExpected: $ExpectedHash`nGot:      $ActualHash"
    }
    Write-Host "[OK] Checksum verified ($ExpectedHash)" -ForegroundColor Green
} finally {
    if (Test-Path $TempChecksums) {
        Remove-Item -Path $TempChecksums -Force -ErrorAction SilentlyContinue
    }
}

# 5. Extract Binaries
Write-Host "==> Installing binaries to $BinDir..." -ForegroundColor Cyan
$StageDir = Join-Path $TempDir "mudcrab-$Tag-stage"
try {
    if (Test-Path $StageDir) { Remove-Item -Path $StageDir -Recurse -Force }
    Expand-Archive -Path $TempZip -DestinationPath $StageDir -Force
    try {
        Copy-Item -Path "$StageDir\*" -Destination $BinDir -Recurse -Force
    } catch {
        throw "Could not replace the installed files in $BinDir. Close Mudcrab (launcher, engine, converter) and run the installer again.`n$_"
    }
} finally {
    if (Test-Path $TempZip) {
        Remove-Item -Path $TempZip -Force -ErrorAction SilentlyContinue
    }
    if (Test-Path $StageDir) {
        Remove-Item -Path $StageDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# 6. Create Shortcuts (Desktop & Start Menu)
try {
    $LauncherExe = "$BinDir\launcher.exe"
    if (Test-Path $LauncherExe) {
        $WScriptShell = New-Object -ComObject WScript.Shell

        # Desktop Shortcut
        $DesktopPath = [Environment]::GetFolderPath("Desktop")
        $DesktopShortcut = $WScriptShell.CreateShortcut("$DesktopPath\Mudcrab.lnk")
        $DesktopShortcut.TargetPath = $LauncherExe
        $DesktopShortcut.WorkingDirectory = $InstallDir
        $DesktopShortcut.Description = "Mudcrab - Modern Skyrim Engine & Converter"
        $DesktopShortcut.Save()
        Write-Host "[OK] Created Desktop shortcut" -ForegroundColor Green

        # Start Menu Shortcut
        $StartMenuPath = [Environment]::GetFolderPath("Programs")
        $MudcrabStartFolder = "$StartMenuPath\Mudcrab"
        if (-not (Test-Path $MudcrabStartFolder)) {
            New-Item -ItemType Directory -Path $MudcrabStartFolder -Force | Out-Null
        }
        $StartShortcut = $WScriptShell.CreateShortcut("$MudcrabStartFolder\Mudcrab.lnk")
        $StartShortcut.TargetPath = $LauncherExe
        $StartShortcut.WorkingDirectory = $InstallDir
        $StartShortcut.Description = "Mudcrab - Modern Skyrim Engine & Converter"
        $StartShortcut.Save()
        Write-Host "[OK] Created Start Menu shortcut" -ForegroundColor Green
    }
} catch {
    Write-Warning "Could not create shortcuts: $_"
}

Write-Host ""
Write-Host "Mudcrab $Tag successfully installed!" -ForegroundColor Green
Write-Host "Open Mudcrab from your Desktop/Start Menu, or run $BinDir\launcher.exe." -ForegroundColor Yellow
