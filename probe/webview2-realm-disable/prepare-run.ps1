$ErrorActionPreference = 'Stop'

$Root = $PSScriptRoot
$Request = Join-Path $Root 'sink-request.json'
if (Test-Path -LiteralPath $Request) {
    throw 'sink-request.json already exists; preserve it and use a fresh extraction.'
}
$Bytes = New-Object byte[] 32
$Rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
try {
    $Rng.GetBytes($Bytes)
} finally {
    $Rng.Dispose()
}
$Token = -join ($Bytes | ForEach-Object { $_.ToString('x2') })
$Value = [ordered]@{
    schema = 'buzz-webview2-realm-disable-sink-request/v1'
    token = $Token
    required_lanes = @('protected-initial','protected-srcdoc','control-initial','control-srcdoc','huddle')
    required_transports = @('stun_udp','turn_udp','turn_tcp','turns_tls')
}
$Utf8NoBom = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllText($Request, (($Value | ConvertTo-Json -Depth 5) + "`n"), $Utf8NoBom)
Write-Host "Created: $Request"
Write-Host 'Send this request file unchanged to the controlled off-host sink operator. Do not paste the token into chat.'
