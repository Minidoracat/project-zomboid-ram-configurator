#Requires -Version 5.1
# Copyright (C) 2023-2026 Minidoracat
# SPDX-License-Identifier: GPL-3.0-only
$ErrorActionPreference = 'Stop'
$build = Join-Path (Split-Path -Parent $PSScriptRoot) 'Build-Standalone.ps1'
$root = Join-Path ([System.IO.Path]::GetTempPath()) ('pz-standalone-' + [guid]::NewGuid().ToString('N'))
$downloads = Join-Path $root "獨立下載 [MOD] ! Player's"

function Invoke-Standalone {
    param([string]$Path, [int]$ExpectedExitCode)
    $process = New-Object System.Diagnostics.Process
    $process.StartInfo = New-Object System.Diagnostics.ProcessStartInfo
    $process.StartInfo.FileName = $env:ComSpec
    $process.StartInfo.Arguments = '/d /c ""' + $Path + '""'
    $process.StartInfo.WorkingDirectory = $root
    $process.StartInfo.UseShellExecute = $false
    $process.StartInfo.RedirectStandardInput = $true
    $process.StartInfo.RedirectStandardOutput = $true
    $process.StartInfo.RedirectStandardError = $true
    $process.StartInfo.StandardOutputEncoding = [System.Text.Encoding]::UTF8
    $process.StartInfo.StandardErrorEncoding = [System.Text.Encoding]::UTF8
    try {
        $null = $process.Start()
        $stdout = $process.StandardOutput.ReadToEndAsync()
        $stderr = $process.StandardError.ReadToEndAsync()
        # 不論進入安裝選擇、手動路徑或主選單，0 都只退出，不修改遊戲。
        $process.StandardInput.WriteLine('0')
        $process.StandardInput.WriteLine('')
        $process.StandardInput.Close()
        if (-not $process.WaitForExit(30000)) {
            $process.Kill()
            $process.WaitForExit()
            throw "Standalone process did not exit: $Path"
        }
        if ($process.ExitCode -ne $ExpectedExitCode) {
            throw "Expected exit $ExpectedExitCode, got $($process.ExitCode): $Path`n$($stdout.Result)`n$($stderr.Result)"
        }
    }
    finally { $process.Dispose() }
}

try {
    & $build -OutputDirectory $downloads
    foreach ($suffix in @('ZH_TW', 'ZH_CN', 'EN_US')) {
        $path = Join-Path $downloads "PZ_Memory_Configurator_$suffix.bat"
        $first = [System.IO.File]::ReadAllText($path, [System.Text.Encoding]::UTF8)
        $published = Join-Path (Split-Path -Parent $PSScriptRoot) "PZ_Memory_Configurator_$suffix.bat"
        if ($first -cne [System.IO.File]::ReadAllText($published, [System.Text.Encoding]::UTF8)) {
            throw "Release BAT is stale. Run Build-Standalone.ps1: $published"
        }
        & $build -OutputDirectory $downloads
        if ($first -cne [System.IO.File]::ReadAllText($path, [System.Text.Encoding]::UTF8)) {
            throw "Packaging is not reproducible: $path"
        }
        Invoke-Standalone -Path $path -ExpectedExitCode 0
        Write-Host "[PASS] $suffix standalone: no sidecar PS1, special-character path, clean quit"

        # 模擬下載被截斷：啟動器必須失敗，不得改讀旁邊的核心或回報成功。
        $marker = [regex]::Match($first, '(?m)^# PZ_CONFIGURATOR_END\r?$')
        if (-not $marker.Success) { throw 'The generated package has no payload marker.' }
        [System.IO.File]::WriteAllText($path, $first.Substring(0, $marker.Index), (New-Object System.Text.UTF8Encoding($false)))
        Invoke-Standalone -Path $path -ExpectedExitCode 1
        Write-Host "[PASS] $suffix truncated package: nonzero exit"
    }
}
finally {
    if ([System.IO.Directory]::Exists($root)) { Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction Stop }
}
