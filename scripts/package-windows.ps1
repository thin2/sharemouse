#requires -Version 7.0
param(
    [Parameter(Mandatory=$true)][string]$GtkPrefix,
    [Parameter(Mandatory=$true)][string]$VcRedistDir,
    [string]$Output = "target\packages\windows-x64",
    [string]$Iscc = "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    [switch]$SkipBuild,
    [switch]$Offline,
    [switch]$PortableOnly
)
$ErrorActionPreference = "Stop"
$Repo = Split-Path -Parent $PSScriptRoot
Push-Location $Repo
try {
    if (Test-Path $Output) { throw "Output exists; choose a new -Output directory." }
    $GtkPrefix = (Resolve-Path $GtkPrefix).Path
    $VcRedistDir = (Resolve-Path $VcRedistDir).Path
    if (-not $PortableOnly -and -not (Test-Path $Iscc)) { throw "Install Inno Setup 6 or pass -Iscc." }
    if (-not (Get-Command dumpbin.exe -ErrorAction SilentlyContinue)) {
        throw "Use the Visual Studio x64 Native Tools terminal (dumpbin.exe is required)."
    }
    if (-not $SkipBuild) {
        $CargoArgs = @("build", "--workspace", "--release", "--locked")
        if ($Offline) { $CargoArgs += "--offline" }
        & cargo @CargoArgs
        if ($LASTEXITCODE -ne 0) { throw "Cargo build failed." }
    }
    if (-not [IO.Path]::IsPathRooted($Output)) { $Output = Join-Path $Repo $Output }
    $Output = [IO.Path]::GetFullPath($Output)
    $Stage = Join-Path $Output "Sharemouse"
    New-Item -ItemType Directory -Path $Stage | Out-Null
    Copy-Item "target\release\lan-mouse.exe" $Stage
    Copy-Item "app-icon.ico" $Stage
    Get-ChildItem (Join-Path $GtkPrefix "bin\*.dll") -File | Copy-Item -Destination $Stage
    $CrtFiles = @(Get-ChildItem $VcRedistDir -Recurse -Filter "*.dll" -File |
        Where-Object { $_.Directory.Name -like "Microsoft.VC*.CRT" -and $_.FullName -match '[\\/]x64[\\/]' })
    if ($CrtFiles.Count -eq 0) { throw "No x64 VC CRT DLLs found in -VcRedistDir." }
    foreach ($Crt in $CrtFiles) {
        $Target = Join-Path $Stage $Crt.Name
        if (Test-Path $Target) {
            if ((Get-FileHash $Target).Hash -ne (Get-FileHash $Crt.FullName).Hash) {
                Write-Warning "Skipping conflicting runtime DLL: $($Crt.Name) (using existing version from GTK)"
            }
        } else { Copy-Item $Crt.FullName $Target }
    }
    foreach ($Relative in @("share\glib-2.0\schemas", "share\gtk-4.0", "share\icons\Adwaita",
                            "share\icons\hicolor", "share\licenses", "lib\gdk-pixbuf-2.0")) {
        $Source = Join-Path $GtkPrefix $Relative
        if (Test-Path $Source) {
            $Target = Join-Path $Stage $Relative
            New-Item -ItemType Directory -Path (Split-Path -Parent $Target) -Force | Out-Null
            Copy-Item $Source $Target -Recurse
        }
    }
    $Schemas = Join-Path $Stage "share\glib-2.0\schemas"
    if (-not (Test-Path $Schemas)) { throw "GTK schemas missing." }
    & (Join-Path $GtkPrefix "bin\glib-compile-schemas.exe") $Schemas
    if ($LASTEXITCODE -ne 0) { throw "Schema compilation failed." }
    $Query = Join-Path $GtkPrefix "bin\gdk-pixbuf-query-loaders.exe"
    if (Test-Path $Query) { Copy-Item $Query $Stage }
    # Fail on dependencies that would otherwise be hidden by the build machine's PATH.
    foreach ($Binary in Get-ChildItem $Stage -Recurse -File |
             Where-Object { $_.Extension -in @(".exe", ".dll") }) {
        $Dump = & dumpbin.exe /NOLOGO /DEPENDENTS $Binary.FullName
        if ($LASTEXITCODE -ne 0) { throw "Cannot inspect $($Binary.FullName)" }
        foreach ($Line in $Dump) {
            if ($Line -match '^\s+([A-Za-z0-9_.-]+\.dll)\s*$') {
                $Dependency = $Matches[1]
                if (-not (Test-Path (Join-Path $Stage $Dependency)) -and
                    -not (Test-Path (Join-Path $env:WINDIR "System32\$Dependency")) -and
                    $Dependency -notmatch '^(api|ext)-ms-win-') {
                    throw "Missing DLL $Dependency required by $($Binary.Name)"
                }
                # VC runtime must ship even when present in the build host's System32.
                if ($Dependency -match '^(vcruntime|msvcp|concrt)\d' -and
                    -not (Test-Path (Join-Path $Stage $Dependency))) {
                    throw "VC runtime was not bundled: $Dependency"
                }
            }
        }
    }
    & python scripts/package-common.py (Join-Path $Stage "Source") (Join-Path $Stage "lan-mouse.exe")
    if ($LASTEXITCODE -ne 0) { throw "Source and dependency inventory failed." }
    Copy-Item "docs\安装与卸载.md", "docs\三机测试清单.md" $Stage
    Copy-Item "LICENSE" $Stage
    # Keep the native build's licenses and source information with the runtime.
    $Native = @{
        GtkPrefixVersion = (& (Join-Path $GtkPrefix "bin\pkgconf.exe") --modversion gtk4 libadwaita-1)
        VcRuntimeFiles = @($CrtFiles | ForEach-Object { $_.Name })
        LicenseNotes = "See share/licenses and GTK/gvsbuild source. VC runtime is Microsoft redistributable code."
    }
    if ($LASTEXITCODE -ne 0) { throw "Cannot read GTK runtime versions." }
    $Native | ConvertTo-Json -Depth 4 | Set-Content (Join-Path $Stage "Source\native-dependencies.json") -Encoding UTF8
    Compress-Archive -Path $Stage -DestinationPath (Join-Path $Output "Sharemouse-windows-x64.zip")
    if (-not $PortableOnly) {
        & $Iscc "/DSourceDir=$Stage" "/DOutputDir=$Output" "build-aux\windows\sharemouse.iss"
        if ($LASTEXITCODE -ne 0) { throw "Inno Setup compilation failed." }
    }
    Write-Output "Test package: $Output"
} finally { Pop-Location }
