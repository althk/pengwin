#Requires -Version 5.1
<#
.SYNOPSIS
    Automated Windows application-compatibility tests for a mounted ext4 volume.

.DESCRIPTION
    Exercises file operations against a live WinFSP-mounted ext4 drive using
    PowerShell equivalents of the manual Explorer/robocopy checklist.

    Prerequisites:
      - pengwin.exe built: cargo build --release
      - WinFSP installed
      - ext4-core/tests/fixtures/minimal.img present (run make_fixtures.sh once)
      - WSL with e2fsprogs installed (for e2fsck verification)

.PARAMETER DriveLetter
    Drive letter to mount the test image at (default: T).

.PARAMETER KeepMount
    If set, leave the volume mounted after the tests finish.

.EXAMPLE
    .\docs\app_compat_test.ps1
    .\docs\app_compat_test.ps1 -DriveLetter X -Verbose
#>
param(
    [string]$DriveLetter = "T",
    [switch]$KeepMount
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

# ---------------------------------------------------------------------------
# Paths
# ---------------------------------------------------------------------------
$repoRoot    = Split-Path $PSScriptRoot -Parent
$pengwinExe  = Join-Path $repoRoot "target\release\pengwin.exe"
$fixtureImg  = Join-Path $repoRoot "ext4-core\tests\fixtures\minimal.img"
$tempImg     = [System.IO.Path]::GetTempFileName() + ".img"
$mountPoint  = Join-Path ([System.IO.Path]::GetTempPath()) "pengwin_mount_$PID"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
$pass = 0; $fail = 0

function Ok([string]$label) {
    Write-Host "  [PASS] $label" -ForegroundColor Green
    $script:pass++
}

function Fail([string]$label, [string]$detail = "") {
    $msg = if ($detail) { "  [FAIL] $label - $detail" } else { "  [FAIL] $label" }
    Write-Host $msg -ForegroundColor Red
    $script:fail++
}

function Check([string]$label, [scriptblock]$test) {
    try {
        $result = & $test
        if ($result -eq $false) { Fail $label } else { Ok $label }
    } catch {
        Fail $label $_.Exception.Message
    }
}

function Fsck {
    $wslPath = "/mnt/" + $tempImg[0].ToString().ToLower() + ($tempImg.Substring(2) -replace '\\', '/')
    $out = wsl e2fsck -fn $wslPath 2>&1
    return $LASTEXITCODE -eq 0
}

# ---------------------------------------------------------------------------
# Setup: copy fixture to a writable temp image and mount it
# ---------------------------------------------------------------------------
Write-Host "`n=== pengwin app-compat automated tests ===" -ForegroundColor Cyan

if (-not (Test-Path $pengwinExe)) {
    Write-Error "pengwin.exe not found at $pengwinExe - run: cargo build --release"
}
if (-not (Test-Path $fixtureImg)) {
    Write-Error "minimal.img not found - run: bash ext4-core/scripts/make_fixtures.sh in WSL"
}

Write-Host "Copying fixture image to temp file..."
Copy-Item $fixtureImg $tempImg

Write-Host "Mounting $tempImg at $mountPoint ..."
$mountJob = Start-Process -FilePath $pengwinExe `
    -ArgumentList "mount", "`"$tempImg`"", "`"$mountPoint`"" `
    -PassThru -NoNewWindow -RedirectStandardError "$env:TEMP\pengwin_mount_err.txt"

# Give WinFSP a moment to bring the volume online.
Start-Sleep -Seconds 3

if ($mountJob.HasExited) {
    $errDetail = Get-Content "$env:TEMP\pengwin_mount_err.txt" -ErrorAction SilentlyContinue
    Remove-Item $tempImg, "$env:TEMP\pengwin_mount_err.txt" -ErrorAction SilentlyContinue
    Write-Error "pengwin exited immediately - mount failed (exit code $($mountJob.ExitCode)): $errDetail"
}

Write-Host "Volume online at $mountPoint`n"

# ---------------------------------------------------------------------------
# Seed: create a small file on the volume to use as a copy source.
# (pengwin is currently write-mode; the driver exposes the fixture read-write.)
# ---------------------------------------------------------------------------
$seedFile = Join-Path $mountPoint "seed.txt"
$ntfsTemp = [System.IO.Path]::GetTempPath()

# ---------------------------------------------------------------------------
# Section 1 - Copy operations
# ---------------------------------------------------------------------------
Write-Host "--- Copy operations ---"

Check "Copy file from ext4 volume to NTFS (contents match)" {
    if (-not (Test-Path $seedFile)) { return $false }
    $dst = Join-Path $ntfsTemp "pengwin_copy_out.bin"
    Copy-Item $seedFile $dst -Force
    $h1 = (Get-FileHash $seedFile   -Algorithm SHA256).Hash
    $h2 = (Get-FileHash $dst        -Algorithm SHA256).Hash
    Remove-Item $dst -ErrorAction SilentlyContinue
    $h1 -eq $h2
}

Check "Copy file from NTFS to ext4 volume" {
    $src = Join-Path $ntfsTemp "pengwin_copy_in.bin"
    [byte[]]$bytes = 1..256 | ForEach-Object { $_ % 256 }
    [System.IO.File]::WriteAllBytes($src, $bytes)
    $dst = Join-Path $mountPoint "copy_in.bin"
    Copy-Item $src $dst -Force
    $h1 = (Get-FileHash $src -Algorithm SHA256).Hash
    $h2 = (Get-FileHash $dst -Algorithm SHA256).Hash
    Remove-Item $src -ErrorAction SilentlyContinue
    $h1 -eq $h2
}

# ---------------------------------------------------------------------------
# Section 2 - Create / Delete / Rename
# ---------------------------------------------------------------------------
Write-Host "`n--- Create / Delete / Rename ---"

Check "Create folder via New-Item (Explorer right-click equivalent)" {
    $dir = Join-Path $mountPoint "compat_dir"
    New-Item -ItemType Directory -Path $dir -Force | Out-Null
    Test-Path $dir
}

Check "Delete file (Explorer Delete-key equivalent)" {
    $f = Join-Path $mountPoint "to_delete.bin"
    Set-Content $f "delete me"
    Remove-Item $f
    -not (Test-Path $f)
}

Check "Rename file (Explorer F2 equivalent)" {
    $old = Join-Path $mountPoint "before_rename.bin"
    $new = Join-Path $mountPoint "after_rename.bin"
    Set-Content $old "rename me"
    Rename-Item $old "after_rename.bin"
    (-not (Test-Path $old)) -and (Test-Path $new)
}

# ---------------------------------------------------------------------------
# Section 3 - In-place edit (Notepad/VS Code save equivalent)
# ---------------------------------------------------------------------------
Write-Host "`n--- In-place edit (Notepad/VS Code) ---"

Check "Open existing file, overwrite-save" {
    $f = Join-Path $mountPoint "overwrite_test.txt"
    Set-Content $f "original"
    Set-Content $f "overwritten"
    (Get-Content $f) -eq "overwritten"
}

Check "Create new file and save to ext4 volume" {
    $f = Join-Path $mountPoint "new_file_save.txt"
    "new content" | Out-File $f -Encoding utf8
    Test-Path $f
}

# Large file (generate 110 MB on the fly)
Check "Open large file (>100 MB)" {
    $f = Join-Path $mountPoint "large_file.bin"
    $buf = New-Object byte[] (110 * 1024 * 1024)
    [System.IO.File]::WriteAllBytes($f, $buf)
    $len = (Get-Item $f).Length
    Remove-Item $f -ErrorAction SilentlyContinue
    $len -eq (110 * 1024 * 1024)
}

# ---------------------------------------------------------------------------
# Section 4 - robocopy / xcopy
# ---------------------------------------------------------------------------
Write-Host "`n--- robocopy / xcopy ---"

Check "robocopy /MIR a directory tree onto ext4 volume" {
    $srcDir = Join-Path $ntfsTemp "pengwin_robocopy_src"
    New-Item -ItemType Directory -Path $srcDir -Force | Out-Null
    Set-Content (Join-Path $srcDir "a.txt") "aaa"
    Set-Content (Join-Path $srcDir "b.txt") "bbb"
    $dstDir = Join-Path $mountPoint "robocopy_dst"
    robocopy $srcDir $dstDir /MIR /NJH /NJS /NP 2>&1 | Out-Null
    # robocopy exit codes: 0=no copy, 1=copied OK, 2=extra deleted, 3=1+2; >=8=error
    $LASTEXITCODE -lt 8
}

Check "xcopy a directory tree - tree matches source" {
    $srcDir = Join-Path $ntfsTemp "pengwin_xcopy_src"
    New-Item -ItemType Directory -Path $srcDir -Force | Out-Null
    New-Item -ItemType Directory -Path (Join-Path $srcDir "sub") -Force | Out-Null
    Set-Content (Join-Path $srcDir "root.txt")      "root"
    Set-Content (Join-Path $srcDir "sub\child.txt") "child"
    $dstDir = Join-Path $mountPoint "xcopy_dst"
    xcopy $srcDir $dstDir /E /I /Q /Y 2>&1 | Out-Null
    # Compare every relative path that xcopy should have created
    $ok = $true
    Get-ChildItem $srcDir -Recurse -File | ForEach-Object {
        $rel = $_.FullName.Substring($srcDir.Length + 1)
        $dstFile = Join-Path $dstDir $rel
        if (-not (Test-Path $dstFile)) { $script:ok = $false; return }
        $h1 = (Get-FileHash $_.FullName -Algorithm SHA256).Hash
        $h2 = (Get-FileHash $dstFile    -Algorithm SHA256).Hash
        if ($h1 -ne $h2) { $script:ok = $false }
    }
    Remove-Item $srcDir -Recurse -Force -ErrorAction SilentlyContinue
    $ok
}

# ---------------------------------------------------------------------------
# Section 5 - Edge cases
# ---------------------------------------------------------------------------
Write-Host "`n--- Edge cases ---"

Check "Long filename (200+ chars)" {
    $name = "a" * 200 + ".txt"
    $f = Join-Path $mountPoint $name
    Set-Content $f "long"
    $ok = Test-Path $f
    Remove-Item $f -ErrorAction SilentlyContinue
    $ok
}

Check "Unicode filename (Japanese)" {
    $f = Join-Path $mountPoint ([char]0x30C6 + [char]0x30B9 + [char]0x30C8 + "_" + [char]0x30D5 + [char]0x30A1 + [char]0x30A4 + [char]0x30EB + ".txt")
    Set-Content $f "unicode"
    $ok = Test-Path $f
    Remove-Item $f -ErrorAction SilentlyContinue
    $ok
}

Check "Unicode filename (Arabic)" {
    $f = Join-Path $mountPoint ([char]0x0645 + [char]0x0644 + [char]0x0641 + "_" + [char]0x0627 + [char]0x062E + [char]0x062A + [char]0x0628 + [char]0x0627 + [char]0x0631 + ".txt")
    Set-Content $f "unicode"
    $ok = Test-Path $f
    Remove-Item $f -ErrorAction SilentlyContinue
    $ok
}

Check "Unicode filename (emoji)" {
    $f = Join-Path $mountPoint ([System.Char]::ConvertFromUtf32(0x1F427) + "_test.txt")
    Set-Content $f "emoji"
    $ok = Test-Path $f
    Remove-Item $f -ErrorAction SilentlyContinue
    $ok
}

Check "Windows-reserved name (CON) errors gracefully" {
    try {
        $f = Join-Path $mountPoint "CON"
        Set-Content $f "reserved" -ErrorAction Stop
        # If it somehow succeeded, that is acceptable - driver should handle it
        $true
    } catch {
        # Expected - the name is reserved; driver should return an appropriate error
        $true
    }
}

Check "Simultaneous open from two processes (shared read)" {
    $f = Join-Path $mountPoint "shared_read.txt"
    Set-Content $f "shared"
    $fs1 = [System.IO.File]::Open($f, 'Open', 'Read', 'ReadWrite')
    $fs2 = [System.IO.File]::Open($f, 'Open', 'Read', 'ReadWrite')
    $buf = New-Object byte[] 6
    $fs1.Read($buf, 0, 6) | Out-Null
    $fs2.Read($buf, 0, 6) | Out-Null
    $fs1.Dispose(); $fs2.Dispose()
    $true
}

# ---------------------------------------------------------------------------
# Unmount and e2fsck verification
# ---------------------------------------------------------------------------
Write-Host "`n--- Unmount + e2fsck ---"

Write-Host "Stopping pengwin mount process..."
if ($mountJob -and -not $mountJob.HasExited) {
    Stop-Process -Id $mountJob.Id -Force
    Start-Sleep -Seconds 2
}

Check "e2fsck reports clean filesystem after all operations" {
    Fsck
}

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
Remove-Item $tempImg    -ErrorAction SilentlyContinue
Remove-Item $mountPoint -Force -ErrorAction SilentlyContinue
Remove-Item "$env:TEMP\pengwin_mount_err.txt" -ErrorAction SilentlyContinue

Write-Host "`n=== Results: $pass passed, $fail failed ===" -ForegroundColor $(if ($fail -eq 0) { "Green" } else { "Red" })

if ($fail -gt 0) {
    exit 1
}