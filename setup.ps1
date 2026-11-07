# Fetches and unpacks the native inputs redjs builds against, at the versions
# pinned in pins.txt:
#
#   prebuilt/bun-webkit/   the Bun WebKit (JavaScriptCore) Windows prebuilt —
#                          internal headers + static libs (~0.85 GB extracted)
#   redjs/cpp/mimalloc/    oven-sh/mimalloc sources (src/, include/, LICENSE),
#                          compiled by redjs/build.rs — this JSC build calls the
#                          EMBEDDER's mimalloc
#   prebuilt/sqlite3mc/    the SQLite3 Multiple Ciphers amalgamation (~14 MB of
#                          C), compiled by redjs/build.rs only with the
#                          `indexeddb-sqlite` feature (IndexedDB on disk)
#
# All three are git-ignored. Idempotent: what is already on disk at the pinned
# version is left alone. What is on disk at ANY OTHER version is replaced — a
# checkout that pulls a new pin and re-runs this script never keeps building
# against the old engine (and redjs/build.rs refuses to, if it is not re-run).
#
# The prebuilt identifies itself (`BUN_WEBKIT_VERSION` in
# include/cmakeconfig.h); that, not the directory name, is what is compared.
#
# Proxy: pass -Proxy or set ZIG_PROXY in the repo .env (same var the Zig
# toolchain setup uses; HTTP proxy, not SOCKS).
#
# Works on Windows PowerShell 5.1 and PowerShell 7.
param(
    [string] $Proxy = '',
    # Use an already-downloaded WebKit tarball instead of fetching it. It is
    # held to the same sha256 as a download.
    [string] $Tarball = '',
    # Re-extract both even when the versions on disk already match.
    [switch] $Force
)

$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot

# --- pins.txt ----------------------------------------------------------------

function Read-Pins([string] $path) {
    $pins = @{}
    foreach ($raw in Get-Content -LiteralPath $path) {
        $line = ($raw -replace '#.*$', '').Trim()
        if (-not $line) { continue }
        $eq = $line.IndexOf('=')
        if ($eq -lt 1) { throw "pins.txt: cannot parse line: $raw" }
        $pins[$line.Substring(0, $eq).Trim()] = $line.Substring($eq + 1).Trim()
    }
    foreach ($k in 'webkit.tag', 'webkit.version', 'webkit.aot', 'webkit.asset', 'webkit.sha256', 'mimalloc.commit', 'mimalloc.tree-sha256',
        'sqlite3mc.version', 'sqlite3mc.asset', 'sqlite3mc.sha256', 'sqlite3mc.tree-sha256') {
        if (-not $pins.ContainsKey($k)) { throw "pins.txt: missing '$k'" }
    }
    return $pins
}

$pins = Read-Pins (Join-Path $root 'pins.txt')

if (-not $Proxy) {
    $envFile = Join-Path $root '../../.env'
    if (Test-Path $envFile) {
        $m = Select-String -Path $envFile -Pattern '^ZIG_PROXY=(.+)$' | Select-Object -First 1
        if ($m) { $Proxy = $m.Matches[0].Groups[1].Value }
    }
}

function Get-Url([string] $url, [string] $outFile) {
    Write-Host "[jscore] downloading $url"
    $iwr = @{ Uri = $url; OutFile = $outFile; UseBasicParsing = $true }
    if ($Proxy) { $iwr.Proxy = $Proxy }
    # Windows PowerShell 5.1's progress bar slows Invoke-WebRequest ~10x.
    $old = $ProgressPreference
    $ProgressPreference = 'SilentlyContinue'
    try { Invoke-WebRequest @iwr } finally { $ProgressPreference = $old }
}

function Assert-Sha256([string] $file, [string] $want) {
    $have = (Get-FileHash -Algorithm SHA256 -LiteralPath $file).Hash.ToLowerInvariant()
    if ($have -ne $want.ToLowerInvariant()) {
        throw "sha256 mismatch for ${file}:`n  pinned: $want`n  file:   $have`nRefusing to use it."
    }
}

# sha256 of "<sha256>  <relative/path>\n" per file, ordinal path order — the
# `sha256sum` manifest of a tree. See the mimalloc.tree-sha256 note in pins.txt.
function Get-TreeSha256([string] $dir) {
    $base = (Resolve-Path -LiteralPath $dir).Path.TrimEnd('\', '/')
    $rel = @(Get-ChildItem -LiteralPath $base -Recurse -File -Force | ForEach-Object {
            $_.FullName.Substring($base.Length + 1).Replace('\', '/')
        })
    [Array]::Sort($rel, [StringComparer]::Ordinal)
    $sb = New-Object System.Text.StringBuilder
    foreach ($r in $rel) {
        $h = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $base $r)).Hash.ToLowerInvariant()
        [void]$sb.Append("$h  $r`n")
    }
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($sb.ToString())
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try { return (-join ($sha.ComputeHash($bytes) | ForEach-Object { $_.ToString('x2') })) }
    finally { $sha.Dispose() }
}

function Assert-ScopedPath([string] $dir) {
    $absolute = [IO.Path]::GetFullPath($dir)
    $boundary = [IO.Path]::GetFullPath($root).TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    if (-not $absolute.StartsWith($boundary, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing filesystem mutation outside redjs: $absolute"
    }
}

function Remove-Tree([string] $dir) {
    Assert-ScopedPath $dir
    if (Test-Path -LiteralPath $dir) { Remove-Item -LiteralPath $dir -Recurse -Force }
}

# --- 1. the bun-webkit prebuilt ------------------------------------------------

function Get-WebKitVersion([string] $dir) {
    $cfg = Join-Path $dir 'include/cmakeconfig.h'
    if (-not (Test-Path -LiteralPath $cfg)) { return $null }
    $m = Select-String -LiteralPath $cfg -Pattern '^#define BUN_WEBKIT_VERSION "([0-9a-f]+)"' | Select-Object -First 1
    if (-not $m) { return '(no BUN_WEBKIT_VERSION)' }
    return $m.Matches[0].Groups[1].Value
}

function Get-WebKitAot([string] $dir) {
    $cfg = Join-Path $dir 'include/cmakeconfig.h'
    if (-not (Test-Path -LiteralPath $cfg)) { return $null }
    $m = Select-String -LiteralPath $cfg -Pattern '^#define BUN_WEBKIT_ENABLE_AOT ([01])$' | Select-Object -First 1
    if ($m) { return $m.Matches[0].Groups[1].Value }
    return $null
}

$dest = Join-Path $root 'prebuilt/bun-webkit'
$want = $pins['webkit.version']
$have = Get-WebKitVersion $dest
$complete = Test-Path -LiteralPath (Join-Path $dest 'lib/JavaScriptCore.lib')

if (-not $Force -and $have -eq $want -and (Get-WebKitAot $dest) -eq $pins['webkit.aot'] -and $complete) {
    Write-Host "[jscore] prebuilt present at the pin ($want): $dest"
}
else {
    if ($have -and $have -ne $want) {
        Write-Host "[jscore] prebuilt on disk is $have, pins.txt says $want — replacing it"
    }
    $tar = $Tarball
    $downloaded = $false
    if (-not $tar) {
        $tar = Join-Path $env:TEMP ("redjs-" + $pins['webkit.asset'])
        Get-Url "https://github.com/oven-sh/WebKit/releases/download/$($pins['webkit.tag'])/$($pins['webkit.asset'])" $tar
        $downloaded = $true
    }
    Assert-Sha256 $tar $pins['webkit.sha256']

    # Extract BESIDE the live directory and swap it in only once the result is
    # verified: an interrupted extraction must not leave a half tree whose
    # cmakeconfig.h already names the new pin. The old tree is removed whole
    # rather than overwritten, because a header the new engine deleted would
    # otherwise stay on the include path.
    $prebuilt = Split-Path $dest
    $staging = Join-Path $prebuilt ".staging-$PID"
    Remove-Tree $staging
    New-Item -ItemType Directory -Force $staging | Out-Null
    Write-Host "[jscore] extracting $tar"
    # bin/ (jsc.exe, the LLInt extractors and ~0.7 GB of their PDBs) is not
    # used by anything that builds or links redjs; skip it.
    tar -xzf $tar -C $staging --exclude 'bun-webkit/bin'
    if ($LASTEXITCODE -ne 0) { throw "tar failed ($LASTEXITCODE) on $tar" }
    $fresh = Join-Path $staging 'bun-webkit'
    $got = Get-WebKitVersion $fresh
    if ($got -ne $want) { throw "the tarball is $got, not the pinned $want" }
    if ((Get-WebKitAot $fresh) -ne $pins['webkit.aot']) { throw "the tarball has the wrong AOT variant" }
    if (-not (Test-Path -LiteralPath (Join-Path $fresh 'lib/JavaScriptCore.lib'))) {
        throw "extraction did not produce lib/JavaScriptCore.lib"
    }
    Remove-Tree $dest
    Move-Item -LiteralPath $fresh -Destination $dest
    Remove-Tree $staging
    if ($downloaded) { Remove-Item -LiteralPath $tar -Force }
    Write-Host "[jscore] prebuilt $want -> $dest"
}

# The prebuilt archive is missing wtf/PlatformEnableWin.h (referenced by
# wtf/PlatformEnable.h). A vendored copy lives in redjs/cpp/compat and is on
# the include path, so nothing to patch — just verify it's there.
$compat = Join-Path $root 'redjs/cpp/compat/wtf/PlatformEnableWin.h'
if (-not (Test-Path $compat)) { throw "missing vendored header: $compat" }

# --- 2. mimalloc ------------------------------------------------------------------

$mi = Join-Path $root 'redjs/cpp/mimalloc'
$stamp = Join-Path $mi '.redjs-pin'
$commit = $pins['mimalloc.commit']
$miHave = $null
if (Test-Path -LiteralPath $stamp) { $miHave = (Get-Content -LiteralPath $stamp -Raw).Trim() }

if (-not $Force -and $miHave -eq $commit -and (Test-Path -LiteralPath (Join-Path $mi 'src/static.c'))) {
    Write-Host "[jscore] mimalloc present at the pin ($commit): $mi"
}
else {
    if ($miHave -and $miHave -ne $commit) {
        Write-Host "[jscore] mimalloc on disk is $miHave, pins.txt says $commit — replacing it"
    }
    $miTar = Join-Path $env:TEMP "redjs-mimalloc-$commit.tar.gz"
    Get-Url "https://github.com/oven-sh/mimalloc/archive/$commit.tar.gz" $miTar
    $miStaging = Join-Path (Split-Path $mi) ".mimalloc-staging-$PID"
    Remove-Tree $miStaging
    New-Item -ItemType Directory -Force $miStaging | Out-Null
    $top = "mimalloc-$commit"
    tar -xzf $miTar -C $miStaging --strip-components=1 "$top/src" "$top/include" "$top/LICENSE"
    if ($LASTEXITCODE -ne 0) { throw "tar failed ($LASTEXITCODE) on $miTar" }
    $tree = Get-TreeSha256 $miStaging
    if ($tree -ne $pins['mimalloc.tree-sha256']) {
        throw "mimalloc tree sha256 mismatch:`n  pinned: $($pins['mimalloc.tree-sha256'])`n  tree:   $tree`nRefusing to use it."
    }
    Remove-Tree $mi
    Move-Item -LiteralPath $miStaging -Destination $mi
    # Written LAST, after the hash: build.rs trusts the stamp.
    Set-Content -LiteralPath $stamp -Value $commit -NoNewline -Encoding ascii
    Remove-Item -LiteralPath $miTar -Force
    Write-Host "[jscore] mimalloc $commit -> $mi"
}

# --- 3. SQLite3 Multiple Ciphers (IndexedDB's persistent backend) ----------------

$mc = Join-Path $root 'prebuilt/sqlite3mc'
$mcStamp = Join-Path $mc '.redjs-pin'
$mcWant = "$($pins['sqlite3mc.version']) $($pins['sqlite3mc.tree-sha256'])"
$mcHave = $null
if (Test-Path -LiteralPath $mcStamp) { $mcHave = (Get-Content -LiteralPath $mcStamp -Raw).Trim() }

if (-not $Force -and $mcHave -eq $mcWant -and (Test-Path -LiteralPath (Join-Path $mc 'sqlite3mc_amalgamation.c'))) {
    Write-Host "[jscore] sqlite3mc present at the pin ($($pins['sqlite3mc.version'])): $mc"
}
else {
    if ($mcHave -and $mcHave -ne $mcWant) {
        Write-Host "[jscore] sqlite3mc on disk is '$mcHave', pins.txt says '$mcWant' — replacing it"
    }
    $ver = $pins['sqlite3mc.version']
    $mcZip = Join-Path $env:TEMP ("redjs-" + $pins['sqlite3mc.asset'])
    Get-Url "https://github.com/utelle/SQLite3MultipleCiphers/releases/download/v$ver/$($pins['sqlite3mc.asset'])" $mcZip
    Assert-Sha256 $mcZip $pins['sqlite3mc.sha256']
    $mcStaging = Join-Path (Split-Path $mc) ".sqlite3mc-staging-$PID"
    Remove-Tree $mcStaging
    New-Item -ItemType Directory -Force $mcStaging | Out-Null
    Expand-Archive -LiteralPath $mcZip -DestinationPath $mcStaging -Force
    # The shell is a program, not a library: never compiled, not kept.
    Get-ChildItem -LiteralPath $mcStaging -Recurse -File -Filter 'shell*.c' | Remove-Item -Force
    $mcTree = Get-TreeSha256 $mcStaging
    if ($mcTree -ne $pins['sqlite3mc.tree-sha256']) {
        throw "sqlite3mc tree sha256 mismatch:`n  pinned: $($pins['sqlite3mc.tree-sha256'])`n  tree:   $mcTree`nRefusing to use it."
    }
    Remove-Tree $mc
    Move-Item -LiteralPath $mcStaging -Destination $mc
    # Written LAST, after both hashes: build.rs trusts the stamp.
    Set-Content -LiteralPath $mcStamp -Value $mcWant -NoNewline -Encoding ascii
    Remove-Item -LiteralPath $mcZip -Force
    Write-Host "[jscore] sqlite3mc $ver -> $mc"
}

Write-Host "[jscore] setup complete. Next: ./build.ps1"
