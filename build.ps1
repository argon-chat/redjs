# Builds redjs (release) and lays out the consumable artifacts:
#   runtimes/win-x64/native/redjs.dll  — self-contained (JSC linked in), for
#                                        JIT/debug runs via DllImport
#   lib/win-x64/redjs.lib              — thin static lib for NativeAOT; the
#                                        consuming link must also add the
#                                        WebKit libs from prebuilt/bun-webkit/lib
# Requires: Rust (stable), clang-cl on PATH (LLVM), prebuilt via setup.ps1.
param(
    [switch] $Test # also run the conformance suite
)

$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot

if (-not (Test-Path (Join-Path $root 'prebuilt/bun-webkit/lib/JavaScriptCore.lib'))) {
    throw "prebuilt missing — run ./setup.ps1 first"
}

Push-Location (Join-Path $root 'redjs')
try {
    cargo rustc --release --crate-type 'rlib,cdylib,staticlib'
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    if ($Test) {
        cargo test --release
        if ($LASTEXITCODE -ne 0) { throw "conformance tests failed" }
    }
}
finally {
    Pop-Location
}

New-Item -ItemType Directory -Force (Join-Path $root 'runtimes/win-x64/native') | Out-Null
New-Item -ItemType Directory -Force (Join-Path $root 'lib/win-x64') | Out-Null
Copy-Item (Join-Path $root 'redjs/target/release/redjs.dll') (Join-Path $root 'runtimes/win-x64/native/redjs.dll') -Force
Copy-Item (Join-Path $root 'redjs/target/release/redjs.lib') (Join-Path $root 'lib/win-x64/redjs.lib') -Force

Write-Host "[redjs] artifacts:"
Get-ChildItem (Join-Path $root 'runtimes/win-x64/native/redjs.dll'), (Join-Path $root 'lib/win-x64/redjs.lib') |
    ForEach-Object { Write-Host ("  {0}  {1:n1} MB" -f $_.FullName, ($_.Length / 1MB)) }
