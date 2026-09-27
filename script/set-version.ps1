[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$Version,

    [switch]$IgnoreUncommittedChanges
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if ($PSVersionTable.PSVersion.Major -lt 7) {
    throw "该脚本需要 PowerShell 7 或更高版本。"
}

$normalizedVersion = $Version.Trim()
if ($normalizedVersion.StartsWith("v", [System.StringComparison]::Ordinal)) {
    $normalizedVersion = $normalizedVersion.Substring(1)
}

$versionPattern = '^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$'
if ($normalizedVersion -notmatch $versionPattern) {
    throw "版本号必须是 SemVer，例如 0.2.0、v0.2.0 或 0.2.0-rc.1。"
}

function Set-SingleVersionMatch {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path,

        [Parameter(Mandatory = $true)]
        [string]$Pattern,

        [Parameter(Mandatory = $true)]
        [string]$NewVersion
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "版本文件不存在：$Path"
    }

    $content = [System.IO.File]::ReadAllText($Path)
    $regex = [System.Text.RegularExpressions.Regex]::new(
        $Pattern,
        [System.Text.RegularExpressions.RegexOptions]::Multiline -bor
            [System.Text.RegularExpressions.RegexOptions]::Singleline
    )
    $matches = $regex.Matches($content)
    if ($matches.Count -ne 1) {
        throw "无法在 $Path 中唯一定位版本号，匹配数量：$($matches.Count)"
    }

    $updated = $regex.Replace(
        $content,
        { param($match) $match.Groups[1].Value + $NewVersion + $match.Groups[2].Value },
        1
    )
    if ($updated -ne $content) {
        $encoding = [System.Text.UTF8Encoding]::new($false)
        [System.IO.File]::WriteAllText($Path, $updated, $encoding)
    }
}

function Invoke-GitChecked {
    param(
        [Parameter(Mandatory = $true)]
        [string]$RepositoryRoot,

        [Parameter(Mandatory = $true)]
        [string[]]$Arguments
    )

    & git -C $RepositoryRoot @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "git $($Arguments[0]) 执行失败，退出码：$LASTEXITCODE"
    }
}

function Get-GitCheckedOutput {
    param(
        [Parameter(Mandatory = $true)]
        [string]$RepositoryRoot,

        [Parameter(Mandatory = $true)]
        [string[]]$Arguments
    )

    $output = @(& git -C $RepositoryRoot @Arguments)
    if ($LASTEXITCODE -ne 0) {
        throw "git $($Arguments[0]) 执行失败，退出码：$LASTEXITCODE"
    }
    return $output
}

$repositoryRoot = Split-Path -Parent $PSScriptRoot
$tagName = "v$normalizedVersion"
$versionFiles = @(
    "VERSION"
    "backend/Cargo.toml"
    "backend/Cargo.lock"
    "frontend/package.json"
)
$gitStatus = @(Get-GitCheckedOutput -RepositoryRoot $repositoryRoot -Arguments @("status", "--porcelain=v1", "--untracked-files=normal"))

if ($gitStatus.Count -gt 0) {
    $changeList = ($gitStatus | ForEach-Object { "  $_" }) -join [Environment]::NewLine
    if (-not $IgnoreUncommittedChanges) {
        throw "检测到已有未提交修改，已停止版本更新、提交和 tag 创建。请先提交或清理这些修改；如确认要保留并继续，请添加 -IgnoreUncommittedChanges。$([Environment]::NewLine)$changeList"
    }

    Write-Warning "检测到已有未提交修改，已按 -IgnoreUncommittedChanges 继续。脚本只提交四个版本文件；这些文件中原有的其他修改也会一并提交。"
}

$versionFile = Join-Path $repositoryRoot "VERSION"
$cargoManifest = Join-Path $repositoryRoot "backend/Cargo.toml"
$cargoLock = Join-Path $repositoryRoot "backend/Cargo.lock"
$frontendPackage = Join-Path $repositoryRoot "frontend/package.json"

$currentBranch = ((Get-GitCheckedOutput -RepositoryRoot $repositoryRoot -Arguments @("branch", "--show-current")) | Out-String).Trim()
if ($currentBranch -ne "main") {
    throw "版本提交和 tag 只能在 main 分支创建，当前分支：$currentBranch"
}

& git -C $repositoryRoot rev-parse -q --verify "refs/tags/$tagName" *> $null
$tagLookupExitCode = $LASTEXITCODE
if ($tagLookupExitCode -eq 0) {
    throw "tag $tagName 已存在，未修改版本文件。"
}
if ($tagLookupExitCode -ne 1) {
    throw "检查 tag $tagName 失败，git rev-parse 退出码：$tagLookupExitCode"
}

$currentVersion = [System.IO.File]::ReadAllText($versionFile).Trim()
if ($currentVersion -notmatch $versionPattern) {
    throw "VERSION 中的当前版本不是有效 SemVer：$currentVersion"
}
if ($currentVersion -eq $normalizedVersion) {
    throw "VERSION 已是 $normalizedVersion，没有可提交的版本变更。"
}

Set-SingleVersionMatch -Path $versionFile -Pattern '(\A)[^\r\n]+(\r?\n?\z)' -NewVersion $normalizedVersion
Set-SingleVersionMatch -Path $cargoManifest -Pattern '(^\[package\]\r?\n.*?^version\s*=\s*")[^"]+(")' -NewVersion $normalizedVersion
Set-SingleVersionMatch -Path $cargoLock -Pattern '(^\[\[package\]\]\r?\nname\s*=\s*"fluxdns"\r?\nversion\s*=\s*")[^"]+(")' -NewVersion $normalizedVersion
Set-SingleVersionMatch -Path $frontendPackage -Pattern '(^\s*"version"\s*:\s*")[^"]+(",\s*$)' -NewVersion $normalizedVersion

$gitAddArguments = @("add", "--") + $versionFiles
Invoke-GitChecked -RepositoryRoot $repositoryRoot -Arguments $gitAddArguments

& git -C $repositoryRoot diff --cached --quiet -- @versionFiles
$diffExitCode = $LASTEXITCODE
if ($diffExitCode -eq 0) {
    throw "版本文件没有产生可提交的变更。"
}
if ($diffExitCode -ne 1) {
    throw "检查暂存版本变更失败，git diff 退出码：$diffExitCode"
}

$commitMessage = "chore(release): 发布 $tagName"
$gitCommitArguments = @("commit", "--only", "-m", $commitMessage, "--") + $versionFiles
Invoke-GitChecked -RepositoryRoot $repositoryRoot -Arguments $gitCommitArguments
Invoke-GitChecked -RepositoryRoot $repositoryRoot -Arguments @("tag", $tagName)

$commitHash = ((Get-GitCheckedOutput -RepositoryRoot $repositoryRoot -Arguments @("rev-parse", "--short", "HEAD")) | Out-String).Trim()
Write-Host "版本号已更新为 $normalizedVersion，并提交为 $commitHash。"
Write-Host "已为该提交创建本地 tag：$tagName"
Write-Host "脚本不会 push；确认后请依次推送 main 和 tag："
Write-Host "  git push origin main"
Write-Host "  git push origin $tagName"
