$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$Root = $PSScriptRoot
$ResultPath = Join-Path $Root 'webview2-realm-disable-results.json'
$LogPath = Join-Path $Root 'webview2-realm-disable-run.log'
$RequestPath = Join-Path $Root 'sink-request.json'
$OffHostPath = Join-Path $Root 'offhost-endpoints.json'
$LoopbackPath = Join-Path $Root 'loopback-endpoints.json'
$ConfigPath = Join-Path $Root 'measurement-config.json'
$SinkStdout = Join-Path $Root 'sink-local.stdout.log'
$SinkStderr = Join-Path $Root 'sink-local.stderr.log'

if ((Test-Path -LiteralPath $ResultPath) -or (Test-Path -LiteralPath $LogPath) -or
    (Test-Path -LiteralPath $LoopbackPath) -or (Test-Path -LiteralPath $ConfigPath) -or
    (Test-Path -LiteralPath $SinkStdout) -or (Test-Path -LiteralPath $SinkStderr)) {
    Write-Error 'Existing run evidence found. Preserve it and use a fresh extracted copy.'
    exit 4
}
if (-not (Test-Path -LiteralPath $RequestPath -PathType Leaf)) {
    Write-Error 'Missing sink-request.json. Run prepare-run.ps1 exactly once first.'
    exit 4
}
if (-not (Test-Path -LiteralPath $OffHostPath -PathType Leaf)) {
    Write-Error 'Missing offhost-endpoints.json returned by the controlled sink operator.'
    exit 4
}

function Invoke-NativeLogged {
    param(
        [Parameter(Mandatory = $true)][string]$FilePath,
        [string[]]$ArgumentList = @()
    )
    $SavedErrorActionPreference = $ErrorActionPreference
    try {
        # Windows PowerShell 5.1 promotes redirected native stderr to error
        # records. Cargo writes normal progress there, so keep it flowing while
        # retaining fail-closed PowerShell errors everywhere else.
        $ErrorActionPreference = 'Continue'
        & $FilePath @ArgumentList 2>&1 |
            Tee-Object -FilePath $LogPath -Append -ErrorAction Stop |
            Out-Host
        $NativeExit = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $SavedErrorActionPreference
    }
    return $NativeExit
}

Push-Location $Root
$SinkProcess = $null
try {
    if ($env:OS -ne 'Windows_NT') {
        throw 'This measurement must run on Windows/WebView2. Linux gates are source checks only.'
    }
    @(
        'Buzz WebView2 realm-complete disable successor measurement',
        ('started_utc=' + [DateTime]::UtcNow.ToString('o')),
        ('powershell=' + $PSVersionTable.PSVersion),
        ('os=' + [Environment]::OSVersion.VersionString),
        ('arch=' + [Runtime.InteropServices.RuntimeInformation]::OSArchitecture)
    ) | Tee-Object -FilePath $LogPath

    $RustcExit = Invoke-NativeLogged -FilePath 'rustc' -ArgumentList @('-Vv')
    if ($RustcExit -ne 0) { throw 'rustc failed; install Rust stable with the MSVC toolchain.' }
    $CargoVersionExit = Invoke-NativeLogged -FilePath 'cargo' -ArgumentList @('-V')
    if ($CargoVersionExit -ne 0) { throw 'cargo failed.' }
    $Python = (Get-Command python -CommandType Application -ErrorAction Stop).Source
    $PythonVersionExit = Invoke-NativeLogged -FilePath $Python -ArgumentList @('--version')
    if ($PythonVersionExit -ne 0) { throw 'Python 3 failed.' }

    $InputFiles = Get-ChildItem -LiteralPath $Root -Recurse -File |
        Where-Object {
            $_.FullName -notmatch '[\/](target|\.git|gen)[\/]' -and
            $_.FullName -notin @($ResultPath,$LogPath,$LoopbackPath,$ConfigPath,$SinkStdout,$SinkStderr,$OffHostPath,$RequestPath)
        } | Sort-Object FullName
    $InputFiles | Get-FileHash -Algorithm SHA256 |
        Select-Object @{Name='Path';Expression={$_.Path.Substring($Root.Length + 1)}}, Hash |
        Format-Table -AutoSize | Out-String | Tee-Object -FilePath $LogPath -Append
    ('sink_request_sha256=' + (Get-FileHash -LiteralPath $RequestPath -Algorithm SHA256).Hash.ToLowerInvariant()) |
        Tee-Object -FilePath $LogPath -Append
    ('offhost_endpoints_sha256=' + (Get-FileHash -LiteralPath $OffHostPath -Algorithm SHA256).Hash.ToLowerInvariant()) |
        Tee-Object -FilePath $LogPath -Append

    'source_tests=starting' | Tee-Object -FilePath $LogPath -Append
    $TestExit = Invoke-NativeLogged -FilePath 'cargo' -ArgumentList @('test','--locked')
    if ($TestExit -ne 0) { throw 'Source tests failed; no runtime verdict was attempted.' }
    $PythonTestExit = Invoke-NativeLogged -FilePath $Python -ArgumentList @('-m','unittest','sink/test_controlled_sink.py')
    if ($PythonTestExit -ne 0) { throw 'Controlled sink tests failed; no runtime verdict was attempted.' }
    $FmtExit = Invoke-NativeLogged -FilePath 'cargo' -ArgumentList @('fmt','--all','--','--check')
    if ($FmtExit -ne 0) { throw 'Rust formatting check failed; no runtime verdict was attempted.' }
    $ClippyExit = Invoke-NativeLogged -FilePath 'cargo' -ArgumentList @('clippy','--locked','--all-targets','--','-D','warnings')
    if ($ClippyExit -ne 0) { throw 'Clippy failed; no runtime verdict was attempted.' }

    'windows_compile_check=starting' | Tee-Object -FilePath $LogPath -Append
    $CheckExit = Invoke-NativeLogged -FilePath 'cargo' -ArgumentList @('check','--locked','--target','x86_64-pc-windows-msvc')
    if ($CheckExit -ne 0) { throw 'Windows compile check failed; no runtime verdict was attempted.' }

    $Request = Get-Content -LiteralPath $RequestPath -Raw | ConvertFrom-Json
    $OffHost = Get-Content -LiteralPath $OffHostPath -Raw | ConvertFrom-Json
    if ($Request.schema -ne 'buzz-webview2-realm-disable-sink-request/v1' -or
        $OffHost.schema -ne 'buzz-controlled-webrtc-sink-endpoints/v1' -or
        $Request.token -ne $OffHost.token) {
        throw 'Off-host endpoints do not match this fresh run request.'
    }
    $SinkScript = Join-Path $Root 'sink\controlled_sink.py'
    $SinkProcess = Start-Process -FilePath $Python -ArgumentList @(
        $SinkScript,'--bind','127.0.0.1','--advertise','127.0.0.1',
        '--token',$Request.token,'--output',$LoopbackPath,'--duration-seconds','180'
    ) -WorkingDirectory $Root -RedirectStandardOutput $SinkStdout -RedirectStandardError $SinkStderr -PassThru -NoNewWindow

    $Deadline = [DateTime]::UtcNow.AddSeconds(15)
    while (-not (Test-Path -LiteralPath $LoopbackPath -PathType Leaf)) {
        if ($SinkProcess.HasExited) { throw "Loopback sink exited early with code $($SinkProcess.ExitCode)." }
        if ([DateTime]::UtcNow -gt $Deadline) { throw 'Loopback sink did not become ready.' }
        Start-Sleep -Milliseconds 200
    }
    $Loopback = Get-Content -LiteralPath $LoopbackPath -Raw | ConvertFrom-Json
    if ($Loopback.token -ne $Request.token) { throw 'Loopback sink token mismatch.' }
    $Combined = [ordered]@{
        schema = 'buzz-webview2-realm-disable-config/v1'
        token = $Request.token
        loopback = $Loopback
        off_host = $OffHost
    }
    $Utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [System.IO.File]::WriteAllText($ConfigPath, (($Combined | ConvertTo-Json -Depth 20) + "`n"), $Utf8NoBom)
    ('measurement_config_sha256=' + (Get-FileHash -LiteralPath $ConfigPath -Algorithm SHA256).Hash.ToLowerInvariant()) |
        Tee-Object -FilePath $LogPath -Append

    'runtime_measurement=starting (three arms; windows close automatically)' |
        Tee-Object -FilePath $LogPath -Append
    $env:WEBVIEW2_SUCCESSOR_CONFIG = $ConfigPath
    $env:RUST_BACKTRACE = '1'
    $CargoExit = Invoke-NativeLogged -FilePath 'cargo' -ArgumentList @('run','--locked')

    if (-not (Test-Path -LiteralPath $ResultPath -PathType Leaf)) {
        throw "Harness exited with code $CargoExit but wrote no result file. Preserve every log; do not rerun."
    }
    $Result = Get-Content -LiteralPath $ResultPath -Raw | ConvertFrom-Json
    '' | Tee-Object -FilePath $LogPath -Append
    ('OVERALL: ' + $Result.overall) | Tee-Object -FilePath $LogPath -Append
    $Result.rows | Select-Object status, name | Format-Table -AutoSize |
        Out-String | Tee-Object -FilePath $LogPath -Append
    Write-Host "Result: $ResultPath"
    Write-Host "Log:    $LogPath"
    Write-Host 'FIRST COMPLETE RESULT PRESERVED — DO NOT RUN AGAIN.'
    if ($Result.overall -ne 'PASS') { exit 2 }
    exit 0
}
catch {
    ('RUNNER_ERROR: ' + $_.Exception.Message) | Tee-Object -FilePath $LogPath -Append
    Write-Error $_
    exit 3
}
finally {
    if ($SinkProcess -and -not $SinkProcess.HasExited) {
        Stop-Process -Id $SinkProcess.Id -Force -ErrorAction SilentlyContinue
        $SinkProcess.WaitForExit()
    }
    Pop-Location
}
