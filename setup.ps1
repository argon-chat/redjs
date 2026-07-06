# Fetches and unpacks the Bun WebKit (JavaScriptCore) Windows prebuilt that
# redjs builds against. Idempotent: skips work when already present.
#
# Version is PINNED — the C++ glue is compiled against these exact internal
# headers, and the CallFrame/JSValue layout export makes ABI drift loud, but
# bumping the prebuilt still requires re-running the redjs test suite.
#
# Proxy: pass -Proxy or set ZIG_PROXY in the repo .env (same var the Zig
# toolchain setup uses; HTTP proxy, not SOCKS).
param(
    [string] $Proxy = '',
    # Use an already-downloaded tarball instead of fetching.
    [string] $Tarball = ''
)

$ErrorActionPreference = 'Stop'

$PINNED = 'autobuild-c9ad5813fd23bd8b98b0738abc3d037ec716aa92'
$ASSET = 'bun-webkit-windows-amd64.tar.gz' # NOT the -lto asset: ThinLTO bitcode won't link with link.exe
$URL = "https://github.com/oven-sh/WebKit/releases/download/$PINNED/$ASSET"

$root = $PSScriptRoot
$dest = Join-Path $root 'prebuilt/bun-webkit'

if (Test-Path (Join-Path $dest 'lib/JavaScriptCore.lib')) {
    Write-Host "[jscore] prebuilt already present: $dest"
}
else {
    $tar = $Tarball
    if (-not $tar) {
        if (-not $Proxy) {
            $envFile = Join-Path $root '../../.env'
            if (Test-Path $envFile) {
                $line = Select-String -Path $envFile -Pattern '^ZIG_PROXY=(.+)$' | Select-Object -First 1
                if ($line) { $Proxy = $line.Matches[0].Groups[1].Value }
            }
        }
        $tar = Join-Path $env:TEMP $ASSET
        Write-Host "[jscore] downloading $URL"
        $args = @{ Uri = $URL; OutFile = $tar }
        if ($Proxy) { $args.Proxy = $Proxy }
        Invoke-WebRequest @args
    }

    Write-Host "[jscore] extracting to $dest"
    New-Item -ItemType Directory -Force (Split-Path $dest) | Out-Null
    # The tarball root is `bun-webkit/`; extract into prebuilt/.
    tar -xzf $tar -C (Split-Path $dest)
    if (-not (Test-Path (Join-Path $dest 'lib/JavaScriptCore.lib'))) {
        throw "extraction did not produce $dest/lib/JavaScriptCore.lib"
    }
}

# The prebuilt archive is missing wtf/PlatformEnableWin.h (referenced by
# wtf/PlatformEnable.h). A vendored copy lives in redjs/cpp/compat and is on
# the include path, so nothing to patch — just verify it's there.
$compat = Join-Path $root 'redjs/cpp/compat/wtf/PlatformEnableWin.h'
if (-not (Test-Path $compat)) { throw "missing vendored header: $compat" }

Write-Host "[jscore] setup complete. Next: ./build.ps1"
