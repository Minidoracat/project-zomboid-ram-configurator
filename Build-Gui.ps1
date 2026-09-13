#Requires -Version 5.1
# SPDX-License-Identifier: GPL-3.0-only
[CmdletBinding()]
param()
$ErrorActionPreference = 'Stop'
Push-Location (Join-Path $PSScriptRoot 'gui')
try {
    cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'GUI build failed.' }
    $destination = Join-Path $PWD 'dist'
    [System.IO.Directory]::CreateDirectory($destination) | Out-Null
    Copy-Item -LiteralPath 'target/x86_64-pc-windows-msvc/release/pz-ram-configurator.exe' -Destination $destination
    Copy-Item -LiteralPath '../LICENSE' -Destination (Join-Path $destination 'LICENSE.txt')
    Copy-Item -LiteralPath 'assets/fonts/OFL.txt' -Destination (Join-Path $destination 'NotoSansCJK-LICENSE.txt')
    Copy-Item -LiteralPath 'assets/icons/LICENSE.txt' -Destination (Join-Path $destination 'Lucide-LICENSE.txt')
    Write-Host (Join-Path $destination 'pz-ram-configurator.exe')
}
finally { Pop-Location }
