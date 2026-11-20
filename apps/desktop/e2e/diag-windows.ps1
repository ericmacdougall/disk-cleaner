# Diagnostics for the Windows WebDriver launch ("DevToolsActivePort file doesn't exist" after
# 60 s, probe runs 44684705816 and 44686507550). Not a check. Usage:
#   pwsh e2e/diag-windows.ps1 -Exe <app.exe> -Out <folder>
param([Parameter(Mandatory)] [string]$Exe, [Parameter(Mandatory)] [string]$Out)
$ErrorActionPreference = 'Continue'
New-Item -ItemType Directory -Force $Out | Out-Null
$id = 'io.github.ericmacdougall.prunedisk'
$local = [Environment]::GetFolderPath('LocalApplicationData')

function Show-Webviews($what) {
  "--- $what: webview processes"
  Get-CimInstance Win32_Process -Filter "Name = 'msedgewebview2.exe'" |
    ForEach-Object { "  pid $($_.ProcessId) parent $($_.ParentProcessId): $($_.CommandLine)" } | Select-Object -First 6
}
function Stop-Tree($p) {
  if ($p -and -not $p.HasExited) { & taskkill /F /T /PID $p.Id | Out-Null }
  Start-Sleep 2
}

"=== 1. the app alone, 15 s"
$p = Start-Process -FilePath $Exe -PassThru -RedirectStandardError "$Out\app1-stderr.txt" -RedirectStandardOutput "$Out\app1-stdout.txt"
Start-Sleep 15
"alive after 15 s: $(-not $p.HasExited)$(if ($p.HasExited) { ", exit code $($p.ExitCode)" })"
Show-Webviews 'alone'
"data folder:"; Get-ChildItem -Recurse -Depth 1 "$local\$id" -ErrorAction SilentlyContinue | Select-Object -First 15 | ForEach-Object { "  $($_.FullName)" }
Stop-Tree $p
"stderr:"; Get-Content "$Out\app1-stderr.txt" -ErrorAction SilentlyContinue | Select-Object -First 20

"=== 2. with WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222"
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = '--remote-debugging-port=9222'
$p = Start-Process -FilePath $Exe -PassThru
Start-Sleep 15
try { "json/version: $((Invoke-WebRequest -UseBasicParsing http://127.0.0.1:9222/json/version -TimeoutSec 5).Content)" } catch { "json/version: no answer ($($_.Exception.Message))" }
Show-Webviews 'with the variable'
"DevToolsActivePort files:"
Get-ChildItem -Recurse -Force -Filter DevToolsActivePort "$local\$id", $env:TEMP -ErrorAction SilentlyContinue | ForEach-Object { "  $($_.FullName)" }
Stop-Tree $p
Remove-Item Env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS

"=== 3. msedgedriver directly, verbose"
$driver = Join-Path $env:EDGEWEBDRIVER 'msedgedriver.exe'
$d = Start-Process -FilePath $driver -ArgumentList '--port=9515', '--verbose', "--log-path=$Out\msedgedriver.log" -PassThru
Start-Sleep 3
$caps = @{ capabilities = @{ alwaysMatch = @{ browserName = 'webview2'; 'ms:edgeChromium' = $true; 'ms:edgeOptions' = @{ binary = $Exe; args = @() } } } } | ConvertTo-Json -Depth 6
try { "session: $((Invoke-WebRequest -UseBasicParsing -Method Post -ContentType 'application/json' -Body $caps http://127.0.0.1:9515/session -TimeoutSec 90).Content)" } catch { "session: $($_.Exception.Message) $($_.ErrorDetails.Message)" }
Show-Webviews 'under msedgedriver'
Stop-Tree $d
"msedgedriver log (launch lines):"
Get-Content "$Out\msedgedriver.log" -ErrorAction SilentlyContinue | Select-String -Pattern 'Launching|command line|WEBVIEW2|user-data|DevToolsActivePort|env|Failed|ERROR|exited' | Select-Object -First 40 | ForEach-Object { "  $_" }
