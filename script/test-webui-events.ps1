[CmdletBinding()]
param([Parameter(Mandatory = $true)][string]$WorkDirectory)
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$work = [IO.Path]::GetFullPath((Join-Path $root $WorkDirectory))
$allowed = [IO.Path]::GetFullPath((Join-Path $root "_fluxdns")) + [IO.Path]::DirectorySeparatorChar
if (-not $work.StartsWith($allowed, [StringComparison]::OrdinalIgnoreCase)) { throw "必须指定 _fluxdns 下的本地夹具" }
for ($directory = Get-Item -LiteralPath $work; $directory.FullName.StartsWith($allowed, [StringComparison]::OrdinalIgnoreCase); $directory = $directory.Parent) {
    if ($directory.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "测试目录不能通过 reparse point 指向其他位置" }
}
$context = Get-Content -LiteralPath (Join-Path $work "test-context.json") -Raw | ConvertFrom-Json
if ($context.kind -ne "fluxdns-webui-local-acceptance") { throw "本地夹具标记不匹配" }
foreach ($port in @($context.webPort, $context.dnsPort)) { if ($port -le 1024 -or $port -gt 65535) { throw "测试端口无效" } }
$origin = "http://127.0.0.1:$($context.webPort)"
$session = New-Object Microsoft.PowerShell.Commands.WebRequestSession

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

function Send-DnsQuery([string]$Name, [int]$Id) {
    $wire = [System.Collections.Generic.List[byte]]::new()
    $wire.Add([byte](($Id -shr 8) -band 0xff)); $wire.Add([byte]($Id -band 0xff))
    $wire.AddRange([byte[]](0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00))
    foreach ($label in $Name.Split('.')) {
        $bytes = [Text.Encoding]::ASCII.GetBytes($label)
        $wire.Add([byte]$bytes.Length); $wire.AddRange($bytes)
    }
    $wire.AddRange([byte[]](0x00, 0x00, 0x01, 0x00, 0x01))
    $udp = [Net.Sockets.UdpClient]::new()
    $udp.Client.ReceiveTimeout = 5000
    $endpoint = [Net.IPEndPoint]::new([Net.IPAddress]::Loopback, $context.dnsPort)
    [void]$udp.Send($wire.ToArray(), $wire.Count, $endpoint)
    $remote = [Net.IPEndPoint]::new([Net.IPAddress]::Any, 0)
    $response = $udp.Receive([ref]$remote)
    $udp.Dispose()
    Assert-True ($response.Length -ge 12) "DNS response is too short"
    Assert-True ($response[0] -eq (($Id -shr 8) -band 0xff) -and $response[1] -eq ($Id -band 0xff)) "DNS response id mismatch"
    Assert-True (($response[3] -band 0x0f) -eq 0) "DNS response RCODE is not NOERROR"
}

function New-Ticket([hashtable]$Headers) {
    return Invoke-RestMethod -Method Post -Uri "$origin/api/v2/events/ticket" -Headers $Headers
}

# 单次 ticket 只通过 subprotocol 传递；连接超时和失败必须释放 socket。
function Connect-Events([string]$Ticket) {
    $socket = [Net.WebSockets.ClientWebSocket]::new()
    $socket.Options.SetRequestHeader("Origin", $origin)
    $socket.Options.AddSubProtocol("fluxdns.v1")
    $socket.Options.AddSubProtocol("fluxdns.ticket.$Ticket")
    $cancel = [Threading.CancellationTokenSource]::new([TimeSpan]::FromSeconds(5))
    try {
        [void]$socket.ConnectAsync([Uri]"ws://127.0.0.1:$($context.webPort)/api/v2/events", $cancel.Token).GetAwaiter().GetResult()
    } catch {
        $socket.Dispose()
        throw
    } finally { $cancel.Dispose() }
    Assert-True ($socket.SubProtocol -eq "fluxdns.v1") "WebSocket selected an unexpected subprotocol"
    return ,$socket
}

function Send-Json([Net.WebSockets.ClientWebSocket]$Socket, [object]$Value) {
    $bytes = [Text.Encoding]::UTF8.GetBytes(($Value | ConvertTo-Json -Depth 12 -Compress))
    [void]$Socket.SendAsync([ArraySegment[byte]]::new($bytes), [Net.WebSockets.WebSocketMessageType]::Text, $true, [Threading.CancellationToken]::None).GetAwaiter().GetResult()
}

# 本夹具只消费短 JSON 事件；分片帧同样受整体字节和时间预算约束。
function Receive-Event([Net.WebSockets.ClientWebSocket]$Socket, [int]$TimeoutSeconds = 10) {
    $buffer = [byte[]]::new(131072)
    $stream = [IO.MemoryStream]::new()
    $cancel = [Threading.CancellationTokenSource]::new([TimeSpan]::FromSeconds($TimeoutSeconds))
    try {
        do {
            $result = $Socket.ReceiveAsync([ArraySegment[byte]]::new($buffer), $cancel.Token).GetAwaiter().GetResult()
            if ($result.MessageType -eq [Net.WebSockets.WebSocketMessageType]::Close) {
                return [pscustomobject]@{ type = "close"; code = [int]$Socket.CloseStatus }
            }
            $stream.Write($buffer, 0, $result.Count)
            if ($stream.Length -gt 131072) { throw "WebSocket 验收响应超过预算" }
        } while (-not $result.EndOfMessage)
        return [Text.Encoding]::UTF8.GetString($stream.ToArray()) | ConvertFrom-Json
    } finally {
        $cancel.Dispose(); $stream.Dispose()
    }
}

$credentials = @{ username = $context.username; password = $context.password } | ConvertTo-Json
$login = Invoke-RestMethod -Method Post -Uri "$origin/api/v2/auth/login" -Headers @{ Origin = $origin } -ContentType "application/json" -Body $credentials -WebSession $session
$headers = @{ Authorization = "Bearer $($login.access_token)"; Origin = $origin }
$metrics = Invoke-RestMethod -Method Get -Uri "$origin/api/v2/service/metrics" -Headers $headers -WebSession $session
Assert-True ($metrics.rss_bytes.state -eq "available") "Bearer metrics request did not return RSS"

$cookieSession = New-Object Microsoft.PowerShell.Commands.WebRequestSession
foreach ($cookie in $session.Cookies.GetCookies([Uri]$origin)) {
    $cookieSession.Cookies.Add([Uri]$origin, $cookie)
}
$cookieOnlyStatus = 0
try {
    Invoke-WebRequest -Method Get -Uri "$origin/api/v2/service/metrics" -WebSession $cookieSession | Out-Null
} catch {
    $cookieOnlyStatus = [int]$_.Exception.Response.StatusCode
}
Assert-True ($cookieOnlyStatus -eq 401) "Cookie-only business request was not rejected"

Send-DnsQuery "cache.p5.test" 0x4101
$now = [DateTimeOffset]::UtcNow
$from = $now.AddDays(-1).ToUnixTimeMilliseconds()
$to = $now.AddDays(1).ToUnixTimeMilliseconds()
$query = @{
    filter = @{ from_ms = $from; to_ms = $to }
    cursor = $null
    direction = "older"
    page_size = 20
    sort = "occurred_at"
    order = "desc"
} | ConvertTo-Json -Depth 8
$page = $null
for ($attempt = 0; $attempt -lt 25; $attempt++) {
    $page = Invoke-RestMethod -Method Post -Uri "$origin/api/v2/queries/search" -Headers $headers -WebSession $session -ContentType "application/json" -Body $query
    if ($page.items.qname -contains "cache.p5.test.") { break }
    Start-Sleep -Milliseconds 200
}
Assert-True ($page.items.qname -contains "cache.p5.test.") "HTTP query snapshot did not include the real UDP request"

$socket = Connect-Events (New-Ticket $headers).ticket
$ready = Receive-Event $socket
Assert-True ($ready.type -eq "ready") "WebSocket did not send ready"
$subscription = @{
    type = "subscribe_queries"
    subscription_id = "p5-smoke"
    filter = @{ from_ms = $from; to_ms = $to }
    after = $page.snapshot_cursor
    retention_revision = $page.retention_revision
}
Send-Json $socket $subscription
Send-DnsQuery "cache.p5.test" 0x4102
$first = Receive-Event $socket
Assert-True ($first.type -eq "queries" -and $first.items.qname -contains "cache.p5.test.") "WebSocket did not push the committed record"
$firstCursor = $first.cursor
$firstRecordId = $first.items[0].id
$socket.Dispose()

Send-DnsQuery "cache.p5.test" 0x4103
Start-Sleep -Milliseconds 200
$socket = Connect-Events (New-Ticket $headers).ticket
$ready = Receive-Event $socket
$subscription.subscription_id = "p5-replay"
$subscription.after = $firstCursor
Send-Json $socket $subscription
$replay = Receive-Event $socket
Assert-True ($replay.type -eq "queries" -and $replay.items.qname -contains "cache.p5.test.") "WebSocket did not replay the disconnected interval"

Assert-True ($replay.items[0].id -ne $firstRecordId) "Replay duplicated the already delivered record"

Invoke-RestMethod -Method Post -Uri "$origin/api/v2/auth/logout" -Headers $headers -WebSession $session | Out-Null
$closed = Receive-Event $socket
Assert-True ($closed.type -eq "close" -and $closed.code -eq 4401) "Logout did not revoke the active WebSocket"
$socket.Dispose()

$report = [pscustomobject]@{
    metrics_http = 200
    cookie_only_http = $cookieOnlyStatus
    dns_rcode = "NOERROR"
    snapshot_sequence = $page.snapshot_cursor.sequence
    pushed_sequence = $firstCursor.sequence
    replay_sequence = $replay.cursor.sequence
    websocket_protocol = "fluxdns.v1"
    session_close = $closed.code
}
$report | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $work "websocket-report-$([DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()).json") -Encoding utf8NoBOM
$report | ConvertTo-Json -Compress
