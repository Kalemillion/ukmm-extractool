# process_msg_mod.ps1
# =====================
# Extract, backup, and convert Msg SARC files from UKMM mods.
#
# Modes:
#   1. Interactive :  .\process_msg_mod.ps1
#       Asks Wii U or Switch, scans the corresponding UKMM mods folder,
#       lets you choose one, extracts it, converts Msg_*.product.sarc files,
#       backups, and cleans up.
#
#   2. Folder mode :  .\process_msg_mod.ps1 -Source "path\to\folder" -ModName "Foo"
#       Takes an already-extracted mod folder, converts all .sarc files,
#       creates backup zip at mods/{platform}/Foo/Foo_backup.zip, deletes source folder.
#
#   3. Quick mode  :  .\process_msg_mod.ps1 -ModsDir "C:\path\to\mods" -ModName "Foo"
#       Finds "Foo" (by name or filename) in the given ModsDir, extracts it,
#       then processes as in folder mode.
#

#   4. Reverse-only :  .\process_msg_mod.ps1 -ReverseOnly -ModName "Foo"
#       Skips extraction/backup. Only rebuilds _modified.zip from existing
#       JSON files and _backup.zip. Use after editing JSON files.
#
# Output structure:
#   mods/{platform}/{ModName}/Msg_{lang}.product.json
#   mods/{platform}/{ModName}/{ModName}_backup.zip
#   mods/{platform}/{ModName}/{ModName}_modified.zip  (reverse-converted SARC files injected)

param(
    [Parameter(Mandatory = $false)]
    [string]$Source = "",
    [Parameter(Mandatory = $false)]
    [string]$ModName = "",
    [Parameter(Mandatory = $false)]
    [string]$ModsDir = "",
    [Parameter(Mandatory = $false)]
    [string]$OutputDir = "",
    [Parameter(Mandatory = $false)]
    [switch]$IncludeSwitch = $false,
    [Parameter(Mandatory = $false)]
    [switch]$ReverseOnly = $false
)

$ErrorActionPreference = "Continue"
$scriptDir = if ($PSScriptRoot) { $PSScriptRoot } else { (Get-Location).Path }

# ═══════════════════════════════════════════════════════════════════════════════
#  HELPERS
# ═══════════════════════════════════════════════════════════════════════════════

function Find-ukmmsg2json {
    # Determine project root: walk up from script dir until Cargo.toml is found
    $projectRoot = $scriptDir
    while ($projectRoot -and -not (Test-Path (Join-Path $projectRoot "Cargo.toml"))) {
        $projectRoot = Split-Path -Parent $projectRoot
    }
    if (-not $projectRoot) { $projectRoot = $scriptDir }

    $candidates = @(
        Join-Path $scriptDir "ukmmsg2json.exe"
        Join-Path $scriptDir "target\release\ukmmsg2json.exe"
        Join-Path $projectRoot "target\release\ukmmsg2json.exe"
    )
    foreach ($c in $candidates) {
        if (Test-Path $c -PathType Leaf) { return (Resolve-Path $c).Path }
    }
    return $null
}

function New-ZipFromDirectory($sourceDir, $zipPath) {
    if (Test-Path $zipPath) { Remove-Item $zipPath -Force }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [System.IO.Compression.ZipFile]::CreateFromDirectory(
        (Resolve-Path $sourceDir).Path,
        $zipPath,
        [System.IO.Compression.CompressionLevel]::NoCompression,
        $false
    )
}

function Get-ModFriendlyName($path, $isDir) {
    if ($isDir) {
        $metaPath = Join-Path $path "meta.yml"
        if (Test-Path $metaPath -PathType Leaf) {
            $metaContent = Get-Content $metaPath -Raw -ErrorAction SilentlyContinue
            if ($metaContent -match '(?m)^name:\s*(.+)$') {
                return $matches[1].Trim() + "  [$([System.IO.Path]::GetFileName($path))]"
            }
        }
        return [System.IO.Path]::GetFileName($path)
    }
    else {
        try {
            $zipStream = [System.IO.Compression.ZipFile]::OpenRead($path)
            $metaEntry = $zipStream.GetEntry("meta.yml")
            if ($metaEntry) {
                $reader = New-Object System.IO.StreamReader($metaEntry.Open())
                $metaContent = $reader.ReadToEnd()
                $reader.Close()
                $zipStream.Dispose()
                if ($metaContent -match '(?m)^name:\s*(.+)$') {
                    return $matches[1].Trim() + "  [$([System.IO.Path]::GetFileNameWithoutExtension($path)).zip]"
                }
            }
            $zipStream.Dispose()
        }
        catch {}
        return [System.IO.Path]::GetFileNameWithoutExtension($path)
    }
}

# ═══════════════════════════════════════════════════════════════════════════════
#  0. CHECK ukmmsg2json
# ═══════════════════════════════════════════════════════════════════════════════

$ukmmsg2json = Find-ukmmsg2json
if (-not $ukmmsg2json) {
    Write-Host "ERROR: ukmmsg2json.exe not found. Build it first:" -ForegroundColor Red
    Write-Host "cargo build --release" -ForegroundColor Yellow
    exit 1
}

# ═══════════════════════════════════════════════════════════════════════════════
#  1. DETERMINE MODE
# ═══════════════════════════════════════════════════════════════════════════════

$mode = ""
if ($Source -and $ModName) {
    $mode = "folder"
}
elseif ($ModsDir -and $ModName) {
    $mode = "quick"
}
else {
    $mode = "interactive"
}

$extractDir = ""        # temp folder where mod is extracted
$modNameFinal = ""      # sanitized name used for output paths
$displayName = ""       # human-readable name for messages
$platformLabel = ""     # "wiiu" or "nx"
$originalSourcePath = "" # path to original source (zip file or directory)
$originalIsDir = $false  # true if source was a directory (loose mod)

if ($ReverseOnly) {
    # ── Mode ReverseOnly: rebuild _modified.zip from existing JSON + backup ZIP ──
    if (-not $ModName) {
        Write-Error "-ReverseOnly requires -ModName"
        exit 1
    }
    if (-not $OutputDir) { $OutputDir = $scriptDir }
    $OutputDir = [System.IO.Path]::GetFullPath($OutputDir)
    $platformLabel = if ($IncludeSwitch) { "nx" } else { "wiiu" }
    $modNameFinal = $ModName
    $modNameSafe = $modNameFinal -replace '[^\w\-\. ]', '_'
    $modsOutDir = Join-Path $OutputDir "mods" $platformLabel $modNameSafe

    Write-Host "`n-- ReverseOnly mode: rebuilding modified ZIP from existing files --" -ForegroundColor Magenta
    Write-Host "  Output: $modsOutDir" -ForegroundColor Gray

    $backupName = "$modNameSafe" + "_backup.zip"
    $backupPath = Join-Path $modsOutDir $backupName

    if (-not (Test-Path $modsOutDir)) {
        Write-Error "Output directory not found: $modsOutDir`nRun the script without -ReverseOnly first to generate JSON files."
        exit 1
    }
    if (-not (Test-Path $backupPath)) {
        Write-Error "Backup ZIP not found: $backupPath`nRun the script without -ReverseOnly first to generate the backup."
        exit 1
    }

    $jsonFiles = Get-ChildItem -Path $modsOutDir -Filter "*.json" -File
    if ($jsonFiles.Count -eq 0) {
        Write-Error "No JSON files found in $modsOutDir"
        exit 1
    }

    $displayName = $ModName
    # Skips mode detection / extraction — jump directly to shared output logic
}
else {

    if ($mode -eq "folder") {
        # ── Mode 2: Source folder already exists ──
        $extractDir = [System.IO.Path]::GetFullPath($Source)
        $modNameFinal = $ModName

        if (-not (Test-Path $extractDir -PathType Container)) {
            Write-Error "Source folder not found: $extractDir"
            exit 1
        }
        $originalSourcePath = $extractDir
        $originalIsDir = $true
        $displayName = (Split-Path $extractDir -Leaf)
        # Default to wiiu for folder mode, override with -IncludeSwitch
        $platformLabel = if ($IncludeSwitch) { "nx" } else { "wiiu" }

    }
    elseif ($mode -eq "quick") {
        # ── Mode 3: Quick find and extract from ModsDir ──
        $modsDir = [System.IO.Path]::GetFullPath($ModsDir)
        if (-not (Test-Path $modsDir -PathType Container)) {
            Write-Error "Mods directory not found: $modsDir"
            exit 1
        }

        # Detect platform from path
        if ($modsDir -match "ukmm\\nx\\mods") {
            $platformLabel = "nx"
        }
        elseif ($modsDir -match "ukmm\\wiiu\\mods") {
            $platformLabel = "wiiu"
        }
        else {
            $platformLabel = if ($IncludeSwitch) { "nx" } else { "wiiu" }
        }

        $modNameFinal = $ModName
        Write-Host "Searching for '$ModName' in $modsDir ..." -ForegroundColor Cyan

        # Try to find by exact name match in filename/dirname
        $found = $null
        $zipCandidates = Get-ChildItem -Path $modsDir -Filter "*.zip" -File | Where-Object { $_.BaseName -like "*$ModName*" }
        $dirCandidates = Get-ChildItem -Path $modsDir -Directory | Where-Object { $_.Name -like "*$ModName*" }

        if ($zipCandidates) {
            $found = @{ Path = $zipCandidates[0].FullName; IsDir = $false }
            Write-Host "  Found zip: $($zipCandidates[0].Name)" -ForegroundColor Green
        }
        elseif ($dirCandidates) {
            $found = @{ Path = $dirCandidates[0].FullName; IsDir = $true }
            Write-Host "  Found directory: $($dirCandidates[0].Name)" -ForegroundColor Green
        }
        else {
            Write-Error "No mod matching '$ModName' found in $modsDir"
            exit 1
        }

        $originalSourcePath = $found.Path
        $originalIsDir = $found.IsDir

        # Extract to temp dir
        $extractDir = Join-Path $scriptDir "._extract_temp_$modNameFinal"
        if (Test-Path $extractDir) { Remove-Item $extractDir -Recurse -Force }

        if ($found.IsDir) {
            Write-Host "Copying mod folder..." -ForegroundColor Yellow
            Copy-Item -Path $found.Path -Destination $extractDir -Recurse -Force
        }
        else {
            Write-Host "Copying and extracting zip..." -ForegroundColor Yellow
            try {
                [System.IO.Compression.ZipFile]::ExtractToDirectory($found.Path, $extractDir)
            }
            catch {
                Write-Error "Failed to extract: $_"
                exit 1
            }
        }

        $displayName = Get-ModFriendlyName $found.Path $found.IsDir

    }
    else {
        # ── Mode 1: Interactive ──
        # Ask which platform first
        Write-Host "`nUKMM Message Tool - Interactive Mode" -ForegroundColor Magenta
        Write-Host "=====================================`n" -ForegroundColor Magenta

        Write-Host "Choose your platform:" -ForegroundColor Cyan
        Write-Host "  [1] Wii U   (~\AppData\Local\ukmm\wiiu\mods)" -ForegroundColor White
        Write-Host "  [2] Switch  (~\AppData\Local\ukmm\nx\mods)" -ForegroundColor White
        Write-Host ""
        $platChoice = Read-Host "Select (1 or 2, default=1)"
        if (-not $platChoice) { $platChoice = "1" }

        if ($platChoice -eq "2") {
            $platformLabel = "nx"
            $modsDir = Join-Path $env:LOCALAPPDATA "ukmm\nx\mods"
            Write-Host "`nScanning Switch mods..." -ForegroundColor Cyan
        }
        else {
            $platformLabel = "wiiu"
            $modsDir = Join-Path $env:LOCALAPPDATA "ukmm\wiiu\mods"
            Write-Host "`nScanning Wii U mods..." -ForegroundColor Cyan
        }

        if (-not (Test-Path $modsDir -PathType Container)) {
            Write-Error "Directory not found: $modsDir"
            Write-Host "Make sure UKMM is installed and you have mods for this platform." -ForegroundColor Yellow
            exit 1
        }

        Write-Host "  $modsDir`n" -ForegroundColor DarkGray

        $zipMods = Get-ChildItem -Path $modsDir -Filter "*.zip" -File | Sort-Object Name
        $dirMods = Get-ChildItem -Path $modsDir -Directory | Sort-Object Name

        $modList = @()
        foreach ($zip in $zipMods) {
            $modList += @{
                DisplayName = Get-ModFriendlyName $zip.FullName $false
                Path        = $zip.FullName
                Type        = "ZIP"
                IsDir       = $false
            }
        }
        foreach ($dir in $dirMods) {
            $metaPath = Join-Path $dir.FullName "meta.yml"
            if (Test-Path $metaPath -PathType Leaf) {
                $modList += @{
                    DisplayName = Get-ModFriendlyName $dir.FullName $true
                    Path        = $dir.FullName
                    Type        = "Directory (loose)"
                    IsDir       = $true
                }
            }
        }

        if ($modList.Count -eq 0) {
            Write-Host "No mods found.`n" -ForegroundColor Yellow
            exit 0
        }

        Write-Host "Found $($modList.Count) mod(s):`n" -ForegroundColor Green
        for ($i = 0; $i -lt $modList.Count; $i++) {
            Write-Host "  [$($i+1)] $($modList[$i].DisplayName)" -ForegroundColor White
            Write-Host "       Type: $($modList[$i].Type)" -ForegroundColor DarkGray
        }

        Write-Host ""
        $selection = Read-Host "Select a mod to process (1-$($modList.Count)) or press Enter to cancel"
        if (-not $selection) { Write-Host "Cancelled.`n"; exit 0 }

        $index = if ([int]::TryParse($selection, [ref]0)) { [int]$selection } else { -1 }
        if ($index -lt 1 -or $index -gt $modList.Count) { Write-Host "Invalid selection.`n"; exit 1 }

        $chosen = $modList[$index - 1]
        $displayName = $chosen.DisplayName

        $originalSourcePath = $chosen.Path
        $originalIsDir = $chosen.IsDir

        # Derive mod name from selected mod
        $modNameFinal = if ($chosen.IsDir) {
            [System.IO.Path]::GetFileName($chosen.Path)
        }
        else {
            [System.IO.Path]::GetFileNameWithoutExtension($chosen.Path)
        }

        # Extract to temp dir
        $extractDir = Join-Path $scriptDir "._extract_temp_$modNameFinal"
        if (Test-Path $extractDir) { Remove-Item $extractDir -Recurse -Force }

        if ($chosen.IsDir) {
            Write-Host "Copying loose mod folder..." -ForegroundColor Yellow
            Copy-Item -Path $chosen.Path -Destination $extractDir -Recurse -Force
        }
        else {
            Write-Host "Extracting zip..." -ForegroundColor Yellow
            try {
                [System.IO.Compression.ZipFile]::ExtractToDirectory($chosen.Path, $extractDir)
            }
            catch {
                Write-Error "Failed to extract: $_"
                exit 1
            }
        }
    }
} # end of else (not ReverseOnly)

# ═══════════════════════════════════════════════════════════════════════════════
#  2. OUTPUT DIRECTORY
# ═══════════════════════════════════════════════════════════════════════════════

if (-not $ReverseOnly) {
    if (-not $OutputDir) { $OutputDir = $scriptDir }
    $OutputDir = [System.IO.Path]::GetFullPath($OutputDir)
}

$modNameSafe = $modNameFinal -replace '[^\w\-\. ]', '_'
$modsOutDir = Join-Path $OutputDir "mods" $platformLabel $modNameSafe
New-Item -ItemType Directory -Path $modsOutDir -Force | Out-Null

if (-not $ReverseOnly) {
    Write-Host "`n-- Processing ------------------------------------------------------" -ForegroundColor Magenta
    Write-Host "  Mod:    $displayName" -ForegroundColor White
    Write-Host "  Temp:   $extractDir" -ForegroundColor DarkGray
    Write-Host "  Output: $modsOutDir" -ForegroundColor Gray

    # ═══════════════════════════════════════════════════════════════════════════════
    #  3. FIND AND CONVERT Msg SARC FILES
    # ═══════════════════════════════════════════════════════════════════════════════

    Write-Host "`n-- Converting Msg SARC files to JSON ------------------------------" -ForegroundColor Cyan

    $msgFiles = Get-ChildItem -Path $extractDir -Recurse -File | Where-Object {
        $_.Name -like "Msg_*.product.s*rc"
    }

    $jsonCount = 0
    foreach ($msgFile in $msgFiles) {
        $jsonCount++
        $relPath = [System.IO.Path]::GetRelativePath($extractDir, $msgFile.FullName)
        Write-Host "  [$jsonCount/$($msgFiles.Count)] $relPath"

        & $ukmmsg2json --mod-dir "$platformLabel/$modNameSafe" $msgFile.FullName 2>&1 | Out-Null
        if ($LASTEXITCODE -ne 0) {
            Write-Warning "    Failed: $($msgFile.Name)"
        }
    }

    Write-Host "  -> $jsonCount files extracted to mods/$platformLabel/$modNameSafe/" -ForegroundColor Green

    # ═══════════════════════════════════════════════════════════════════════════════
    #  4. CREATE BACKUP ZIP
    # ═══════════════════════════════════════════════════════════════════════════════

    Write-Host "`n-- Creating backup zip ---------------------------------------------" -ForegroundColor Cyan

    $backupName = "$modNameSafe" + "_backup.zip"
    $backupPath = Join-Path $modsOutDir $backupName
    Write-Host "  Saving: $backupPath" -ForegroundColor Yellow

    if (-not $originalIsDir -and $originalSourcePath -and (Test-Path $originalSourcePath -PathType Leaf)) {
        # Original was a .zip — copy it byte-for-byte (UKMM uses Stored compression)
        Copy-Item -Path $originalSourcePath -Destination $backupPath -Force
        Write-Host "  Copied original ZIP (byte-for-byte)" -ForegroundColor Green
    }
    else {
        # Source was a directory — re-zip with NoCompression to match UKMM format
        New-ZipFromDirectory $extractDir $backupPath
    }
    Write-Host "  Done: $backupName" -ForegroundColor Green
}

# ═══════════════════════════════════════════════════════════════════════════════
#  5. REVERSE CONVERT JSON → SARC AND BUILD MODIFIED ZIP
# ═══════════════════════════════════════════════════════════════════════════════
# Only runs in -ReverseOnly mode (rebuild from existing JSON without re-extracting)

$modifiedName = "$modNameSafe" + "_modified.zip"
$modifiedPath = Join-Path $modsOutDir $modifiedName

if ($ReverseOnly) {
    $jsonFiles = Get-ChildItem -Path $modsOutDir -Filter "*.json" -File
    Write-Host "`n-- Building modified ZIP with reverse-converted SARC files --------" -ForegroundColor Cyan
    Write-Host "  Creating: $modifiedPath" -ForegroundColor Yellow

    # Step 5a: reverse-convert each JSON → .sarc
    $convertedSarcs = @{}
    $reverseCount = 0
    foreach ($jsonFile in $jsonFiles) {
        $reverseCount++
        $sarcName = $jsonFile.BaseName + ".sarc"
        $sarcTempPath = Join-Path $modsOutDir $sarcName
        Write-Host "  [$reverseCount/$($jsonFiles.Count)] $($jsonFile.Name) → $sarcName"
        & $ukmmsg2json $jsonFile.FullName -r -o $sarcTempPath 2>&1 | Out-Null
        if ($LASTEXITCODE -eq 0 -and (Test-Path $sarcTempPath)) {
            $convertedSarcs[$sarcName] = $sarcTempPath
        }
        else {
            Write-Warning "    Failed to convert $($jsonFile.Name)"
        }
    }

    # Step 5b: copy backup ZIP, replace SARC entries with modified versions
    if ($convertedSarcs.Count -gt 0) {
        Copy-Item -Path $backupPath -Destination $modifiedPath -Force
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $modifiedZip = [System.IO.Compression.ZipFile]::Open($modifiedPath, [System.IO.Compression.ZipArchiveMode]::Update)
        try {
            foreach ($sarcName in $convertedSarcs.Keys) {
                $entryName = "Message/$sarcName"
                $oldEntry = $modifiedZip.GetEntry($entryName)
                if ($oldEntry) { $oldEntry.Delete() }
                $sarcBytes = [System.IO.File]::ReadAllBytes($convertedSarcs[$sarcName])
                $newEntry = $modifiedZip.CreateEntry($entryName, [System.IO.Compression.CompressionLevel]::NoCompression)
                $stream = $newEntry.Open()
                $stream.Write($sarcBytes, 0, $sarcBytes.Length)
                $stream.Close()
                Write-Host "  Replaced: $entryName" -ForegroundColor Yellow
            }
            Write-Host "  Modified ZIP: $modifiedName" -ForegroundColor Green
        }
        finally {
            $modifiedZip.Dispose()
        }

        # Clean up temp SARC files
        foreach ($sarcPath in $convertedSarcs.Values) {
            Remove-Item $sarcPath -Force -ErrorAction SilentlyContinue
        }
    }
}

# ═══════════════════════════════════════════════════════════════════════════════
#  6. DELETE TEMP EXTRACTED FOLDER
# ═══════════════════════════════════════════════════════════════════════════════

if (-not $ReverseOnly) {
    Write-Host "`n-- Cleaning up ----------------------------------------------------" -ForegroundColor Cyan
    Write-Host "  Deleting temp folder: $extractDir"
    if (Test-Path $extractDir) {
        Remove-Item $extractDir -Recurse -Force
        Write-Host "  Done!" -ForegroundColor Green
    }
}

# ═══════════════════════════════════════════════════════════════════════════════
#  7. SUMMARY
# ═══════════════════════════════════════════════════════════════════════════════

Write-Host "`n-- Summary ---------------------------------------------------------" -ForegroundColor Cyan
Write-Host "  Platform:       $platformLabel" -ForegroundColor White
Write-Host "  Mod:            $displayName" -ForegroundColor White
if (-not $ReverseOnly) {
    Write-Host "  JSON files:     $jsonCount" -ForegroundColor White
}
Write-Host "  Output folder:  $modsOutDir" -ForegroundColor Green
if (-not $ReverseOnly) {
    Write-Host "  Backup zip:     $backupName" -ForegroundColor Green
}
if (Test-Path $modifiedPath) {
    Write-Host "  Modified zip:   $modifiedName" -ForegroundColor Green
}
Write-Host "`nDone!`n" -ForegroundColor Cyan
