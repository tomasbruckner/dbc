<#
.SYNOPSIS
  Builds the shipped dbc binaries and zips them into .\dist.

.DESCRIPTION
  One command, no CI. Produces dist\dbc-<version>-windows-x64.zip holding
  dbc-ui.exe, dbc.exe, dbc-mcp.exe and README.md.

  The build differs from `cargo build --release` in two ways:
    * the C runtime is linked STATICALLY (+crt-static), so a colleague's
      machine needs no VC++ redistributable;
    * it goes to its own target dir (target-release), because the rustflags
      above would otherwise invalidate the shared dev cache.

  Version = workspace Cargo.toml. Bump it with `chore: vX.Y.Z` first.

.PARAMETER SkipBuild
  Re-zip what is already built (handy when only README changed).
#>
[CmdletBinding()]
param([switch]$SkipBuild)

$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
Set-Location $root

$version = (Select-String -Path "$root\Cargo.toml" -Pattern '^version = "([^"]+)"').Matches[0].Groups[1].Value
if (-not $version) { throw "version not found in Cargo.toml" }
$target = "x86_64-pc-windows-msvc"
$outDir = "$root\target-release\$target\release"

if (-not $SkipBuild) {
    Write-Host "== building dbc $version ($target, static CRT) =="
    $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS = "-C target-feature=+crt-static"
    $env:CARGO_TARGET_DIR = "$root\target-release"
    # DBC_DATA_DIR must NOT leak in from .cargo/config.toml's dev setting;
    # cargo only sets it for `cargo run`, but be explicit for the reader.
    Remove-Item Env:DBC_DATA_DIR -ErrorAction SilentlyContinue
    cargo build --release --target $target -p dbc-ui -p dbc-cli -p dbc-mcp
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
}

foreach ($exe in "dbc-ui.exe", "dbc.exe", "dbc-mcp.exe") {
    if (-not (Test-Path "$outDir\$exe")) { throw "missing $outDir\$exe" }
}

$dist = "$root\dist"
New-Item -ItemType Directory -Force $dist | Out-Null
$stage = "$dist\dbc-$version-windows-x64"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory $stage | Out-Null
Copy-Item "$outDir\dbc-ui.exe", "$outDir\dbc.exe", "$outDir\dbc-mcp.exe", "$root\README.md" $stage

$zip = "$stage.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path "$stage\*" -DestinationPath $zip
Remove-Item -Recurse -Force $stage

$mb = [math]::Round((Get-Item $zip).Length / 1MB, 1)
Write-Host "== $zip ($mb MB) =="
Get-FileHash $zip -Algorithm SHA256 | ForEach-Object { "SHA256 $($_.Hash)" }
