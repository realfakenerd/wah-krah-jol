[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Assets,
    [string]$Worldspace = "0x3c",
    [int]$GridX = 0,
    [int]$GridY = 0,
    [ValidateRange(0, 16)]
    [int]$StreamRadius = 2,
    [ValidateRange(1, 100000)]
    [int]$Frames = 240,
    [ValidateRange(0, 100000)]
    [int]$WarmupFrames = 60,
    [string]$OutputDirectory,
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$repository = Split-Path -Parent $PSScriptRoot
$engine = Join-Path $repository "target\release\engine.exe"
$inspector = Join-Path $repository "target\release\world-inspect.exe"
$resolvedAssets = (Resolve-Path -LiteralPath $Assets).Path
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
if (-not $OutputDirectory) {
    $OutputDirectory = Join-Path $repository "target\visual-baselines\$stamp"
}
$output = [IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Force -Path $output | Out-Null

Push-Location $repository
try {
    $required = @(
        "conversion-manifest.json",
        "integration-report.json",
        "skyrim_world.db",
        "cell_cache.rkyv"
    )
    foreach ($name in $required) {
        $path = Join-Path $resolvedAssets $name
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            throw "Strict visual baseline requires $name in $resolvedAssets"
        }
    }
    $manifest = Get-Content -LiteralPath (Join-Path $resolvedAssets "conversion-manifest.json") -Raw | ConvertFrom-Json
    if ($manifest.complete -ne $true) { throw "conversion-manifest.json is not complete" }
    if ($manifest.schema_version -ne 14) { throw "conversion-manifest.json schema must be 14" }
    $integration = Get-Content -LiteralPath (Join-Path $resolvedAssets "integration-report.json") -Raw | ConvertFrom-Json
    if ($integration.passed -ne $true) { throw "integration-report.json did not pass" }
    if ($integration.schema_version -ne 4) { throw "integration-report.json schema must be 4" }

    if (-not $SkipBuild -or -not (Test-Path -LiteralPath $engine -PathType Leaf) -or -not (Test-Path -LiteralPath $inspector -PathType Leaf)) {
        & cargo build --release -p engine --bins -j1
        if ($LASTEXITCODE -ne 0) { throw "Release build failed with exit code $LASTEXITCODE" }
    }

    $commit = (& git rev-parse HEAD 2>$null)
    if (-not $commit) { $commit = "unknown" }
    & git diff --quiet --ignore-submodules HEAD 2>$null
    $dirty = $LASTEXITCODE -ne 0
    $gpu = Get-CimInstance Win32_VideoController | Select-Object -First 1
    $cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
    $os = Get-CimInstance Win32_OperatingSystem
    $contracts = @{}
    foreach ($name in $required) {
        $path = Join-Path $resolvedAssets $name
        $contracts[$name] = [ordered]@{
            path = $path
            length = (Get-Item -LiteralPath $path).Length
            sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
        }
    }
    $metadata = [ordered]@{
        format_version = 1
        generated_at = (Get-Date).ToString("o")
        commit = $commit
        dirty_worktree = $dirty
        assets_root = $resolvedAssets
        worldspace = $Worldspace
        grid = @($GridX, $GridY)
        stream_radius = $StreamRadius
        resolution = @(1600, 900)
        camera = [ordered]@{
            target_local = @(2048.0, "terrain-center-height", -2048.0)
            position_offset = @(0.0, 1200.0, 2500.0)
            up = @(0.0, 1.0, 0.0)
        }
        cpu = $cpu.Name
        gpu = $gpu.Name
        driver = $gpu.DriverVersion
        os = $os.Caption
        contracts = $contracts
    }
    $metadata | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $output "baseline-metadata.json") -Encoding utf8

    $inspection = Join-Path $output "world-inspection.json"
    & $inspector $resolvedAssets $Worldspace $GridX $GridY --radius $StreamRadius --output $inspection
    if ($LASTEXITCODE -ne 0) { throw "World inspection failed with exit code $LASTEXITCODE" }

    $benchmark = Join-Path $output "benchmark-report.json"
    $profile = Join-Path $output "profile"
    $screenshot = Join-Path $output "scene.png"
    $stdout = Join-Path $output "engine.stdout.log"
    $stderr = Join-Path $output "engine.stderr.log"
    $arguments = @(
        "--assets", $resolvedAssets,
        "--worldspace", $Worldspace,
        "--grid-x", "$GridX",
        "--grid-y", "$GridY",
        "--stream-radius", "$StreamRadius",
        "--benchmark-frames", "$Frames",
        "--benchmark-warmup-frames", "$WarmupFrames",
        "--benchmark-output", $benchmark,
        "--profile-output", $profile,
        "--profile-scenario", "visual-baseline",
        "--profile-run-id", $stamp,
        "--profile-commit", $commit,
        "--profile-hardware", "$($cpu.Name) / $($gpu.Name) / $($gpu.DriverVersion)",
        "--acceptance-screenshot", $screenshot
    )
    if ($dirty) { $arguments += "--profile-dirty-worktree" }
    [ordered]@{ executable = $engine; arguments = $arguments } |
        ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $output "engine-command.json") -Encoding utf8
    & $engine @arguments 1> $stdout 2> $stderr
    if ($LASTEXITCODE -ne 0) { throw "Strict visual baseline failed with exit code $LASTEXITCODE; see $stderr" }
    if (-not (Test-Path -LiteralPath $screenshot -PathType Leaf)) { throw "Visual baseline did not produce $screenshot" }

    Write-Host "Visual baseline complete: $output"
}
finally {
    Pop-Location
}
