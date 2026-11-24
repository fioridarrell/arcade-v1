# Check the installer payload, then unpack it.
#
# The trailer format is crates/arcade-install (and scripts/release/payload_trailer.py).
# This is the copy the Windows installer runs. The hash is compared before
# Expand-Archive, and before this script writes the payload anywhere. A mismatch
# leaves -DestDir untouched.
#
# Windows PowerShell 5.1 (the powershell.exe NSIS starts). No PowerShell 7 syntax.

param(
    [Parameter(Mandatory = $true)][string]$Installer,
    [Parameter(Mandatory = $true)][string]$Expected,
    [Parameter(Mandatory = $true)][string]$DestDir
)

$ErrorActionPreference = 'Stop'
# ARCDPAY1
$Magic = [byte[]](0x41, 0x52, 0x43, 0x44, 0x50, 0x41, 0x59, 0x31)

function Fail([string]$Message) {
    Write-Host $Message
    exit 1
}

function Read-U16([byte[]]$Image, [int]$Offset) {
    if ($Offset -lt 0 -or ($Offset + 2) -gt $Image.Length) {
        Fail "This installer is not a readable PE file. Nothing was extracted and nothing was installed."
    }
    return [BitConverter]::ToUInt16($Image, $Offset)
}

function Read-U32([byte[]]$Image, [int]$Offset) {
    if ($Offset -lt 0 -or ($Offset + 4) -gt $Image.Length) {
        Fail "This installer is not a readable PE file. Nothing was extracted and nothing was installed."
    }
    return [BitConverter]::ToUInt32($Image, $Offset)
}

function Read-U64([byte[]]$Image, [int]$Offset) {
    if ($Offset -lt 0 -or ($Offset + 8) -gt $Image.Length) {
        Fail "The installer payload length is not valid. Nothing was extracted and nothing was installed."
    }
    return [BitConverter]::ToUInt64($Image, $Offset)
}

# File offset where the payload trailer ends: the certificate table, or the
# end of an unsigned file. Returns -1 when the image is not PE (a raw buffer).
function Get-CertificateOffset([byte[]]$Image) {
    if ($Image.Length -lt 0x40 -or $Image[0] -ne 0x4D -or $Image[1] -ne 0x5A) {
        return -1
    }
    $eLfanew = [int](Read-U32 $Image 0x3C)
    if (($eLfanew + 24) -gt $Image.Length) {
        Fail "This installer is not a readable PE file. Nothing was extracted and nothing was installed."
    }
    if ($Image[$eLfanew] -ne 0x50 -or $Image[$eLfanew + 1] -ne 0x45 -or $Image[$eLfanew + 2] -ne 0 -or $Image[$eLfanew + 3] -ne 0) {
        Fail "This installer is not a readable PE file. Nothing was extracted and nothing was installed."
    }
    $coff = $eLfanew + 4
    $optSize = [int](Read-U16 $Image ($coff + 16))
    $opt = $coff + 20
    $magic = [int](Read-U16 $Image $opt)
    $numOff = 0
    $ddOff = 0
    if ($magic -eq 0x10B) {
        $numOff = 92
        $ddOff = 96
    } elseif ($magic -eq 0x20B) {
        $numOff = 108
        $ddOff = 112
    } else {
        Fail "This installer is not a readable PE file. Nothing was extracted and nothing was installed."
    }
    if (($opt + $ddOff) -gt $Image.Length -or $optSize -lt $ddOff) {
        Fail "This installer is not a readable PE file. Nothing was extracted and nothing was installed."
    }
    $num = [int64](Read-U32 $Image ($opt + $numOff))
    if ($num -le 4) { return -1 }
    $entry = $opt + $ddOff + 32
    $off = [int64](Read-U32 $Image $entry)
    $size = [int64](Read-U32 $Image ($entry + 4))
    if ($off -eq 0 -or $size -eq 0) { return -1 }
    if ($off -gt $Image.Length -or $size -gt ($Image.Length - $off)) {
        Fail "The Authenticode certificate table points outside the installer. Nothing was extracted and nothing was installed."
    }
    return [int]$off
}

function Test-MagicAt([byte[]]$Image, [int]$At) {
    if ($At -lt 0 -or ($At + 8) -gt $Image.Length) { return $false }
    for ($i = 0; $i -lt 8; $i++) {
        if ($Image[$At + $i] -ne $Magic[$i]) { return $false }
    }
    return $true
}

function Get-PayloadBytes([byte[]]$Image) {
    $cert = Get-CertificateOffset $Image
    $end = $Image.Length
    if ($cert -ge 0) { $end = $cert }
    if ($end -lt 16) {
        Fail "The installer is too small to carry a payload. Nothing was extracted and nothing was installed."
    }
    $magicAt = -1
    for ($pad = 0; $pad -le 7; $pad++) {
        $at = $end - $pad - 8
        if ($at -lt 8) { break }
        if (Test-MagicAt $Image $at) {
            $magicAt = $at
            break
        }
    }
    if ($magicAt -lt 0) {
        Fail "This installer has no Arcade payload trailer (ARCDPAY1). Nothing was extracted and nothing was installed."
    }
    $lenAt = $magicAt - 8
    $payloadLen = [int64](Read-U64 $Image $lenAt)
    if ($payloadLen -le 0 -or $payloadLen -gt $lenAt) {
        Fail "The installer payload length is not valid. Nothing was extracted and nothing was installed."
    }
    $start = [int]($lenAt - $payloadLen)
    $out = New-Object byte[] $payloadLen
    [System.Buffer]::BlockCopy($Image, $start, $out, 0, [int]$payloadLen)
    return ,$out
}

if (-not (Test-Path -LiteralPath $Installer)) {
    Fail "The installer file was not found. Nothing was extracted and nothing was installed."
}

$expected = $Expected.Trim().ToLower()
if ($expected -notmatch '^[0-9a-f]{64}$') {
    Fail "The embedded SHA-256 is not 64 hex characters. Nothing was extracted and nothing was installed."
}

$image = [System.IO.File]::ReadAllBytes($Installer)
$payload = Get-PayloadBytes $image
$sha = [System.Security.Cryptography.SHA256]::Create()
try {
    $hashBytes = $sha.ComputeHash($payload)
} finally {
    $sha.Dispose()
}
$actual = -join ($hashBytes | ForEach-Object { $_.ToString('x2') })

# The payload has been hashed in memory. It is not written, and Expand-Archive
# does not run, unless the hash is the one embedded in this installer.
if ($actual -ne $expected) {
    Fail "Arcade payload SHA-256 does not match the hash embedded in this installer. Expected $expected, got $actual. Nothing was extracted and nothing was installed."
}

$zip = Join-Path ([System.IO.Path]::GetTempPath()) ("arcade-payload-" + [guid]::NewGuid().ToString('n') + ".zip")
try {
    [System.IO.File]::WriteAllBytes($zip, $payload)
    if (Test-Path -LiteralPath $DestDir) {
        Remove-Item -LiteralPath $DestDir -Recurse -Force
    }
    Expand-Archive -LiteralPath $zip -DestinationPath $DestDir -Force
} finally {
    Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue
}

if (-not (Test-Path -LiteralPath $DestDir)) {
    Fail "The payload matched its SHA-256 but did not unpack. Nothing was installed."
}

Write-Host "Payload matches the embedded SHA-256. Unpacked to $DestDir"
exit 0
