[CmdletBinding()]
param(
    [string]$Distro = "Ubuntu",
    [string]$LlamaServer = "/usr/local/bin/llama-server",
    [Parameter(Mandatory)][string]$ModelRoot,
    [Parameter(Mandatory)][string]$ModelKey,
    [string]$SecondModelKey,
    [string]$ExePath = "target/release/model-launcher.exe",
    [uri]$BaseUrl = "http://127.0.0.1:1234",
    [string]$Token = $env:MODEL_LAUNCHER_SMOKE_TOKEN,
    [switch]$SkipBuild,
    [switch]$ManualResourceChecks,
    [ValidateRange(30, 3600)][int]$TimeoutSeconds = 300
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$script:OwnedLauncherPid = $null
$script:Evidence = [ordered]@{
    schema = 1; started_utc = [DateTime]::UtcNow.ToString("o"); host = $env:COMPUTERNAME
    distro = $Distro; base_url = $BaseUrl.AbsoluteUri; checks = @(); manual = @()
}
$ArtifactDir = Join-Path (Resolve-Path ".") "artifacts/windows-wsl"
$EvidenceJson = Join-Path $ArtifactDir "evidence.json"
$EvidenceMarkdown = Join-Path $ArtifactDir "evidence.md"

function Add-Check([string]$Name, [string]$Status, [string]$Detail) {
    $script:Evidence.checks += [ordered]@{ name = $Name; status = $Status; detail = $Detail; utc = [DateTime]::UtcNow.ToString("o") }
    Write-Host "[$Status] $Name - $Detail"
}

function Save-Evidence {
    New-Item -ItemType Directory -Force $ArtifactDir | Out-Null
    $script:Evidence.finished_utc = [DateTime]::UtcNow.ToString("o")
    [IO.File]::WriteAllText($EvidenceJson, ($script:Evidence | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))
    $lines = @("# Windows/WSL acceptance evidence", "", "Generated: $($script:Evidence.finished_utc)", "", "| Check | Status | Detail |", "|---|---|---|")
    foreach ($item in $script:Evidence.checks) { $lines += "| $($item.name) | $($item.status) | $($item.detail -replace '\|','/') |" }
    if ($script:Evidence.manual.Count -gt 0) {
        $lines += @("", "## Manual observations", "")
        foreach ($item in $script:Evidence.manual) { $lines += "- **$($item.name)**: $($item.value)" }
    }
    [IO.File]::WriteAllLines($EvidenceMarkdown, $lines, [Text.UTF8Encoding]::new($false))
}

function Wait-Until([scriptblock]$Condition, [string]$Description) {
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        try { if (& $Condition) { return } } catch { }
        Start-Sleep -Milliseconds 500
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for $Description"
}

function Invoke-Api([string]$Method, [string]$Path, $Body = $null) {
    $headers = @{}
    if ($Token) { $headers.Authorization = "Bearer $Token" }
    $parameters = @{ Method = $Method; Uri = [uri]::new($BaseUrl, $Path); Headers = $headers; TimeoutSec = $TimeoutSeconds }
    if ($null -ne $Body) { $parameters.ContentType = "application/json"; $parameters.Body = ($Body | ConvertTo-Json -Depth 8 -Compress) }
    Invoke-RestMethod @parameters
}

function Stop-OwnedLauncher {
    if ($null -ne $script:OwnedLauncherPid) {
        $owned = Get-Process -Id $script:OwnedLauncherPid -ErrorAction SilentlyContinue
        if ($owned) {
            Stop-Process -Id $script:OwnedLauncherPid
            Wait-Process -Id $script:OwnedLauncherPid -Timeout 15 -ErrorAction SilentlyContinue
        }
        $script:OwnedLauncherPid = $null
    }
}

try {
    if ($env:OS -ne "Windows_NT") { throw "This harness requires Windows" }
    if (-not (Test-Path -LiteralPath $ModelRoot -PathType Container)) { throw "ModelRoot is not a directory: $ModelRoot" }
    if (-not (Get-ChildItem -LiteralPath $ModelRoot -Filter *.gguf -File -Recurse | Select-Object -First 1)) { throw "ModelRoot contains no GGUF files" }
    $distroList = (& wsl.exe --list --quiet) -join "`n"
    if ($distroList -notmatch [regex]::Escape($Distro)) { throw "WSL distribution is not installed: $Distro" }
    & wsl.exe -d $Distro -- test -x $LlamaServer
    if ($LASTEXITCODE -ne 0) { throw "llama-server is missing or not executable: $LlamaServer" }
    & wsl.exe -d $Distro -- $LlamaServer --help | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "llama-server --help probe failed" }
    Add-Check "preflight" "PASS" "Windows, WSL, executable, and GGUF inputs found"

    if (-not $SkipBuild) { cargo build -p model-launcher --release; if ($LASTEXITCODE -ne 0) { throw "release build failed" } }
    $resolvedExe = (Resolve-Path -LiteralPath $ExePath).Path
    Add-Check "release executable" "PASS" $resolvedExe

    # Use an isolated data directory, so the harness never overwrites the user's launcher settings.
    $UserConfig = Join-Path $env:LOCALAPPDATA "ModelLauncher/config/config.json"
    $HarnessLocalAppData = Join-Path $ArtifactDir "local-app-data"
    $env:LOCALAPPDATA = $HarnessLocalAppData
    $configDir = Join-Path $HarnessLocalAppData "ModelLauncher/config"
    New-Item -ItemType Directory -Force $configDir | Out-Null
    if (Test-Path -LiteralPath $UserConfig) {
        $settings = Get-Content -Raw -LiteralPath $UserConfig | ConvertFrom-Json
        Add-Check "isolated settings" "PASS" "Copied existing token hashes; original config remains untouched"
    } else {
        if ($Token) { throw "Token was supplied but no existing config holds its Argon2 hash; generate a token once in the launcher first" }
        $settings = [pscustomobject][ordered]@{
            version = 1
            config = [pscustomobject][ordered]@{
            models = @(); auth_token_hashes = @(); engine_distribution = $Distro
            engine_executable = $LlamaServer; catalog_directory = (Resolve-Path -LiteralPath $ModelRoot).Path
            default_launch_settings = [ordered]@{
                context_length = $null; gpu_layers = $null; cpu_threads = $null; batch_size = $null
                parallel_slots = $null; flash_attention = $null; kv_cache_type = $null
            }
            }
        }
    }
    $settings.config.engine_distribution = $Distro
    $settings.config.engine_executable = $LlamaServer
    $settings.config.catalog_directory = (Resolve-Path -LiteralPath $ModelRoot).Path
    [IO.File]::WriteAllText((Join-Path $configDir "config.json"), ($settings | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))
    $env:MODEL_LAUNCHER_WSL_DISTRO = $Distro
    $env:MODEL_LAUNCHER_LLAMA_SERVER = $LlamaServer
    $launcher = Start-Process -FilePath $resolvedExe -PassThru
    $script:OwnedLauncherPid = $launcher.Id
    Add-Check "owned launch" "PASS" "PID $($launcher.Id) recorded for PID-only cleanup"

    if (-not $Token) {
        Write-Host "MANUAL: In Settings, generate a token once, copy it, then return here. The plaintext is never written to evidence."
        $Token = Read-Host "Bearer token"
    }
    if (-not $Token) { throw "A token generated by this isolated launcher instance is required" }
    Wait-Until { (Invoke-Api GET "/api/v1/models").models.Count -gt 0 } "authenticated catalog discovery"
    $catalog = Invoke-Api GET "/api/v1/models"
    if ($ModelKey -notin @($catalog.models | ForEach-Object { $_.key })) { throw "ModelKey '$ModelKey' was not discovered" }
    Add-Check "configure/probe/discover" "PASS" "Discovered $($catalog.models.Count) model(s), including $ModelKey"

    $loaded = Invoke-Api POST "/api/v1/models/load" @{ model = $ModelKey; echo_load_config = $true }
    if ($loaded.status -ne "loaded") { throw "load did not return loaded" }
    Add-Check "load" "PASS" "instance $($loaded.model_instance_id)"

    $models = Invoke-Api GET "/v1/models"
    if ($ModelKey -notin @($models.data | ForEach-Object { $_.id })) { throw "OpenAI model list omitted ModelKey" }
    $chat = Invoke-Api POST "/v1/chat/completions" @{ model = $ModelKey; stream = $false; messages = @(@{ role = "user"; content = "Reply with OK" }) }
    if (-not $chat.choices) { throw "non-streaming chat returned no choices" }
    Add-Check "chat non-streaming" "PASS" "response contained choices"

    $headers = @{ Accept = "text/event-stream"; Authorization = "Bearer $Token" }
    $sseBody = @{ model = $ModelKey; stream = $true; messages = @(@{ role = "user"; content = "Reply with OK" }) } | ConvertTo-Json -Depth 6 -Compress
    $sse = Invoke-WebRequest -Method Post -Uri ([uri]::new($BaseUrl, "/v1/chat/completions")) -Headers $headers -ContentType "application/json" -Body $sseBody -TimeoutSec $TimeoutSeconds
    if ($sse.Content -notmatch "data:") { throw "streaming chat returned no SSE data frames" }
    Add-Check "chat streaming" "PASS" "SSE data frame observed"

    Invoke-Api POST "/api/v1/models/unload" @{ instance_id = $loaded.model_instance_id } | Out-Null
    Add-Check "unload" "PASS" "primary instance ejected"
    $jit = Invoke-Api POST "/v1/completions" @{ model = $ModelKey; stream = $false; prompt = "Reply with OK" }
    if (-not $jit.choices) { throw "JIT completion returned no choices" }
    Add-Check "JIT load" "PASS" "completion triggered load"

    if ($SecondModelKey) {
        $script:Evidence.manual += [ordered]@{ name = "model_busy"; value = "Run a long primary generation while requesting '$SecondModelKey'; record the model_busy response code manually." }
        Add-Check "model_busy" "MANUAL" "Second-model concurrency timing requires an operator-controlled long generation"
    } else { Add-Check "model_busy" "NOT_RUN" "SecondModelKey was not supplied" }

    if ($ManualResourceChecks) {
        $before = Get-Process -Id $script:OwnedLauncherPid
        Write-Host "MANUAL SECTION: perform 50 tray Open/Close cycles. Never exit the launcher."
        $cycles = Read-Host "Number of successful Open/Close cycles"
        $after = Get-Process -Id $script:OwnedLauncherPid
        $growthMiB = [math]::Round(($after.WorkingSet64 - $before.WorkingSet64) / 1MB, 2)
        $script:Evidence.manual += [ordered]@{ name = "window_cycles"; value = "$cycles/50; working-set delta ${growthMiB} MiB (tolerance: <= 32 MiB after settling)" }
        Add-Check "window/resource loops" ($(if ([int]$cycles -eq 50 -and $growthMiB -le 32) { "PASS" } else { "FAIL" })) "50 cycles requested; working-set delta ${growthMiB} MiB"

        $cpu0 = (Get-Process -Id $script:OwnedLauncherPid).CPU
        Start-Sleep -Seconds 30
        $cpu1 = (Get-Process -Id $script:OwnedLauncherPid).CPU
        $cpuPercent = [math]::Round((($cpu1 - $cpu0) / 30) * 100, 2)
        $script:Evidence.manual += [ordered]@{ name = "idle_cpu"; value = "$cpuPercent% of one logical CPU over 30s (tolerance: <= 1%)" }
        Add-Check "idle CPU" ($(if ($cpuPercent -le 1) { "PASS" } else { "FAIL" })) "$cpuPercent% of one logical CPU"

        Write-Host "MANUAL SECTION: kill only the llama-server PID printed in launcher logs; observe capped backoff, then Eject during backoff."
        $backoff = Read-Host "Observed restart backoff and no restart after Eject? (yes/no + notes)"
        $bounds = Read-Host "After overflow fixtures, logs <= 2000 entries/2 MiB and catalog <= 1024 models? (yes/no + notes)"
        $script:Evidence.manual += [ordered]@{ name = "crash_backoff_eject"; value = $backoff }
        $script:Evidence.manual += [ordered]@{ name = "log_catalog_bounds"; value = $bounds }
        Add-Check "crash/backoff/eject" "MANUAL" $backoff
        Add-Check "log/catalog bounds" "MANUAL" $bounds
    } else {
        Add-Check "resource lifecycle" "NOT_RUN" "Re-run with -ManualResourceChecks on interactive Windows hardware"
    }

    Stop-OwnedLauncher
    $restarted = Start-Process -FilePath $resolvedExe -PassThru
    $script:OwnedLauncherPid = $restarted.Id
    Start-Sleep -Seconds 3
    $restartCatalog = Invoke-Api GET "/api/v1/models"
    $ownedWslChildren = @(Get-CimInstance Win32_Process | Where-Object { $_.ParentProcessId -eq $script:OwnedLauncherPid -and $_.Name -eq "wsl.exe" })
    if ($ownedWslChildren.Count -ne 0) { throw "restart unexpectedly spawned a WSL backend before any load request" }
    Add-Check "restart no-autoload" "PASS" "settings/catalog returned and launcher owns no wsl.exe child"
}
catch {
    Add-Check "harness" "FAIL" $_.Exception.Message
    throw
}
finally {
    Stop-OwnedLauncher
    Save-Evidence
}
