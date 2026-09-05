[CmdletBinding()]
param(
    [string]$RootPath = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../../../..'))
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Invoke-DocumentationCheck {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$RootPath,
        [Parameter(Mandatory)][string[]]$Files,
        [switch]$CheckStructure
    )

    # 使用 PowerShell 自带解析器，避免把代码示例和转义文本当成 Markdown 链接。
    if ($PSVersionTable.PSVersion.Major -lt 7) {
        throw '文档检查需要 PowerShell 7，不会自动安装依赖。'
    }
    ConvertFrom-Markdown -InputObject '# parser bootstrap' | Out-Null
    $builder = [Markdig.MarkdownPipelineBuilder]::new()
    [Markdig.MarkdownExtensions]::UseAutoIdentifiers(
        $builder, [Markdig.Extensions.AutoIdentifiers.AutoIdentifierOptions]::GitHub
    ) | Out-Null
    [Markdig.MarkdownExtensions]::UsePipeTables(
        $builder, [Markdig.Extensions.Tables.PipeTableOptions]::new()
    ) | Out-Null
    [Markdig.MarkdownExtensions]::UseYamlFrontMatter($builder) | Out-Null
    $pipeline = $builder.Build()
    $root = [IO.Path]::GetFullPath($RootPath)
    $utf8 = [Text.UTF8Encoding]::new($false, $true)
    $documents = [Collections.Generic.Dictionary[string, object]]::new([StringComparer]::Ordinal)
    $checkedTargets = [Collections.Generic.Dictionary[string, string]]::new([StringComparer]::Ordinal)
    $issues = [Collections.Generic.List[string]]::new()
    $links = [Collections.Generic.List[object]]::new()

    function Add-Issue([string]$Path, [int]$Line, [string]$Message) {
        $name = [IO.Path]::GetRelativePath($root, $Path).Replace('\', '/')
        $issues.Add("${name}:${Line}: $Message")
    }

    function Test-InRoot([string]$Path) {
        $relative = [IO.Path]::GetRelativePath($root, $Path)
        return -not ([IO.Path]::IsPathRooted($relative) -or
            $relative -eq '..' -or $relative.StartsWith('..' + [IO.Path]::DirectorySeparatorChar))
    }

    function Test-Target([string]$Path) {
        if ($checkedTargets.ContainsKey($Path)) { return $checkedTargets[$Path] }
        if (-not (Test-InRoot $Path)) { return '链接越出仓库范围' }
        $relative = [IO.Path]::GetRelativePath($root, $Path)
        $current = $root
        foreach ($part in $relative.Split([IO.Path]::DirectorySeparatorChar)) {
            if ($part -eq '.') { continue }
            $names = @([IO.Directory]::EnumerateFileSystemEntries($current) |
                ForEach-Object { [IO.Path]::GetFileName($_) })
            if ($names -cnotcontains $part) {
                $checkedTargets[$Path] = '目标不存在或路径大小写不一致'
                return $checkedTargets[$Path]
            }
            $current = Join-Path $current $part
            if (([IO.File]::GetAttributes($current) -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                $checkedTargets[$Path] = '不支持符号链接或 reparse point 目标'
                return $checkedTargets[$Path]
            }
        }
        $checkedTargets[$Path] = ''
        return ''
    }

    function Read-Document([string]$Path) {
        if ($documents.ContainsKey($Path)) { return $documents[$Path] }
        $targetError = Test-Target $Path
        if ($targetError) {
            Add-Issue $Path 1 $targetError
            return $null
        }
        try {
            $bytes = [IO.File]::ReadAllBytes($Path)
            $text = $utf8.GetString($bytes)
        } catch {
            Add-Issue $Path 1 '无法以严格 UTF-8 读取文件'
            return $null
        }
        if ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and
            $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF) {
            Add-Issue $Path 1 'UTF-8 文件不得包含 BOM'
            $text = $text.Substring(1)
        }
        if ($text.Contains("`r")) { Add-Issue $Path 1 '换行必须使用 LF' }
        if (-not $text.EndsWith("`n")) { Add-Issue $Path 1 '文件末尾缺少 LF' }
        $ast = [Markdig.Markdown]::Parse($text, $pipeline, $null)
        $nodes = @([Markdig.Syntax.MarkdownObjectExtensions]::Descendants($ast))
        $headings = @($nodes | Where-Object { $_ -is [Markdig.Syntax.HeadingBlock] })
        $anchors = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
        foreach ($heading in $headings) {
            $attributes = [Markdig.Renderers.Html.HtmlAttributesExtensions]::TryGetAttributes($heading)
            if ($null -ne $attributes -and $attributes.Id) { $anchors.Add($attributes.Id) | Out-Null }
        }
        $document = [pscustomobject]@{
            Text = $text; Nodes = $nodes; Headings = $headings; Anchors = $anchors
        }
        $documents[$Path] = $document
        return $document
    }

    foreach ($file in ($Files | Sort-Object -Unique -CaseSensitive)) {
        $path = [IO.Path]::GetFullPath((Join-Path $root $file))
        $document = Read-Document $path
        if ($null -eq $document) { continue }
        $headings = $document.Headings
        if (@($headings | Where-Object Level -eq 1).Count -ne 1 -or
            $headings.Count -eq 0 -or $headings[0].Level -ne 1) {
            Add-Issue $path 1 '文档必须有且仅有一个起始 H1'
        }
        $previousLevel = 0
        foreach ($heading in $headings) {
            if ($previousLevel -gt 0 -and $heading.Level -gt $previousLevel + 1) {
                Add-Issue $path ($heading.Line + 1) '标题层级跳跃'
            }
            $previousLevel = $heading.Level
        }

        $relative = [IO.Path]::GetRelativePath($root, $path).Replace('\', '/')
        if ($relative -notmatch '^\.agents/skills/[^/]+/SKILL\.md$') {
            $headerEnd = if ($headings.Count -gt 1) { $headings[1].Span.Start } else { $document.Text.Length }
            $header = $document.Text.Substring(0, $headerEnd)
            foreach ($field in @('文档状态', '适用范围')) {
                if ($header -notmatch "(?m)^> ${field}：\S.+$") {
                    Add-Issue $path 1 "缺少维护信息：$field"
                }
            }
            if ($header -notmatch '(?m)^> 文档状态：(草案|有效|已废弃)\s*$') {
                Add-Issue $path 1 '文档状态不在允许枚举中'
            }
            if ([IO.Path]::GetFileName($path) -cne 'README.md') {
                $required = switch -Regex ($relative) {
                    '^docs/plans/' { @('计划状态', '代码基线') }
                    '^docs/architecture/' { @('最后评审') }
                    '^docs/implementation/' { @('最后核对', '核对基线') }
                    default { @() }
                }
                foreach ($field in $required) {
                    if ($header -notmatch "(?m)^> ${field}：\S.+$") {
                        Add-Issue $path 1 "缺少分类维护信息：$field"
                    }
                }
                if ($relative -match '^docs/plans/' -and
                    $header -notmatch '(?m)^> 计划状态：(待评审|待实施|实施中|待验收|已完成|已取消)\s*$') {
                    Add-Issue $path 1 '计划状态不在允许枚举中'
                }
            }
        }

        $prose = $document.Text.ToCharArray()
        foreach ($node in $document.Nodes) {
            $line = $node.Line + 1
            if ($node -is [Markdig.Syntax.FencedCodeBlock] -and $node.ClosingFencedCharCount -eq 0) {
                Add-Issue $path $line 'fenced code block 未闭合'
            }
            if ($node -is [Markdig.Syntax.HtmlBlock] -or $node -is [Markdig.Syntax.Inlines.HtmlInline]) {
                Add-Issue $path $line '不支持原始 HTML；请使用 Markdown 链接或行内代码'
            }
            $isEscaped = $node -is [Markdig.Syntax.Inlines.LiteralInline] -and $node.IsFirstCharacterEscaped
            if ($node -is [Markdig.Syntax.CodeBlock] -or
                $node -is [Markdig.Syntax.Inlines.CodeInline] -or
                $node -is [Markdig.Syntax.Inlines.LinkInline] -or
                $node -is [Markdig.Syntax.LinkReferenceDefinition] -or
                $node -is [Markdig.Syntax.Inlines.AutolinkInline] -or
                $node -is [Markdig.Syntax.HtmlBlock] -or
                $node -is [Markdig.Syntax.Inlines.HtmlInline] -or $isEscaped) {
                for ($index = $node.Span.Start; $index -le $node.Span.End; $index++) {
                    $prose[$index] = ' '
                }
            }
            if ($node -is [Markdig.Syntax.Inlines.LinkInline] -or
                $node -is [Markdig.Syntax.LinkReferenceDefinition]) {
                $links.Add([pscustomobject]@{ Path = $path; Line = $line; Url = $node.Url })
            } elseif ($node -is [Markdig.Syntax.Inlines.AutolinkInline]) {
                $links.Add([pscustomobject]@{ Path = $path; Line = $line; Url = $node.Url; })
            }
        }
        # 未解析引用会被拆成多个 LiteralInline；只在 AST 排除代码/链接/转义后的正文检测。
        foreach ($match in [regex]::Matches(
            (-join $prose), '\[[^\]\r\n]+\]\[[^\]\r\n]*\]|\[\^[^\]]+\]|\{#[^}]+\}|\{\{.+?\}\}'
        )) {
            $line = $document.Text.Substring(0, $match.Index).Split("`n").Count
            Add-Issue $path $line '未解析的引用链接或不支持的 footnote/自定义锚点/模板'
        }
    }

    foreach ($link in $links) {
        $url = [string]$link.Url
        if ($url -match '^(https?://|mailto:|//)' -or $url -match '^[^/]+@[^/]+\.[^/]+$') { continue }
        if ($url -match '^[a-zA-Z][a-zA-Z0-9+.-]*:|^/|\\|[?]|\{\{|\$\{') {
            Add-Issue $link.Path $link.Line "不支持的本地链接语法：$url"
            continue
        }
        if ($url -match '%(?![0-9a-fA-F]{2})') {
            Add-Issue $link.Path $link.Line "非法百分号编码：$url"
            continue
        }
        $parts = $url.Split('#', 2)
        $local = [Uri]::UnescapeDataString($parts[0])
        if ([IO.Path]::IsPathRooted($local) -or $local.Contains('\')) {
            Add-Issue $link.Path $link.Line "本地链接必须使用相对路径和正斜杠：$url"
            continue
        }
        try {
            $target = if ($local.Length -eq 0) { $link.Path } else {
                [IO.Path]::GetFullPath((Join-Path ([IO.Path]::GetDirectoryName($link.Path)) $local))
            }
            $targetError = Test-Target $target
        } catch {
            $targetError = '无效的本地路径'
        }
        if ($targetError) {
            Add-Issue $link.Path $link.Line "${targetError}：$url"
            continue
        }
        if ($parts.Count -eq 2 -and $parts[1].Length -gt 0) {
            if ([IO.Path]::GetExtension($target) -ine '.md') {
                Add-Issue $link.Path $link.Line "仅支持 Markdown 标题锚点，不支持代码行号或目录片段：$url"
                continue
            }
            $targetDocument = Read-Document $target
            $fragment = [Uri]::UnescapeDataString($parts[1])
            if ($null -ne $targetDocument -and -not $targetDocument.Anchors.Contains($fragment)) {
                Add-Issue $link.Path $link.Line "标题锚点不存在：$url"
            }
        }
    }

    if ($CheckStructure) {
        $docsRoot = Join-Path $root 'docs'
        $categories = @('plans', 'architecture', 'implementation', 'rules')
        foreach ($category in $categories) {
            $index = Join-Path $docsRoot "$category/README.md"
            if (-not [IO.File]::Exists($index)) { Add-Issue $index 1 '缺少分类目录索引' }
        }
        if (-not [IO.File]::Exists((Join-Path $docsRoot 'README.md'))) {
            Add-Issue (Join-Path $docsRoot 'README.md') 1 '缺少文档总索引'
        }
        if ([IO.Directory]::Exists($docsRoot)) {
            foreach ($directory in Get-ChildItem -LiteralPath $docsRoot -Directory -Recurse) {
                $relative = [IO.Path]::GetRelativePath($docsRoot, $directory.FullName).Replace('\', '/')
                if ($relative.Split('/')[0] -notin $categories -or
                    $directory.Name -in @('archive', 'history', 'backup')) {
                    Add-Issue $directory.FullName 1 '文档目录职责不在基准结构中'
                }
                if (-not [IO.File]::Exists((Join-Path $directory.FullName 'README.md'))) {
                    Add-Issue $directory.FullName 1 '文档目录缺少 README.md'
                }
                if ($directory.Name -cnotmatch '^[a-z][a-z0-9]*(-[a-z0-9]+)*$') {
                    Add-Issue $directory.FullName 1 '目录名必须使用小写 kebab-case'
                }
            }
            foreach ($file in $Files | Where-Object { $_.Replace('\', '/') -match '^docs/' }) {
                $name = [IO.Path]::GetFileName($file)
                if ($name -cne 'README.md' -and $name -cnotmatch '^[a-z][a-z0-9]*(-[a-z0-9]+)*\.md$') {
                    Add-Issue (Join-Path $root $file) 1 '文档文件名必须使用小写 kebab-case'
                }
            }
        }
    }
    return [pscustomobject]@{ FileCount = $Files.Count; LinkCount = $links.Count; Issues = @($issues) }
}

$root = [IO.Path]::GetFullPath($RootPath)
$listed = & git -C $root -c core.quotepath=false ls-files -z --cached --others --exclude-standard
if ($LASTEXITCODE -ne 0) { throw '无法从 Git 枚举 Markdown 文件。' }
$files = @(($listed -join "`n").Split([char]0, [StringSplitOptions]::RemoveEmptyEntries) |
    Where-Object { [IO.Path]::GetExtension($_) -ieq '.md' -and
        [IO.File]::Exists((Join-Path $root $_)) } | Sort-Object -Unique -CaseSensitive)
if ($files.Count -eq 0) { throw '没有找到可检查的 Markdown 文件。' }
$result = Invoke-DocumentationCheck -RootPath $root -Files $files -CheckStructure
if ($result.Issues.Count -gt 0) {
    $result.Issues | ForEach-Object { Write-Output $_ }
    Write-Output "文档检查失败：$($result.Issues.Count) 项问题。"
    exit 1
}
Write-Output "文档检查通过：$($result.FileCount) 个 Markdown，$($result.LinkCount) 个链接/引用；未检查外链网络或产品语义。"
