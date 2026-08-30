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
   TimeoutSeconds].each do |parameter|
  require_match(smoke, /\$#{parameter}\b/, "smoke.ps1 is missing parameter #{parameter}")
end
require_match(smoke, /Stop-Process\s+-Id\s+\$script:OwnedLauncherPid/,
              "smoke.ps1 must clean up only its recorded launcher PID")
abort("smoke.ps1 must never kill by process name") if smoke.match?(/(?:taskkill|Stop-Process\s+-Name|Get-Process\s+[^\n]*\|\s*Stop-Process)/i)
require_match(smoke, /evidence\.json/, "smoke.ps1 must write JSON evidence")
require_match(smoke, /evidence\.md/, "smoke.ps1 must write Markdown evidence")

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

rc = read("apps/model-launcher/resources/app.rc")
{
  /FileDescription.*Model Launcher/ => "FileDescription",
  /CompanyName.*Axolotl/ => "CompanyName",
  /ProductName.*Model Launcher/ => "ProductName",
  /app\.manifest/ => "manifest",
  /app\.ico/ => "icon"
}.each { |pattern, field| require_match(rc, pattern, "Windows resource missing #{field}") }

puts "Windows packaging contracts passed"
