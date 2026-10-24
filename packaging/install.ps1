<#
.SYNOPSIS
  OmpDeck installer for Windows (per-user, no admin rights).

.DESCRIPTION
  Downloads the latest (or -Version) OmpDeck release zip for this machine's architecture, verifies its sha256
  against the published .sha256 asset, installs it to %LOCALAPPDATA%\Programs\OmpDeck, creates a Start Menu
  shortcut (carrying OmpDeck's AppUserModelID, which desktop notifications need) and installs omp (the coding-agent TUI OmpDeck wraps) when it is not found.

  Install:   irm https://raw.githubusercontent.com/lavinhoque33/omp-deck/main/packaging/install.ps1 | iex
  Options:   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/lavinhoque33/omp-deck/main/packaging/install.ps1))) -Version 0.9.0 -NoOmp
  Uninstall: & ([scriptblock]::Create((irm https://raw.githubusercontent.com/lavinhoque33/omp-deck/main/packaging/install.ps1))) -Uninstall

.PARAMETER Version
  Install this release (x.y.z, with or without a leading v) instead of the latest.

.PARAMETER ZipPath
  Install from this local release zip instead of downloading. It is verified against <zip>.sha256 next to it
  when that file exists (otherwise a warning says it is unverified). -Version is ignored.

.PARAMETER NoOmp
  Do not install omp when it is missing.

.PARAMETER Uninstall
  Remove the installed files and the Start Menu shortcut. Never touches %USERPROFILE%\.omp or OmpDeck's
  settings/data.
#>
[CmdletBinding()]
param(
    [string]$Version = '',
    [switch]$NoOmp,
    [switch]$Uninstall,
    [string]$ZipPath = ''
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'   # Invoke-WebRequest's progress bar makes downloads very slow on 5.1
try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12 } catch { }

$Repo = 'lavinhoque33/omp-deck'
$InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\OmpDeck'
$Shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'OmpDeck.lnk'
$OmpInstallUrl = 'https://omp.sh/install.ps1'
# AppUserModelID of OmpDeck: desktop notifications (toasts) need the Start Menu shortcut to carry it, and the app
# sets the same id on its process (crates/omp-deck/src/notify.rs).
$AppUserModelId = 'io.github.lavinhoque33.OmpDeck'

function Say([string]$m) { Write-Host $m }
function Step([string]$m) { Write-Host "==> $m" -ForegroundColor Cyan }
function Warn([string]$m) { Write-Warning $m }

# Stamps System.AppUserModel.ID (PKEY_AppUserModel_ID {9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3},5) on a .lnk through
# IPersistFile + IPropertyStore; WScript.Shell shortcuts cannot carry property-store values.
function Set-ShortcutAppUserModelId([string]$path, [string]$appId) {
    if (-not ('OmpDeckShortcutAumid' -as [type])) {
        Add-Type -Language CSharp -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;

public static class OmpDeckShortcutAumid
{
    [StructLayout(LayoutKind.Sequential, Pack = 4)]
    struct PropertyKey { public Guid fmtid; public uint pid; }

    [StructLayout(LayoutKind.Sequential)]
    struct PropVariant { public ushort vt; public ushort r1; public ushort r2; public ushort r3; public IntPtr p; public IntPtr p2; }

    [ComImport, Guid("886D8EEB-8CF2-4446-8D02-CDBA1DBDCF99"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    interface IPropertyStore
    {
        [PreserveSig] int GetCount(out uint count);
        [PreserveSig] int GetAt(uint index, out PropertyKey key);
        [PreserveSig] int GetValue(ref PropertyKey key, out PropVariant value);
        [PreserveSig] int SetValue(ref PropertyKey key, ref PropVariant value);
        [PreserveSig] int Commit();
    }

    [ComImport, Guid("00021401-0000-0000-C000-000000000046")]
    class CShellLink { }

    [DllImport("propsys.dll", CharSet = CharSet.Unicode)]
    static extern int InitPropVariantFromString(string value, out PropVariant variant);

    [DllImport("ole32.dll")]
    static extern int PropVariantClear(ref PropVariant variant);

    static void Check(int hr, string what)
    {
        if (hr < 0) throw new COMException(what + " failed", hr);
    }

    public static void Set(string path, string appId)
    {
        object link = new CShellLink();
        try
        {
            ((IPersistFile)link).Load(path, 0);
            var store = (IPropertyStore)link;
            var key = new PropertyKey { fmtid = new Guid("9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3"), pid = 5 };
            PropVariant value;
            Check(InitPropVariantFromString(appId, out value), "InitPropVariantFromString");
            try
            {
                Check(store.SetValue(ref key, ref value), "IPropertyStore.SetValue");
                Check(store.Commit(), "IPropertyStore.Commit");
            }
            finally { PropVariantClear(ref value); }
            ((IPersistFile)link).Save(path, true);
        }
        finally { Marshal.FinalReleaseComObject(link); }
    }
}
'@
    }
    [OmpDeckShortcutAumid]::Set($path, $appId)
}

# Renames a possibly locked file aside (running exes can be renamed, not overwritten or deleted).
function Move-Aside([string]$path, [string]$stamp) {
    if (-not (Test-Path -LiteralPath $path)) { return }
    $aside = "$path.old-$stamp"
    Move-Item -LiteralPath $path -Destination $aside -Force
}

# Best effort: leftovers of earlier updates are removable once the old processes exited.
function Remove-Leftovers([string]$dir) {
    if (-not (Test-Path -LiteralPath $dir)) { return }
    Get-ChildItem -LiteralPath $dir -Filter '*.old-*' -File -ErrorAction SilentlyContinue | ForEach-Object {
        try { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction Stop } catch { }
    }
}

if ($Uninstall) {
    Step 'Uninstalling OmpDeck'
    # Stop running instances so their exes can go; host daemons keep omp sessions alive, so tell the user.
    $running = @(Get-Process -ErrorAction SilentlyContinue | Where-Object {
            try { $_.Path -and $_.Path.StartsWith($InstallDir, [StringComparison]::OrdinalIgnoreCase) } catch { $false }
        })
    if ($running.Count -gt 0) {
        Warn ("{0} OmpDeck process(es) are running (including session hosts that keep omp sessions alive); they are left running and their files are renamed aside." -f $running.Count)
    }
    if (Test-Path -LiteralPath $Shortcut) {
        Remove-Item -LiteralPath $Shortcut -Force
        Say "Removed $Shortcut"
    }
    if (Test-Path -LiteralPath $InstallDir) {
        try {
            Remove-Item -LiteralPath $InstallDir -Recurse -Force -ErrorAction Stop
        }
        catch {
            # Locked exes: rename aside so the directory content is gone from the user's view; delete what we can.
            $stamp = Get-Date -Format 'yyyyMMddHHmmss'
            Get-ChildItem -LiteralPath $InstallDir -File -Recurse | ForEach-Object {
                try { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction Stop }
                catch { try { Move-Aside $_.FullName $stamp } catch { } }
            }
            Warn "Some files are in use and were renamed to *.old-$stamp in $InstallDir; delete the folder after closing OmpDeck."
        }
        if (-not (Test-Path -LiteralPath $InstallDir)) { Say "Removed $InstallDir" }
    }
    Say 'OmpDeck uninstalled. Your omp data (%USERPROFILE%\.omp) and OmpDeck settings were left in place.'
    return
}

# --- architecture -------------------------------------------------------------------------------------------
$rawArch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
switch ($rawArch.ToUpperInvariant()) {
    'AMD64' { $assetArch = 'x86_64' }
    'ARM64' { $assetArch = 'aarch64' }
    default { throw "Unsupported Windows architecture: $rawArch (release builds exist for x64 and arm64)" }
}

# --- version ------------------------------------------------------------------------------------------------
$headers = @{ 'User-Agent' = 'omp-deck-installer'; 'Accept' = 'application/vnd.github+json' }
$localZip = [bool]$ZipPath
if ($localZip) {
    if ($Version) { Say "Note: -Version is ignored because -ZipPath was given." }
    if (-not (Test-Path -LiteralPath $ZipPath -PathType Leaf)) { throw "ZipPath not found: $ZipPath" }
    $ZipPath = (Resolve-Path -LiteralPath $ZipPath).ProviderPath
    if ((Split-Path -Leaf $ZipPath) -match '^omp-deck-(.+?)-(x86_64|aarch64)-windows\.zip$') {
        $Version = $Matches[1]
        if ($Matches[2] -ne $assetArch) {
            throw "$ZipPath is a $($Matches[2]) build but this machine needs $assetArch."
        }
    }
    else {
        $Version = 'local'
    }
}
elseif ($Version) {
    $Version = $Version.TrimStart('v', 'V')
}
else {
    Step 'Looking up the latest release'
    try {
        $rel = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers $headers
    }
    catch {
        throw "Could not determine the latest release (https://github.com/$Repo/releases): $($_.Exception.Message)"
    }
    $Version = ([string]$rel.tag_name).TrimStart('v', 'V')
    if (-not $Version) { throw "Could not determine the latest release (https://github.com/$Repo/releases)" }
}

$name = "omp-deck-$Version-$assetArch-windows"
$base = "https://github.com/$Repo/releases/download/v$Version"
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("omp-deck-install-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

try {
    # --- download + verify ----------------------------------------------------------------------------------
    if ($localZip) {
        $zip = $ZipPath
        $sumFile = "$ZipPath.sha256"
        if (Test-Path -LiteralPath $sumFile -PathType Leaf) {
            Step 'Verifying sha256'
            $sumText = Get-Content -Raw -LiteralPath $sumFile
        }
        else {
            Warn "No $sumFile next to the zip; installing UNVERIFIED."
            $sumText = $null
        }
    }
    else {
        $zip = Join-Path $tmp "$name.zip"
        Step "Downloading $name.zip"
        try { Invoke-WebRequest -UseBasicParsing -Uri "$base/$name.zip" -OutFile $zip -Headers @{ 'User-Agent' = 'omp-deck-installer' } }
        catch { throw "Download failed: $base/$name.zip ($($_.Exception.Message)). Does release v$Version have a $assetArch Windows build?" }

        Step 'Verifying sha256'
        try { $sumText = (Invoke-WebRequest -UseBasicParsing -Uri "$base/$name.zip.sha256" -Headers @{ 'User-Agent' = 'omp-deck-installer' }).Content }
        catch { throw "Could not fetch the checksum file $base/$name.zip.sha256; refusing to install an unverified download." }
        if ($sumText -is [byte[]]) { $sumText = [Text.Encoding]::UTF8.GetString($sumText) }
    }
    if ($null -ne $sumText) {
        $expected = ([string]$sumText).Trim().Split([char[]]" `t`r`n", [StringSplitOptions]::RemoveEmptyEntries)[0].ToLowerInvariant()
        $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $zip).Hash.ToLowerInvariant()
        if ($expected -notmatch '^[0-9a-f]{64}$') { throw "Malformed checksum file for $zip" }
        if ($actual -ne $expected) { throw "Checksum mismatch for $zip (expected $expected, got $actual)" }
        Say "sha256 OK: $actual"
    }

    # --- install --------------------------------------------------------------------------------------------
    Step "Installing to $InstallDir"
    $extract = Join-Path $tmp 'x'
    Expand-Archive -LiteralPath $zip -DestinationPath $extract -Force
    $payload = $extract
    if (-not (Test-Path -LiteralPath (Join-Path $payload 'omp-deck.exe'))) {
        $sub = Get-ChildItem -LiteralPath $extract -Directory | Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'omp-deck.exe') } | Select-Object -First 1
        if (-not $sub) { throw 'Unexpected archive layout: omp-deck.exe not found' }
        $payload = $sub.FullName
    }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    $stamp = Get-Date -Format 'yyyyMMddHHmmss'
    foreach ($item in Get-ChildItem -LiteralPath $payload -File) {
        $dest = Join-Path $InstallDir $item.Name
        # Running exes/dlls (the GUI and session hosts) cannot be overwritten, but can be renamed.
        if ($item.Extension -in '.exe', '.dll') { Move-Aside $dest $stamp }
        Copy-Item -LiteralPath $item.FullName -Destination $dest -Force
        Say "  $($item.Name)"
    }
    Remove-Leftovers $InstallDir

    # --- Start Menu shortcut --------------------------------------------------------------------------------
    $exe = Join-Path $InstallDir 'omp-deck.exe'
    try {
        $ws = New-Object -ComObject WScript.Shell
        $lnk = $ws.CreateShortcut($Shortcut)
        $lnk.TargetPath = $exe
        $lnk.WorkingDirectory = $InstallDir
        $lnk.IconLocation = "$exe,0"
        $lnk.Description = 'OmpDeck - desktop wrapper for the omp coding agent'
        $lnk.Save()
        Say "Start Menu shortcut: $Shortcut"
        try { Set-ShortcutAppUserModelId $Shortcut $AppUserModelId }
        catch { Warn "Could not set the shortcut's AppUserModelID (desktop notifications may show PowerShell as their source): $($_.Exception.Message)" }
    }
    catch {
        Warn "Could not create the Start Menu shortcut: $($_.Exception.Message)"
    }

    Say ''
    Say "OmpDeck $Version installed to $InstallDir"

    # --- omp --------------------------------------------------------------------------------------------------
    $ompFound = [bool](Get-Command omp -ErrorAction SilentlyContinue) -or
    (Test-Path -LiteralPath (Join-Path $env:LOCALAPPDATA 'omp\omp.exe')) -or
    (Test-Path -LiteralPath (Join-Path $env:USERPROFILE '.bun\bin\omp.exe'))
    if ($ompFound) {
        Say 'omp found.'
    }
    elseif ($NoOmp) {
        Warn "'omp' was not found; OmpDeck needs it. Run: irm $OmpInstallUrl | iex   (or use OmpDeck's 'Install omp' button)"
    }
    else {
        Step "omp was not found; installing it with its official installer ($OmpInstallUrl)"
        try {
            & ([scriptblock]::Create((Invoke-RestMethod -Uri $OmpInstallUrl))) -Binary
        }
        catch {
            Warn "Installing omp failed ($($_.Exception.Message)); OmpDeck offers an 'Install omp' button to retry."
        }
    }
    Say "Launch OmpDeck from the Start Menu or run: $exe"
}
finally {
    Remove-Item -Recurse -Force -LiteralPath $tmp -ErrorAction SilentlyContinue
}
