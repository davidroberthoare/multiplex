#!/usr/bin/env pwsh
<#
.SYNOPSIS
  Bundles multiplex binaries with the GStreamer runtime into a portable .zip
  for Windows. Run after `cargo build --release`.
.PARAMETER Version
  The release version tag (e.g. "v0.1.0").
.PARAMETER GstVersion
  GStreamer runtime version to bundle (e.g. "1.24.11").
#>
param(
  [Parameter(Mandatory)] [string] $Version,
  [Parameter(Mandatory)] [string] $GstVersion
)

$ErrorActionPreference = "Stop"

$ArchiveName  = "multiplex-${Version}-x86_64-windows"
$StagingDir   = "dist/${ArchiveName}"
$GstUrlBase   = "https://gstreamer.freedesktop.org/data/pkg/windows/${GstVersion}/msvc"
$RuntimeMsi   = "gstreamer-1.0-msvc-x86_64-${GstVersion}.msi"
$GstExtract   = "dist/gst-runtime"

# ── 1. Download GStreamer runtime MSI ──
Write-Host "Downloading GStreamer runtime MSI ..."
if (-not (Test-Path $RuntimeMsi)) {
  Invoke-WebRequest -Uri "${GstUrlBase}/${RuntimeMsi}" -OutFile $RuntimeMsi
}

# ── 2. Extract runtime MSI (administrative install — no actual install) ──
Write-Host "Extracting GStreamer runtime ..."
Remove-Item -Recurse -Force $GstExtract -ErrorAction SilentlyContinue
# Resolve-Path fails on paths that don't exist yet — create it first.
New-Item -ItemType Directory -Path $GstExtract -Force | Out-Null
$extractDir = Resolve-Path ".\${GstExtract}"
$proc = Start-Process msiexec.exe -Wait -NoNewWindow -PassThru -ArgumentList @(
  "/a", "`"$(Resolve-Path $RuntimeMsi)`"", "/qn", "TARGETDIR=`"${extractDir}`""
)
if ($proc.ExitCode -ne 0) {
  throw "msiexec admin install failed with exit code $($proc.ExitCode)"
}

# Find the actual bin/ and lib/gstreamer-1.0/ directories (the MSI nests
# them under gstreamer/1.0/msvc_x86_64/ or similar).
$gstBin   = Get-ChildItem -Recurse "${GstExtract}" -Filter "gstreamer-1.0-0.dll" | Select-Object -ExpandProperty Directory -First 1
$gstPluginDir = Get-ChildItem -Recurse "${GstExtract}" -Filter "gstcoreelements.dll" | Select-Object -ExpandProperty Directory -First 1

if (-not $gstBin)   { throw "Could not find GStreamer bin directory in extracted MSI" }
if (-not $gstPluginDir) { throw "Could not find GStreamer plugin directory in extracted MSI" }

# ── 3. Set up staging ──
Write-Host "Setting up staging directory ..."
Remove-Item -Recurse -Force $StagingDir -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path "${StagingDir}\gstreamer\bin" -Force | Out-Null
New-Item -ItemType Directory -Path "${StagingDir}\gstreamer\lib\gstreamer-1.0" -Force | Out-Null

# Copy the binaries
Copy-Item "target\release\multiplex-controller.exe" $StagingDir
Copy-Item "target\release\multiplex-client.exe" $StagingDir

# Copy all GStreamer runtime DLLs
Write-Host "  Copying DLLs from $($gstBin.FullName) ..."
Copy-Item "$($gstBin.FullName)\*.dll" "${StagingDir}\gstreamer\bin\"

Write-Host "  Copying plugin DLLs from $($gstPluginDir.FullName) ..."
Copy-Item "$($gstPluginDir.FullName)\*.dll" "${StagingDir}\gstreamer\lib\gstreamer-1.0\"

# ── 4. Create launcher batch files ──
@"
@echo off
set "DIR=%~dp0"
set "GSTREAMER_ROOT=%DIR%gstreamer"
set "GST_PLUGIN_PATH=%GSTREAMER_ROOT%\lib\gstreamer-1.0"
set "PATH=%GSTREAMER_ROOT%\bin;%PATH%"
"%~dp0multiplex-controller.exe" %*
"@ | Out-File -FilePath "${StagingDir}\run-controller.bat" -Encoding ascii

@"
@echo off
set "DIR=%~dp0"
set "GSTREAMER_ROOT=%DIR%gstreamer"
set "GST_PLUGIN_PATH=%GSTREAMER_ROOT%\lib\gstreamer-1.0"
set "PATH=%GSTREAMER_ROOT%\bin;%PATH%"
"%~dp0multiplex-client.exe" %*
"@ | Out-File -FilePath "${StagingDir}\run-client.bat" -Encoding ascii

# ── 5. Create .zip ──
Write-Host "Creating archive ..."
Add-Type -AssemblyName System.IO.Compression.FileSystem
[System.IO.Compression.ZipFile]::CreateFromDirectory(
  (Resolve-Path $StagingDir),
  "${PSScriptRoot}\..\dist\${ArchiveName}.zip",
  [System.IO.Compression.CompressionLevel]::Optimal,
  $false
)

Write-Host "Done: dist/${ArchiveName}.zip"
