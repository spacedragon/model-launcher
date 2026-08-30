[CmdletBinding()]
param(
    [string]$Distro = "Ubuntu", [string]$LlamaServer = "/usr/local/bin/llama-server",
    [Parameter(Mandatory)][string]$LlamaCommit, [Parameter(Mandatory)][string]$ModelRoot,
    [Parameter(Mandatory)][string]$ModelKey, [Parameter(Mandatory)][string]$ModelProvenance,
    [string]$ModelSha256, [string]$SecondModelKey,
    [string]$ExePath = "target/release/model-launcher.exe", [uri]$BaseUrl = "http://127.0.0.1:1234",
    [string]$Token = $env:MODEL_LAUNCHER_SMOKE_TOKEN, [switch]$SkipBuild,
    [switch]$ManualResourceChecks, [switch]$NonInteractive,
    [ValidateRange(30, 3600)][int]$TimeoutSeconds = 300
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$script:OwnedLauncherPid = $null
$script:HadFailure = $false
$script:Evidence = [ordered]@{ schema = 2; started_utc = [DateTime]::UtcNow.ToString("o"); metadata = [ordered]@{}; checks = @(); manual_checks = @() }
$ArtifactDir = Join-Path (Resolve-Path ".") "artifacts/windows-wsl"
$EvidenceJson = Join-Path $ArtifactDir "evidence.json"
$EvidenceMarkdown = Join-Path $ArtifactDir "evidence.md"

function Add-Check([string]$Name, [ValidateSet("PASS", "FAIL", "NOT_RUN")][string]$Status, [string]$Detail) {
    if ($Status -eq "FAIL") { $script:HadFailure = $true }
    $script:Evidence.checks += [ordered]@{ name = $Name; status = $Status; detail = $Detail; utc = [DateTime]::UtcNow.ToString("o") }
    Write-Host "[$Status] $Name - $Detail"
}
function Read-PassFail([string]$Name, [string]$Instruction) {
    if ($NonInteractive) { throw "Manual check '$Name' cannot prompt in -NonInteractive mode" }
    Write-Host "MANUAL CHECK: $Instruction"
    $answer = Read-Host "$Name (type PASS only after verifying; anything else is FAIL)"
    $status = if ($answer -ceq "PASS") { "PASS" } else { "FAIL" }
    if ($status -eq "FAIL") { $script:HadFailure = $true }
    $script:Evidence.manual_checks += [ordered]@{ name = $Name; status = $status; instruction = $Instruction; utc = [DateTime]::UtcNow.ToString("o") }
    Add-Check $Name $status "operator verification: $status"
}
function Save-Evidence {
    New-Item -ItemType Directory -Force $ArtifactDir | Out-Null
    $script:Evidence.finished_utc = [DateTime]::UtcNow.ToString("o")
    [IO.File]::WriteAllText($EvidenceJson, ($script:Evidence | ConvertTo-Json -Depth 10), [Text.UTF8Encoding]::new($false))
    $lines = @("# Windows/WSL acceptance evidence", "", "Generated: $($script:Evidence.finished_utc)", "", "## Metadata", "")
    foreach ($entry in $script:Evidence.metadata.GetEnumerator()) { $lines += "- **$($entry.Key)**: $($entry.Value)" }
    $lines += @("", "## Checks", "", "| Check | Status | Detail |", "|---|---|---|")
    foreach ($item in $script:Evidence.checks) { $lines += "| $($item.name) | $($item.status) | $($item.detail -replace '\|','/') |" }
    [IO.File]::WriteAllLines($EvidenceMarkdown, $lines, [Text.UTF8Encoding]::new($false))
}
function Get-TextSha256([string]$Text) {
    $hash = [Security.Cryptography.SHA256]::Create().ComputeHash([Text.Encoding]::UTF8.GetBytes($Text))
    ($hash | ForEach-Object { $_.ToString("x2") }) -join ""
}
function Wait-Until([scriptblock]$Condition, [string]$Description) {
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    do { try { if (& $Condition) { return } } catch { }; Start-Sleep -Milliseconds 500 } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for $Description"
}
function Invoke-Api([string]$Method, [string]$Path, $Body = $null) {
    $headers = @{}; if ($Token) { $headers.Authorization = "Bearer $Token" }
    $requestParameters = @{ Method = $Method; Uri = [uri]::new($BaseUrl, $Path); Headers = $headers; TimeoutSec = $TimeoutSeconds }
    if ($null -ne $Body) { $requestParameters.ContentType = "application/json"; $requestParameters.Body = ($Body | ConvertTo-Json -Depth 8 -Compress) }
    Invoke-RestMethod @requestParameters
}
function Stop-OwnedLauncher {
    if ($null -ne $script:OwnedLauncherPid) {
        if (Get-Process -Id $script:OwnedLauncherPid -ErrorAction SilentlyContinue) {
            Stop-Process -Id $script:OwnedLauncherPid
            Wait-Process -Id $script:OwnedLauncherPid -Timeout 15 -ErrorAction SilentlyContinue
        }
        $script:OwnedLauncherPid = $null
    }
}

try {
    if ($env:OS -ne "Windows_NT") { throw "This harness requires Windows" }
    if ($NonInteractive -and $ManualResourceChecks) { throw "-ManualResourceChecks is incompatible with -NonInteractive" }
    if (-not (Test-Path -LiteralPath $ModelRoot -PathType Container)) { throw "ModelRoot is not a directory: $ModelRoot" }
    $modelShard = Get-ChildItem -LiteralPath $ModelRoot -Filter *.gguf -File -Recurse | Sort-Object FullName | Select-Object -First 1
    if (-not $modelShard) { throw "ModelRoot contains no GGUF files" }
    $os = Get-CimInstance Win32_OperatingSystem
    $cpu = Get-CimInstance Win32_Processor | Select-Object -ExpandProperty Name
    $gpu = Get-CimInstance Win32_VideoController | Select-Object -ExpandProperty Name
    $wslVersion = (& wsl.exe --version 2>&1) -join " | "
    $distroList = (& wsl.exe --list --quiet) -join "`n"
    if ($distroList -notmatch [regex]::Escape($Distro)) { throw "WSL distribution is not installed: $Distro" }
    $wslKernel = (& wsl.exe -d $Distro -- uname -r 2>&1) -join " "
    $wslDistroVersion = (& wsl.exe -d $Distro -- cat /etc/os-release 2>&1) -join " | "
    & wsl.exe -d $Distro -- test -x $LlamaServer
    if ($LASTEXITCODE -ne 0) { throw "llama-server is missing or not executable: $LlamaServer" }
    $llamaVersion = (& wsl.exe -d $Distro -- $LlamaServer --version 2>&1) -join " | "
    $llamaHelp = (& wsl.exe -d $Distro -- $LlamaServer --help 2>&1) -join "`n"
    if ($LASTEXITCODE -ne 0) { throw "llama-server --help probe failed" }
    $llamaHashLine = (& wsl.exe -d $Distro -- sha256sum -- $LlamaServer 2>&1) -join " "
    $llamaHash = if ($LASTEXITCODE -eq 0) { ($llamaHashLine -split '\s+')[0] } else { "unavailable: $llamaHashLine" }
    $computedModelHash = if ($ModelSha256) { $ModelSha256 } else { (Get-FileHash -LiteralPath $modelShard.FullName -Algorithm SHA256).Hash.ToLowerInvariant() }
    $appCommit = (& git rev-parse HEAD 2>&1) -join ""
    $script:Evidence.metadata = [ordered]@{
        windows_version = "$($os.Caption) $($os.Version) build $($os.BuildNumber)"; powershell_version = $PSVersionTable.PSVersion.ToString()
        wsl_version = $wslVersion; wsl_kernel = $wslKernel; wsl_distro_version = $wslDistroVersion; distro = $Distro
        llama_version = $llamaVersion; llama_commit = $LlamaCommit; llama_help_sha256 = Get-TextSha256 $llamaHelp
        llama_executable_path = $LlamaServer; llama_executable_sha256 = $llamaHash
        model_first_shard = $modelShard.FullName; model_first_shard_bytes = $modelShard.Length; model_sha256 = $computedModelHash; model_provenance = $ModelProvenance
        cpu = $cpu -join "; "; gpu = $gpu -join "; "; ram_bytes = [int64]$os.TotalVisibleMemorySize * 1024
        app_version = "0.1.0"; app_commit = $appCommit; executable_path = $ExePath; executable_sha256 = $null; bind = $BaseUrl.AbsoluteUri
        sanitized_command = "smoke.ps1 -Distro '$Distro' -LlamaServer '$LlamaServer' -LlamaCommit '$LlamaCommit' -ModelRoot '$ModelRoot' -ModelKey '$ModelKey' -ModelProvenance '$ModelProvenance' -ModelSha256 '$ModelSha256' -SecondModelKey '$SecondModelKey' -ExePath '$ExePath' -BaseUrl '$($BaseUrl.AbsoluteUri)' -Token [REDACTED] -SkipBuild:$($SkipBuild.IsPresent) -ManualResourceChecks:$($ManualResourceChecks.IsPresent) -NonInteractive:$($NonInteractive.IsPresent) -TimeoutSeconds $TimeoutSeconds"
        config = [ordered]@{ data_root = "isolated artifact LOCALAPPDATA"; distro = $Distro; llama_server = $LlamaServer; model_root = $ModelRoot; bind = $BaseUrl.AbsoluteUri; authentication = "copied Argon2 hash; plaintext redacted" }
    }
    Add-Check "preflight and metadata" "PASS" "Windows, WSL, llama-server, model, and hardware evidence captured"
    if (-not $SkipBuild) { cargo build -p model-launcher --release; if ($LASTEXITCODE -ne 0) { throw "release build failed" } }
    $resolvedExe = (Resolve-Path -LiteralPath $ExePath).Path
    $script:Evidence.metadata.executable_path = $resolvedExe
    $script:Evidence.metadata.executable_sha256 = (Get-FileHash -LiteralPath $resolvedExe -Algorithm SHA256).Hash.ToLowerInvariant()

    $UserConfig = Join-Path $env:LOCALAPPDATA "ModelLauncher/config/config.json"
    $HarnessLocalAppData = Join-Path $ArtifactDir "local-app-data"; $env:LOCALAPPDATA = $HarnessLocalAppData
    $configDir = Join-Path $HarnessLocalAppData "ModelLauncher/config"; New-Item -ItemType Directory -Force $configDir | Out-Null
    if (Test-Path -LiteralPath $UserConfig) { $settings = Get-Content -Raw -LiteralPath $UserConfig | ConvertFrom-Json }
    else {
        if ($NonInteractive) { throw "Noninteractive smoke needs an existing user config containing the supplied token hash" }
        $settings = [pscustomobject][ordered]@{ version = 1; config = [pscustomobject][ordered]@{
            models = @(); auth_token_hashes = @(); engine_distribution = $Distro; engine_executable = $LlamaServer; catalog_directory = $modelShard.DirectoryName
            default_launch_settings = [ordered]@{ context_length = $null; gpu_layers = $null; cpu_threads = $null; batch_size = $null; parallel_slots = $null; flash_attention = $null; kv_cache_type = $null }
        } }
    }
    $settings.config.engine_distribution = $Distro; $settings.config.engine_executable = $LlamaServer; $settings.config.catalog_directory = (Resolve-Path -LiteralPath $ModelRoot).Path
    [IO.File]::WriteAllText((Join-Path $configDir "config.json"), ($settings | ConvertTo-Json -Depth 8), [Text.UTF8Encoding]::new($false))
    $env:MODEL_LAUNCHER_WSL_DISTRO = $Distro; $env:MODEL_LAUNCHER_LLAMA_SERVER = $LlamaServer
    $launcher = Start-Process -FilePath $resolvedExe -PassThru; $script:OwnedLauncherPid = $launcher.Id
    Add-Check "owned launch" "PASS" "PID $($launcher.Id) recorded for PID-only cleanup"
    if (-not $Token) {
        if ($NonInteractive) { throw "Noninteractive smoke requires MODEL_LAUNCHER_SMOKE_TOKEN secret" }
        Write-Host "Generate a one-time token in Settings; plaintext will not be recorded."
        $Token = Read-Host "Bearer token"
    }
    if (-not $Token) { throw "A token generated by this launcher configuration is required" }
    Wait-Until { (Invoke-Api GET "/api/v1/models").models.Count -gt 0 } "authenticated catalog discovery"
    $catalog = Invoke-Api GET "/api/v1/models"
    if ($ModelKey -notin @($catalog.models | ForEach-Object { $_.key })) { throw "ModelKey '$ModelKey' was not discovered" }
    Add-Check "configure/probe/discover" "PASS" "Discovered $($catalog.models.Count) models including $ModelKey"
    $loaded = Invoke-Api POST "/api/v1/models/load" @{ model = $ModelKey; echo_load_config = $true }
    if ($loaded.status -ne "loaded") { throw "load did not return loaded" }; Add-Check "load" "PASS" "instance $($loaded.model_instance_id)"
    $models = Invoke-Api GET "/v1/models"
    if ($ModelKey -notin @($models.data | ForEach-Object { $_.id })) { throw "OpenAI model list omitted ModelKey" }
    $chat = Invoke-Api POST "/v1/chat/completions" @{ model = $ModelKey; stream = $false; messages = @(@{ role = "user"; content = "Reply with OK" }) }
    if (-not $chat.choices) { throw "non-streaming chat returned no choices" }; Add-Check "chat non-streaming" "PASS" "response contained choices"
    $headers = @{ Accept = "text/event-stream"; Authorization = "Bearer $Token" }
    $sseBody = @{ model = $ModelKey; stream = $true; messages = @(@{ role = "user"; content = "Reply with OK" }) } | ConvertTo-Json -Depth 6 -Compress
    $sse = Invoke-WebRequest -Method Post -Uri ([uri]::new($BaseUrl, "/v1/chat/completions")) -Headers $headers -ContentType "application/json" -Body $sseBody -TimeoutSec $TimeoutSeconds
    if ($sse.Content -notmatch "data:") { throw "streaming chat returned no SSE data" }; Add-Check "chat streaming" "PASS" "SSE data observed"
    Invoke-Api POST "/api/v1/models/unload" @{ instance_id = $loaded.model_instance_id } | Out-Null; Add-Check "unload" "PASS" "primary ejected"
    $jit = Invoke-Api POST "/v1/completions" @{ model = $ModelKey; stream = $false; prompt = "Reply with OK" }
    if (-not $jit.choices) { throw "JIT completion returned no choices" }; Add-Check "JIT load" "PASS" "completion triggered load"
    if ($SecondModelKey) { Add-Check "model_busy" "NOT_RUN" "Timing-sensitive busy check is operator-local" }

    if ($ManualResourceChecks) {
        $before = Get-Process -Id $script:OwnedLauncherPid
        $activeBackendBefore = @(Get-CimInstance Win32_Process | Where-Object { $_.ParentProcessId -eq $script:OwnedLauncherPid -and $_.Name -eq "wsl.exe" } | Select-Object -ExpandProperty ProcessId)
        Read-PassFail "window_cycles_50" "Open/close the tray window exactly 50 times without quitting."
        Start-Sleep -Seconds 3; $after = Get-Process -Id $script:OwnedLauncherPid
        $growthMiB = [math]::Round(($after.WorkingSet64 - $before.WorkingSet64) / 1MB, 2)
        Add-Check "working_set_tolerance" ($(if ($growthMiB -le 32) { "PASS" } else { "FAIL" })) "settled delta ${growthMiB} MiB; tolerance <= 32 MiB"
        Read-PassFail "window_weak_released" "Verify debug/instrumentation output reports every destroyed Slint window/component weak handle released."
        $survivalModels = Invoke-Api GET "/v1/models"
        $activeBackendAfter = @(Get-CimInstance Win32_Process | Where-Object { $_.ParentProcessId -eq $script:OwnedLauncherPid -and $_.Name -eq "wsl.exe" } | Select-Object -ExpandProperty ProcessId)
        $backendSurvived = $activeBackendBefore.Count -gt 0 -and @($activeBackendBefore | Where-Object { $_ -in $activeBackendAfter }).Count -gt 0
        if (-not $backendSurvived) { Add-Check "post_cycle_active_model" "FAIL" "owned WSL backend PID did not survive all window cycles" }
        else { Add-Check "post_cycle_active_model" "PASS" "owned WSL backend PID survived all window cycles" }
        if ($ModelKey -notin @($survivalModels.data | ForEach-Object { $_.id })) { Add-Check "post_cycle_api" "FAIL" "model list unavailable after cycles" }
        else {
            $survivalChat = Invoke-Api POST "/v1/chat/completions" @{ model = $ModelKey; stream = $false; messages = @(@{ role = "user"; content = "Reply with OK" }) }
            Add-Check "post_cycle_api" ($(if ($survivalChat.choices) { "PASS" } else { "FAIL" })) "API and active-model chat checked after cycles"
        }
        $cpu0 = (Get-Process -Id $script:OwnedLauncherPid).CPU; Start-Sleep -Seconds 30; $cpu1 = (Get-Process -Id $script:OwnedLauncherPid).CPU
        $cpuPercent = [math]::Round((($cpu1 - $cpu0) / 30) * 100, 2)
        Add-Check "idle_cpu" ($(if ($cpuPercent -le 1) { "PASS" } else { "FAIL" })) "$cpuPercent% of one CPU; tolerance <= 1%"
        Read-PassFail "crash_backoff_eject" "Kill only the exact logged llama-server PID; verify capped backoff, then Eject and no restart."
        Read-PassFail "log_catalog_bounds" "With overflow fixtures verify logs <= 2000 records/2 MiB and catalog <= 1024 models with bounded diagnostics."
    } else { Add-Check "resource lifecycle" "NOT_RUN" "Run -ManualResourceChecks only in local interactive PowerShell" }

    Stop-OwnedLauncher
    $restarted = Start-Process -FilePath $resolvedExe -PassThru; $script:OwnedLauncherPid = $restarted.Id; Start-Sleep -Seconds 3
    Invoke-Api GET "/api/v1/models" | Out-Null
    $ownedWslChildren = @(Get-CimInstance Win32_Process | Where-Object { $_.ParentProcessId -eq $script:OwnedLauncherPid -and $_.Name -eq "wsl.exe" })
    if ($ownedWslChildren.Count -ne 0) { throw "restart unexpectedly spawned a WSL backend" }
    Add-Check "restart no-autoload" "PASS" "catalog returned and launcher owns no wsl.exe child"
} catch { Add-Check "harness" "FAIL" $_.Exception.Message }
finally { Stop-OwnedLauncher; Save-Evidence }

if ($script:HadFailure) { exit 1 }
