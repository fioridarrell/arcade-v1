# Authenticode-sign Windows release binaries.
#
# The certificate is a CI secret, not a file in the repo (there isn't one yet):
#   WINDOWS_CODESIGN_PFX       base64 PKCS#12 (.pfx), a code-signing certificate
#   WINDOWS_CODESIGN_PASSWORD  password for that PFX; empty if the PFX has none
#
# When WINDOWS_CODESIGN_PFX is not set this exits 0 and says so. Martin has to
# add the secret before Install.exe, setup.exe, arcaded.exe, arcade.exe and
# arcade-desktop.exe can be signed. When the secret is set, a signing failure
# fails the job.
#
# Call this on arcaded.exe, arcade.exe and arcade-desktop.exe before the
# installer payload is hashed, and on setup.exe after the payload is appended.
# Install.exe is a copy of the signed setup.exe, so it does not need its own
# signtool run.

$ErrorActionPreference = 'Stop'

$pfxB64 = $env:WINDOWS_CODESIGN_PFX
if ([string]::IsNullOrWhiteSpace($pfxB64)) {
    Write-Host "Authenticode signing skipped: the CI secret WINDOWS_CODESIGN_PFX is not set. Windows binaries are unsigned. Martin needs to put a code-signing certificate (base64 PKCS#12) in WINDOWS_CODESIGN_PFX and its password in WINDOWS_CODESIGN_PASSWORD before these files can be signed: Install.exe, setup.exe, arcaded.exe, arcade.exe, arcade-desktop.exe."
    exit 0
}

$files = @($args | Where-Object { $_ })
if ($files.Count -eq 0) {
    throw "sign-authenticode.ps1: no files to sign"
}
foreach ($file in $files) {
    if (-not (Test-Path -LiteralPath $file)) {
        throw "sign-authenticode.ps1: missing $file"
    }
}

$kitRoot = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
$signtool = Get-ChildItem -Path $kitRoot -Filter signtool.exe -Recurse -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -match '\\x64\\signtool\.exe$' } |
    Sort-Object FullName -Descending |
    Select-Object -First 1
if (-not $signtool) {
    throw "Authenticode signing was requested (WINDOWS_CODESIGN_PFX is set) but signtool.exe was not found under $kitRoot."
}

$work = Join-Path $env:TEMP ("arcade-codesign-" + [guid]::NewGuid().ToString("n"))
New-Item -ItemType Directory -Path $work | Out-Null
$pfxPath = Join-Path $work "codesign.pfx"
try {
    $clean = ($pfxB64 -replace '\s', '')
    [System.IO.File]::WriteAllBytes($pfxPath, [Convert]::FromBase64String($clean))
    $pass = $env:WINDOWS_CODESIGN_PASSWORD
    if ($null -eq $pass) { $pass = '' }
    $timestamps = @(
        'http://timestamp.digicert.com',
        'http://timestamp.sectigo.com',
        'http://timestamp.acs.microsoft.com'
    )
    foreach ($file in $files) {
        $signed = $false
        $last = "signtool did not run"
        foreach ($url in $timestamps) {
            $signArgs = @('sign', '/fd', 'SHA256', '/td', 'SHA256', '/tr', $url, '/f', $pfxPath, '/d', 'Arcade V1.0')
            if ($pass -ne '') { $signArgs += @('/p', $pass) }
            $signArgs += $file
            & $signtool.FullName @signArgs
            if ($LASTEXITCODE -eq 0) {
                $signed = $true
                break
            }
            $last = "signtool exit $LASTEXITCODE using timestamp $url"
        }
        if (-not $signed) {
            throw "Authenticode signing failed for $file. $last"
        }
        Write-Host "Authenticode signed $file"
    }
} finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
exit 0
