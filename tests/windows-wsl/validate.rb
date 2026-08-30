# frozen_string_literal: true

require "yaml"

ROOT = File.expand_path("../..", __dir__)

def read(relative)
  File.read(File.join(ROOT, relative), encoding: "UTF-8")
end

def require_match(text, pattern, message)
  abort(message) unless text.match?(pattern)
end

smoke = read("tests/windows-wsl/smoke.ps1")
%w[Distro LlamaServer ModelRoot ModelKey ExePath BaseUrl Token SkipBuild ManualResourceChecks
   NonInteractive TimeoutSeconds LlamaCommit ModelProvenance].each do |parameter|
  require_match(smoke, /\$#{parameter}\b/, "smoke.ps1 is missing parameter #{parameter}")
end
require_match(smoke, /Stop-Process\s+-Id\s+\$ownedPid/,
              "smoke.ps1 must clean up only its recorded launcher PID")
abort("smoke.ps1 must never kill by process name") if smoke.match?(/(?:taskkill|Stop-Process\s+-Name|Get-Process\s+[^\n]*\|\s*Stop-Process)/i)
require_match(smoke, /evidence\.json/, "smoke.ps1 must write JSON evidence")
require_match(smoke, /evidence\.md/, "smoke.ps1 must write Markdown evidence")
abort("smoke must not copy or serialize authentication hashes") if smoke.match?(/auth_token_hashes|Copy-Item.*config/i)
require_match(smoke, /MODEL_LAUNCHER_SHUTDOWN_FILE/, "smoke needs graceful shutdown sentinel")
require_match(smoke, /\/proc\/.*\/stat/, "smoke must capture Linux process start-time identity")
require_match(smoke, /function Sanitize-Evidence/, "evidence strings need control/Markdown sanitization")
require_match(smoke, /SkipModelHash/, "large-model hashing needs an explicit skip switch")
require_match(smoke, /Get-FileHash.*modelShard/, "provided model hashes must still be recomputed")
require_match(smoke, /function Read-PassFail/, "manual checks need a strict Read-PassFail helper")
require_match(smoke, /\$script:HadFailure/, "smoke.ps1 must aggregate failures")
require_match(smoke, /schema\s*=\s*2/, "evidence schema must be explicitly versioned")
require_match(smoke, /manual_checks/, "evidence must separate manual checks")
abort("manual evidence must not use a MANUAL pseudo-status") if smoke.match?(/["']MANUAL["']/)
require_match(smoke, /if \(\$script:HadFailure\) \{ exit 1 \}/, "aggregated failures must produce a nonzero exit")
require_match(smoke, /if \(\$NonInteractive\).*throw/, "noninteractive prompt paths must fail instead of hanging")
%w[windows_version powershell_version wsl_version wsl_kernel wsl_distro_version llama_version llama_help_sha256
   llama_executable_sha256 model_sha256 cpu gpu ram_bytes app_commit sanitized_command].each do |field|
  require_match(smoke, /\b#{field}\b/, "evidence metadata missing #{field}")
end
require_match(smoke, /window_weak_released/, "weak window release needs independent evidence")
require_match(smoke, /Invoke-Api GET "\/v1\/models"/, "resource cycle must recheck API health")

ci = YAML.safe_load(read(".github/workflows/ci.yml"), aliases: false)
abort("CI needs contents: read permission") unless ci.dig("permissions", "contents") == "read"
matrix = ci.dig("jobs", "test", "strategy", "matrix", "os")
expected = %w[windows-latest macos-latest ubuntu-latest]
abort("CI OS matrix mismatch: #{matrix.inspect}") unless matrix == expected
abort("CI test job needs a timeout") unless ci.dig("jobs", "test", "timeout-minutes").is_a?(Integer)

windows = YAML.safe_load(read(".github/workflows/windows.yml"), aliases: false)
triggers = windows.fetch("on")
abort("real WSL workflow must be manual only") unless triggers.keys == ["workflow_dispatch"]
abort("real WSL workflow needs contents: read permission") unless windows.dig("permissions", "contents") == "read"
workflow_text = read(".github/workflows/windows.yml")
abort("workflow_dispatch must not accept a token input") if windows.dig("on", "workflow_dispatch", "inputs")&.key?("token")
require_match(workflow_text, /secrets\.MODEL_LAUNCHER_SMOKE_TOKEN/, "workflow token must come from a repository secret")
require_match(workflow_text, /::add-mask::/, "workflow must mask the optional token")
require_match(workflow_text, /-NonInteractive/, "workflow smoke must be explicitly noninteractive")
abort("workflow must not run interactive resource checks") if workflow_text.include?("-ManualResourceChecks")
abort("workflow must not contain Read-Host") if workflow_text.match?(/Read-Host/i)
abort("workflow artifact upload must not use a directory or glob") if workflow_text.match?(/path:\s*(?:artifacts\/windows-wsl\/?|.*[*])/)
%w[artifacts/windows-wsl/evidence.json artifacts/windows-wsl/evidence.md].each do |path|
  require_match(workflow_text, /#{Regexp.escape(path)}/, "workflow must upload only #{path}")
end
artifact_paths = windows.dig("jobs", "real-wsl", "steps").find { |step| step["name"] == "Upload automated evidence" }.dig("with", "path").lines.map(&:strip).reject(&:empty?)
abort("artifact upload contains unexpected paths: #{artifact_paths.inspect}") unless artifact_paths == %w[artifacts/windows-wsl/evidence.json artifacts/windows-wsl/evidence.md]
abort("all workflow actions must be pinned to full SHAs") if workflow_text.scan(/uses:\s*([^\s]+)/).flatten.any? { |use| !use.match?(/@[0-9a-f]{40}\z/) }
abort("self-hosted acceptance needs a protected environment") unless windows.dig("jobs", "real-wsl", "environment") == "windows-wsl-acceptance"
require_match(workflow_text, /github\.ref.*refs\/heads\/main/, "acceptance workflow must validate the main ref")

ci_text = read(".github/workflows/ci.yml")
abort("all CI actions must be pinned to full SHAs") if ci_text.scan(/uses:\s*([^\s]+)/).flatten.any? { |use| !use.match?(/@[0-9a-f]{40}\z/) }

main = read("apps/model-launcher/src/main.rs")
require_match(main, /MODEL_LAUNCHER_SHUTDOWN_FILE/, "application must support graceful smoke shutdown")

rc = read("apps/model-launcher/resources/app.rc")
{
  /FileDescription.*Model Launcher/ => "FileDescription",
  /CompanyName.*Axolotl/ => "CompanyName",
  /ProductName.*Model Launcher/ => "ProductName",
  /app\.manifest/ => "manifest",
  /app\.ico/ => "icon"
}.each { |pattern, field| require_match(rc, pattern, "Windows resource missing #{field}") }

puts "Windows packaging contracts passed"
