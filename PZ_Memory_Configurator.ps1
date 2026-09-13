#Requires -Version 5.1
# Project Zomboid RAM Configurator
# Copyright (C) 2023-2026 Minidoracat
# SPDX-License-Identifier: GPL-3.0-only
# This program comes with ABSOLUTELY NO WARRANTY. See the LICENSE file for details.
<#
.SYNOPSIS
    Project Zomboid heap-size (-Xmx) configurator core + trilingual console UI.
.DESCRIPTION
    Dot-source this file to load the functions only (no UI):
        . .\PZ_Memory_Configurator.ps1
    Run it to start the interactive UI:
        powershell -NoProfile -ExecutionPolicy Bypass -File .\PZ_Memory_Configurator.ps1 -Language en-US
#>
[CmdletBinding()]
param(
    [ValidateSet('zh-TW', 'zh-CN', 'en-US')]
    [string]$Language = 'zh-TW',

    [string]$GameDirectory
)

Add-Type -AssemblyName System.Runtime.Serialization
$script:PzVersion = '2.0.0'

# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------

function Resolve-PzLiteralPath {
    param([Parameter(Mandatory = $true)][AllowEmptyString()][string]$Path)

    if ([string]::IsNullOrWhiteSpace($Path)) { throw 'Path must not be empty.' }
    return $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($Path)
}

function Read-PzFileText {
    param([Parameter(Mandatory = $true)][string]$Path)

    $bytes = [System.IO.File]::ReadAllBytes($Path)
    $hasBom = ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF)
    $offset = 0
    if ($hasBom) { $offset = 3 }

    $encoding = New-Object System.Text.UTF8Encoding($false, $true)
    try {
        $text = $encoding.GetString($bytes, $offset, $bytes.Length - $offset)
    }
    catch {
        throw "File is not valid UTF-8 text: $Path"
    }

    return [pscustomobject]@{
        Text   = $text
        Bytes  = $bytes
        HasBom = $hasBom
    }
}

function Get-PzJvmSize {
    param([Parameter(Mandatory = $true)][string]$Argument, [Parameter(Mandatory = $true)][string]$Option)

    $match = [regex]::Match($Argument, ('^-' + $Option + '(?<num>[0-9]+)(?<unit>[kKmMgG]?)$'))
    if (-not $match.Success) { throw "Malformed -$Option argument: '$Argument'." }

    $digits = $match.Groups['num'].Value
    if ($digits.TrimStart('0').Length -gt 18) { throw "Value of -$Option is out of range: '$Argument'." }

    try {
        $number = [long]::Parse($digits, [System.Globalization.CultureInfo]::InvariantCulture)
    }
    catch {
        throw "Value of -$Option is out of range: '$Argument'."
    }

    $multiplier = 1L
    switch ($match.Groups['unit'].Value.ToLowerInvariant()) {
        'k' { $multiplier = 1024L }
        'm' { $multiplier = 1048576L }
        'g' { $multiplier = 1073741824L }
    }

    if ($number -gt ([long]::MaxValue / $multiplier)) { throw "Value of -$Option is out of range: '$Argument'." }
    return [long]($number * $multiplier)
}

function Get-PzArgumentInfo {
    param([Parameter(Mandatory = $true)][AllowEmptyString()][string]$Text)

    try {
        $json = $Text | ConvertFrom-Json
        # PS 5.1 會折疊重複鍵且接受 JavaScript 語法；使用內建 reader 補上語法與鍵名檢查。
        $reader = [System.Runtime.Serialization.Json.JsonReaderWriterFactory]::CreateJsonReader(
            [System.Text.Encoding]::UTF8.GetBytes($Text), [System.Xml.XmlDictionaryReaderQuotas]::Max)
        try {
            $objects = @{}
            $numberDepth = -1
            while ($reader.Read()) {
                if ($reader.NodeType -eq [System.Xml.XmlNodeType]::Element) {
                    if ($objects.ContainsKey($reader.Depth - 1)) {
                        $key = $reader.GetAttribute('item')
                        if ($null -eq $key) { $key = $reader.LocalName }
                        if (-not $objects[$reader.Depth - 1].Add($key)) {
                            throw "Duplicate JSON property: $key"
                        }
                    }
                    $type = $reader.GetAttribute('type')
                    if ($type -eq 'object') {
                        $objects[$reader.Depth] = New-Object 'System.Collections.Generic.HashSet[string]' ([StringComparer]::Ordinal)
                        # reader 將第一個 __type 鍵映射為 XML 屬性，仍須納入重複鍵檢查。
                        if ($null -ne $reader.GetAttribute('__type')) {
                            $null = $objects[$reader.Depth].Add('__type')
                        }
                    }
                    $numberDepth = -1
                    if ($type -eq 'number') { $numberDepth = $reader.Depth }
                }
                elseif ($reader.NodeType -eq [System.Xml.XmlNodeType]::EndElement) {
                    $objects.Remove($reader.Depth)
                }
                elseif ($reader.NodeType -eq [System.Xml.XmlNodeType]::Text -and $reader.Depth -eq $numberDepth + 1) {
                    if ($reader.Value -cnotmatch '^-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?$') {
                        throw "Invalid JSON number: $($reader.Value)"
                    }
                }
            }
        }
        finally { $reader.Close() }
    }
    catch {
        throw "Not valid JSON: $($_.Exception.Message)"
    }

    if ($null -eq $json) { throw 'JSON content is empty.' }
    if ($json -is [System.Array] -or $json -is [string] -or $json -is [System.ValueType]) {
        throw 'JSON root must be an object containing vmArgs.'
    }

    $names = @($json.PSObject.Properties | ForEach-Object { $_.Name })
    if ($names -cnotcontains 'vmArgs') { throw 'JSON root has no exact vmArgs entry.' }

    $vmArgs = @($json.vmArgs)
    if ($json.vmArgs -isnot [array] -or @($vmArgs | Where-Object { $_ -isnot [string] }).Count) {
        throw 'vmArgs must be an array of JVM argument strings.'
    }
    $xmx = @($vmArgs | Where-Object { $_ -cmatch '^-Xmx' })
    if ($xmx.Count -eq 0) { throw 'No -Xmx argument found in root vmArgs.' }
    if ($xmx.Count -gt 1) { throw "Found $($xmx.Count) -Xmx arguments in root vmArgs; expected exactly one." }

    $argument = [string]$xmx[0]
    $heapBytes = Get-PzJvmSize -Argument $argument -Option 'Xmx'
    if ($heapBytes -le 0) { throw 'The -Xmx heap limit must be positive.' }

    # Preserve Xms, but refuse configurations whose initial heap is ambiguous.
    $xmsBytes = $null
    $xms = @($vmArgs | Where-Object { $_ -cmatch '^-Xms' })
    if ($xms.Count -gt 1) { throw 'Multiple -Xms arguments are not supported; keep exactly one.' }
    if ($xms.Count -eq 1) { $xmsBytes = Get-PzJvmSize -Argument ([string]$xms[0]) -Option 'Xms' }

    return [pscustomobject]@{
        Argument  = $argument
        HeapBytes = $heapBytes
        XmsBytes  = $xmsBytes
    }
}

function Assert-PzSnapshot {
    param([byte[]]$Expected, [byte[]]$Actual)

    if ($Expected.Length -eq $Actual.Length) {
        for ($i = 0; $i -lt $Actual.Length; $i++) {
            if ($Expected[$i] -ne $Actual[$i]) { break }
        }
        if ($i -eq $Actual.Length) { return }
    }
    throw 'The configuration changed since it was displayed. Read it again and confirm the new state; nothing was written.'
}

function Write-PzFileAtomic {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][byte[]]$Bytes,
        [scriptblock]$Validate,
        [byte[]]$ExpectedBytes,
        [string]$BackupPath
    )

    $directory = [System.IO.Path]::GetDirectoryName($Path)
    if ([string]::IsNullOrEmpty($directory)) { throw "Cannot determine the folder of: $Path" }

    $temp = [System.IO.Path]::Combine(
        $directory,
        ([System.IO.Path]::GetFileName($Path) + '.pztmp' + [System.Guid]::NewGuid().ToString('N').Substring(0, 8)))
    $previousBackup = $null
    $retainRecoveryFiles = $false

    try {
        [System.IO.File]::WriteAllBytes($temp, $Bytes)
        if ($Validate) { & $Validate $temp }
        if ($null -ne $ExpectedBytes) {
            Assert-PzSnapshot -Expected $ExpectedBytes -Actual ([System.IO.File]::ReadAllBytes($Path))
        }

        if ($BackupPath -and [System.IO.File]::Exists($BackupPath)) {
            $previousBackup = $temp + '.previous-backup'
            [System.IO.File]::Copy($BackupPath, $previousBackup, $false)
        }
        # ReplaceFileW 極少數錯誤可能已搬移檔案；失敗時保留候選檔與舊備份供回復。
        $retainRecoveryFiles = $true
        if ($null -ne $ExpectedBytes) {
            if ($BackupPath) { [System.IO.File]::Replace($temp, $Path, $BackupPath) }
            else { [System.IO.File]::Replace($temp, $Path, [NullString]::Value) }
        }
        else {
            [System.IO.File]::Move($temp, $Path)
        }
        $retainRecoveryFiles = $false
    }
    catch {
        if ($retainRecoveryFiles) {
            throw "Replacement failed. Do not retry until checking these recovery paths: target '$Path'; backup '$BackupPath'; candidate '$temp'; previous backup '$previousBackup'. $($_.Exception.Message)"
        }
        throw
    }
    finally {
        if (-not $retainRecoveryFiles) {
            if ([System.IO.File]::Exists($temp)) { [System.IO.File]::Delete($temp) }
            if ($previousBackup -and [System.IO.File]::Exists($previousBackup)) {
                [System.IO.File]::Delete($previousBackup)
            }
        }
    }
}

function Add-PzUniquePath {
    param(
        [Parameter(Mandatory = $true)][AllowEmptyCollection()][System.Collections.Generic.List[string]]$List,
        [Parameter(Mandatory = $true)][string]$Path
    )

    $normalized = $Path.TrimEnd('\', '/')
    if ([string]::IsNullOrWhiteSpace($normalized)) { return }
    foreach ($existing in $List) {
        if ($existing.Equals($normalized, [System.StringComparison]::OrdinalIgnoreCase)) { return }
    }
    $List.Add($normalized)
}

function Convert-PzVdfString {
    param([Parameter(Mandatory = $true)][AllowEmptyString()][string]$Value)
    # Only \\ and \" are real VDF escapes; a lone backslash (hand-edited files) stays as-is.
    return [regex]::Replace($Value, '\\(["\\])', '$1')
}

# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------

function Get-PzMemoryRecommendation {
    [CmdletBinding()]
    param([Parameter(Mandatory = $true)][long]$TotalBytes)

    if ($TotalBytes -le 0) { return $null }
    if ($TotalBytes -lt 8589934592L) { return $null }

    $totalGb = [double]$TotalBytes / 1073741824.0
    $recommended = [Math]::Floor([Math]::Min($totalGb * 0.8, $totalGb - 4.0))
    if ($recommended -lt 1) { return $null }
    return [int]$recommended
}

function Get-PzConfiguration {
    [CmdletBinding()]
    param([Parameter(Mandatory = $true)][string]$Path)

    $ErrorActionPreference = 'Stop'
    $full = Resolve-PzLiteralPath -Path $Path
    if (-not [System.IO.File]::Exists($full)) { throw "File not found: $full" }

    $file = Read-PzFileText -Path $full
    $info = Get-PzArgumentInfo -Text $file.Text

    return [pscustomobject]@{
        Path          = $full
        Argument      = $info.Argument
        HeapBytes     = $info.HeapBytes
        HeapGigabytes = [Math]::Round(([double]$info.HeapBytes / 1073741824.0), 2)
        XmsBytes      = $info.XmsBytes
        BackupPath    = $full + '.bak'
    }
}

function Set-PzMemory {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Gigabytes,
        [Parameter(Mandatory = $true)][long]$TotalBytes,
        [byte[]]$ExpectedBytes
    )

    $ErrorActionPreference = 'Stop'

    if ($Gigabytes -cnotmatch '^[1-9][0-9]{0,5}$') {
        throw "Invalid memory value: '$Gigabytes'. Use a plain positive integer number of gigabytes (1-999999)."
    }
    if ($TotalBytes -le 0) { throw 'TotalBytes must be the detected physical memory size in bytes.' }

    $requested = [long]$Gigabytes * 1073741824L
    if ($requested -ge $TotalBytes) {
        throw ("Requested $Gigabytes GB is not below the physical memory of " +
            ('{0:N2}' -f ([double]$TotalBytes / 1073741824.0)) + ' GB; refusing to write.')
    }

    $full = Resolve-PzLiteralPath -Path $Path
    if (-not [System.IO.File]::Exists($full)) { throw "File not found: $full" }

    $target = New-Object System.IO.FileInfo($full)
    if ((($target.Attributes -band [System.IO.FileAttributes]::ReadOnly)) -ne 0) {
        throw "Target file is read-only: $full"
    }

    $file = Read-PzFileText -Path $full
    if ($null -ne $ExpectedBytes) { Assert-PzSnapshot -Expected $ExpectedBytes -Actual $file.Bytes }
    $info = Get-PzArgumentInfo -Text $file.Text

    # Same effective heap size: no write, no backup rotation.
    if ($info.HeapBytes -eq $requested) {
        $result = Get-PzConfiguration -Path $full
        $result | Add-Member -NotePropertyName Changed -NotePropertyValue $false
        return $result
    }

    if ($null -ne $info.XmsBytes -and $info.XmsBytes -gt $requested) {
        throw ('The existing -Xms (' + ('{0:N2}' -f ([double]$info.XmsBytes / 1073741824.0)) +
            " GB) would exceed the requested -Xmx ($Gigabytes GB). Lower -Xms yourself first; " +
            'this tool never rewrites -Xms.')
    }

    $newArgument = '-Xmx' + $Gigabytes + 'g'
    $hits = [regex]::Matches($file.Text, '-Xmx[0-9]+[kKmMgG]?')
    if ($hits.Count -ne 1) {
        throw "Expected exactly one literal -Xmx token in the file but found $($hits.Count); refusing to edit."
    }
    if ($hits[0].Value -cne $info.Argument) {
        throw "The -Xmx token in the file text does not match the parsed value; refusing to edit."
    }

    $newText = $file.Text.Substring(0, $hits[0].Index) + $newArgument +
    $file.Text.Substring($hits[0].Index + $hits[0].Length)

    $body = [System.Text.Encoding]::UTF8.GetBytes($newText)
    $stream = New-Object System.IO.MemoryStream
    try {
        if ($file.HasBom) { $stream.Write([byte[]](0xEF, 0xBB, 0xBF), 0, 3) }
        $stream.Write($body, 0, $body.Length)
        $newBytes = $stream.ToArray()
    }
    finally {
        $stream.Dispose()
    }


    Write-PzFileAtomic -Path $full -Bytes $newBytes -ExpectedBytes $file.Bytes -BackupPath ($full + '.bak') -Validate {
        param($TempPath)
        $check = Get-PzConfiguration -Path $TempPath
        if ($check.HeapBytes -ne $requested) { throw 'Rewritten content failed validation; nothing was replaced.' }
    }

    $result = Get-PzConfiguration -Path $full
    if ($result.HeapBytes -ne $requested -or $result.Argument -cne $newArgument) {
        throw "Write verification failed: the file now reports '$($result.Argument)'. Use Restore-PzMemory to roll back."
    }
    $result | Add-Member -NotePropertyName Changed -NotePropertyValue $true
    return $result
}

function Restore-PzMemory {
    [CmdletBinding()]
    param([Parameter(Mandatory = $true)][string]$Path)

    $ErrorActionPreference = 'Stop'
    $full = Resolve-PzLiteralPath -Path $Path
    $backup = $full + '.bak'

    if (-not [System.IO.File]::Exists($backup)) { throw "No backup file found: $backup" }

    $file = Read-PzFileText -Path $backup
    $info = Get-PzArgumentInfo -Text $file.Text

    if ([System.IO.File]::Exists($full)) {
        $target = New-Object System.IO.FileInfo($full)
        if ((($target.Attributes -band [System.IO.FileAttributes]::ReadOnly)) -ne 0) {
            throw "Target file is read-only: $full"
        }
    }
    $expected = $null
    if ([System.IO.File]::Exists($full)) { $expected = [System.IO.File]::ReadAllBytes($full) }

    Write-PzFileAtomic -Path $full -Bytes $file.Bytes -ExpectedBytes $expected -Validate {
        param($TempPath)
        $null = Get-PzConfiguration -Path $TempPath
    }

    $result = Get-PzConfiguration -Path $full
    if ($result.HeapBytes -ne $info.HeapBytes) {
        throw "Restore verification failed: the file now reports '$($result.Argument)'. The backup is still at $backup."
    }
    return $result
}

function Find-PzInstallations {
    [CmdletBinding()]
    param([string[]]$SteamPaths)

    $roots = New-Object System.Collections.Generic.List[string]

    if ($PSBoundParameters.ContainsKey('SteamPaths') -and $null -ne $SteamPaths) {
        foreach ($candidate in $SteamPaths) {
            if (-not [string]::IsNullOrWhiteSpace($candidate)) { $roots.Add([string]$candidate) }
        }
    }
    else {
        $keys = @(
            @{ Path = 'HKCU:\Software\Valve\Steam'; Name = 'SteamPath' },
            @{ Path = 'HKLM:\SOFTWARE\WOW6432Node\Valve\Steam'; Name = 'InstallPath' },
            @{ Path = 'HKLM:\SOFTWARE\Valve\Steam'; Name = 'InstallPath' }
        )
        foreach ($key in $keys) {
            $value = $null
            try {
                $item = Get-ItemProperty -LiteralPath $key.Path -Name $key.Name -ErrorAction Stop
                $value = $item.($key.Name)
            }
            catch {
                $value = $null
            }
            if (-not [string]::IsNullOrWhiteSpace($value)) { $roots.Add([string]$value) }
        }
    }

    $libraries = New-Object System.Collections.Generic.List[string]
    foreach ($root in $roots) {
        $clean = ([string]$root).Trim().Trim('"')
        if ([string]::IsNullOrWhiteSpace($clean)) { continue }
        $clean = $clean.Replace('/', '\')
        try { $clean = Resolve-PzLiteralPath -Path $clean } catch { continue }
        if (-not [System.IO.Directory]::Exists($clean)) { continue }

        Add-PzUniquePath -List $libraries -Path $clean

        $vdf = [System.IO.Path]::Combine($clean, 'steamapps', 'libraryfolders.vdf')
        if (-not [System.IO.File]::Exists($vdf)) { continue }

        $text = ''
        try { $text = [System.IO.File]::ReadAllText($vdf) } catch { continue }

        foreach ($match in [regex]::Matches($text, '(?i)"path"\s*"((?:[^"\\]|\\.)*)"')) {
            $library = (Convert-PzVdfString -Value $match.Groups[1].Value).Trim()
            if ([string]::IsNullOrWhiteSpace($library)) { continue }
            try { $library = Resolve-PzLiteralPath -Path $library } catch { continue }
            if ([System.IO.Directory]::Exists($library)) { Add-PzUniquePath -List $libraries -Path $library }
        }
    }

    $installations = New-Object System.Collections.Generic.List[string]
    foreach ($library in $libraries) {
        $manifest = [System.IO.Path]::Combine($library, 'steamapps', 'appmanifest_108600.acf')
        if (-not [System.IO.File]::Exists($manifest)) { continue }

        $text = ''
        try { $text = [System.IO.File]::ReadAllText($manifest) } catch { continue }

        $match = [regex]::Match($text, '(?i)"installdir"\s*"((?:[^"\\]|\\.)*)"')
        if (-not $match.Success) { continue }

        # Folder names keep their inner spaces; only the quoted value is used.
        $installDir = Convert-PzVdfString -Value $match.Groups[1].Value
        if ([string]::IsNullOrWhiteSpace($installDir)) { continue }

        $gameDirectory = [System.IO.Path]::Combine($library, 'steamapps', 'common', $installDir)
        $json = [System.IO.Path]::Combine($gameDirectory, 'ProjectZomboid64.json')
        if ([System.IO.File]::Exists($json) -or [System.IO.File]::Exists($json + '.bak')) {
            Add-PzUniquePath -List $installations -Path $gameDirectory
        }
    }

    return $installations.ToArray()
}

# ---------------------------------------------------------------------------
# Console UI
# ---------------------------------------------------------------------------

function Get-PzUiStrings {
    param([Parameter(Mandatory = $true)][string]$Language)

    $zh = @{
        Title           = "Project Zomboid RAM Configurator v$script:PzVersion — 記憶體配置器"
        Detecting       = '正在偵測系統記憶體與遊戲安裝位置...'
        TotalRam        = '系統實體記憶體：{0} GB'
        TotalRamUnknown = '無法偵測系統實體記憶體，因此不提供建議值，也不會寫入任何設定。'
        FreeRam         = '目前可用記憶體：{0} GB'
        FreeRamUnknown  = '目前可用記憶體：無法偵測'
        GameFile        = '設定檔位置：{0}'
        CurrentLimit    = '目前記憶體上限：{0}（約 {1} GB）'
        CurrentUnknown  = '目前記憶體上限：無法讀取（設定檔可能已損壞）'
        ReadFailed      = '讀取設定檔失敗：{0}'
        Recommend       = '建議上限：{0} GB'
        RecommendHow    = '重度 MOD 起始策略：min(實體記憶體 x 80%, 實體記憶體 - 4 GB)，向下取整；不是官方最佳值。'
        RecommendNone   = '實體記憶體小於 8 GB，不提供自動建議值；你仍可自行輸入數值。'
        NoteHeader      = '說明：'
        NoteNotUsed     = '- 這是「上限」，不會立刻占用記憶體，也不代表遊戲一定會用到。'
        NoteNoFps       = '- 調高上限不保證提升 FPS，只是讓重度 MOD 較不容易因記憶體不足而崩潰。'
        NoteCloseGame   = '- 操作前請先完全關閉遊戲。'
        NoteNextLaunch  = '- 修改會在下一次啟動遊戲時生效。'
        NoteAboveRec    = '- 目前設定已高於建議值，若你刻意這樣設定就保持原狀即可，本工具不會自動調低。'
        Searching       = '正在搜尋 Steam 的 Project Zomboid 安裝目錄...'
        NoneFound       = '找不到 Project Zomboid 安裝目錄。'
        EnterFolder     = '請輸入遊戲資料夾（可直接把資料夾拖進視窗），或輸入 0 離開'
        InvalidFolder   = '在該資料夾找不到 ProjectZomboid64.json 或其 .bak 備份，請確認是遊戲主目錄。'
        MultipleFound   = '找到多個安裝，請選擇要設定的目錄：'
        ChooseInstall   = '請輸入編號（0 離開）'
        MenuHeader      = '請選擇操作：'
        Menu1           = '1. 套用建議值 {0} GB'
        Menu1Disabled   = '1. 套用建議值（目前無法提供建議值）'
        Menu2           = '2. 自行輸入數值（GB）'
        Menu3           = '3. 從備份還原（{0}）'
        Menu0           = '0. 離開'
        MenuPrompt      = '輸入選項'
        InvalidChoice   = '選項無效，請重新輸入。'
        RecUnavailable  = '目前沒有可用的建議值，請改用自行輸入或離開。'
        WriteDisabled   = '因為無法偵測系統記憶體，目前只能還原備份或離開。'
        CustomPrompt    = '請輸入記憶體上限（只輸入數字，單位 GB），或輸入 0 返回'
        CustomInvalid   = '請輸入正整數 GB，不要包含小數、文字或單位。'
        CustomTooHigh   = '請輸入小於系統實體記憶體 {0} GB 的整數；設定檔沒有變更。'
        WarnHeader      = '請注意：'
        WarnExceedRec   = '- {0} GB 高於建議值 {1} GB，系統其他程式可用的記憶體會變少。'
        WarnBelowCur    = '- {0} GB 低於目前設定的 {1} GB，這會調降遊戲可用的記憶體。'
        WarnFreeRam     = '- {0} GB 已接近或超過目前可用的 {1} GB，遊戲可能會變慢甚至無法啟動。'
        ConfirmApply    = '確定將記憶體上限設定為 {0} GB 嗎？(y/N)'
        ConfirmRestore  = '還原會覆蓋整份設定檔，可能撤銷遊戲更新或其他修改。確定使用備份嗎？(y/N)'
        Cancelled       = '已取消，設定檔沒有任何變更。'
        Applying        = '正在寫入設定...'
        ApplySuccess    = '設定成功，回讀結果：{0}（約 {1} GB）'
        ApplyUnchanged  = '目前設定已經是 {0} GB，沒有變更檔案，也沒有更新備份。'
        BackupAt        = '寫入前的原始檔已備份在：{0}'
        Restoring       = '正在從備份還原...'
        RestoreSuccess  = '還原完成，回讀結果：{0}（約 {1} GB）'
        BackupKept      = '備份檔已保留：{0}'
        Failed          = '操作失敗：{0}'
        RecoveryHeader  = '可以這樣處理：'
        Recovery1       = '- 如果先前已經有備份，可選擇「3. 從備份還原」把設定檔還原回去。'
        Recovery2       = '- 請先完全關閉遊戲與 Steam 的下載／驗證作業，再重新執行本工具。'
        Recovery3       = '- 若檔案是唯讀或權限不足，請取消唯讀屬性，或以管理員身分重新執行。'
        Recovery4       = '- 若設定檔本身已損壞，可用 Steam 的「驗證遊戲檔案完整性」還原它。'
        Bye             = '已離開。'
    }

    $cn = @{
        Title           = "Project Zomboid RAM Configurator v$script:PzVersion — 内存配置器"
        Detecting       = '正在检测系统内存与游戏安装位置...'
        TotalRam        = '系统物理内存：{0} GB'
        TotalRamUnknown = '无法检测系统物理内存，因此不提供推荐值，也不会写入任何配置。'
        FreeRam         = '当前可用内存：{0} GB'
        FreeRamUnknown  = '当前可用内存：无法检测'
        GameFile        = '配置文件位置：{0}'
        CurrentLimit    = '当前内存上限：{0}（约 {1} GB）'
        CurrentUnknown  = '当前内存上限：无法读取（配置文件可能已损坏）'
        ReadFailed      = '读取配置文件失败：{0}'
        Recommend       = '推荐上限：{0} GB'
        RecommendHow    = '重度 MOD 起始策略：min(物理内存 x 80%, 物理内存 - 4 GB)，向下取整；不是官方最优值。'
        RecommendNone   = '物理内存小于 8 GB，不提供自动推荐值；你仍然可以自己输入数值。'
        NoteHeader      = '说明：'
        NoteNotUsed     = '- 这是「上限」，不会立刻占用内存，也不代表游戏一定会用到。'
        NoteNoFps       = '- 调高上限不保证提升 FPS，只是让重度 MOD 不容易因内存不足而崩溃。'
        NoteCloseGame   = '- 操作前请先完全关闭游戏。'
        NoteNextLaunch  = '- 修改会在下一次启动游戏时生效。'
        NoteAboveRec    = '- 当前配置已高于推荐值，如果你是故意这样设置就保持原样，本工具不会自动调低。'
        Searching       = '正在搜索 Steam 的 Project Zomboid 安装目录...'
        NoneFound       = '找不到 Project Zomboid 安装目录。'
        EnterFolder     = '请输入游戏文件夹（可以直接把文件夹拖进窗口），或输入 0 退出'
        InvalidFolder   = '在该文件夹里找不到 ProjectZomboid64.json 或其 .bak 备份，请确认是游戏主目录。'
        MultipleFound   = '找到多个安装，请选择要配置的目录：'
        ChooseInstall   = '请输入编号（0 退出）'
        MenuHeader      = '请选择操作：'
        Menu1           = '1. 应用推荐值 {0} GB'
        Menu1Disabled   = '1. 应用推荐值（当前无法提供推荐值）'
        Menu2           = '2. 自己输入数值（GB）'
        Menu3           = '3. 从备份还原（{0}）'
        Menu0           = '0. 退出'
        MenuPrompt      = '输入选项'
        InvalidChoice   = '选项无效，请重新输入。'
        RecUnavailable  = '当前没有可用的推荐值，请改用自己输入或退出。'
        WriteDisabled   = '由于无法检测系统内存，当前只能还原备份或退出。'
        CustomPrompt    = '请输入内存上限（只输入数字，单位 GB），或输入 0 返回'
        CustomInvalid   = '请输入正整数 GB，不要包含小数、文字或单位。'
        CustomTooHigh   = '请输入小于系统物理内存 {0} GB 的整数；配置文件没有变更。'
        WarnHeader      = '请注意：'
        WarnExceedRec   = '- {0} GB 高于推荐值 {1} GB，系统里其他程序可用的内存会变少。'
        WarnBelowCur    = '- {0} GB 低于当前设置的 {1} GB，这会调低游戏可用的内存。'
        WarnFreeRam     = '- {0} GB 已接近或超过当前可用的 {1} GB，游戏可能会变慢甚至无法启动。'
        ConfirmApply    = '确定把内存上限设置为 {0} GB 吗？(y/N)'
        ConfirmRestore  = '还原会覆盖整个配置文件，可能撤销游戏更新或其他修改。确定使用备份吗？(y/N)'
        Cancelled       = '已取消，配置文件没有任何变更。'
        Applying        = '正在写入配置...'
        ApplySuccess    = '配置成功，回读结果：{0}（约 {1} GB）'
        ApplyUnchanged  = '当前配置已经是 {0} GB，没有变更文件，也没有更新备份。'
        BackupAt        = '写入前的原始文件已备份在：{0}'
        Restoring       = '正在从备份还原...'
        RestoreSuccess  = '还原完成，回读结果：{0}（约 {1} GB）'
        BackupKept      = '备份文件已保留：{0}'
        Failed          = '操作失败：{0}'
        RecoveryHeader  = '可以这样处理：'
        Recovery1       = '- 如果之前已经有备份，可以选择「3. 从备份还原」把配置文件恢复回去。'
        Recovery2       = '- 请先完全关闭游戏以及 Steam 的下载／验证任务，再重新运行本工具。'
        Recovery3       = '- 如果文件是只读或权限不足，请取消只读属性，或以管理员身份重新运行。'
        Recovery4       = '- 如果配置文件本身已损坏，可以用 Steam 的「验证游戏文件完整性」还原它。'
        Bye             = '已退出。'
    }

    $en = @{
        Title           = "Project Zomboid RAM Configurator v$script:PzVersion"
        Detecting       = 'Detecting system memory and game installation...'
        TotalRam        = 'Physical memory: {0} GB'
        TotalRamUnknown = 'Physical memory could not be detected, so no recommendation is offered and nothing will be written.'
        FreeRam         = 'Currently free memory: {0} GB'
        FreeRamUnknown  = 'Currently free memory: unknown'
        GameFile        = 'Configuration file: {0}'
        CurrentLimit    = 'Current memory limit: {0} (about {1} GB)'
        CurrentUnknown  = 'Current memory limit: unreadable (the configuration file may be damaged)'
        ReadFailed      = 'Reading the configuration file failed: {0}'
        Recommend       = 'Recommended limit: {0} GB'
        RecommendHow    = 'Heavy-mod starting policy: floor(min(physical RAM x 80%, physical RAM - 4 GB)); not an official optimum.'
        RecommendNone   = 'Physical memory is below 8 GB, so no automatic recommendation is offered; you can still enter a value yourself.'
        NoteHeader      = 'Notes:'
        NoteNotUsed     = '- This is an upper limit. It is not reserved up front and the game may never use all of it.'
        NoteNoFps       = '- A higher limit does not guarantee more FPS; it mainly reduces out-of-memory crashes with heavy mods.'
        NoteCloseGame   = '- Close the game completely before making changes.'
        NoteNextLaunch  = '- The change takes effect the next time the game starts.'
        NoteAboveRec    = '- The current value is already above the recommendation. If that is intentional, keep it; this tool never lowers it for you.'
        Searching       = 'Searching Steam for Project Zomboid installations...'
        NoneFound       = 'No Project Zomboid installation was found.'
        EnterFolder     = 'Enter the game folder (you can drag the folder into this window), or 0 to quit'
        InvalidFolder   = 'Neither ProjectZomboid64.json nor its .bak backup was found; choose the game root folder.'
        MultipleFound   = 'Multiple installations found, pick the one to configure:'
        ChooseInstall   = 'Enter a number (0 to quit)'
        MenuHeader      = 'Choose an action:'
        Menu1           = '1. Apply the recommended {0} GB'
        Menu1Disabled   = '1. Apply the recommended value (unavailable)'
        Menu2           = '2. Enter a value myself (GB)'
        Menu3           = '3. Restore from backup ({0})'
        Menu0           = '0. Quit'
        MenuPrompt      = 'Your choice'
        InvalidChoice   = 'Invalid choice, please try again.'
        RecUnavailable  = 'No recommendation is available; enter a value yourself or quit.'
        WriteDisabled   = 'Because physical memory could not be detected, only restoring a backup or quitting is possible.'
        CustomPrompt    = 'Enter the memory limit in GB (digits only), or 0 to go back'
        CustomInvalid   = 'Enter a positive whole number in GB, without decimals, text or units.'
        CustomTooHigh   = 'Enter a whole number below the installed {0} GB. The configuration was not changed.'
        WarnHeader      = 'Please note:'
        WarnExceedRec   = '- {0} GB is above the recommended {1} GB; less memory is left for the rest of the system.'
        WarnBelowCur    = '- {0} GB is below the current {1} GB; this lowers the memory available to the game.'
        WarnFreeRam     = '- {0} GB is close to or above the {1} GB currently free; the game may stutter or fail to start.'
        ConfirmApply    = 'Set the memory limit to {0} GB? (y/N)'
        ConfirmRestore  = 'Restore replaces the whole file and may undo game updates or other edits. Use this backup? (y/N)'
        Cancelled       = 'Cancelled. The configuration file was not changed.'
        Applying        = 'Writing the configuration...'
        ApplySuccess    = 'Done. Read back from the file: {0} (about {1} GB)'
        ApplyUnchanged  = 'The limit is already {0} GB, so no file was written and no backup was rotated.'
        BackupAt        = 'The original file was backed up to: {0}'
        Restoring       = 'Restoring from backup...'
        RestoreSuccess  = 'Restored. Read back from the file: {0} (about {1} GB)'
        BackupKept      = 'The backup file was kept at: {0}'
        Failed          = 'The operation failed: {0}'
        RecoveryHeader  = 'What you can do:'
        Recovery1       = '- If a backup already exists, pick "3. Restore from backup" to roll the file back.'
        Recovery2       = '- Close the game and any Steam download/verify job, then run this tool again.'
        Recovery3       = '- If the file is read-only or access is denied, clear the read-only flag or run as administrator.'
        Recovery4       = '- If the configuration file itself is damaged, use Steam "Verify integrity of game files" to restore it.'
        Bye             = 'Exited.'
    }

    if ($Language -eq 'en-US') { return $en }
    if ($Language -eq 'zh-CN') { return $cn }
    return $zh
}

function Get-PzCleanInput {
    param([AllowEmptyString()][AllowNull()][string]$Value)

    if ($null -eq $Value) { return '' }
    $text = $Value.Trim()
    while ($text.Length -ge 2 -and $text.StartsWith('"') -and $text.EndsWith('"')) {
        $text = $text.Substring(1, $text.Length - 2).Trim()
    }
    if ($text.Length -ge 2 -and $text.StartsWith("'") -and $text.EndsWith("'")) {
        $text = $text.Substring(1, $text.Length - 2)
    }
    return $text
}

function Show-PzError {
    param(
        [Parameter(Mandatory = $true)]$Strings,
        [Parameter(Mandatory = $true)][string]$Message
    )

    Write-Host ''
    Write-Host ($Strings.Failed.Replace('{0}', $Message)) -ForegroundColor Red
    Write-Host $Strings.RecoveryHeader -ForegroundColor Yellow
    Write-Host $Strings.Recovery1
    Write-Host $Strings.Recovery2
    Write-Host $Strings.Recovery3
    Write-Host $Strings.Recovery4
    Write-Host ''
}

function Invoke-PzConfigurator {
    [CmdletBinding()]
    param(
        [ValidateSet('zh-TW', 'zh-CN', 'en-US')]
        [string]$Language = 'zh-TW',

        [string]$GameDirectory
    )

    $ErrorActionPreference = 'Stop'
    try { [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false) } catch { }

    $strings = Get-PzUiStrings -Language $Language
    $gb = 1073741824.0

    Write-Host ''
    Write-Host $strings.Title -ForegroundColor Cyan
    Write-Host ('-' * 60)
    Write-Host $strings.Detecting

    $totalBytes = 0L
    $freeBytes = 0L
    try {
        # Sum the installed memory modules: Win32_ComputerSystem.TotalPhysicalMemory hides the
        # hardware-reserved region, which would push a 16 GB machine down to an 11 GB suggestion.
        $modules = Get-CimInstance -ClassName Win32_PhysicalMemory -ErrorAction Stop
        $sum = ($modules | Measure-Object -Property Capacity -Sum).Sum
        if ($sum -and $sum -gt 0) { $totalBytes = [long]$sum }
    }
    catch { $totalBytes = 0L }
    try {
        $os = Get-CimInstance -ClassName Win32_OperatingSystem -ErrorAction Stop
        if ($os -and $os.FreePhysicalMemory) { $freeBytes = [long]$os.FreePhysicalMemory * 1024L }
    }
    catch { $freeBytes = 0L }

    # ----- locate the configuration file -----
    $jsonPath = $null
    if (-not [string]::IsNullOrWhiteSpace($GameDirectory)) {
        $folder = Get-PzCleanInput -Value $GameDirectory
        $candidate = ''
        try { $candidate = [System.IO.Path]::Combine((Resolve-PzLiteralPath -Path $folder), 'ProjectZomboid64.json') } catch { $candidate = '' }
        if (-not $candidate -or (-not [System.IO.File]::Exists($candidate) -and -not [System.IO.File]::Exists($candidate + '.bak'))) {
            Write-Host ''
            Write-Host $strings.InvalidFolder -ForegroundColor Red
            Write-Host ''
            return
        }
        $jsonPath = $candidate
    }
    else {
        Write-Host $strings.Searching
        $found = @(Find-PzInstallations)

        if ($found.Count -eq 1) {
            $jsonPath = [System.IO.Path]::Combine($found[0], 'ProjectZomboid64.json')
        }
        elseif ($found.Count -gt 1) {
            while ($null -eq $jsonPath) {
                Write-Host ''
                Write-Host $strings.MultipleFound
                for ($i = 0; $i -lt $found.Count; $i++) {
                    Write-Host ('{0}. {1}' -f ($i + 1), $found[$i])
                }
                Write-Host $strings.Menu0
                $answer = Get-PzCleanInput -Value (Read-Host $strings.ChooseInstall)
                if ($answer -eq '0') {
                    Write-Host $strings.Bye
                    return
                }
                if ($answer -cmatch '^[1-9][0-9]{0,2}$' -and [int]$answer -le $found.Count) {
                    $jsonPath = [System.IO.Path]::Combine($found[[int]$answer - 1], 'ProjectZomboid64.json')
                }
                else {
                    Write-Host $strings.InvalidChoice -ForegroundColor Yellow
                }
            }
        }
        else {
            Write-Host $strings.NoneFound -ForegroundColor Yellow
            while ($null -eq $jsonPath) {
                $answer = Get-PzCleanInput -Value (Read-Host $strings.EnterFolder)
                if ($answer -eq '0') {
                    Write-Host $strings.Bye
                    return
                }
                if ([string]::IsNullOrWhiteSpace($answer)) {
                    Write-Host $strings.InvalidFolder -ForegroundColor Yellow
                    continue
                }
                $candidate = ''
                try { $candidate = [System.IO.Path]::Combine((Resolve-PzLiteralPath -Path $answer), 'ProjectZomboid64.json') } catch { $candidate = '' }
                if ($candidate -and ([System.IO.File]::Exists($candidate) -or [System.IO.File]::Exists($candidate + '.bak'))) { $jsonPath = $candidate }
                else { Write-Host $strings.InvalidFolder -ForegroundColor Yellow }
            }
        }
    }

    # ----- current state -----
    $current = $null
    $readError = $null
    try { $current = Get-PzConfiguration -Path $jsonPath } catch { $readError = $_.Exception.Message }

    $recommended = $null
    if ($totalBytes -gt 0) { $recommended = Get-PzMemoryRecommendation -TotalBytes $totalBytes }

    Write-Host ''
    Write-Host ($strings.GameFile.Replace('{0}', $jsonPath))
    if ($totalBytes -gt 0) {
        Write-Host ($strings.TotalRam -f ('{0:N2}' -f ($totalBytes / $gb)))
    }
    else {
        Write-Host $strings.TotalRamUnknown -ForegroundColor Yellow
    }
    if ($freeBytes -gt 0) {
        Write-Host ($strings.FreeRam -f ('{0:N2}' -f ($freeBytes / $gb)))
    }
    else {
        Write-Host $strings.FreeRamUnknown
    }

    if ($current) {
        Write-Host ($strings.CurrentLimit -f $current.Argument, ('{0:N2}' -f ($current.HeapBytes / $gb)))
    }
    else {
        Write-Host $strings.CurrentUnknown -ForegroundColor Yellow
        Write-Host ($strings.ReadFailed.Replace('{0}', $readError)) -ForegroundColor Yellow
    }

    if ($null -ne $recommended) {
        Write-Host ($strings.Recommend -f $recommended) -ForegroundColor Green
        Write-Host $strings.RecommendHow
    }
    elseif ($totalBytes -gt 0) {
        Write-Host $strings.RecommendNone -ForegroundColor Yellow
    }

    Write-Host ''
    Write-Host $strings.NoteHeader
    Write-Host $strings.NoteNotUsed
    Write-Host $strings.NoteNoFps
    Write-Host $strings.NoteCloseGame
    Write-Host $strings.NoteNextLaunch
    if ($current -and $null -ne $recommended -and $current.HeapBytes -gt ([long]$recommended * 1073741824L)) {
        Write-Host $strings.NoteAboveRec
    }
    if ($totalBytes -le 0) {
        Write-Host ''
        Write-Host $strings.WriteDisabled -ForegroundColor Yellow
    }

    $backupPath = $jsonPath + '.bak'

    while ($true) {
        Write-Host ''
        Write-Host $strings.MenuHeader
        if ($null -ne $recommended) { Write-Host ($strings.Menu1 -f $recommended) }
        else { Write-Host $strings.Menu1Disabled }
        Write-Host $strings.Menu2
        Write-Host ($strings.Menu3.Replace('{0}', $backupPath))
        Write-Host $strings.Menu0

        $choice = Get-PzCleanInput -Value (Read-Host $strings.MenuPrompt)
        $wanted = $null

        if ($choice -eq '0') {
            Write-Host $strings.Bye
            break
        }
        elseif ($choice -eq '3') {
            $confirm = Get-PzCleanInput -Value (Read-Host ($strings.ConfirmRestore))
            if ($confirm -notmatch '^[yY]$') {
                Write-Host $strings.Cancelled
                continue
            }
            Write-Host $strings.Restoring
            try {
                $restored = Restore-PzMemory -Path $jsonPath
                Write-Host ($strings.RestoreSuccess -f $restored.Argument, ('{0:N2}' -f ($restored.HeapBytes / $gb))) -ForegroundColor Green
                Write-Host ($strings.BackupKept.Replace('{0}', $backupPath))
                Write-Host $strings.NoteCloseGame
                Write-Host $strings.NoteNextLaunch
                $current = $restored
            }
            catch {
                Show-PzError -Strings $strings -Message $_.Exception.Message
            }
            continue
        }
        elseif ($choice -eq '1') {
            if ($null -eq $recommended) {
                Write-Host $strings.RecUnavailable -ForegroundColor Yellow
                continue
            }
            $wanted = [string]$recommended
        }
        elseif ($choice -eq '2') {
            if ($totalBytes -le 0) {
                Write-Host $strings.WriteDisabled -ForegroundColor Yellow
                continue
            }
            while ($null -eq $wanted) {
                $answer = Get-PzCleanInput -Value (Read-Host $strings.CustomPrompt)
                if ($answer -eq '0') { break }
                if ($answer -cmatch '^[1-9][0-9]{0,5}$') { $wanted = $answer }
                else { Write-Host $strings.CustomInvalid -ForegroundColor Yellow }
            }
            if ($null -eq $wanted) { continue }
        }
        else {
            Write-Host $strings.InvalidChoice -ForegroundColor Yellow
            continue
        }

        if ($totalBytes -le 0) {
            Write-Host $strings.WriteDisabled -ForegroundColor Yellow
            continue
        }

        $wantedBytes = [long]$wanted * 1073741824L
        if ($wantedBytes -ge $totalBytes) {
            Write-Host ($strings.CustomTooHigh -f ('{0:N2}' -f ($totalBytes / $gb))) -ForegroundColor Yellow
            continue
        }
        try {
            $confirmedFile = Read-PzFileText -Path $jsonPath
            $current = Get-PzArgumentInfo -Text $confirmedFile.Text
        }
        catch {
            Show-PzError -Strings $strings -Message $_.Exception.Message
            continue
        }
        Write-Host ($strings.CurrentLimit -f $current.Argument, ('{0:N2}' -f ($current.HeapBytes / $gb)))
        $warnings = New-Object System.Collections.Generic.List[string]
        if ($null -ne $recommended -and [int]$wanted -gt $recommended) {
            $warnings.Add(($strings.WarnExceedRec -f $wanted, $recommended))
        }
        if ($current -and $wantedBytes -lt $current.HeapBytes) {
            $warnings.Add(($strings.WarnBelowCur -f $wanted, ('{0:N2}' -f ($current.HeapBytes / $gb))))
        }
        if ($freeBytes -gt 0 -and $wantedBytes -ge ($freeBytes * 0.9)) {
            $warnings.Add(($strings.WarnFreeRam -f $wanted, ('{0:N2}' -f ($freeBytes / $gb))))
        }
        if ($warnings.Count -gt 0) {
            Write-Host ''
            Write-Host $strings.WarnHeader -ForegroundColor Yellow
            foreach ($warning in $warnings) { Write-Host $warning -ForegroundColor Yellow }
        }

        $confirm = Get-PzCleanInput -Value (Read-Host ($strings.ConfirmApply -f $wanted))
        if ($confirm -notmatch '^[yY]$') {
            Write-Host $strings.Cancelled
            continue
        }

        Write-Host $strings.Applying
        try {
            $result = Set-PzMemory -Path $jsonPath -Gigabytes $wanted -TotalBytes $totalBytes -ExpectedBytes $confirmedFile.Bytes
            Write-Host ''
            if (-not $result.Changed) {
                Write-Host ($strings.ApplyUnchanged -f $wanted) -ForegroundColor Green
            }
            else {
                Write-Host ($strings.ApplySuccess -f $result.Argument, ('{0:N2}' -f ($result.HeapBytes / $gb))) -ForegroundColor Green
                Write-Host ($strings.BackupAt.Replace('{0}', $backupPath))
            }
            Write-Host $strings.NoteCloseGame
            Write-Host $strings.NoteNextLaunch
            $current = $result
            break
        }
        catch {
            Show-PzError -Strings $strings -Message $_.Exception.Message
        }
    }

}

# Dot-sourcing loads the functions only; running the file starts the UI.
if ($MyInvocation.InvocationName -ne '.') {
    $arguments = @{ Language = $Language }
    if ($PSBoundParameters.ContainsKey('GameDirectory')) { $arguments['GameDirectory'] = $GameDirectory }
    Invoke-PzConfigurator @arguments
}
