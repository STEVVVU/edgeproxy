param(
    [int]$Requests = 20000,
    [int]$Concurrency = 128,
    # Also measure the HTTPS listener. Requires a certificate, which is generated
    # automatically into benchmark/tls/ if absent.
    [switch]$IncludeTls
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot "TestCertificate.ps1")

$processes = @()

Push-Location $root
try {
    cargo build --release --bins --examples
    New-Item -ItemType Directory -Force -Path "benchmark" | Out-Null

    $configuration = "benchmark/edgeproxy.bench.toml"
    if ($IncludeTls) {
        New-TestCertificate -Directory (Join-Path $root "benchmark/tls")
        $configuration = "benchmark/edgeproxy.bench.tls.toml"
    }

    $fixtureA = Start-Process `
        -FilePath "target/release/examples/fixture_upstream.exe" `
        -ArgumentList "--listen", "127.0.0.1:19001", "--id", "a" `
        -RedirectStandardOutput "benchmark/fixture-a.log" `
        -RedirectStandardError "benchmark/fixture-a.err.log" `
        -WindowStyle Hidden `
        -PassThru
    $processes += $fixtureA

    $fixtureB = Start-Process `
        -FilePath "target/release/examples/fixture_upstream.exe" `
        -ArgumentList "--listen", "127.0.0.1:19002", "--id", "b" `
        -RedirectStandardOutput "benchmark/fixture-b.log" `
        -RedirectStandardError "benchmark/fixture-b.err.log" `
        -WindowStyle Hidden `
        -PassThru
    $processes += $fixtureB

    $proxy = Start-Process `
        -FilePath "target/release/edgeproxy.exe" `
        -ArgumentList "--config", $configuration `
        -RedirectStandardOutput "benchmark/edgeproxy.log" `
        -RedirectStandardError "benchmark/edgeproxy.err.log" `
        -WindowStyle Hidden `
        -PassThru
    $processes += $proxy

    Start-Sleep -Seconds 2

    Write-Host "`n=== Plaintext HTTP ==="
    & "target/release/loadgen.exe" `
        --address "127.0.0.1:18080" `
        --requests $Requests `
        --concurrency $Concurrency |
        Tee-Object -FilePath "benchmark/latest.json"
    if ($LASTEXITCODE -ne 0) {
        throw "loadgen failed with exit code $LASTEXITCODE"
    }

    if ($IncludeTls) {
        Write-Host "`n=== TLS (HTTPS) ==="
        & "target/release/loadgen.exe" `
            --address "127.0.0.1:18443" `
            --tls `
            --tls-server-name "localhost" `
            --requests $Requests `
            --concurrency $Concurrency |
            Tee-Object -FilePath "benchmark/latest-tls.json"
        if ($LASTEXITCODE -ne 0) {
            throw "TLS loadgen failed with exit code $LASTEXITCODE"
        }
    }
}
finally {
    foreach ($process in $processes) {
        if ($null -ne $process -and -not $process.HasExited) {
            Stop-Process -Id $process.Id -Force
        }
    }
    Pop-Location
}
