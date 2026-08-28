# Asserts that files written by the harness scripts are BOM-free, LF-only UTF-8.
#
#   .\scripts\Assert-TextEncoding.ps1 -Path benchmark/latest.json -Json
#   .\scripts\Assert-TextEncoding.ps1 -Path demo/edgeproxy.demo.toml
#
# This exists because the encoding is a real, previously-shipped defect and not a
# style preference. `Tee-Object -FilePath` and `Set-Content -Encoding utf8` in
# Windows PowerShell 5.1 emit UTF-16LE and UTF-8-with-BOM respectively, so
# reports that looked correct in the console were unparseable as JSON on disk.
# scripts/benchmark.ps1 and scripts/demo.ps1 write through
# [System.IO.File]::WriteAllText with UTF8Encoding($false) instead; this script
# is the executable check that they still do, so a regression to a convenience
# cmdlet fails CI rather than silently corrupting the artifacts again.
#
# Exits 0 when every path passes, 1 with a per-file reason otherwise.
param(
    # Files to check. Each must already exist: a writer that produced nothing is
    # itself a failure, so a missing path is an error rather than a skip.
    [Parameter(Mandatory = $true)][string[]]$Path,

    # Additionally require the content to parse as JSON and to begin with the
    # bytes `7b 0a` ("{" then LF) — the exact header the original BOM fix was
    # verified against.
    [switch]$Json
)

$ErrorActionPreference = "Stop"

# Leading byte sequences that identify a byte-order mark. UTF-8 is the one the
# PowerShell cmdlets actually emit; the UTF-16/32 forms are listed so that a
# switch to a different cmdlet or encoding is also caught rather than mistaken
# for valid UTF-8.
$boms = @(
    @{ Name = "UTF-8"; Bytes = @(0xEF, 0xBB, 0xBF) }
    @{ Name = "UTF-16LE"; Bytes = @(0xFF, 0xFE) }
    @{ Name = "UTF-16BE"; Bytes = @(0xFE, 0xFF) }
    @{ Name = "UTF-32LE"; Bytes = @(0xFF, 0xFE, 0x00, 0x00) }
    @{ Name = "UTF-32BE"; Bytes = @(0x00, 0x00, 0xFE, 0xFF) }
)

function Format-Head {
    param([byte[]]$Bytes, [int]$Count = 4)

    $take = [Math]::Min($Count, $Bytes.Length)
    if ($take -eq 0) { return "(empty)" }
    (($Bytes[0..($take - 1)]) | ForEach-Object { $_.ToString("x2") }) -join " "
}

$failures = New-Object System.Collections.Generic.List[string]

foreach ($file in $Path) {
    if (-not (Test-Path -LiteralPath $file -PathType Leaf)) {
        $failures.Add("${file}: does not exist (the writer produced no output)")
        continue
    }

    $bytes = [System.IO.File]::ReadAllBytes((Resolve-Path -LiteralPath $file).Path)
    $head = Format-Head -Bytes $bytes
    Write-Host ("{0}: {1} bytes, head {2}" -f $file, $bytes.Length, $head)

    if ($bytes.Length -eq 0) {
        $failures.Add("${file}: is empty")
        continue
    }

    $matched = $null
    foreach ($bom in $boms) {
        $signature = $bom.Bytes
        if ($bytes.Length -lt $signature.Length) { continue }
        $same = $true
        for ($i = 0; $i -lt $signature.Length; $i++) {
            if ($bytes[$i] -ne $signature[$i]) { $same = $false; break }
        }
        # Longest match wins so a UTF-32LE BOM is not reported as UTF-16LE.
        if ($same -and ($null -eq $matched -or $signature.Length -gt $matched.Bytes.Length)) {
            $matched = $bom
        }
    }
    if ($null -ne $matched) {
        $failures.Add("${file}: starts with a $($matched.Name) byte-order mark (head $head)")
        continue
    }

    if ($bytes -contains 0x0D) {
        $failures.Add("${file}: contains CR (0x0d); expected LF-only line endings")
        continue
    }

    # Strict UTF-8: throwOnInvalidBytes catches a mis-encoded write that happens
    # to carry no BOM, e.g. UTF-16 without one or a legacy code page.
    $strict = New-Object System.Text.UTF8Encoding($false, $true)
    try {
        $text = $strict.GetString($bytes)
    }
    catch {
        $failures.Add("${file}: is not valid UTF-8 ($($_.Exception.Message))")
        continue
    }

    if ($Json) {
        # `{` then LF. The console output of a BOM-ful report looked identical to
        # a clean one, so the header bytes are the check that actually discriminates.
        if ($bytes.Length -lt 2 -or $bytes[0] -ne 0x7B -or $bytes[1] -ne 0x0A) {
            $failures.Add("${file}: expected the header bytes 7b 0a (`{` then LF), saw $head")
            continue
        }
        try {
            ConvertFrom-Json $text | Out-Null
        }
        catch {
            $failures.Add("${file}: does not parse as JSON ($($_.Exception.Message))")
            continue
        }
        Write-Host "  ok: BOM-free LF UTF-8, header 7b 0a, parses as JSON"
    }
    else {
        Write-Host "  ok: BOM-free LF UTF-8"
    }
}

if ($failures.Count -gt 0) {
    Write-Host ""
    foreach ($failure in $failures) {
        Write-Host "FAIL $failure"
    }
    throw "$($failures.Count) file(s) failed the encoding check"
}

Write-Host "`nAll $($Path.Count) file(s) are BOM-free LF UTF-8."
