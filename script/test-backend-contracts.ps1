[CmdletBinding()]
param(
    [ValidateSet("Local", "Connections")]
    [string]$Suite = "Local",
    [ValidateRange(1, 20)]
    [int]$Repeat = 1,
    [ValidateRange(30, 1800)]
    [int]$TimeoutSeconds = 300
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
if ($PSVersionTable.PSVersion.Major -lt 7) {
    throw "契约验证需要 PowerShell 7，不会自动安装工具。"
}

$root = Split-Path -Parent $PSScriptRoot
$runDirectory = Join-Path $root "_fluxdns/contract-validation/$([DateTime]::UtcNow.ToString('yyyyMMddTHHmmssfffZ'))-$PID"
$temporaryDirectory = Join-Path $root "_fluxdns/test-temp"
$records = [System.Collections.Generic.List[object]]::new()
$report = [ordered]@{
    suite = $Suite
    repeat = $Repeat
    status = "incomplete"
    startedAtUtc = [DateTime]::UtcNow.ToString("O")
    os = [System.Runtime.InteropServices.RuntimeInformation]::OSDescription
    architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
    logicalProcessors = [Environment]::ProcessorCount
    build = "debug; default features; locked dependencies"
    temporaryDirectory = $temporaryDirectory
    commands = $records
}

function Invoke-Recorded {
    param([string]$Name, [string[]]$Arguments, [switch]$RequireTests)
    $executable = (Get-Command $Name -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source
    $start = [System.Diagnostics.ProcessStartInfo]::new()
    $start.FileName = $executable
    $start.WorkingDirectory = $root
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $start.Environment["TEMP"] = $temporaryDirectory
    $start.Environment["TMP"] = $temporaryDirectory
    $start.Environment["TMPDIR"] = $temporaryDirectory
    foreach ($argument in $Arguments) { $start.ArgumentList.Add($argument) }
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $start
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    $timedOut = $false
    $prefix = "{0:D2}-{1}" -f $records.Count, $Name
    Write-Host "$Name $($Arguments -join ' ')"
    try {
        if (-not $process.Start()) { throw "无法启动 $Name" }
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            $timedOut = $true
            $process.Kill($true)
            $process.WaitForExit()
        }
        $stdout = $stdoutTask.GetAwaiter().GetResult()
        $stderr = $stderrTask.GetAwaiter().GetResult()
        [IO.File]::WriteAllText((Join-Path $runDirectory "$prefix.stdout.log"), $stdout)
        [IO.File]::WriteAllText((Join-Path $runDirectory "$prefix.stderr.log"), $stderr)
        $records.Add([ordered]@{
            executable = $executable
            arguments = $Arguments
            elapsedSeconds = $watch.Elapsed.TotalSeconds
            exitCode = $process.ExitCode
            watchdogKilled = $timedOut
            stdout = "$prefix.stdout.log"
            stderr = "$prefix.stderr.log"
        })
        if ($timedOut -or $process.ExitCode -ne 0) {
            throw "命令失败或 watchdog 终止：$Name；证据目录 $runDirectory"
        }
        if ($RequireTests -and $stdout -notmatch 'test result: ok\. [1-9][0-9]* passed; 0 failed;') {
            throw "未观察到实际通过的测试，不能把空筛选当作验收：$Name"
        }
        return $stdout.Trim()
    }
    finally {
        $process.Dispose()
    }
}

# 指纹包括未提交及未忽略新增源码，避免仅记 HEAD 而漏掉被验证的工作树。
function Get-SourceFingerprint {
    $files = @(& git ls-files --cached --others --exclude-standard -- backend)
    if ($LASTEXITCODE -ne 0) { throw "无法枚举后端源码。" }
    $hashes = foreach ($file in ($files | Sort-Object -Unique)) {
        if (Test-Path -LiteralPath $file -PathType Leaf) {
            "$file $((Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash)"
        }
        else { "$file deleted" }
    }
    $bytes = [Text.Encoding]::UTF8.GetBytes(($hashes -join "`n"))
    return [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($bytes))
}

Push-Location $root
try {
    New-Item -ItemType Directory -Force $runDirectory, $temporaryDirectory | Out-Null
    $report.commit = Invoke-Recorded "git" @("rev-parse", "HEAD")
    $report.toolchain = Invoke-Recorded "mise" @("ls", "rust", "--current")
    $report.rustc = Invoke-Recorded "rustc" @("--version")
    $report.cargo = Invoke-Recorded "cargo" @("--version")
    $report.sourceFingerprint = Get-SourceFingerprint
    $report.lockfileSha256 = (Get-FileHash -LiteralPath "backend/Cargo.lock").Hash
    if ($Suite -eq "Local") {
        $null = Invoke-Recorded "cargo" @("fmt", "--manifest-path", "backend/Cargo.toml", "--", "--check")
        $null = Invoke-Recorded "cargo" @("check", "--manifest-path", "backend/Cargo.toml", "--locked")
    }
    for ($iteration = 1; $iteration -le $Repeat; $iteration++) {
        $arguments = @("test", "--manifest-path", "backend/Cargo.toml", "--locked")
        $arguments += if ($Suite -eq "Connections") {
            @("service::tests::contract_v6_real_session_capacity_releases_and_recovers",
                "--", "--ignored", "--exact", "--nocapture", "--test-threads=1")
        }
        else {
            @("contract_v", "--", "--test-threads=4")
        }
        $null = Invoke-Recorded "cargo" $arguments -RequireTests
    }
    if ($Suite -eq "Local") {
        $null = Invoke-Recorded "cargo" @("test", "--manifest-path", "backend/Cargo.toml",
            "--locked", "--quiet", "--", "--test-threads=4") -RequireTests
    }
    $report.finalSourceFingerprint = Get-SourceFingerprint
    if ($report.finalSourceFingerprint -ne $report.sourceFingerprint) {
        throw "运行期间后端源码发生变化，本轮不能作为固定版本验收。"
    }
    $report.status = "passed"
}
catch {
    $report.status = "failed"
    $report.failure = $_.Exception.Message
    throw
}
finally {
    $report.finishedAtUtc = [DateTime]::UtcNow.ToString("O")
    if (Test-Path -LiteralPath $runDirectory) {
        $report | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $runDirectory "report.json") -Encoding utf8NoBOM
        Write-Host "契约验证状态：$($report.status)；证据目录：$runDirectory"
    }
    Pop-Location
}
