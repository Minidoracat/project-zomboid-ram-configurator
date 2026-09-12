#Requires -Version 5.1
<#
    PZ Memory Configurator 行為回歸測試（無框架、Windows PowerShell 5.1 自包含）

    執行方式：
        powershell.exe -NoProfile -ExecutionPolicy Bypass -File tests\MemoryConfigurator.Tests.ps1

    只在唯一暫存目錄操作合成的 JSON / VDF / ACF，不需安裝 Project Zomboid 或 Steam，
    不啟動 UI，也不會碰到真實遊戲設定。任何案例失敗即 throw（結束碼非 0）。
#>
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$corePath = Join-Path (Split-Path -Parent $PSScriptRoot) 'PZ_Memory_Configurator.ps1'
if (-not (Test-Path -LiteralPath $corePath)) { throw "找不到核心腳本：$corePath" }
. $corePath

$script:CaseCount = 0
$script:Utf8NoBom = New-Object System.Text.UTF8Encoding($false)

$tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ('pzmc-tests-' + [guid]::NewGuid().ToString('N'))
[System.IO.Directory]::CreateDirectory($tempRoot) | Out-Null

# 測試與檔案小工具

function Invoke-Case {
    param([string]$Name, [scriptblock]$Body)
    try { & $Body | Out-Null }
    catch { throw ("[FAIL] {0} -> {1}" -f $Name, $_.Exception.Message) }
    $script:CaseCount++
    Write-Host ("  [PASS] {0}" -f $Name)
}

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw $Message }
}

function Assert-Equal {
    param($Expected, $Actual, [string]$Message)
    if ($Expected -ne $Actual) {
        throw ("{0}（預期 '{1}'，實際 '{2}'）" -f $Message, $Expected, $Actual)
    }
}

function Assert-Throws {
    param([scriptblock]$Body, [string]$Message)
    $threw = $false
    try { & $Body | Out-Null } catch { $threw = $true }
    if (-not $threw) { throw ("應該失敗但成功了：{0}" -f $Message) }
}

function New-CaseDir {
    param([string]$Name)
    $dir = Join-Path $tempRoot $Name
    [System.IO.Directory]::CreateDirectory($dir) | Out-Null
    $dir
}

function Write-Text {
    param([string]$Path, [string]$Text)
    [System.IO.Directory]::CreateDirectory([System.IO.Path]::GetDirectoryName($Path)) | Out-Null
    [System.IO.File]::WriteAllText($Path, $Text, $script:Utf8NoBom)
}

function Read-Text {
    param([string]$Path)
    [System.IO.File]::ReadAllText($Path, [System.Text.Encoding]::UTF8)
}

function Get-Sha {
    param([string]$Path)
    $hash = [System.Security.Cryptography.SHA256]::Create()
    try { [Convert]::ToBase64String($hash.ComputeHash([System.IO.File]::ReadAllBytes($Path))) }
    finally { $hash.Dispose() }
}

# 合成的 ProjectZomboid64.json：含巢狀欄位與中文，用來驗證「只改 Xmx」
function New-GameJson {
    param([string]$Path, [string[]]$VmArgs)
    $argLines = ($VmArgs | ForEach-Object { '        "' + $_ + '"' }) -join ",`r`n"
    $text = @"
{
    "mainClass": "zombie/gameStates/MainScreenState",
    "classpath": [ "." ],
    "note": "遊戲設定 – 請勿破壞",
    "nested": { "launcher": { "profiles": [ 1, 2, 3 ], "label": "中文標籤" } },
    "vmArgs": [
$argLines
    ]
}
"@
    Write-Text -Path $Path -Text $text
    $Path
}

function New-SteamLibrary {
    param([string]$Root, [string]$InstallDir, [switch]$WithJson)
    $common = Join-Path $Root 'steamapps\common'
    [System.IO.Directory]::CreateDirectory($common) | Out-Null
    if (-not $InstallDir) { return }
    $text = @"
"AppState"
{
	"appid"		"108600"
	"name"		"Project Zomboid"
	"installdir"		"$InstallDir"
	"LastUpdated"		"1700000000"
}
"@

    Write-Text -Path (Join-Path $Root 'steamapps\appmanifest_108600.acf') -Text $text
    $gameDir = Join-Path $common $InstallDir
    [System.IO.Directory]::CreateDirectory($gameDir) | Out-Null
    if ($WithJson) {
        New-GameJson -Path (Join-Path $gameDir 'ProjectZomboid64.json') -VmArgs @('-Xms1024m', '-Xmx3072m') | Out-Null
    }
    $gameDir
}

# 故意混用 tab / 空白縮排與 \\ / \ 兩種轉義，確保解析不依賴固定縮排或位置
function New-LibraryFoldersVdf {
    param([string]$Root, [string[]]$Paths)
    $entries = for ($index = 0; $index -lt $Paths.Count; $index++) {
        $p = $Paths[$index]
        if ($index % 2 -eq 0) {
            $value = $p -replace '\\', '\\'
            @"
	"$index"
	{
		"path"		"$value"
		"label"		""
		"apps"
		{
			"108600"		"9999"
		}
	}
"@
        } else {
            @"
     "$index"
     {
               "path"     "$p"
     }
"@
        }
    }
    Write-Text -Path (Join-Path $Root 'steamapps\libraryfolders.vdf') -Text (
        "`"libraryfolders`"`r`n{`r`n" + ($entries -join "`r`n") + "`r`n}")
}

# 記憶體規則與設定解析

Write-Host "PZ Memory Configurator 回歸測試"

try {

    Invoke-Case '建議值公式：80% 與「總量 - 4GB」取小後向下取整' {
        $rows = @(
            @{ Bytes = 8GB;          Expected = 4 },
            @{ Bytes = 24GB;         Expected = 19 },
            @{ Bytes = 16GB - 128MB; Expected = 11 }
        )
        foreach ($row in $rows) {
            $actual = Get-PzMemoryRecommendation -TotalBytes ([long]$row.Bytes)
            Assert-Equal $row.Expected $actual ("{0} bytes 的建議值" -f $row.Bytes)
        }
    }

    Invoke-Case '建議值：不足 8GiB 或偵測失敗時回傳 $null' {
        foreach ($bytes in @(0, -1, (8GB - 1))) {
            $actual = Get-PzMemoryRecommendation -TotalBytes ([long]$bytes)
            Assert-Equal $null $actual ("{0} bytes 不應給建議值" -f $bytes)
        }
    }

    Invoke-Case 'Get-PzConfiguration：解析無單位與 k/m/g（大小寫皆可）' {
        $dir = New-CaseDir 'parse'
        $rows = @(
            @{ Arg = '-Xmx12g';        Bytes = 12GB },
            @{ Arg = '-Xmx16G';        Bytes = 16GB },
            @{ Arg = '-Xmx3072m';      Bytes = 3072MB },
            @{ Arg = '-Xmx4096M';      Bytes = 4096MB },
            @{ Arg = '-Xmx8388608k';   Bytes = 8388608KB },
            @{ Arg = '-Xmx6291456K';   Bytes = 6291456KB },
            @{ Arg = '-Xmx2147483648'; Bytes = 2147483648 }
        )
        foreach ($row in $rows) {
            $path = New-GameJson -Path (Join-Path $dir 'parse.json') -VmArgs @('-Djava.awt.headless=true', '-Xms1024m', $row.Arg)
            $cfg = Get-PzConfiguration -Path $path
            Assert-Equal ([long]$row.Bytes) $cfg.HeapBytes ("{0} 的 HeapBytes" -f $row.Arg)
            Assert-Equal $row.Arg $cfg.Argument ("{0} 的 Argument 應為完整 vmArgs 項目" -f $row.Arg)
        }
    }

    Invoke-Case 'Get-PzConfiguration：缺失 / 重複 / 非法 Xmx 一律拒絕' {
        $path = Join-Path (New-CaseDir 'parse-reject') 'bad.json'
        $rows = @(
            @{ Name = '沒有 Xmx';        Text = '{ "vmArgs": [ "-Xms1024m" ] }' },
            @{ Name = '兩個 Xmx';        Text = '{ "vmArgs": [ "-Xmx4g", "-Xmx8g" ] }' },
            @{ Name = '小數';            Text = '{ "vmArgs": [ "-Xmx1.5g" ] }' },
            @{ Name = '單位錯誤';        Text = '{ "vmArgs": [ "-Xmx12gb" ] }' },
            @{ Name = '非數字';          Text = '{ "vmArgs": [ "-Xmxabc" ] }' },
            @{ Name = '負數';            Text = '{ "vmArgs": [ "-Xmx-4g" ] }' },
            @{ Name = '空值';            Text = '{ "vmArgs": [ "-Xmx" ] }' },
            @{ Name = '千分位';          Text = '{ "vmArgs": [ "-Xmx12,288m" ] }' },
            @{ Name = '壞 JSON';         Text = '{ "vmArgs": [ "-Xmx4g" ' },
            @{ Name = '空檔案';          Text = '' },
            @{ Name = '沒有 vmArgs';     Text = '{ "mainClass": "zombie/gameStates/MainScreenState" }' },
            @{ Name = '只有巢狀 vmArgs'; Text = '{ "launcher": { "vmArgs": [ "-Xmx4g" ] } }' }
        )
        foreach ($row in $rows) {
            Write-Text -Path $path -Text $row.Text
            Assert-Throws { Get-PzConfiguration -Path $path } $row.Name
        }
    }

    Invoke-Case '模糊或非標準 JSON 不得覆寫設定或既有備份' {
        $dir = New-CaseDir 'ambiguous-json'
        $path = Join-Path $dir 'ProjectZomboid64.json'
        New-GameJson -Path "$path.bak" -VmArgs @('-Xmx3g') | Out-Null
        $backupHash = Get-Sha "$path.bak"
        foreach ($text in @(
            '{"vmArgs":[],"vmArgs":["-Xmx4g"]}',
            '{"vmArgs":[],"vm\u0041rgs":["-Xmx4g"]}',
            '{"__type":"a","__type":"b","vmArgs":["-Xmx4g"]}',
            '{"__type":"a","\u005f_type":"b","vmArgs":["-Xmx4g"]}',
            '{"VMARGS":["-Xmx4g"]}',
            "{vmArgs:['-Xmx4g']}",
            "{'vmArgs':['-Xmx4g']}",
            '{"vmArgs":["-Xmx4g"],"n":01}',
            '{"vmArgs":["-Xmx4g"],"n":NaN}',
            '{"vmArgs":["-Xmx4g"],}',
            '{"vmArgs":"-Xmx4g"}',
            '{"vmArgs":["-Xmx0g"]}'
        )) {
            Write-Text $path $text
            $originalHash = Get-Sha $path
            Assert-Throws { Set-PzMemory -Path $path -Gigabytes 8 -TotalBytes 16GB } $text
            Assert-Equal $originalHash (Get-Sha $path) '拒絕模糊 JSON 後原檔不得變更'
            Assert-Equal $backupHash (Get-Sha "$path.bak") '不得把模糊 JSON 輪替進既有備份'
        }
    }

    Invoke-Case '確認後被外部修改的設定不得覆寫或輪替備份' {
        $path = New-GameJson -Path (Join-Path (New-CaseDir 'confirmation-race') 'ProjectZomboid64.json') -VmArgs @('-Xmx4g')
        $confirmed = [System.IO.File]::ReadAllBytes($path)
        New-GameJson -Path "$path.bak" -VmArgs @('-Xmx3g') | Out-Null
        New-GameJson -Path $path -VmArgs @('-Xmx6g') | Out-Null
        $targetHash = Get-Sha $path
        $backupHash = Get-Sha "$path.bak"
        Assert-Throws { Set-PzMemory -Path $path -Gigabytes 4 -TotalBytes 16GB -ExpectedBytes $confirmed } '確認後外部修改'
        Assert-Equal $targetHash (Get-Sha $path) '不得覆寫未經確認的新設定'
        Assert-Equal $backupHash (Get-Sha "$path.bak") '不得輪替既有備份'
    }

    Invoke-Case '主檔遺失時仍可找到安裝並從備份還原' {
        $steam = New-CaseDir 'backup-only'
        $game = New-SteamLibrary -Root $steam -InstallDir 'Project Zomboid'
        $path = Join-Path $game 'ProjectZomboid64.json'
        New-GameJson -Path "$path.bak" -VmArgs @('-Xmx4g') | Out-Null
        $backupHash = Get-Sha "$path.bak"
        Assert-Equal $game (@(Find-PzInstallations -SteamPaths @($steam)) -join '') '僅有備份仍應找到安裝'
        Assert-Equal ([long]4GB) (Restore-PzMemory -Path $path).HeapBytes '還原遺失的主檔'
        Assert-Equal $backupHash (Get-Sha $path) '主檔完整重建'
        Assert-Equal $backupHash (Get-Sha "$path.bak") '備份仍保留'
    }

    Invoke-Case '替換被檔案鎖阻擋時保留候選檔、舊備份與回復位置' {
        $dir = New-CaseDir 'replace-locked'
        $path = New-GameJson -Path (Join-Path $dir 'ProjectZomboid64.json') -VmArgs @('-Xmx4g')
        New-GameJson -Path "$path.bak" -VmArgs @('-Xmx3g') | Out-Null
        $targetHash = Get-Sha $path
        $backupHash = Get-Sha "$path.bak"
        $lock = [System.IO.File]::Open($path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::Read)
        $message = ''
        try {
            try { Set-PzMemory -Path $path -Gigabytes 8 -TotalBytes 16GB | Out-Null }
            catch { $message = $_.Exception.Message }
        }
        finally { $lock.Dispose() }
        Assert-True ($message.Length -gt 0) '替換失敗必須回報'
        Assert-Equal $targetHash (Get-Sha $path) '被鎖定原檔不得變更'
        Assert-Equal $backupHash (Get-Sha "$path.bak") '舊備份不得變更'
        $recovery = @(Get-ChildItem -LiteralPath $dir -File | Where-Object { $_.FullName -ne $path -and $_.FullName -ne "$path.bak" })
        Assert-True (@($recovery | Where-Object { (Get-Sha $_.FullName) -eq $backupHash }).Count -gt 0) '保留舊備份的回復副本'
        Assert-True (@($recovery | Where-Object { (Read-Text $_.FullName).Contains('-Xmx8g') }).Count -gt 0) '保留已驗證的新設定'
        foreach ($file in $recovery) {
            Assert-True ($message.Contains($file.FullName)) '錯誤必須指出回復檔案位置'
        }
    }

    Invoke-Case '預期缺失的主檔在還原期間出現時，不得覆蓋新檔' {
        $path = Join-Path (New-CaseDir 'restore-create-race') 'ProjectZomboid64.json'
        $candidate = [System.Text.Encoding]::UTF8.GetBytes('{"vmArgs":["-Xmx4g"]}')
        $external = '{"vmArgs":["-Xmx12g"]}'
        Assert-Throws {
            Write-PzFileAtomic -Path $path -Bytes $candidate -Validate {
                param($TempPath)
                $null = Get-PzConfiguration -Path $TempPath
                [System.IO.File]::WriteAllText($path, $external)
            }
        } '驗證暫存檔期間模擬另一程序建立主檔'
        Assert-Equal $external (Read-Text $path) '不得覆蓋新出現的主檔'
    }

    # 寫入、備份與還原
    Invoke-Case 'Set-PzMemory：只改 Xmx，保留其他 vmArgs / 巢狀欄位 / 中文，備份等於原始位元組' {
        $dir = New-CaseDir 'set-ok'
        $others = @('-Djava.awt.headless=true', '-Xms1024m', '-XX:+UseZGC', '-Dzomboid.note=中文參數', '-Dsun.java2d.noddraw=true')
        $path = New-GameJson -Path (Join-Path $dir 'ProjectZomboid64.json') -VmArgs ($others + '-Xmx3072m')
        $originalSha = Get-Sha $path
        $expected = (Read-Text $path).Replace('-Xmx3072m', '-Xmx12g')
        $cfg = Set-PzMemory -Path $path -Gigabytes '12' -TotalBytes ([long]16GB)
        Assert-Equal ([long]12GB) $cfg.HeapBytes '回傳的 HeapBytes'
        Assert-True ($cfg.Argument -match '^-Xmx12[gG]$') ("回傳的 Argument 應為 -Xmx12g，實際 '{0}'" -f $cfg.Argument)

        $bak = "$path.bak"
        Assert-Equal $originalSha (Get-Sha $bak) '.bak 必須逐位元組等於寫入前的原始檔'

        Assert-Equal $expected (Read-Text $path) '除 Xmx 外的文字、縮排、順序與中文皆須原樣保留'
        Assert-Equal ([long]12GB) (Get-PzConfiguration -Path $path).HeapBytes '寫回的檔案必須能重新解析'
    }

    Invoke-Case 'Set-PzMemory：非法 / 超大 / 達到實體記憶體的輸入一律拒絕且不動原檔與備份' {
        $dir = New-CaseDir 'set-reject'
        $path = New-GameJson -Path (Join-Path $dir 'ProjectZomboid64.json') -VmArgs @('-Djava.awt.headless=true', '-Xms1024m', '-Xmx3072m')
        $sha = Get-Sha $path
        $values = @(
            '0', '', ' ', 'abc', '1.5', '-4', '12g', '0x8', '1e3', '12,288', '１２', '١٢',
            '12; Remove-Item C:\ -Recurse', '12" & calc', '$(1+1)',
            '999999999999999999999', '16', '20'
        )
        foreach ($value in $values) {
            Assert-Throws { Set-PzMemory -Path $path -Gigabytes $value -TotalBytes ([long]16GB) } ("輸入 '{0}'" -f $value)
            Assert-Equal $sha (Get-Sha $path) ("拒絕 '{0}' 後原檔不得變更" -f $value)
            Assert-True (-not (Test-Path -LiteralPath "$path.bak")) ("拒絕 '{0}' 後不得產生備份" -f $value)
        }
    }

    Invoke-Case 'Set-PzMemory：新值小於既有 Xms 時拒絕' {
        $dir = New-CaseDir 'set-xms'
        $path = New-GameJson -Path (Join-Path $dir 'ProjectZomboid64.json') -VmArgs @('-Xms8g', '-Xmx12g')
        $sha = Get-Sha $path
        Assert-Throws { Set-PzMemory -Path $path -Gigabytes '4' -TotalBytes ([long]16GB) } 'Xmx 4g 小於 Xms 8g'
        Assert-Equal $sha (Get-Sha $path) '拒絕後原檔不得變更'
        Assert-True (-not (Test-Path -LiteralPath "$path.bak")) '拒絕後不得產生備份'
    }

    Invoke-Case 'Set-PzMemory：唯讀目標寫入失敗且不動原檔' {
        $dir = New-CaseDir 'set-readonly'
        $path = New-GameJson -Path (Join-Path $dir 'ProjectZomboid64.json') -VmArgs @('-Xms1024m', '-Xmx3072m')
        $sha = Get-Sha $path
        [System.IO.File]::SetAttributes($path, [System.IO.FileAttributes]::ReadOnly)
        try {
            Assert-Throws { Set-PzMemory -Path $path -Gigabytes '8' -TotalBytes ([long]16GB) } '唯讀目標'
            Assert-Equal $sha (Get-Sha $path) '唯讀目標失敗後原檔不得變更'
        } finally {
            [System.IO.File]::SetAttributes($path, [System.IO.FileAttributes]::Normal)
        }
    }

    Invoke-Case 'Set-PzMemory：連續修改時備份保留上一版；相同值不寫檔也不輪替備份' {
        $dir = New-CaseDir 'set-rotate'
        $path = New-GameJson -Path (Join-Path $dir 'ProjectZomboid64.json') -VmArgs @('-Xms1024m', '-Xmx3072m')
        $bak = "$path.bak"
        $original = Get-Sha $path

        Set-PzMemory -Path $path -Gigabytes '8' -TotalBytes ([long]16GB) | Out-Null
        Assert-Equal $original (Get-Sha $bak) '第一次修改的備份應逐位元組等於原始檔'
        $afterFirst = Get-Sha $path

        Set-PzMemory -Path $path -Gigabytes '10' -TotalBytes ([long]16GB) | Out-Null
        Assert-Equal ([long]10GB) (Get-PzConfiguration -Path $path).HeapBytes '第二次修改應生效'
        Assert-Equal $afterFirst (Get-Sha $bak) '第二次修改的備份應逐位元組等於上一次的內容'

        $fileSha = Get-Sha $path
        $bakSha = Get-Sha $bak
        $fileTime = [System.IO.File]::GetLastWriteTimeUtc($path)
        $bakTime = [System.IO.File]::GetLastWriteTimeUtc($bak)

        $cfg = Set-PzMemory -Path $path -Gigabytes '10' -TotalBytes ([long]16GB)
        Assert-Equal ([long]10GB) $cfg.HeapBytes '相同值仍應回傳目前設定'
        Assert-Equal $fileSha (Get-Sha $path) '相同值不得改動目標檔'
        Assert-Equal $bakSha (Get-Sha $bak) '相同值不得改動備份內容'
        Assert-Equal $fileTime ([System.IO.File]::GetLastWriteTimeUtc($path)) '相同值不得重寫目標檔'
        Assert-Equal $bakTime ([System.IO.File]::GetLastWriteTimeUtc($bak)) '相同值不得輪替備份'
    }

    Invoke-Case 'Restore-PzMemory：完整還原原始檔並保留備份' {
        $dir = New-CaseDir 'restore-ok'
        $path = New-GameJson -Path (Join-Path $dir 'ProjectZomboid64.json') -VmArgs @('-Djava.awt.headless=true', '-Xms1024m', '-Xmx3072m', '-Dzomboid.note=中文參數')
        $originalSha = Get-Sha $path

        Set-PzMemory -Path $path -Gigabytes '8' -TotalBytes ([long]16GB) | Out-Null
        $cfg = Restore-PzMemory -Path $path
        Assert-Equal ([long]3072MB) $cfg.HeapBytes '還原後應回傳原始設定'
        Assert-Equal $originalSha (Get-Sha $path) '還原後應逐位元組等於原始檔'
        Assert-Equal $originalSha (Get-Sha "$path.bak") '還原後備份必須保留且內容不變'
    }

    Invoke-Case 'Restore-PzMemory：目標檔壞掉但備份有效時仍可還原' {
        $dir = New-CaseDir 'restore-broken-target'
        $path = New-GameJson -Path (Join-Path $dir 'ProjectZomboid64.json') -VmArgs @('-Xms1024m', '-Xmx3072m')
        $originalSha = Get-Sha $path

        Set-PzMemory -Path $path -Gigabytes '8' -TotalBytes ([long]16GB) | Out-Null
        Write-Text -Path $path -Text '{ 這不是有效的 JSON'

        $cfg = Restore-PzMemory -Path $path
        Assert-Equal ([long]3072MB) $cfg.HeapBytes '還原後應回傳原始設定'
        Assert-Equal $originalSha (Get-Sha $path) '還原後應逐位元組等於原始檔'
        Assert-Equal $originalSha (Get-Sha "$path.bak") '備份不得被壞資料覆寫'
    }

    Invoke-Case 'Restore-PzMemory：缺失 / 損壞備份與唯讀目標一律拒絕且不動原檔' {
        $dir = New-CaseDir 'restore-reject'

        $noBak = New-GameJson -Path (Join-Path $dir 'no-backup.json') -VmArgs @('-Xmx3072m')
        $noBakSha = Get-Sha $noBak
        Assert-Throws { Restore-PzMemory -Path $noBak } '沒有備份'
        Assert-Equal $noBakSha (Get-Sha $noBak) '沒有備份時不得動原檔'

        $badBaks = @(
            @{ Name = '備份是壞 JSON'; Text = '{ "vmArgs": [ "-Xmx4g" ' },
            @{ Name = '備份沒有 Xmx'; Text = '{ "vmArgs": [ "-Xms1024m" ] }' },
            @{ Name = '備份是空檔案'; Text = '' }
        )
        foreach ($row in $badBaks) {
            $path = New-GameJson -Path (Join-Path $dir 'bad-backup.json') -VmArgs @('-Xmx8g')
            Write-Text -Path "$path.bak" -Text $row.Text
            $pathSha = Get-Sha $path
            $bakSha = Get-Sha "$path.bak"
            Assert-Throws { Restore-PzMemory -Path $path } $row.Name
            Assert-Equal $pathSha (Get-Sha $path) ("{0}：不得動原檔" -f $row.Name)
            Assert-Equal $bakSha (Get-Sha "$path.bak") ("{0}：不得動備份" -f $row.Name)
        }

        $roPath = New-GameJson -Path (Join-Path $dir 'readonly.json') -VmArgs @('-Xmx8g')
        New-GameJson -Path "$roPath.bak" -VmArgs @('-Xmx3072m') | Out-Null
        $roSha = Get-Sha $roPath
        $roBakSha = Get-Sha "$roPath.bak"
        [System.IO.File]::SetAttributes($roPath, [System.IO.FileAttributes]::ReadOnly)
        try {
            Assert-Throws { Restore-PzMemory -Path $roPath } '唯讀目標'
            Assert-Equal $roSha (Get-Sha $roPath) '唯讀目標不得變更'
            Assert-Equal $roBakSha (Get-Sha "$roPath.bak") '唯讀目標失敗時備份不得變更'
        } finally {
            [System.IO.File]::SetAttributes($roPath, [System.IO.FileAttributes]::Normal)
        }
    }

    Invoke-Case '特殊字元路徑（單引號 / 方括號 / 空白 / 中文 / 驚嘆號）完整讀改還原' {
        $dir = New-CaseDir "it's [odd] 中文 目錄 !"
        $path = New-GameJson -Path (Join-Path $dir "Project Zomboid64 [測試]'s !.json") -VmArgs @('-Xms1024m', '-Xmx3072m', '-Dzomboid.note=中文參數')
        $originalSha = Get-Sha $path

        Assert-Equal ([long]3072MB) (Get-PzConfiguration -Path $path).HeapBytes '特殊路徑讀取'
        Assert-Equal ([long]8GB) (Set-PzMemory -Path $path -Gigabytes '8' -TotalBytes ([long]16GB)).HeapBytes '特殊路徑寫入'
        Assert-Equal $originalSha (Get-Sha "$path.bak") '特殊路徑備份應等於原始檔'
        Assert-Equal ([long]3072MB) (Restore-PzMemory -Path $path).HeapBytes '特殊路徑還原'
        Assert-Equal $originalSha (Get-Sha $path) '特殊路徑還原後應等於原始檔'
    }

    # Steam 安裝搜尋
    Invoke-Case 'Find-PzInstallations：多收藏庫、含空白 installdir、去重，只回傳真有 json 的目錄' {
        $dir = New-CaseDir 'steam'
        $steam = Join-Path $dir 'Steam'
        $lib2 = Join-Path $dir 'D Games\SteamLibrary'
        $libNoGame = Join-Path $dir 'Empty Library'
        $libNoJson = Join-Path $dir 'Broken Library'

        $game1 = New-SteamLibrary -Root $steam -InstallDir 'Project Zomboid' -WithJson
        $game2 = New-SteamLibrary -Root $lib2 -InstallDir 'Project Zomboid Beta' -WithJson
        New-SteamLibrary -Root $libNoGame | Out-Null
        New-SteamLibrary -Root $libNoJson -InstallDir 'Project Zomboid' | Out-Null
        New-LibraryFoldersVdf -Root $steam -Paths @($steam, $lib2, $libNoGame, $libNoJson, $lib2, (Join-Path $dir '不存在的收藏庫'))

        $found = @(Find-PzInstallations -SteamPaths @($steam, $steam))
        $expected = (@($game1, $game2) | Sort-Object) -join '|'
        $actual = ($found | Sort-Object) -join '|'
        Assert-Equal $expected $actual '回傳的遊戲目錄'
    }

    Invoke-Case 'Find-PzInstallations：找不到遊戲或收藏庫不存在時回傳空集合' {
        $dir = New-CaseDir 'steam-empty'
        $steam = Join-Path $dir 'Steam'
        $libNoGame = Join-Path $dir 'Empty Library'
        New-SteamLibrary -Root $steam | Out-Null
        New-SteamLibrary -Root $libNoGame | Out-Null
        New-LibraryFoldersVdf -Root $steam -Paths @($steam, $libNoGame)

        $found = @(Find-PzInstallations -SteamPaths @($steam, (Join-Path $dir '完全不存在')))
        Assert-Equal 0 $found.Count ("應為空集合，實際：{0}" -f ($found -join ' ; '))
    }

    Write-Host ("`n全部 {0} 個案例通過" -f $script:CaseCount) -ForegroundColor Green

} finally {
    if (Test-Path -LiteralPath $tempRoot) {
        Remove-Item -LiteralPath $tempRoot -Recurse -Force -ErrorAction Stop
    }
}
