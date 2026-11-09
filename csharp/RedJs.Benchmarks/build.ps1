param(
    [switch] $SkipNativeBuild,
    [switch] $NativeAot,
    [string] $Frontend,
    [string] $BuiltinTable
)

$ErrorActionPreference = 'Stop'
$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../..'))

if ($NativeAot) {
    if (-not $Frontend -or -not $BuiltinTable) {
        throw '-NativeAot requires -Frontend <pinned bun.exe> and -BuiltinTable <matching generated table>'
    }
    $Frontend = (Resolve-Path -LiteralPath $Frontend).Path
    $BuiltinTable = (Resolve-Path -LiteralPath $BuiltinTable).Path
    $revision = (& $Frontend --revision) -join ''
    if ($LASTEXITCODE -ne 0 -or -not $revision.EndsWith('7ca0042d3')) { throw 'AOT frontend must be Bun 7ca0042d3' }
}

if (-not $SkipNativeBuild) {
    Push-Location (Join-Path $repoRoot 'redjs')
    try {
        cargo rustc --release --lib --crate-type 'rlib,cdylib,staticlib' --no-default-features --target x86_64-pc-windows-msvc --target-dir target/benchmarks
        if ($LASTEXITCODE -ne 0) { throw 'Native redjs build failed' }
        if ($NativeAot) {
            $previousTable = $env:REDJS_AOT_BUILTIN_TABLE_DIR
            try {
                $env:REDJS_AOT_BUILTIN_TABLE_DIR = $BuiltinTable
                cargo rustc --release --lib --crate-type 'rlib,cdylib,staticlib' --no-default-features --features native-aot --target x86_64-pc-windows-msvc --target-dir target/benchmarks-aot
                if ($LASTEXITCODE -ne 0) { throw 'Native AOT redjs build failed' }
                cargo build --release --example aot-image --no-default-features --features native-aot --target x86_64-pc-windows-msvc --target-dir target/benchmarks-aot
                if ($LASTEXITCODE -ne 0) { throw 'Native AOT probe build failed' }
            }
            finally { $env:REDJS_AOT_BUILTIN_TABLE_DIR = $previousTable }
        }
    }
    finally { Pop-Location }
}

$nativeDll = Join-Path $repoRoot 'redjs/target/benchmarks/x86_64-pc-windows-msvc/release/redjs.dll'
if (-not (Test-Path -LiteralPath $nativeDll)) {
    throw "redjs.dll missing: run this script without -SkipNativeBuild (run ../../setup.ps1 first if needed)"
}

Push-Location $PSScriptRoot
try {
    dotnet build -c Release
    if ($LASTEXITCODE -ne 0) { throw 'C# benchmark build failed' }
    if ($NativeAot) {
        $compiler = Join-Path $repoRoot 'redjs/target/benchmarks-aot/x86_64-pc-windows-msvc/release/examples/aot-image.exe'
        $infoText = & $compiler info
        if ($LASTEXITCODE -ne 0) { throw 'AOT compiler info failed' }
        $buildInfo = $infoText | ConvertFrom-Json
        $stage = Join-Path $PSScriptRoot 'obj/native-images'
        dotnet run -c Release --no-build -- --write-aot-sources $stage
        if ($LASTEXITCODE -ne 0) { throw 'AOT source export failed' }
        foreach ($source in Get-ChildItem -LiteralPath $stage -Filter '*.js' -File) {
            $ir = Join-Path $stage ($source.BaseName + '.ir')
            $image = Join-Path $stage ($source.BaseName + '.image')
            & $Frontend compile $source.FullName --no-builtin-modules "--aot-out=$ir" --aot-threads=1 --aot-testing=NoExecutable --outfile (Join-Path $stage 'unused.exe')
            if ($LASTEXITCODE -ne 0) { throw "AOT frontend failed: $($source.Name)" }
            & $compiler compile $ir $image
            if ($LASTEXITCODE -ne 0) { throw "AOT native compiler failed: $($source.Name)" }
            $entries = @([IO.File]::ReadAllLines("$image.entries") | Where-Object { $_.Trim() })
            if ($entries.Count -ne 1 -or [uint32]$entries[0] -eq 0) { throw 'Expected exactly one AOT entry' }
            $manifest = [ordered]@{
                schema = 'redjs-benchmark-native-v1'
                build = $buildInfo
                entry = [uint32]$entries[0]
                identity = [Convert]::ToHexString([IO.File]::ReadAllBytes("$image.identity")).ToLowerInvariant()
                imageSha256 = (Get-FileHash -LiteralPath $image -Algorithm SHA256).Hash.ToLowerInvariant()
                sourceSha256 = (Get-FileHash -LiteralPath $source.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
            }
            [IO.File]::WriteAllText("$image.json", ($manifest | ConvertTo-Json -Depth 5), [Text.UTF8Encoding]::new($false))
        }
        dotnet build -c Release
        if ($LASTEXITCODE -ne 0) { throw 'Copying benchmark AOT artifacts failed' }
    }
}
finally { Pop-Location }
