[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [ValidateSet("start", "status", "stop")]
    [string]$Action,

    [Parameter(Mandatory = $false)]
    [string]$ConfigPath,

    [Parameter(Mandatory = $false)]
    [string]$BinaryPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if ($PSVersionTable.PSVersion.Major -lt 7) {
    throw "该脚本需要 PowerShell 7 或更高版本。"
}

$scriptDirectory = Split-Path -Parent $MyInvocation.MyCommand.Path
$repositoryRoot = Split-Path -Parent $scriptDirectory
$runtimeDirectory = Join-Path $repositoryRoot "_fluxdns"
$logDirectory = Join-Path $runtimeDirectory "logs"
$statePath = Join-Path $runtimeDirectory "dev-process.json"
$stdoutLogPath = Join-Path $logDirectory "dev.stdout.log"
$stderrLogPath = Join-Path $logDirectory "dev.stderr.log"

function Test-SamePath {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Left,

        [Parameter(Mandatory = $true)]
        [string]$Right
    )

    $comparison = if ($IsWindows) {
        [System.StringComparison]::OrdinalIgnoreCase
    } else {
        [System.StringComparison]::Ordinal
    }
    return [string]::Equals(
        [System.IO.Path]::GetFullPath($Left),
        [System.IO.Path]::GetFullPath($Right),
        $comparison
    )
}

function Read-ProcessState {
    if (-not (Test-Path -LiteralPath $statePath -PathType Leaf)) {
        return $null
    }

    try {
        $state = [System.IO.File]::ReadAllText($statePath) | ConvertFrom-Json -ErrorAction Stop
    }
    catch {
        throw "进程状态文件无法读取：$statePath。请先人工确认 FluxDNS 进程，再处理该文件。"
    }
    if ($null -eq $state) {
        throw "进程状态文件内容为空：$statePath。请先人工确认 FluxDNS 进程，再处理该文件。"
    }

    foreach ($property in @("Pid", "BinaryPath", "ConfigPath", "StartTimeUtcTicks", "StartedAtUtc")) {
        if ($null -eq $state.PSObject.Properties[$property]) {
            throw "进程状态文件缺少字段 `$property`：$statePath。请先人工确认 FluxDNS 进程，再处理该文件。"
        }
    }

    $processId = 0
    $startTimeUtcTicks = 0L
    if (-not [int]::TryParse([string]$state.Pid, [ref]$processId) -or $processId -le 0) {
        throw "进程状态文件包含非法 PID：$statePath。请先人工确认 FluxDNS 进程，再处理该文件。"
    }
    if (-not [long]::TryParse([string]$state.StartTimeUtcTicks, [ref]$startTimeUtcTicks) -or $startTimeUtcTicks -le 0) {
        throw "进程状态文件包含非法启动时间：$statePath。请先人工确认 FluxDNS 进程，再处理该文件。"
    }
    if ([string]::IsNullOrWhiteSpace([string]$state.BinaryPath)) {
        throw "进程状态文件包含空二进制路径：$statePath。请先人工确认 FluxDNS 进程，再处理该文件。"
    }
    if ([string]::IsNullOrWhiteSpace([string]$state.StartedAtUtc)) {
        throw "进程状态文件包含空启动时间：$statePath。请先人工确认 FluxDNS 进程，再处理该文件。"
    }

    $startedAtUtc = if ($state.StartedAtUtc -is [datetime]) {
        $state.StartedAtUtc.ToUniversalTime().ToString("O")
    } else {
        [string]$state.StartedAtUtc
    }

    return [pscustomobject]@{
        Pid = $processId
        BinaryPath = [string]$state.BinaryPath
        ConfigPath = [string]$state.ConfigPath
        StartTimeUtcTicks = $startTimeUtcTicks
        StartedAtUtc = $startedAtUtc
    }
}

function Get-ManagedProcessStatus {
    param(
        [Parameter(Mandatory = $true)]
        [pscustomobject]$State
    )

    $process = Get-Process -Id $State.Pid -ErrorAction SilentlyContinue
    if ($null -eq $process) {
        return [pscustomobject]@{ Status = "stale"; Process = $null; Reason = "PID 不存在" }
    }

    try {
        $actualStartTimeUtcTicks = $process.StartTime.ToUniversalTime().Ticks
    }
    catch {
        return [pscustomobject]@{ Status = "unverifiable"; Process = $null; Reason = "无法读取进程启动时间" }
    }
    if ($actualStartTimeUtcTicks -ne $State.StartTimeUtcTicks) {
        return [pscustomobject]@{ Status = "stale"; Process = $null; Reason = "PID 已被其他进程复用" }
    }

    try {
        $actualBinaryPath = $process.Path
    }
    catch {
        $actualBinaryPath = $null
    }
    if (-not [string]::IsNullOrWhiteSpace($actualBinaryPath) -and -not (Test-SamePath -Left $actualBinaryPath -Right $State.BinaryPath)) {
        return [pscustomobject]@{ Status = "stale"; Process = $null; Reason = "PID 对应的可执行文件不匹配" }
    }

    return [pscustomobject]@{ Status = "running"; Process = $process; Reason = $null }
}

function Remove-ProcessState {
    if (Test-Path -LiteralPath $statePath -PathType Leaf) {
        Remove-Item -LiteralPath $statePath -Force
    }
}

function Save-ProcessState {
    param(
        [Parameter(Mandatory = $true)]
        [System.Diagnostics.Process]$Process,

        [Parameter(Mandatory = $true)]
        [string]$ResolvedBinaryPath,

        [Parameter(Mandatory = $true)]
        [string]$ResolvedConfigPath
    )

    $startedAtUtc = $Process.StartTime.ToUniversalTime()
    $payload = [ordered]@{
        pid = $Process.Id
        binaryPath = $ResolvedBinaryPath
        configPath = $ResolvedConfigPath
        startTimeUtcTicks = $startedAtUtc.Ticks
        startedAtUtc = $startedAtUtc.ToString("O")
    }
    $temporaryStatePath = "$statePath.partial-$PID"
    try {
        $json = $payload | ConvertTo-Json
        [System.IO.File]::WriteAllText(
            $temporaryStatePath,
            "$json$([Environment]::NewLine)",
            [System.Text.UTF8Encoding]::new($false)
        )
        Move-Item -LiteralPath $temporaryStatePath -Destination $statePath -Force
    }
    finally {
        if (Test-Path -LiteralPath $temporaryStatePath -PathType Leaf) {
            Remove-Item -LiteralPath $temporaryStatePath -Force
        }
    }
}

function Resolve-ServiceBinaryPath {
    param(
        [Parameter(Mandatory = $false)]
        [string]$RequestedPath
    )

    if ([string]::IsNullOrWhiteSpace($RequestedPath)) {
        if ($IsWindows -and [Environment]::Is64BitOperatingSystem) {
            $RequestedPath = Join-Path $repositoryRoot "deploy/fluxdns-windows-x86_64.exe"
        }
        elseif ($IsLinux -and [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq [System.Runtime.InteropServices.Architecture]::X64) {
            $RequestedPath = Join-Path $repositoryRoot "deploy/fluxdns-linux-x86_64"
        }
        else {
            throw "无法为当前系统自动选择 x86_64 发布二进制；请通过 -BinaryPath 指定可执行文件。"
        }
    }
    elseif (-not [System.IO.Path]::IsPathRooted($RequestedPath)) {
        $RequestedPath = Join-Path $repositoryRoot $RequestedPath
    }

    $resolvedPath = (Resolve-Path -LiteralPath $RequestedPath -ErrorAction Stop).Path
    if (-not (Test-Path -LiteralPath $resolvedPath -PathType Leaf)) {
        throw "发布二进制不是普通文件：$resolvedPath"
    }
    return $resolvedPath
}

if ($Action -ne "start" -and (
        -not [string]::IsNullOrWhiteSpace($ConfigPath) -or
        -not [string]::IsNullOrWhiteSpace($BinaryPath)
    )) {
    throw "-ConfigPath 和 -BinaryPath 只能与 start 动作一起使用。"
}

$existingState = Read-ProcessState
$existingStatus = if ($null -eq $existingState) {
    $null
} else {
    Get-ManagedProcessStatus -State $existingState
}

if ($null -ne $existingStatus -and $existingStatus.Status -eq "unverifiable") {
    throw "无法安全确认 PID $($existingState.Pid) 的进程身份：$($existingStatus.Reason)。未执行启动或停止操作。"
}

if ($null -ne $existingStatus -and $existingStatus.Status -eq "stale") {
    Write-Host "清理失效的进程状态：PID=$($existingState.Pid)，原因=$($existingStatus.Reason)。"
    Remove-ProcessState
    $existingState = $null
    $existingStatus = $null
}

switch ($Action) {
    "status" {
        if ($null -eq $existingStatus) {
            Write-Host "FluxDNS 开发服务未运行。"
            exit 3
        }

        Write-Host "FluxDNS 开发服务正在运行：PID=$($existingState.Pid)"
        Write-Host "二进制：$($existingState.BinaryPath)"
        Write-Host "配置：$($existingState.ConfigPath)"
        Write-Host "启动时间（UTC）：$($existingState.StartedAtUtc)"
        exit 0
    }

    "stop" {
        if ($null -eq $existingStatus) {
            Write-Host "FluxDNS 开发服务未运行。"
            exit 0
        }

        $process = $existingStatus.Process
        Write-Host "停止 FluxDNS 开发服务：PID=$($process.Id)"
        Stop-Process -Id $process.Id -ErrorAction Stop
        if (-not $process.WaitForExit(10000)) {
            throw "等待 PID $($process.Id) 退出超时；保留进程状态文件以便继续诊断。"
        }
        Remove-ProcessState
        Write-Host "FluxDNS 开发服务已停止。"
        exit 0
    }

    "start" {
        if ([string]::IsNullOrWhiteSpace($ConfigPath)) {
            throw "start 动作必须通过 -ConfigPath 显式传入配置文件。"
        }
        if ($null -ne $existingStatus) {
            throw "FluxDNS 开发服务已在运行：PID=$($existingState.Pid)。如需重启，请先执行 dev.ps1 stop。"
        }

        $resolvedConfigPath = (Resolve-Path -LiteralPath $ConfigPath -ErrorAction Stop).Path
        if (-not (Test-Path -LiteralPath $resolvedConfigPath -PathType Leaf)) {
            throw "配置文件不是普通文件：$resolvedConfigPath"
        }
        if ($resolvedConfigPath.Contains('"')) {
            throw "配置文件路径不能包含双引号。"
        }
        $resolvedBinaryPath = Resolve-ServiceBinaryPath -RequestedPath $BinaryPath

        New-Item -ItemType Directory -Path $logDirectory -Force | Out-Null
        $startParameters = @{
            FilePath = $resolvedBinaryPath
            ArgumentList = @("run", "--config", "`"$resolvedConfigPath`"")
            WorkingDirectory = $repositoryRoot
            PassThru = $true
            RedirectStandardOutput = $stdoutLogPath
            RedirectStandardError = $stderrLogPath
        }
        if ($IsWindows) {
            $startParameters.WindowStyle = "Hidden"
        }

        $process = Start-Process @startParameters
        Start-Sleep -Milliseconds 300
        $process.Refresh()
        if ($process.HasExited) {
            throw "FluxDNS 启动后立即退出（exit=$($process.ExitCode)）；请检查 $stdoutLogPath 和 $stderrLogPath。"
        }

        try {
            Save-ProcessState -Process $process -ResolvedBinaryPath $resolvedBinaryPath -ResolvedConfigPath $resolvedConfigPath
        }
        catch {
            Stop-Process -Id $process.Id -ErrorAction SilentlyContinue
            throw "FluxDNS 已启动但无法记录进程状态，已尝试停止 PID $($process.Id)：$($_.Exception.Message)"
        }

        Write-Host "FluxDNS 开发服务已启动：PID=$($process.Id)"
        Write-Host "状态文件：$statePath"
        Write-Host "标准输出：$stdoutLogPath"
        Write-Host "标准错误：$stderrLogPath"
        exit 0
    }
}
