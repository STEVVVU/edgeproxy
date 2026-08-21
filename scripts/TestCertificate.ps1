# Shared helper for the local harnesses in this directory. Dot-source it:
#
#   . (Join-Path $PSScriptRoot "TestCertificate.ps1")
#
# Kept separate so scripts/benchmark.ps1 and scripts/demo.ps1 generate their
# throwaway certificates identically instead of drifting apart.

# Generates a throwaway self-signed certificate for a local TLS listener.
# Requires openssl on PATH (Git for Windows ships one).
function New-TestCertificate {
    param([Parameter(Mandatory = $true)][string]$Directory)

    $certificate = Join-Path $Directory "test-only-localhost.crt"
    $key = Join-Path $Directory "test-only-localhost.key"
    if ((Test-Path $certificate) -and (Test-Path $key)) {
        Write-Host "Reusing existing test certificate."
        return
    }

    New-Item -ItemType Directory -Force -Path $Directory | Out-Null
    Write-Host "Generating throwaway self-signed certificate..."
    & openssl req -x509 -newkey rsa:2048 -sha256 -days 30 -nodes `
        -keyout $key -out $certificate `
        -subj "/CN=localhost/O=edgeproxy test only" `
        -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" 2>$null
    if ($LASTEXITCODE -ne 0) {
        throw "openssl failed to generate a certificate (exit $LASTEXITCODE)"
    }
}
