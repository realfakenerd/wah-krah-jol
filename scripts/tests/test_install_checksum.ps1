# Run from the repository root: pwsh -NoProfile -File scripts/tests/test_install_checksum.ps1
# The child processes replace network and extraction calls with local fixtures.
param(
    [switch]$Child,
    [string]$Scenario,
    [string]$FixtureDir
)

$ErrorActionPreference = 'Stop'
$Installer = Join-Path (Split-Path -Parent $PSScriptRoot) 'install.ps1'

if ($Child) {
    $env:TEMP = $FixtureDir
    $env:TMP = $FixtureDir
    $env:TMPDIR = $FixtureDir
    $env:PROCESSOR_ARCHITECTURE = 'AMD64'
    $env:MUDCRAB_VERSION = "vtest-$PID"
    $env:MUDCRAB_INSTALL_DIR = Join-Path $FixtureDir 'install'
    $ArchiveName = "mudcrab-$env:MUDCRAB_VERSION-x86_64-pc-windows-msvc.zip"
    $Sample = Join-Path $FixtureDir 'sample'
    Set-Content -Path $Sample -Value 'archive fixture'
    $Hash = (Get-FileHash -Path $Sample -Algorithm SHA256).Hash.ToLower()
    # A stale, valid manifest must never make a failed download succeed.
    $StaleManifest = Join-Path ([System.IO.Path]::GetTempPath()) "SHA256SUMS-$env:MUDCRAB_VERSION.txt"
    Set-Content -Path $StaleManifest -Value "$Hash  $ArchiveName"

    function Invoke-WebRequest {
        param($Uri, $OutFile, [switch]$UseBasicParsing, $ErrorAction)
        if ($Uri -notmatch 'SHA256SUMS.txt$') {
            Copy-Item -Path $Sample -Destination $OutFile
            return
        }
        switch ($Scenario) {
            'fetch-error' { throw 'network failure' }
            'fetch-nonterminating' { Write-Error 'HTTP error' -ErrorAction $ErrorAction }
            'missing-file' { return }
            'missing-entry' { Set-Content -Path $OutFile -Value "$Hash  another.zip" }
            'empty-file' { Set-Content -Path $OutFile -Value '' }
            'mismatch' { Set-Content -Path $OutFile -Value "$('0' * 64)  $ArchiveName" }
            'match' { Set-Content -Path $OutFile -Value "$Hash  $ArchiveName" }
            default { throw "Unknown scenario: $Scenario" }
        }
    }
    function Expand-Archive {
        param($Path, $DestinationPath, [switch]$Force)
        Set-Content -Path (Join-Path $FixtureDir 'extracted') -Value 'called'
        throw 'EXTRACTION_REACHED'
    }
    try {
        & $Installer
    } catch {
        if ($_.Exception.Message -eq 'EXTRACTION_REACHED') { exit 0 }
        throw
    }
    throw 'Installer returned without reaching extraction'
}

$FixtureRoot = Join-Path ([System.IO.Path]::GetTempPath()) "mudcrab-checksum-tests-$PID"
$PowerShell = (Get-Process -Id $PID).Path
$Cases = @(
    @{ Name = 'fetch-error'; Message = 'network failure' },
    @{ Name = 'fetch-nonterminating'; Message = 'HTTP error' },
    @{ Name = 'missing-file'; Message = 'SHA256SUMS.txt was not downloaded' },
    @{ Name = 'missing-entry'; Message = 'No entry for' },
    @{ Name = 'empty-file'; Message = 'No entry for' },
    @{ Name = 'mismatch'; Message = 'Expected:' },
    @{ Name = 'match'; Message = 'Checksum verified' }
)
try {
    foreach ($Case in $Cases) {
        $CaseDir = Join-Path $FixtureRoot $Case.Name
        New-Item -ItemType Directory -Path $CaseDir -Force | Out-Null
        $ErrorActionPreference = 'Continue'
        $Output = & $PowerShell -NoProfile -File $PSCommandPath -Child -Scenario $Case.Name -FixtureDir $CaseDir 2>&1 | Out-String
        $ExitCode = $LASTEXITCODE
        $ErrorActionPreference = 'Stop'
        $Extracted = Test-Path (Join-Path $CaseDir 'extracted')
        $ShouldExtract = $Case.Name -eq 'match'
        if (($ExitCode -eq 0) -ne $ShouldExtract -or $Extracted -ne $ShouldExtract -or $Output -notlike "*$($Case.Message)*") {
            throw "$($Case.Name) failed: exit=$ExitCode extracted=$Extracted output=$Output"
        }
        Write-Host "PASS $($Case.Name)"
    }
} finally {
    Remove-Item -Path $FixtureRoot -Recurse -Force -ErrorAction SilentlyContinue
}
