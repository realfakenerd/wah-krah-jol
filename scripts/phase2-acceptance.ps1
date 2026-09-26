[CmdletBinding()]
param(
    [string]$Assets,
    [string]$Worldspace = "0x3c",
    [int]$RuralGridX = 0, [int]$RuralGridY = 0,
    [int]$DenseGridX = 0, [int]$DenseGridY = 0,
    [int]$WaterGridX = 0, [int]$WaterGridY = 0,
    [int]$SyntheticSeconds = 120,
    [int]$WorldSeconds = 300,
    [int]$StressSeconds = 600,
    [int]$StabilitySeconds = 1800,
    [int]$Repetitions = 3,
    [double]$MinimumFps = 60.0,
    [double]$MaximumP95Ms = 16.67,
    [double]$MaximumMemoryGrowthGiB = 0.5,
    [string]$Baseline = "acceptance-baseline.json",
    [string]$VisualReview,
    [string]$OutputRoot,
    [switch]$UpdateBaseline,
    [switch]$RequireVisualSignoff,
    [switch]$Quick,
    [switch]$SkipQualityGates,
    [switch]$SkipRobustness,
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$repository = Split-Path -Parent $PSScriptRoot
$engine = Join-Path $repository "target\release\engine.exe"
$testTemp = Join-Path $repository "target\test-temp"
$outputBase = if ($OutputRoot) {
    if ([IO.Path]::IsPathRooted($OutputRoot)) { $OutputRoot } else { Join-Path $repository $OutputRoot }
} else { Join-Path $repository "target\acceptance" }
New-Item -ItemType Directory -Force -Path $outputBase, $testTemp | Out-Null
$env:TEMP = $testTemp
$env:TMP = $testTemp
if ($Quick) {
    $SyntheticSeconds = 3; $WorldSeconds = 5; $StressSeconds = 5; $StabilitySeconds = 5
    $Repetitions = 1
}
if ($Repetitions -lt 1) { throw "Repetitions must be at least 1" }
$benchmarkPriority = [Diagnostics.ProcessPriorityClass]::AboveNormal
try {
    (Get-Process -Id $PID).PriorityClass = $benchmarkPriority
} catch {
    throw "Could not set the acceptance runner priority to $benchmarkPriority`: $($_.Exception.Message)"
}

function Get-SafeHardware {
    try {
        $cpu = (Get-CimInstance Win32_Processor | Select-Object -First 1 -ExpandProperty Name) -replace '[^a-zA-Z0-9]+', '-'
        $gpuInfo = Get-CimInstance Win32_VideoController | Select-Object -First 1
        $gpu = $gpuInfo.Name -replace '[^a-zA-Z0-9]+', '-'
        $driver = $gpuInfo.DriverVersion -replace '[^a-zA-Z0-9.]+', '-'
        return "$cpu-$gpu-driver-$driver".Trim('-')
    } catch { return "unknown-hardware" }
}

function Write-JsonFile($Value, [string]$Path, [int]$Depth = 10) {
    $json = if ($Value -is [array] -and $Value.Count -eq 0) {
        "[]"
    } elseif ($null -eq $Value) {
        "null"
    } else {
        $Value | ConvertTo-Json -Depth $Depth
    }
    Set-Content -LiteralPath $Path -Value $json -Encoding utf8
}

function Get-Median([double[]]$Values) {
    $ordered = @($Values | Sort-Object)
    if ($ordered.Count -eq 0) { return 0.0 }
    $middle = [int][Math]::Floor($ordered.Count / 2)
    if ($ordered.Count % 2) { return [double]$ordered[$middle] }
    return ([double]$ordered[$middle - 1] + [double]$ordered[$middle]) / 2.0
}

function Get-RegressionComparison([string]$Metric, [double]$Old, [double]$New) {
    $absoluteDelta = if ($Metric -eq "average_fps") { $Old - $New } else { $New - $Old }
    $regression = $absoluteDelta / $Old * 100.0
    $noiseFloor = switch ($Metric) {
        "average_fps" { 5.0 }
        { $_ -in @("frame_ms_p95", "frame_ms_p99") } { 1.5 }
        { $_ -in @("peak_process_memory_gib", "process_memory_growth_gib") } { 0.05 }
        default { 0.0 }
    }
    $materialRegression = $absoluteDelta -gt $noiseFloor
    $status = if ($materialRegression -and $regression -gt 10.0) {
        "fail"
    } elseif ($materialRegression -and $regression -gt 5.0) {
        "warn"
    } else {
        "pass"
    }
    return [ordered]@{
        regression_percent = $regression
        regression_absolute = $absoluteDelta
        noise_floor = $noiseFloor
        status = $status
    }
}

function Invoke-RecordedCommand {
    param([string]$Name, [string]$Executable, [string[]]$Arguments, [string]$LogDirectory)
    $log = Join-Path $LogDirectory "$Name.log"
    $started = Get-Date
    & $Executable @Arguments *> $log
    $exitCode = $LASTEXITCODE
    return [ordered]@{
        name = $Name; command = "$Executable $($Arguments -join ' ')"; exit_code = $exitCode
        elapsed_seconds = ((Get-Date) - $started).TotalSeconds; log = $log; passed = $exitCode -eq 0
    }
}

$hardware = Get-SafeHardware
$campaign = Join-Path $outputBase "$(Get-Date -Format 'yyyyMMdd-HHmmss')-$hardware"
$directories = [ordered]@{
    root = $campaign; logs = Join-Path $campaign "logs"; performance = Join-Path $campaign "performance"
    profiling = Join-Path $campaign "profiling"; screenshots = Join-Path $campaign "screenshots"
}
New-Item -ItemType Directory -Force -Path $directories.Values | Out-Null
$commit = (& git -C $repository rev-parse --short=12 HEAD 2>$null)
if (-not $commit) { $commit = "unknown" }
& git -C $repository diff --quiet --ignore-submodules HEAD 2>$null
$dirty = $LASTEXITCODE -ne 0
$metadata = [ordered]@{
    format_version = 1; generated_at = (Get-Date).ToString("o"); commit = $commit
    dirty_worktree = $dirty; build_profile = "release"; hardware = $hardware
    powershell = $PSVersionTable.PSVersion.ToString(); worldspace = $Worldspace
    process_priority = $benchmarkPriority.ToString()
    thresholds = [ordered]@{
        minimum_average_fps = $MinimumFps; maximum_p95_frame_ms = $MaximumP95Ms
        maximum_memory_growth_gib = $MaximumMemoryGrowthGiB
        regression_warning_percent = 5.0; regression_failure_percent = 10.0
        regression_noise_floors = [ordered]@{ fps = 5.0; frame_ms = 1.5; memory_gib = 0.05 }
    }
}
Write-JsonFile $metadata (Join-Path $campaign "metadata.json")

$preflight = [System.Collections.ArrayList]::new()
function Add-Preflight([string]$Name, [bool]$Passed, [string]$Message, [bool]$Required = $true) {
    [void]$preflight.Add([ordered]@{ name = $Name; passed = $Passed; required = $Required; message = $Message })
}
$driveName = [IO.Path]::GetPathRoot($repository).TrimEnd('\').TrimEnd(':')
$freeGiB = (Get-PSDrive -Name $driveName).Free / 1GB
Add-Preflight "operating-system" $IsWindows "Windows=$IsWindows"
Add-Preflight "powershell-version" ($PSVersionTable.PSVersion.Major -ge 7) $PSVersionTable.PSVersion.ToString()
Add-Preflight "cargo" ($null -ne (Get-Command cargo -ErrorAction SilentlyContinue)) "cargo must be on PATH"
Add-Preflight "git" ($null -ne (Get-Command git -ErrorAction SilentlyContinue)) "git must be on PATH"
Add-Preflight "disk-space" ($freeGiB -ge 10.0) ("{0:N1} GiB free" -f $freeGiB)
Add-Preflight "worktree" (-not $dirty) $(if ($dirty) { "dirty worktree recorded" } else { "clean" }) $false

$resolvedAssets = $null
if ($Assets) {
    try { $resolvedAssets = (Resolve-Path -LiteralPath $Assets).Path; Add-Preflight "assets-directory" $true $resolvedAssets }
    catch { Add-Preflight "assets-directory" $false $_.Exception.Message }
    if ($resolvedAssets) {
        $worldCoordinates = @(
            "$RuralGridX,$RuralGridY", "$DenseGridX,$DenseGridY", "$WaterGridX,$WaterGridY"
        )
        Add-Preflight "representative-world-coordinates" (@($worldCoordinates | Select-Object -Unique).Count -eq 3) ($worldCoordinates -join "; ")
        foreach ($required in @("conversion-manifest.json", "integration-report.json", "skyrim_world.db", "cell_cache.rkyv")) {
            $exists = Test-Path -LiteralPath (Join-Path $resolvedAssets $required) -PathType Leaf
            Add-Preflight "asset-$required" $exists $(if ($exists) { "present" } else { "missing" })
        }
        try {
            $manifest = Get-Content -LiteralPath (Join-Path $resolvedAssets "conversion-manifest.json") -Raw | ConvertFrom-Json
            Add-Preflight "converter-schema" ($manifest.schema_version -eq 14) "schema=$($manifest.schema_version), expected=14"
            Add-Preflight "conversion-complete" ([bool]$manifest.complete) "complete=$($manifest.complete)"
        } catch { Add-Preflight "conversion-manifest-valid" $false $_.Exception.Message }
        try {
            $integration = Get-Content -LiteralPath (Join-Path $resolvedAssets "integration-report.json") -Raw | ConvertFrom-Json
            Add-Preflight "database-schema" ($integration.schema_version -eq 4) "schema=$($integration.schema_version), expected=4"
            Add-Preflight "integration-report-passed" ([bool]$integration.passed) "passed=$($integration.passed)"
        } catch { Add-Preflight "integration-report-valid" $false $_.Exception.Message }
    }
} else { Add-Preflight "real-assets" $false "not supplied; only synthetic acceptance can run" $false }
Write-JsonFile $preflight (Join-Path $campaign "preflight.json")
$preflightFailed = @($preflight | Where-Object { $_.required -and -not $_.passed }).Count -gt 0

$quality = @(); $robustness = @(); $functional = @(); $performance = [ordered]@{}; $comparisons = @(); $releaseBuildExecuted = $false
$warnings = [System.Collections.ArrayList]::new(); $failures = [System.Collections.ArrayList]::new()
if ($preflightFailed) { [void]$failures.Add("required preflight checks failed") }
else {
    Push-Location $repository
    try {
        if (-not $SkipQualityGates) {
            $quality += Invoke-RecordedCommand "format" "cargo" @("fmt", "--all", "--", "--check") $directories.logs
            $quality += Invoke-RecordedCommand "tests" "cargo" @("test", "--workspace", "--all-targets", "-j1") $directories.logs
            $quality += Invoke-RecordedCommand "clippy" "cargo" @("clippy", "--workspace", "--all-targets", "-j1", "--", "-D", "warnings") $directories.logs
        } else { [void]$warnings.Add("quality gates were skipped") }
        if (-not $SkipBuild -or -not (Test-Path -LiteralPath $engine -PathType Leaf)) {
            $releaseBuildExecuted = $true
            $quality += Invoke-RecordedCommand "release-build" "cargo" @("build", "--workspace", "--release", "-j1") $directories.logs
        }
        if (-not (Test-Path -LiteralPath $engine -PathType Leaf)) { [void]$failures.Add("release engine executable was not produced") }

        if ($resolvedAssets) {
            $closureTool = Join-Path $repository "target\release\asset-closure.exe"
            $auditTool = Join-Path $repository "target\release\runtime-asset-audit.exe"
            $closureReport = Join-Path $campaign "asset-closure.json"
            $auditReport = Join-Path $campaign "runtime-asset-audit.json"
            $closureAvailable = Test-Path -LiteralPath $closureTool -PathType Leaf
            $auditAvailable = Test-Path -LiteralPath $auditTool -PathType Leaf
            Add-Preflight "asset-closure-tool" $closureAvailable $closureTool
            Add-Preflight "runtime-asset-audit-tool" $auditAvailable $auditTool
            if ($closureAvailable) {
                $closureGate = Invoke-RecordedCommand "asset-closure" $closureTool @(
                    $resolvedAssets, $closureReport,
                    (Join-Path $resolvedAssets "vfs\meshes"),
                    (Join-Path $resolvedAssets "vfs\textures")
                ) $directories.logs
                Add-Preflight "asset-closure" $closureGate.passed "exit=$($closureGate.exit_code); report=$closureReport"
            }
            if ($auditAvailable) {
                $assetAuditGate = Invoke-RecordedCommand "runtime-asset-audit" $auditTool @($resolvedAssets, $auditReport) $directories.logs
                Add-Preflight "runtime-glb-ktx2-audit" $assetAuditGate.passed "exit=$($assetAuditGate.exit_code); report=$auditReport"
            }
            $preflightFailed = @($preflight | Where-Object { $_.required -and -not $_.passed }).Count -gt 0
            Write-JsonFile $preflight (Join-Path $campaign "preflight.json")
            if ($preflightFailed) { [void]$failures.Add("asset closure or runtime asset audit failed") }
        }

        if (-not $SkipRobustness -and (Test-Path -LiteralPath $engine -PathType Leaf)) {
            foreach ($test in @(
                "app::tests::rejects_stale_or_incomplete_runtime_assets",
                "streaming::tests::validates_loaded_material_images_and_rejects_missing_required_texture",
                "streaming::tests::creates_upward_wound_quadrants_with_continuous_uvs",
                "streaming::tests::accepts_matching_neighbor_edges_and_rejects_cracks",
                "render::tests::reflects_camera_above_and_below_the_water_plane",
                "world::cache::tests::rejects_previous_cache_version",
                "world::cache::tests::rejects_truncated_cache",
                "world::database::tests::rejects_previous_database_schema",
                "world::database::tests::rejects_truncated_database",
                "world::database::tests::drop_drains_a_full_request_queue_and_joins_worker",
                "app::tests::rejects_truncated_manifest_and_integration_report",
                "streaming::tests::repeated_rebasing_preserves_camera_and_cell_root_locality",
                "streaming::tests::lifecycle_validator_detects_duplicate_and_orphaned_roots",
                "profiling::tests::writes_complete_profile_bundle"
            )) {
                $robustness += Invoke-RecordedCommand ($test -replace '[:]+', '-') "cargo" @("test", "-p", "engine", $test, "-j1") $directories.logs
            }
            $robustness += Invoke-RecordedCommand "runtime-asset-audit-negative" "cargo" @("test", "-p", "converter", "--bin", "runtime-asset-audit", "-j1") $directories.logs
            $missingAssets = Join-Path $campaign "deliberately-missing-assets"
            $negative = Invoke-RecordedCommand "runtime-rejects-missing-assets" $engine @("--assets", $missingAssets, "--benchmark-frames", "1") $directories.logs
            $negative.passed = $negative.exit_code -ne 0; $negative.expected_exit = "non-zero"; $robustness += $negative
        } elseif ($SkipRobustness) { [void]$warnings.Add("robustness gates were skipped") }

        $qualityFailed = @($quality | Where-Object { -not $_.passed }).Count -gt 0
        $robustnessFailed = @($robustness | Where-Object { -not $_.passed }).Count -gt 0
        if ($qualityFailed) { [void]$failures.Add("one or more quality gates failed") }
        if ($robustnessFailed) { [void]$failures.Add("one or more robustness gates failed") }
        if (-not $qualityFailed -and (Test-Path -LiteralPath $engine -PathType Leaf)) {
            $scenarios = @(
                [ordered]@{ name = "materials"; seconds = $SyntheticSeconds; arguments = @("--material-fixture") },
                [ordered]@{ name = "terrain-water"; seconds = $SyntheticSeconds; arguments = @("--terrain-water-fixture") },
                [ordered]@{ name = "transform-bounds"; seconds = $SyntheticSeconds; arguments = @("--transform-bounds-fixture") },
                [ordered]@{ name = "renderer"; seconds = $SyntheticSeconds; arguments = @("--renderer-fixture") },
                [ordered]@{ name = "streaming"; seconds = $SyntheticSeconds; arguments = @("--streaming-fixture", "--max-commit-ms", "16.67") },
                [ordered]@{ name = "synthetic"; seconds = $SyntheticSeconds; arguments = @("--benchmark-only", "--synthetic-instances", "250000") }
            )
            if ($resolvedAssets) {
                $worldBase = @("--assets", $resolvedAssets, "--worldspace", $Worldspace)
                $scenarios += [ordered]@{ name = "rural"; seconds = $WorldSeconds; arguments = $worldBase + @("--grid-x", "$RuralGridX", "--grid-y", "$RuralGridY") }
                $scenarios += [ordered]@{ name = "dense"; seconds = $WorldSeconds; arguments = $worldBase + @("--grid-x", "$DenseGridX", "--grid-y", "$DenseGridY") }
                $scenarios += [ordered]@{ name = "water"; seconds = $WorldSeconds; arguments = $worldBase + @("--grid-x", "$WaterGridX", "--grid-y", "$WaterGridY") }
                $scenarios += [ordered]@{ name = "stress"; seconds = $StressSeconds; arguments = $worldBase + @("--grid-x", "$RuralGridX", "--grid-y", "$RuralGridY", "--stream-radius", "3", "--auto-fly-speed", "5000") }
                $scenarios += [ordered]@{ name = "stability"; seconds = $StabilitySeconds; arguments = $worldBase + @("--grid-x", "$DenseGridX", "--grid-y", "$DenseGridY", "--auto-fly-speed", "900") }
            }
            foreach ($scenario in $scenarios) {
                $runs = @()
                for ($run = 1; $run -le $Repetitions; $run++) {
                    $runDirectory = Join-Path $directories.profiling "$($scenario.name)\run-$run"
                    New-Item -ItemType Directory -Force -Path $runDirectory | Out-Null
                    $screenshot = Join-Path $directories.screenshots "$($scenario.name).png"
                    $arguments = $scenario.arguments + @(
                        "--benchmark-duration", "$($scenario.seconds)", "--benchmark-output", (Join-Path $runDirectory "acceptance.json"),
                        "--profile-output", $runDirectory, "--profile-scenario", $scenario.name, "--profile-run-id", "run-$run",
                        "--profile-commit", $commit, "--profile-hardware", $hardware,
                        "--accept-min-fps", "$MinimumFps", "--accept-p95-ms", "$MaximumP95Ms",
                        "--accept-max-memory-growth-gib", "$MaximumMemoryGrowthGiB"
                    )
                    if ($dirty) { $arguments += "--profile-dirty-worktree" }
                    if ($run -eq 1) { $arguments += @("--acceptance-screenshot", $screenshot) }
                    $execution = Invoke-RecordedCommand "$($scenario.name)-run-$run" $engine $arguments $directories.logs
                    $runtimeLogLines = if (Test-Path -LiteralPath $execution.log) { @(Get-Content -LiteralPath $execution.log) } else { @() }
                    $relevantLogLines = @($runtimeLogLines | Where-Object {
                        $_ -match "(?i)(thread .*panicked at|panic at|ERROR\s+(engine|bevy_app)|diagnostic fallback|failed strict validation|asset[^\r\n]*failed)" -and
                        $_ -notmatch "(?i)(ReShade64\.dll|wgpu_hal::vulkan::instance)"
                    })
                    $runtimeLogClean = $relevantLogLines.Count -eq 0
                    if (-not $runtimeLogClean) {
                        [void]$failures.Add("relevant runtime error was logged by $($scenario.name) run $run")
                    }
                    $reportPath = Join-Path $runDirectory "acceptance.json"
                    $requiredBundleFiles = @(
                        "metadata.json", "frame-metrics.json", "cpu-spans.json", "gpu-passes.json",
                        "streaming.json", "renderer.json", "memory.json", "summary.md"
                    )
                    $missingBundleFiles = @($requiredBundleFiles | Where-Object {
                        -not (Test-Path -LiteralPath (Join-Path $runDirectory $_) -PathType Leaf)
                    })
                    $bundleComplete = $missingBundleFiles.Count -eq 0
                    if (-not $bundleComplete) {
                        [void]$failures.Add("profiling bundle is incomplete for $($scenario.name) run ${run}: $($missingBundleFiles -join ', ')")
                    }
                    if (Test-Path -LiteralPath $reportPath) {
                        $report = Get-Content -LiteralPath $reportPath -Raw | ConvertFrom-Json; $runs += $report
                        $functional += [ordered]@{
                            scenario = $scenario.name; run = $run; engine_exit_code = $execution.exit_code; report_present = $true
                            streaming_failures = if ($report.streaming) { $report.streaming.failed_cells } else { 0 }
                            asset_failures = if ($report.streaming) { $report.streaming.asset_load_failures } else { 0 }
                            material_validation_failures = if ($report.streaming) { $report.streaming.material_validation_failures } else { 0 }
                            diagnostic_fallbacks = if ($report.streaming) { $report.streaming.diagnostic_fallbacks } else { 0 }
                            pending_assets = if ($report.streaming) { $report.streaming.pending_asset_instances } else { 0 }
                            pending_surfaces = if ($report.streaming) { $report.streaming.pending_surface_instances } else { 0 }
                            terrain_failures = if ($report.streaming) { $report.streaming.terrain_validation_failures } else { 0 }
                            water_failures = if ($report.streaming) { $report.streaming.water_validation_failures } else { 0 }
                            lifecycle_failures = if ($report.streaming) { $report.streaming.streaming_invariant_failures } else { 0 }
                            duplicate_roots = if ($report.streaming) { $report.streaming.duplicate_cell_roots } else { 0 }
                            orphaned_roots = if ($report.streaming) { $report.streaming.orphaned_cell_roots } else { 0 }
                            missing_roots = if ($report.streaming) { $report.streaming.missing_cell_roots } else { 0 }
                            out_of_range_roots = if ($report.streaming) { $report.streaming.out_of_range_cell_roots } else { 0 }
                            commit_budget_violations = if ($report.streaming) { $report.streaming.commit_budget_violations } else { 0 }
                            streaming_fixture_validated = if ($scenario.name -eq "streaming") { [bool]$report.streaming.streaming_fixture_validated } else { $null }
                            runtime_log_clean = $runtimeLogClean; relevant_log_lines = $relevantLogLines
                            bundle_complete = $bundleComplete; missing_bundle_files = $missingBundleFiles
                            passed = [bool]$report.passed -and $execution.exit_code -eq 0 -and $runtimeLogClean -and $bundleComplete
                        }
                    } else {
                        $functional += [ordered]@{ scenario = $scenario.name; run = $run; engine_exit_code = $execution.exit_code; report_present = $false; streaming_failures = $null; runtime_log_clean = $runtimeLogClean; bundle_complete = $bundleComplete; passed = $false }
                    }
                }
                if ($runs.Count -gt 0) {
                    $performance[$scenario.name] = [ordered]@{
                        average_fps = Get-Median @($runs.average_fps); frame_ms_p95 = Get-Median @($runs.frame_ms_p95)
                        frame_ms_p99 = Get-Median @($runs.frame_ms_p99); peak_process_memory_gib = Get-Median @($runs.peak_process_memory_gib)
                        process_memory_growth_gib = Get-Median @($runs.process_memory_growth_gib)
                    }
                }
            }
        }
    } finally { Pop-Location }
}

Write-JsonFile $quality (Join-Path $campaign "quality-gates.json")
Write-JsonFile $robustness (Join-Path $campaign "robustness.json")
Write-JsonFile $functional (Join-Path $campaign "functional-results.json")
Write-JsonFile $performance (Join-Path $directories.performance "medians.json")
$expectedScenarios = @("materials", "terrain-water", "transform-bounds", "renderer", "streaming", "synthetic")
if ($resolvedAssets) { $expectedScenarios += @("rural", "dense", "water", "stress", "stability") }
foreach ($scenario in $expectedScenarios) {
    if (-not $performance.Contains($scenario)) {
        [void]$failures.Add("required scenario was omitted: $scenario")
    }
    $actualRuns = @($functional | Where-Object { $_.scenario -eq $scenario }).Count
    if ($actualRuns -ne $Repetitions) {
        [void]$failures.Add("scenario $scenario produced $actualRuns/$Repetitions required runs")
    }
}
$screenshotEvidence = @()
foreach ($scenario in $performance.Keys) {
    $path = Join-Path $directories.screenshots "$scenario.png"
    $present = Test-Path -LiteralPath $path -PathType Leaf
    $screenshotEvidence += [ordered]@{
        scenario = $scenario
        path = $path
        present = $present
        bytes = if ($present) { (Get-Item -LiteralPath $path).Length } else { 0 }
        sha256 = if ($present) { (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash } else { $null }
    }
    if (-not $present) { [void]$failures.Add("screenshot was not captured for scenario $scenario") }
}
Write-JsonFile $screenshotEvidence (Join-Path $campaign "screenshots.json")
$baselinePath = if ([IO.Path]::IsPathRooted($Baseline)) { $Baseline } else { Join-Path $repository $Baseline }
$baselineAvailable = Test-Path -LiteralPath $baselinePath -PathType Leaf
$baselineCompatible = $false
$baselineData = @{}
if ($baselineAvailable) {
    try {
        $baselineEnvelope = Get-Content -LiteralPath $baselinePath -Raw | ConvertFrom-Json -AsHashtable
        if ($baselineEnvelope.ContainsKey("scenarios")) {
            $baselineData = $baselineEnvelope.scenarios
            $baselineCompatible = $baselineEnvelope.hardware -eq $hardware
            if (-not $baselineCompatible) {
                [void]$failures.Add("acceptance baseline hardware does not match this campaign")
            }
        } else {
            $baselineData = $baselineEnvelope
            [void]$warnings.Add("legacy baseline has no hardware identity and cannot prove same-hardware reproducibility")
        }
    } catch {
        [void]$failures.Add("acceptance baseline could not be read: $($_.Exception.Message)")
    }
} else {
    [void]$warnings.Add("no acceptance baseline was available")
}
foreach ($scenario in $performance.Keys) {
    if (-not $baselineData.ContainsKey($scenario)) {
        [void]$warnings.Add("no baseline was available for scenario $scenario")
        continue
    }
    foreach ($metric in @("average_fps", "frame_ms_p95", "frame_ms_p99", "peak_process_memory_gib", "process_memory_growth_gib")) {
        $old = [double]$baselineData[$scenario][$metric]; $new = [double]$performance[$scenario][$metric]
        if ($old -le 0) { continue }
        $comparison = Get-RegressionComparison $metric $old $new
        $comparisons += [ordered]@{
            scenario = $scenario; metric = $metric; baseline = $old; current = $new
            regression_percent = $comparison.regression_percent
            regression_absolute = $comparison.regression_absolute
            noise_floor = $comparison.noise_floor
            status = $comparison.status
        }
        if ($comparison.status -eq "fail") { [void]$failures.Add("$scenario/$metric regressed by $([Math]::Round($comparison.regression_percent, 2))%") }
        if ($comparison.status -eq "warn") { [void]$warnings.Add("$scenario/$metric regressed by $([Math]::Round($comparison.regression_percent, 2))%") }
    }
}
Write-JsonFile $comparisons (Join-Path $campaign "comparison.json")

$visualTemplate = [ordered]@{
    format_version = 2; reviewer = ""; reviewed_at = ""; signature = ""
    checkpoints = @(
        [ordered]@{ scenario = "materials"; status = "pending"; notes = "opaque, cutout, blend, emissive, double-sided and normal-map fixture" },
        [ordered]@{ scenario = "terrain-water"; status = "pending"; notes = "four coherent quadrants, six layers, seams, flow normal and non-recursive reflection" },
        [ordered]@{ scenario = "transform-bounds"; status = "pending"; notes = "assembled hierarchy, rotations, non-uniform scales and eight-corner aggregate bounds" },
        [ordered]@{ scenario = "renderer"; status = "pending"; notes = "front/occluded objects, camera turns, non-uniform scales, HZB and indirect rendering" },
        [ordered]@{ scenario = "streaming"; status = "pending"; notes = "rapid traversal, teleport, stale discard, unload and repeated rebasing" },
        [ordered]@{ scenario = "synthetic"; status = "pending"; notes = "250k-instance indirect rendering and stable visibility" },
        [ordered]@{ scenario = "rural"; status = "pending"; notes = "terrain seams, object visibility, terrain layers" },
        [ordered]@{ scenario = "dense"; status = "pending"; notes = "dense references, materials, shadows" },
        [ordered]@{ scenario = "water"; status = "pending"; notes = "reflection stability, flow normal, no recursion" },
        [ordered]@{ scenario = "stress"; status = "pending"; notes = "no duplicate/orphaned cells or severe popping" },
        [ordered]@{ scenario = "stability"; status = "pending"; notes = "stable memory and lifecycle during the full-duration traversal" }
    )
}
Write-JsonFile $visualTemplate (Join-Path $campaign "visual-review-template.json")
$visualResult = [ordered]@{ supplied = $false; passed = $false; reviewer = $null; reviewed_at = $null; signature = $null; checkpoints = @() }
if ($VisualReview) {
    try {
        $reviewPath = (Resolve-Path -LiteralPath $VisualReview).Path; $review = Get-Content -LiteralPath $reviewPath -Raw | ConvertFrom-Json
        $visualResult.supplied = $true; $visualResult.reviewer = $review.reviewer; $visualResult.reviewed_at = $review.reviewed_at; $visualResult.signature = $review.signature; $visualResult.checkpoints = $review.checkpoints
        $requiredVisualScenarios = $expectedScenarios
        $reviewedScenarios = @($review.checkpoints | Where-Object { $_.status -eq "pass" } | ForEach-Object { $_.scenario })
        $missingVisualScenarios = @($requiredVisualScenarios | Where-Object { $_ -notin $reviewedScenarios })
        $visualResult.passed = [bool]$review.reviewer -and [bool]$review.reviewed_at -and [bool]$review.signature -and $missingVisualScenarios.Count -eq 0
        Copy-Item -LiteralPath $reviewPath -Destination (Join-Path $campaign "visual-review.json") -Force
    } catch { [void]$failures.Add("visual review could not be read: $($_.Exception.Message)") }
}
if ($RequireVisualSignoff -and -not $visualResult.passed) { [void]$failures.Add("required visual sign-off is missing or incomplete") }
elseif (-not $visualResult.passed) { [void]$warnings.Add("visual sign-off is pending") }
if (-not $resolvedAssets) { [void]$warnings.Add("real-world scenarios were not executed") }
if ($dirty) { [void]$warnings.Add("campaign ran from a dirty worktree") }
if (-not $releaseBuildExecuted) { [void]$warnings.Add("release build gate was skipped") }
if (-not $Quick -and $Repetitions -lt 3) { [void]$warnings.Add("final acceptance requires at least three repetitions") }
if (-not $Quick -and ($SyntheticSeconds -lt 120 -or $WorldSeconds -lt 300 -or $StressSeconds -lt 600 -or $StabilitySeconds -lt 1800)) {
    [void]$warnings.Add("final acceptance durations are below the required stability campaign")
}
if (@($functional | Where-Object { -not $_.passed }).Count -gt 0) { [void]$failures.Add("one or more functional/performance runs failed") }
$failures = @($failures | Select-Object -Unique); $warnings = @($warnings | Select-Object -Unique)
$verdict = if ($failures.Count -gt 0) { "rejected" } elseif ($warnings.Count -gt 0) { "accepted-with-warnings" } else { "accepted" }
$functionalPassed = $functional.Count -gt 0 -and @($functional | Where-Object { -not $_.passed }).Count -eq 0
$qualityPassed = -not $SkipQualityGates -and $quality.Count -gt 0 -and @($quality | Where-Object { -not $_.passed }).Count -eq 0
$robustnessPassed = -not $SkipRobustness -and $robustness.Count -gt 0 -and @($robustness | Where-Object { -not $_.passed }).Count -eq 0
$report = [ordered]@{
    format_version = 2; verdict = $verdict; release_ready = $verdict -eq "accepted"; campaign = $campaign; metadata = $metadata; preflight_passed = -not $preflightFailed
    quality_passed = $qualityPassed
    robustness_passed = $robustnessPassed
    release_build_passed = $releaseBuildExecuted -and @($quality | Where-Object { $_.name -eq "release-build" -and -not $_.passed }).Count -eq 0
    functional_passed = $functionalPassed
    baseline = [ordered]@{ path = $baselinePath; available = $baselineAvailable; same_hardware = $baselineCompatible }
    expected_scenarios = $expectedScenarios; visual = $visualResult; scenarios = $performance; failures = $failures; warnings = $warnings
}
Write-JsonFile $report (Join-Path $campaign "acceptance-report.json")
$summary = @(
    "# Phase 2 acceptance — $verdict", "", "- Commit: $commit", "- Hardware: $hardware", "- Dirty worktree: $dirty",
    "- Repetitions: $Repetitions", "- Real assets: $([bool]$resolvedAssets)", "- Visual sign-off: $($visualResult.passed)", "",
    "| Scenario | Median FPS | P95 ms | P99 ms | Peak GiB | Growth GiB |", "|---|---:|---:|---:|---:|---:|"
)
foreach ($scenario in $performance.Keys) {
    $value = $performance[$scenario]
    $summary += "| $scenario | $([Math]::Round($value.average_fps, 2)) | $([Math]::Round($value.frame_ms_p95, 3)) | $([Math]::Round($value.frame_ms_p99, 3)) | $([Math]::Round($value.peak_process_memory_gib, 3)) | $([Math]::Round($value.process_memory_growth_gib, 3)) |"
}
$summary += @("", "## Failures", "")
$summary += if ($failures.Count) { @($failures | ForEach-Object { "- $_" }) } else { "- None" }
$summary += @("", "## Warnings", "")
$summary += if ($warnings.Count) { @($warnings | ForEach-Object { "- $_" }) } else { "- None" }
$summary | Set-Content -LiteralPath (Join-Path $campaign "acceptance-summary.md") -Encoding utf8
if ($UpdateBaseline) {
    if ($verdict -eq "rejected") { throw "Cannot update baseline from a rejected campaign: $campaign" }
    if (-not $resolvedAssets -or $Repetitions -lt 3 -or -not $visualResult.passed -or $dirty -or -not $qualityPassed -or -not $robustnessPassed -or -not $releaseBuildExecuted) {
        throw "Baseline requires real assets, three repetitions, all quality/robustness/release gates, signed visual review and a clean worktree"
    }
    $newBaseline = [ordered]@{
        format_version = 2; generated_at = (Get-Date).ToString("o"); hardware = $hardware
        commit = $commit; scenarios = $performance
    }
    Write-JsonFile $newBaseline $baselinePath
}
Write-Host "Phase 2 acceptance verdict: $verdict"
Write-Host "Evidence: $campaign"
if ($verdict -eq "rejected" -or (-not $Quick -and $verdict -ne "accepted")) { exit 1 }
