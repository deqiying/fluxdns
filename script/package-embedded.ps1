[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if ($PSVersionTable.PSVersion.Major -lt 7) {
    throw "该脚本需要 PowerShell 7 或更高版本。"
}

function Invoke-CheckedCommand {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Name,

        [Parameter(Mandatory = $true)]
        [string[]]$Arguments
    )

    & $Name @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "命令失败（退出码 $LASTEXITCODE）：$Name $($Arguments -join ' ')"
    }
}

function Require-Command {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Name
    )

    if ($null -eq (Get-Command $Name -ErrorAction SilentlyContinue)) {
        throw "未找到命令 `$Name`，请先按项目工具链规范准备 Node.js、pnpm 和 Rust。"
    }
}

$scriptDirectory = Split-Path -Parent $MyInvocation.MyCommand.Path
$repositoryRoot = Split-Path -Parent $scriptDirectory
$frontendDirectory = Join-Path $repositoryRoot "frontend"
$frontendDistDirectory = Join-Path $frontendDirectory "dist"
$backendManifest = Join-Path $repositoryRoot "backend/Cargo.toml"
$deployDirectory = Join-Path $repositoryRoot "deploy"

$architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
if ($architecture -ne [System.Runtime.InteropServices.Architecture]::X64) {
    throw "当前打包脚本只支持 x86_64，检测到架构：$architecture。"
}

$target = if ($IsWindows) {
    [pscustomobject]@{
        Triple = "x86_64-pc-windows-msvc"
        SourceName = "fluxdns.exe"
        DestinationName = "fluxdns-windows-x86_64.exe"
    }
}
elseif ($IsLinux) {
    [pscustomobject]@{
        Triple = "x86_64-unknown-linux-gnu"
        SourceName = "fluxdns"
        DestinationName = "fluxdns-linux-x86_64"
    }
}
else {
    throw "当前打包脚本只支持 Windows x86_64 与 Linux x86_64。"
}

Require-Command -Name "pnpm"
Require-Command -Name "cargo"

if (-not (Test-Path -LiteralPath $backendManifest -PathType Leaf)) {
    throw "后端 manifest 不存在：$backendManifest"
}

Write-Host "阶段 1/3：构建前端独立构建物（frontend/dist）..."
Push-Location $frontendDirectory
try {
    Invoke-CheckedCommand -Name "pnpm" -Arguments @("install", "--frozen-lockfile")
    Invoke-CheckedCommand -Name "pnpm" -Arguments @("run", "build")
}
finally {
    Pop-Location
}

$frontendEntry = Join-Path $frontendDistDirectory "index.html"
if (-not (Test-Path -LiteralPath $frontendEntry -PathType Leaf)) {
    throw "前端构建完成但缺少 frontend/dist/index.html。"
}

Write-Host "阶段 2/3：构建后端独立构建物（默认 feature）..."
Invoke-CheckedCommand -Name "cargo" -Arguments @(
    "build",
    "--manifest-path", $backendManifest,
    "--locked",
    "--release"
)
$backendBinary = Join-Path $repositoryRoot "backend/target/release/$($target.SourceName)"
if (-not (Test-Path -LiteralPath $backendBinary -PathType Leaf)) {
    throw "后端构建完成但未找到独立构建物：$backendBinary"
}

# 独立构建物完成后再检查内嵌发布能力；目标平台缺失不应阻止前两阶段产物落地。
Write-Host "阶段 3/3：检查 webui-embed feature 与 $($target.Triple) target..."
$metadataOutput = & cargo metadata --locked --no-deps --format-version 1 --manifest-path $backendManifest
if ($LASTEXITCODE -ne 0) {
    throw "无法读取 Cargo metadata，无法确认 webui-embed feature。"
}
$metadata = $metadataOutput | ConvertFrom-Json
$package = @($metadata.packages | Where-Object { $_.name -eq "fluxdns" }) | Select-Object -First 1
$featureNames = if ($null -eq $package -or $null -eq $package.features) {
    @()
} else {
    @($package.features.PSObject.Properties | ForEach-Object { $_.Name })
}
if ($featureNames -notcontains "webui-embed") {
    throw "backend/Cargo.toml 尚未声明 webui-embed feature；前后端独立构建物已保留，未生成发布物。"
}

if ($null -ne (Get-Command "rustup" -ErrorAction SilentlyContinue)) {
    $installedTargets = @(rustup target list --installed)
    if ($LASTEXITCODE -ne 0) {
        throw "无法读取 Rust target 列表；请确认 rustup/toolchain 可用。"
    }
    if ($installedTargets -notcontains $target.Triple) {
        throw "Rust target $($target.Triple) 未安装；前后端独立构建物已保留，请准备当前平台 target 和 linker 后重试。脚本不会自动安装工具链。"
    }
}

Write-Host "构建内嵌 WebUI 的单一发布物（$($target.Triple)）..."
Invoke-CheckedCommand -Name "cargo" -Arguments @(
    "build",
    "--manifest-path", $backendManifest,
    "--locked",
    "--release",
    "--features", "webui-embed",
    "--target", $target.Triple
)

$embeddedBinary = Join-Path $repositoryRoot "backend/target/$($target.Triple)/release/$($target.SourceName)"
if (-not (Test-Path -LiteralPath $embeddedBinary -PathType Leaf)) {
    throw "内嵌 WebUI 构建完成但未找到目标文件：$embeddedBinary"
}

# Cargo target 目录保留原生构建物；deploy 每次只接收当前平台的最终发布文件。
New-Item -ItemType Directory -Path $deployDirectory -Force | Out-Null
$destinationBinary = Join-Path $deployDirectory $target.DestinationName
$temporaryBinary = "$destinationBinary.partial-$PID"
try {
    Copy-Item -LiteralPath $embeddedBinary -Destination $temporaryBinary -Force
    Move-Item -LiteralPath $temporaryBinary -Destination $destinationBinary -Force

    if ($IsLinux) {
        Invoke-CheckedCommand -Name "chmod" -Arguments @("+x", $destinationBinary)
    }
}
finally {
    if (Test-Path -LiteralPath $temporaryBinary -PathType Leaf) {
        Remove-Item -LiteralPath $temporaryBinary -Force
    }
}

Write-Host "打包完成。"
Write-Host "前端独立构建物：$frontendDistDirectory"
Write-Host "后端独立构建物：$backendBinary"
Write-Host "内嵌 WebUI Cargo 构建物：$embeddedBinary"
Write-Host "当前平台发布物：$destinationBinary"
