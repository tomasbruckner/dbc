<#
.SYNOPSIS
  Builds the shipped dbc binaries and packs them with Velopack into .\dist.

.DESCRIPTION
  Two stages, each skippable, so CI can sign the executables in between:

    build  cargo build --release  ->  dist\stage\   (dbc-ui.exe, dbc.exe,
                                                     dbc-mcp.exe, README.md)
    pack   vpk pack               ->  dist\releases\ (dbc-win-Setup.exe,
                                                      dbc-win-Portable.zip,
                                                      dbc-<ver>-full.nupkg,
                                                      dbc-<ver>-delta.nupkg,
                                                      releases.win.json)

  The build differs from `cargo build --release` in two ways:
    * the C runtime is linked STATICALLY (+crt-static), so a colleague's
      machine needs no VC++ redistributable;
    * it goes to its own target dir (target-release), because the rustflags
      above would otherwise invalidate the shared dev cache.

  The pack needs the Velopack CLI:  dotnet tool install -g vpk
  Before packing it downloads the previous release from GitHub so the new
  one ships a delta package too; the first release has nothing to diff
  against and that step just warns.

  Version = workspace Cargo.toml. Bump it with `chore: vX.Y.Z` first.

.PARAMETER SkipBuild
  Pack what is already in dist\stage (CI does this after signing).

.PARAMETER SkipPack
  Build into dist\stage and stop (CI does this before signing).

.PARAMETER NoDelta
  Do not look at GitHub for the previous release. Faster, no delta package.

.PARAMETER ReleaseNotes
  Markdown file with this version's notes, embedded into the package.
#>
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [switch]$SkipPack,
    [switch]$NoDelta,
    [string]$ReleaseNotes
)

$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
Set-Location $root

$version = (Select-String -Path "$root\Cargo.toml" -Pattern '^version = "([^"]+)"').Matches[0].Groups[1].Value
if (-not $version) { throw "version not found in Cargo.toml" }
$target = "x86_64-pc-windows-msvc"
$outDir = "$root\target-release\$target\release"
$stage = "$root\dist\stage"
$releases = "$root\dist\releases"
$repo = "https://github.com/tomasbruckner/dbc"

if (-not $SkipBuild) {
    Write-Host "== building dbc $version ($target, static CRT) =="
    $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS = "-C target-feature=+crt-static"
    $env:CARGO_TARGET_DIR = "$root\target-release"
    # DBC_DATA_DIR must NOT leak in from .cargo/config.toml's dev setting;
    # cargo only sets it for `cargo run`, but be explicit for the reader.
    Remove-Item Env:DBC_DATA_DIR -ErrorAction SilentlyContinue
    cargo build --release --target $target -p dbc-ui -p dbc-cli -p dbc-mcp
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

    foreach ($exe in "dbc-ui.exe", "dbc.exe", "dbc-mcp.exe") {
        if (-not (Test-Path "$outDir\$exe")) { throw "missing $outDir\$exe" }
    }
    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    New-Item -ItemType Directory -Force $stage | Out-Null
    Copy-Item "$outDir\dbc-ui.exe", "$outDir\dbc.exe", "$outDir\dbc-mcp.exe", "$root\README.md" $stage
    Write-Host "== staged in $stage =="
}

if ($SkipPack) { return }

foreach ($exe in "dbc-ui.exe", "dbc.exe", "dbc-mcp.exe") {
    if (-not (Test-Path "$stage\$exe")) { throw "missing $stage\$exe — run without -SkipBuild first" }
}
if (-not (Get-Command vpk -ErrorAction SilentlyContinue)) {
    throw "vpk not found. Install the Velopack CLI:  dotnet tool install -g vpk"
}

New-Item -ItemType Directory -Force $releases | Out-Null
if (-not $NoDelta) {
    Write-Host "== previous release from $repo (for the delta package) =="
    # Not fatal: the first Velopack release has nothing to diff against, and
    # an offline machine can still build a full package.
    vpk download github --repoUrl $repo --outputDir $releases --channel win
    if ($LASTEXITCODE -ne 0) { Write-Warning "no previous Velopack release found; packing without a delta" }
}

Write-Host "== vpk pack dbc $version =="
$packArgs = @(
    "pack",
    "--packId", "dbc",
    "--packVersion", $version,
    "--packDir", $stage,
    "--mainExe", "dbc-ui.exe",
    "--packTitle", "dbc",
    "--packAuthors", "Tomáš Bruckner",
    "--icon", "$root\crates\dbc-ui\assets\dbc.ico",
    "--outputDir", $releases,
    "--channel", "win",
    # Start menu only; nobody asked for a desktop icon.
    "--shortcuts", "StartMenuRoot"
)
if ($ReleaseNotes) { $packArgs += @("--releaseNotes", (Resolve-Path $ReleaseNotes).Path) }
& vpk @packArgs
if ($LASTEXITCODE -ne 0) { throw "vpk pack failed" }

Write-Host "== $releases =="
Get-ChildItem $releases | ForEach-Object {
    $mb = [math]::Round($_.Length / 1MB, 1)
    $hash = (Get-FileHash $_.FullName -Algorithm SHA256).Hash
    "{0,-40} {1,8} MB  SHA256 {2}" -f $_.Name, $mb, $hash
}
