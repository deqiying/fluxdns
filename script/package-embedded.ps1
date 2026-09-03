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

Require-Command -Name "pnpm"
Require-Command -Name "cargo"

if (-not (Test-Path -LiteralPath $backendManifest -PathType Leaf)) {
    throw "后端 manifest 不存在：$backendManifest"
}

# 先确认发布 feature，避免在前端构建后才发现只能生成未内嵌资源的普通 binary。
Write-Host "检查 webui-embed feature..."
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
    throw "backend/Cargo.toml 尚未声明 webui-embed feature；为避免生成未内嵌前端资源的发布物，打包已停止。"
}

$targets = @(
    [pscustomobject]@{
        Triple = "x86_64-unknown-linux-gnu"
        SourceName = "fluxdns"
        DestinationName = "fluxdns-linux-x86_64"
    },
    [pscustomobject]@{
        Triple = "x86_64-pc-windows-msvc"
        SourceName = "fluxdns.exe"
        DestinationName = "fluxdns-windows-x86_64.exe"
    }
)

if ($null -ne (Get-Command "rustup" -ErrorAction SilentlyContinue)) {
    $installedTargets = @(rustup target list --installed)
    if ($LASTEXITCODE -ne 0) {
        throw "无法读取 Rust target 列表；请确认 rustup/toolchain 可用。"
    }
    foreach ($target in $targets) {
        if ($installedTargets -notcontains $target.Triple) {
            throw "Rust target $($target.Triple) 未安装；请在运行脚本前准备该 target 和对应 linker。脚本不会自动安装工具链。"
        }
    }
}

Write-Host "构建前端（产物保留在 frontend/dist）..."
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

New-Item -ItemType Directory -Path $deployDirectory -Force | Out-Null
$temporaryOutputs = [System.Collections.Generic.List[string]]::new()
try {
    foreach ($target in $targets) {
        Write-Host "构建 $($target.Triple)（Cargo 产物保留在 backend/target）..."
        Invoke-CheckedCommand -Name "cargo" -Arguments @(
            "build",
            "--manifest-path", $backendManifest,
            "--locked",
            "--release",
            "--features", "webui-embed",
            "--target", $target.Triple
        )

        $sourceBinary = Join-Path $repositoryRoot "backend/target/$($target.Triple)/release/$($target.SourceName)"
        if (-not (Test-Path -LiteralPath $sourceBinary -PathType Leaf)) {
            throw "Cargo 构建成功但未找到目标文件：$sourceBinary"
        }

        # Cargo target 目录保留原生构建物；deploy 只接收最终发布文件。
        $destinationBinary = Join-Path $deployDirectory $target.DestinationName
        $temporaryBinary = "$destinationBinary.partial-$PID"
        $temporaryOutputs.Add($temporaryBinary)
        Copy-Item -LiteralPath $sourceBinary -Destination $temporaryBinary -Force
        Move-Item -LiteralPath $temporaryBinary -Destination $destinationBinary -Force
        $temporaryOutputs.Remove($temporaryBinary) | Out-Null

        if ($IsLinux) {
            Invoke-CheckedCommand -Name "chmod" -Arguments @("+x", $destinationBinary)
        }
        Write-Host "已输出：$destinationBinary"
    }
}
finally {
    foreach ($temporaryBinary in $temporaryOutputs) {
        if (Test-Path -LiteralPath $temporaryBinary) {
            Remove-Item -LiteralPath $temporaryBinary -Force
        }
    }
}

Write-Host "打包完成。前端独立产物：$frontendDistDirectory；后端 Cargo 产物：$(Join-Path $repositoryRoot 'backend/target')；发布二进制：$deployDirectory。"
