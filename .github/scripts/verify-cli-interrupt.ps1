<#
Windows CLI 进程验收：真实 Ctrl-C 与 TerminateProcess，不向云端发送请求。
仅使用合成凭据和临时工作区；本机代理接受 CONNECT 后不响应，保持 CLI 在 Running。
强杀恢复等待生产租约自然到期，不改数据库时间；任何失败都阻止门禁通过。
#>
param(
    [string]$FluxPath = (Join-Path $PSScriptRoot '../../target/debug/flux.exe'),
    [string]$EvidenceDir = (Join-Path ([IO.Path]::GetTempPath()) ('flux-cli-interrupt-' + [guid]::NewGuid().ToString('N'))),
    [ValidateSet('All', 'CtrlC', 'Kill')][string]$Mode = 'All'
)
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'UNVERIFIED: this acceptance script requires Windows console APIs' }
$fluxExecutable = (Resolve-Path -LiteralPath $FluxPath).Path
$evidenceRoot = [IO.Path]::GetFullPath($EvidenceDir)
New-Item -ItemType Directory -Path $evidenceRoot -ErrorAction Stop | Out-Null
$shellExecutable = (Get-Process -Id $PID).Path
$signalFile = Join-Path $evidenceRoot 'send-ctrl-c.ps1'
# 信号在独立辅助进程内发送；辅助进程先脱离自己的控制台，避免影响调用者。
@'
param([int]$TargetProcessId)
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class FluxConsoleSignal {
    [DllImport("kernel32.dll", SetLastError=true)] public static extern bool FreeConsole();
    [DllImport("kernel32.dll", SetLastError=true)] public static extern bool AttachConsole(uint pid);
    [DllImport("kernel32.dll", SetLastError=true)] public static extern bool SetConsoleCtrlHandler(IntPtr handler, bool add);
    [DllImport("kernel32.dll", SetLastError=true)] public static extern bool GenerateConsoleCtrlEvent(uint kind, uint group);
}
"@
[FluxConsoleSignal]::FreeConsole() | Out-Null
if (-not [FluxConsoleSignal]::AttachConsole($TargetProcessId)) { throw "AttachConsole failed: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())" }
try {
    if (-not [FluxConsoleSignal]::SetConsoleCtrlHandler([IntPtr]::Zero, $true)) { throw 'cannot ignore helper Ctrl-C' }
    if (-not [FluxConsoleSignal]::GenerateConsoleCtrlEvent(0, 0)) { throw 'cannot generate CTRL_C_EVENT' }
    Start-Sleep -Milliseconds 500
} finally { [FluxConsoleSignal]::FreeConsole() | Out-Null }
'@ | Set-Content -LiteralPath $signalFile

# 只修改本脚本进程的环境，finally 恢复；不读取或输出真实密钥。
$environmentNames = @('DEEPSEEK_API_KEY', 'HTTPS_PROXY', 'HTTP_PROXY', 'ALL_PROXY', 'NO_PROXY', 'https_proxy', 'http_proxy', 'all_proxy', 'no_proxy') | Select-Object -Unique
$savedEnvironment = @{}
foreach ($name in $environmentNames) { $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
$results = @()
try {
    foreach ($scenario in @('CtrlC', 'Kill')) {
        if ($Mode -ne 'All' -and $Mode -ne $scenario) { continue }
        $scenarioDir = Join-Path $evidenceRoot $scenario
        $workspaceDir = Join-Path $scenarioDir 'workspace'
        $dataDir = Join-Path $scenarioDir 'data'
        New-Item -ItemType Directory -Path $workspaceDir -Force | Out-Null
        $stdoutFile = Join-Path $scenarioDir 'stdout.txt'
        $stderrFile = Join-Path $scenarioDir 'stderr.txt'
        $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
        $listener.Start()
        $proxy = 'http://127.0.0.1:' + $listener.LocalEndpoint.Port
        foreach ($name in @('HTTPS_PROXY', 'HTTP_PROXY', 'ALL_PROXY')) { [Environment]::SetEnvironmentVariable($name, $proxy, 'Process') }
        [Environment]::SetEnvironmentVariable('NO_PROXY', '', 'Process')
        [Environment]::SetEnvironmentVariable('DEEPSEEK_API_KEY', 'sk-test-cli-interrupt-not-real', 'Process')
        $proxyConnection = $listener.AcceptTcpClientAsync()
        $child = $null
        $proxyClient = $null
        try {
            $arguments = @('run', '--engine', 'deepseek', '--workspace', ('"{0}"' -f $workspaceDir), '--prompt', 'interrupt-acceptance', '--data-dir', ('"{0}"' -f $dataDir))
            $child = Start-Process -FilePath $fluxExecutable -ArgumentList $arguments -WorkingDirectory $workspaceDir -WindowStyle Hidden -PassThru -RedirectStandardOutput $stdoutFile -RedirectStandardError $stderrFile
            $deadline = [DateTime]::UtcNow.AddSeconds(7)
            $runId = $null
            do {
                if ($child.HasExited) { throw "CLI exited before interruption: $(Get-Content -LiteralPath $stderrFile -Raw)" }
                $text = Get-Content -LiteralPath $stdoutFile -Raw -ErrorAction SilentlyContinue
                if ($text -match 'run_id=(run_[a-z0-9-]+)') { $runId = $Matches[1] }
                if ($runId -and $proxyConnection.IsCompletedSuccessfully) { break }
                Start-Sleep -Milliseconds 50
            } while ([DateTime]::UtcNow -lt $deadline)
            if (-not $runId -or -not $proxyConnection.IsCompletedSuccessfully) { throw 'CLI did not reach the local stalled proxy' }
            $proxyClient = $proxyConnection.Result
            # 检查第二个真实 CLI 进程不会误收仍在运行、租约未过期的 Run。
            $fresh = & $fluxExecutable status $runId --data-dir $dataDir
            if ($LASTEXITCODE -ne 0 -or $fresh -notcontains 'status=Running') { throw 'fresh live lease was incorrectly reaped' }
            if ($scenario -eq 'CtrlC') {
                $helper = Start-Process -FilePath $shellExecutable -ArgumentList @('-NoProfile', '-File', ('"{0}"' -f $signalFile), '-TargetProcessId', $child.Id) -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $scenarioDir 'signal-stdout.txt') -RedirectStandardError (Join-Path $scenarioDir 'signal-stderr.txt')
                if (-not $helper.WaitForExit(15000)) {
                    Stop-Process -Id $helper.Id -Force
                    throw 'Ctrl-C helper timed out'
                }
                if ($helper.ExitCode -ne 0) { throw "Ctrl-C sender failed: $(Get-Content -LiteralPath (Join-Path $scenarioDir 'signal-stderr.txt') -Raw)" }
                if (-not $child.WaitForExit(7000)) { throw 'CLI did not exit after real Ctrl-C' }
                if ($child.ExitCode -eq 0) { throw 'Cancelled CLI must not exit successfully' }
                $status = & $fluxExecutable status $runId --data-dir $dataDir
                if ($LASTEXITCODE -ne 0 -or $status -notcontains 'status=Cancelled' -or $status -notcontains 'terminal_reason=Some(Cancelled)') { throw 'real Ctrl-C did not persist Cancelled' }
                $events = & $fluxExecutable events $runId --data-dir $dataDir
                if ($LASTEXITCODE -ne 0 -or -not ($events -match 'type=run.cancelled') -or -not ($events -match 'type=attempt.ended')) { throw 'cancelled lifecycle events missing' }
            } else {
                Stop-Process -Id $child.Id -Force
                $child.WaitForExit()
                $deadline = [DateTime]::UtcNow.AddSeconds(80)
                do {
                    $status = & $fluxExecutable status $runId --data-dir $dataDir
                    if ($LASTEXITCODE -ne 0) { throw 'restart status failed' }
                    if ($status -contains 'status=Failed') { break }
                    Start-Sleep -Milliseconds 500
                } while ([DateTime]::UtcNow -lt $deadline)
                if ($status -notcontains 'status=Failed' -or $status -notcontains 'terminal_reason=Some(HostRestart)') { throw 'terminated CLI was not reaped after natural lease expiry' }
                $events = & $fluxExecutable events $runId --data-dir $dataDir
                if ($LASTEXITCODE -ne 0 -or -not ($events -match 'type=attempt.orphaned') -or -not ($events -match 'type=run.failed')) { throw 'orphan lifecycle events missing' }
            }
            $status | Set-Content -LiteralPath (Join-Path $scenarioDir 'status.txt')
            $events | Set-Content -LiteralPath (Join-Path $scenarioDir 'events.txt')
            $results += [pscustomobject]@{ scenario = $scenario; run_id = $runId; result = 'passed'; os = 'Windows'; data_dir = $dataDir }
            Write-Output "$scenario passed run_id=$runId"
        } finally {
            if ($child -and -not $child.HasExited) { Stop-Process -Id $child.Id -Force }
            if ($proxyClient) { $proxyClient.Dispose() }
            $listener.Stop()
        }
    }
    $results | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $evidenceRoot 'evidence.json')
    Write-Output "EVIDENCE_DIR=$evidenceRoot"
} finally {
    foreach ($name in $environmentNames) { [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process') }
}
