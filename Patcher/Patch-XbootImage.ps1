#Requires -RunAsAdministrator
<#
.SYNOPSIS
  XBoot Patcher: готовит офлайн VHD/VHDX-образ Windows к бездисковой iSCSI-загрузке.

.DESCRIPTION
  Монтирует образ, правит куст SYSTEM (boot-start драйвер сетевой карты, iScsiPrt,
  CriticalDeviceDatabase, анти-дисконнект твики) и при необходимости инжектит
  PnP-состояние драйвера, снятое скриптом Harvest-NicDriver.ps1 с эталонной машины.
  Подробности и порядок применения профилей — в Patcher/README.md.

  ВАЖНО: патчить копию образа, не боевой мастер. Перед правкой внутри образа
  создаётся резервная копия куста: Windows\System32\config\SYSTEM.xboot-bak.

.PARAMETER ImagePath
  Путь к VHD/VHDX-образу.

.PARAMETER Profile
  AutoFix (по умолчанию) | InjectPnp | DismStage. Можно несколько через запятую.
  AutoFix выполняется всегда (последним).

.PARAMETER DriverPack
  Папка driverpack'а (результат Harvest-NicDriver.ps1). Нужна для InjectPnp/DismStage.

.PARAMETER HardwareId
  Часть hardware ID сетевой карты. По умолчанию VEN_10EC&DEV_8168 (RTL8168/8111/8168H).

.PARAMETER WhatIfOnly
  Показать все шаги, ничего не менять.

.EXAMPLE
  .\Patch-XbootImage.ps1 -ImagePath D:\images\win11-patched.vhd

.EXAMPLE
  .\Patch-XbootImage.ps1 -ImagePath D:\images\win11-patched.vhd -Profile InjectPnp -DriverPack .\driverpacks\rtl8168h
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$ImagePath,

    [ValidateSet('AutoFix', 'InjectPnp', 'DismStage')]
    [Alias('Profile')]
    [string[]]$Profiles = @('AutoFix'),

    [string]$DriverPack,

    [string]$HardwareId = 'VEN_10EC&DEV_8168',

    [switch]$DisablePagefile,
    [switch]$NoNicTweaks,
    [switch]$NoTcpipBootStart,
    [switch]$WhatIfOnly,
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
$HiveName = 'XBOOT_SYS'
$NetClassGuid = '{4d36e972-e325-11ce-bfc1-08002be10318}'
# Известные имена служб драйверов Realtek PCIe GbE (Win7..Win11) — запасной вариант,
# если в Enum образа нет инстанса устройства, но драйвер когда-то ставился.
$KnownNicServices = @('rt640x64', 'rt68cx21x64', 'rt25cx21x64', 'rtcx21x64', 'rt630x64', 'rt64win7', 'RTL8167')

$script:Changes = 0

function Write-Step([string]$Msg) { Write-Host "==> $Msg" -ForegroundColor Cyan }
function Write-Info([string]$Msg) { Write-Host "    $Msg" }
function Write-Change([string]$Msg) { $script:Changes++; Write-Host "    [+] $Msg" -ForegroundColor Green }
function Write-Skip([string]$Msg) { Write-Host "    [-] $Msg" -ForegroundColor DarkGray }

function Set-RegValue {
    param([string]$Path, [string]$Name, $Value, [string]$Kind)
    $cur = $null
    if (Test-Path -LiteralPath $Path) {
        # .GetValue вместо Get-ItemProperty: имена вроде '*EEE' не должны трактоваться как wildcard
        $cur = (Get-Item -LiteralPath $Path).GetValue($Name, $null)
    }
    $curStr = if ($cur -is [array]) { $cur -join ',' } else { "$cur" }
    $newStr = if ($Value -is [array]) { $Value -join ',' } else { "$Value" }
    if ($null -ne $cur -and $curStr -eq $newStr) { Write-Skip "$Path : $Name уже = '$newStr'"; return }
    if ($WhatIfOnly) { Write-Change "[dry-run] $Path : $Name = '$newStr' ($Kind)"; return }
    if (-not (Test-Path -LiteralPath $Path)) { New-Item -Path $Path -Force | Out-Null }
    New-ItemProperty -LiteralPath $Path -Name $Name -Value $Value -PropertyType $Kind -Force | Out-Null
    Write-Change "$Path : $Name = '$newStr'"
}

function Remove-RegKey([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return }
    if ($WhatIfOnly) { Write-Change "[dry-run] удалить ключ $Path"; return }
    Remove-Item -LiteralPath $Path -Recurse -Force
    Write-Change "удалён ключ $Path"
}

function Set-BootStart {
    param([string]$CsBase, [string]$Service, [switch]$EnsureNdisGroup)
    $p = "$CsBase\Services\$Service"
    if (-not (Test-Path -LiteralPath $p)) {
        Write-Warning "служба $Service не найдена в $CsBase"
        return $false
    }
    Set-RegValue $p 'Start' 0 DWord
    # StartOverride на Win8+ тихо перекрывает Start — обязательно удалить
    Remove-RegKey "$p\StartOverride"
    if ($EnsureNdisGroup) {
        $grp = (Get-ItemProperty -LiteralPath $p -Name Group -ErrorAction SilentlyContinue).Group
        if (-not $grp) { Set-RegValue $p 'Group' 'NDIS' String }
    }
    return $true
}

function Invoke-AutoFix([string]$Cs) {
    $base = "HKLM:\$HiveName\$Cs"
    Write-Step "AutoFix: $Cs"

    # 1. Инстансы сетевой карты в Enum\PCI образа
    $instances = @()
    $pci = "$base\Enum\PCI"
    if (Test-Path -LiteralPath $pci) {
        foreach ($dev in (Get-ChildItem -LiteralPath $pci | Where-Object { $_.PSChildName -like "*$HardwareId*" })) {
            foreach ($inst in (Get-ChildItem -LiteralPath $dev.PSPath)) {
                $v = Get-ItemProperty -LiteralPath $inst.PSPath
                $path = $inst.Name -replace '^HKEY_LOCAL_MACHINE', 'HKLM:'
                Write-Info "найден инстанс: $($dev.PSChildName)\$($inst.PSChildName) (Service=$($v.Service))"
                $instances += [pscustomobject]@{ Path = $path; Service = $v.Service; Driver = $v.Driver }
            }
        }
    }

    # 2. Служба драйвера → boot-start
    $services = @($instances | ForEach-Object Service | Where-Object { $_ } | Sort-Object -Unique)
    if (-not $services.Count) {
        $services = @($KnownNicServices | Where-Object { Test-Path -LiteralPath "$base\Services\$_" })
        if ($services.Count) {
            Write-Warning "инстансы $HardwareId не найдены в Enum; найдены известные службы: $($services -join ', ')"
        } else {
            Write-Warning ("в $Cs нет ни устройства $HardwareId, ни известной службы драйвера — " +
                "образ не видел эту карту. Используйте -Profile InjectPnp с driverpack.")
        }
    }
    foreach ($svc in $services) { [void](Set-BootStart $base $svc -EnsureNdisGroup) }

    # 3. Твики инстанса и параметров адаптера (анти-дисконнект)
    foreach ($i in $instances) {
        Set-RegValue $i.Path 'ConfigFlags' 0 DWord
        if ($i.Driver -and -not $NoNicTweaks) {
            $classKey = "$base\Control\Class\$($i.Driver)"
            if (Test-Path -LiteralPath $classKey) {
                Set-RegValue $classKey 'PnPCapabilities' 0x118 DWord   # запрет отключения питания карты
                $names = (Get-Item -LiteralPath $classKey).GetValueNames()
                foreach ($n in '*EEE', 'EnableGreenEthernet', 'AdvancedEEE', 'GigaLite') {
                    if ($names -contains $n) { Set-RegValue $classKey $n '0' String }
                }
            }
        }
    }

    # 4. CriticalDeviceDatabase (работает на Win7, безвредно на Win10+)
    if ($services.Count) {
        $cddKey = "$base\Control\CriticalDeviceDatabase\pci#$($HardwareId.ToLower())"
        Set-RegValue $cddKey 'Service' $services[0] String
        Set-RegValue $cddKey 'ClassGUID' $NetClassGuid String
    }

    # 5. iSCSI-стек в boot-группу
    [void](Set-BootStart $base 'iScsiPrt')
    if (-not $NoTcpipBootStart) { [void](Set-BootStart $base 'Tcpip') }
    Set-RegValue "$base\Services\Tcpip\Parameters" 'DisableDHCPMediaSense' 1 DWord

    # 6. Fast Startup / гибернация несовместимы с volatile-writeback
    Set-RegValue "$base\Control\Session Manager\Power" 'HiberbootEnabled' 0 DWord
    Set-RegValue "$base\Control\Power" 'HibernateEnabled' 0 DWord

    # 7. Файл подкачки (опционально)
    if ($DisablePagefile) {
        Set-RegValue "$base\Control\Session Manager\Memory Management" 'PagingFiles' ([string[]]@()) MultiString
    }
}

function Copy-FileChecked([string]$Src, [string]$Dst) {
    if (-not (Test-Path -LiteralPath $Src)) { throw "нет файла в driverpack: $Src" }
    if (Test-Path -LiteralPath $Dst) {
        $same = (Get-FileHash -LiteralPath $Src).Hash -eq (Get-FileHash -LiteralPath $Dst).Hash
        if ($same) { Write-Skip "файл уже на месте: $Dst"; return }
        if (-not $Force) {
            throw ("в образе уже есть $Dst с ДРУГИМ содержимым. Это конфликт имён (например, oemNN.inf " +
                "занят другим драйвером). Перепроверьте driverpack или используйте -Force для перезаписи.")
        }
    }
    if ($WhatIfOnly) { Write-Change "[dry-run] копировать $Src -> $Dst"; return }
    New-Item -ItemType Directory -Force -Path (Split-Path $Dst -Parent) | Out-Null
    Copy-Item -LiteralPath $Src -Destination $Dst -Force
    Write-Change "скопирован $Dst"
}

function Copy-Tree([string]$Src, [string]$Dst) {
    if (-not (Test-Path -LiteralPath $Src)) { throw "нет каталога в driverpack: $Src" }
    if (Test-Path -LiteralPath $Dst) { Write-Skip "каталог уже в образе: $Dst"; return }
    if ($WhatIfOnly) { Write-Change "[dry-run] копировать каталог $Src -> $Dst"; return }
    New-Item -ItemType Directory -Force -Path (Split-Path $Dst -Parent) | Out-Null
    Copy-Item -LiteralPath $Src -Destination $Dst -Recurse
    Write-Change "скопирован каталог $Dst"
}

function Import-TransformedReg([string]$File, [string]$From, [string]$To) {
    if ($WhatIfOnly) { Write-Change "[dry-run] импорт $File ($From -> $To)"; return }
    $text = (Get-Content -LiteralPath $File -Raw).Replace($From, $To)
    $tmp = Join-Path $env:TEMP ('xboot-' + [IO.Path]::GetRandomFileName() + '.reg')
    Set-Content -LiteralPath $tmp -Value $text -Encoding Unicode
    $out = & reg.exe import $tmp 2>&1
    $code = $LASTEXITCODE
    Remove-Item -LiteralPath $tmp -Force
    if ($code -ne 0) { throw "reg import не удался для $File : $out" }
    Write-Change "импортирован $([IO.Path]::GetFileName($File)) -> $To"
}

function Invoke-InjectPnp {
    Write-Step "InjectPnp: driverpack $DriverPack"
    $manifestPath = Join-Path $DriverPack 'manifest.json'
    if (-not (Test-Path -LiteralPath $manifestPath)) { throw "нет manifest.json в driverpack: $DriverPack" }
    $m = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json

    # 1. Файлы: пакет в DriverStore, .sys в drivers, INF в Windows\INF
    Copy-Tree (Join-Path $DriverPack "package\$($m.packageDirName)") `
              (Join-Path $WinRoot "Windows\System32\DriverStore\FileRepository\$($m.packageDirName)")
    foreach ($sys in $m.sysFiles) {
        Copy-FileChecked (Join-Path $DriverPack "drivers\$sys") (Join-Path $WinRoot "Windows\System32\drivers\$sys")
    }
    $infSrc = Join-Path $DriverPack "inf\$($m.infName)"
    if (Test-Path -LiteralPath $infSrc) {
        Copy-FileChecked $infSrc (Join-Path $WinRoot "Windows\INF\$($m.infName)")
    }

    # 2. Реестр: фрагменты CurrentControlSet — в каждый ControlSetNNN образа
    $csDir = Join-Path $DriverPack 'registry\controlset'
    if (Test-Path -LiteralPath $csDir) {
        foreach ($cs in $ControlSets) {
            foreach ($f in (Get-ChildItem -LiteralPath $csDir -Filter *.reg | Sort-Object Name)) {
                Import-TransformedReg $f.FullName 'HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet' "HKEY_LOCAL_MACHINE\$HiveName\$cs"
            }
        }
    }
    # 3. DriverDatabase — один на куст
    $ddDir = Join-Path $DriverPack 'registry\driverdatabase'
    if (Test-Path -LiteralPath $ddDir) {
        foreach ($f in (Get-ChildItem -LiteralPath $ddDir -Filter *.reg | Sort-Object Name)) {
            Import-TransformedReg $f.FullName 'HKEY_LOCAL_MACHINE\SYSTEM\DriverDatabase' "HKEY_LOCAL_MACHINE\$HiveName\DriverDatabase"
        }
    }
}

# ────────────────────────────── основной поток ──────────────────────────────

if (($Profiles -contains 'InjectPnp' -or $Profiles -contains 'DismStage') -and -not $DriverPack) {
    throw 'для профилей InjectPnp/DismStage нужен -DriverPack (см. Harvest-NicDriver.ps1)'
}
if ($DriverPack) { $DriverPack = (Resolve-Path -LiteralPath $DriverPack).Path }
$resolved = (Resolve-Path -LiteralPath $ImagePath).Path

$mounted = $false
$hiveLoaded = $false
$tempAccess = @()
$WinRoot = $null

try {
    Write-Step "монтирование $resolved"
    Mount-DiskImage -ImagePath $resolved | Out-Null
    $mounted = $true
    $disk = Get-DiskImage -ImagePath $resolved | Get-Disk

    foreach ($part in ($disk | Get-Partition)) {
        $root = $null
        if ($part.DriveLetter) {
            $root = "$($part.DriveLetter):"
        } else {
            $mnt = Join-Path $env:TEMP "xboot_mnt_$($disk.Number)_$($part.PartitionNumber)"
            New-Item -ItemType Directory -Force -Path $mnt | Out-Null
            try {
                Add-PartitionAccessPath -DiskNumber $disk.Number -PartitionNumber $part.PartitionNumber -AccessPath $mnt
            } catch { continue }
            $tempAccess += , @{ Disk = $disk.Number; Part = $part.PartitionNumber; Path = $mnt }
            $root = $mnt
        }
        if (Test-Path -LiteralPath (Join-Path $root 'Windows\System32\config\SYSTEM')) { $WinRoot = $root; break }
    }
    if (-not $WinRoot) { throw 'не найден раздел с \Windows\System32\config\SYSTEM' }
    Write-Step "раздел Windows: $WinRoot"

    # DISM работает по файловой системе и требует НЕзагруженных кустов — до reg load
    if ($Profiles -contains 'DismStage') {
        Write-Step 'DismStage: dism /Add-Driver (офлайн-стейджинг в DriverStore)'
        if ($WhatIfOnly) {
            Write-Change "[dry-run] dism /Image:$WinRoot\ /Add-Driver /Driver:$DriverPack\package /Recurse"
        } else {
            & dism.exe /Image:"$WinRoot\" /Add-Driver /Driver:"$(Join-Path $DriverPack 'package')" /Recurse
            if ($LASTEXITCODE) { throw "dism завершился с кодом $LASTEXITCODE" }
            $script:Changes++
        }
    }

    $sysHive = Join-Path $WinRoot 'Windows\System32\config\SYSTEM'
    if (-not $WhatIfOnly) {
        Copy-Item -LiteralPath $sysHive -Destination "$sysHive.xboot-bak" -Force
        Write-Info 'резервная копия куста: SYSTEM.xboot-bak'
    }

    Write-Step "загрузка куста SYSTEM -> HKLM\$HiveName"
    & reg.exe load "HKLM\$HiveName" $sysHive | Out-Null
    if ($LASTEXITCODE) { throw 'reg load не удался (куст занят или повреждён?)' }
    $hiveLoaded = $true

    $ControlSets = @(Get-ChildItem "HKLM:\$HiveName" |
        Where-Object { $_.PSChildName -match '^ControlSet\d+$' } |
        ForEach-Object PSChildName)
    Write-Info "control sets: $($ControlSets -join ', ')"

    if ($Profiles -contains 'InjectPnp') { Invoke-InjectPnp }
    foreach ($cs in $ControlSets) { Invoke-AutoFix $cs }

    Write-Step "готово: изменений — $script:Changes $(if ($WhatIfOnly) { '(dry-run, образ не тронут)' })"
}
finally {
    if ($hiveLoaded) {
        [gc]::Collect(); [gc]::WaitForPendingFinalizers()
        for ($i = 0; $i -lt 10; $i++) {
            & reg.exe unload "HKLM\$HiveName" 2>$null | Out-Null
            if ($LASTEXITCODE -eq 0) { break }
            Start-Sleep -Seconds 1
            [gc]::Collect()
        }
        if ($LASTEXITCODE) { Write-Warning "не удалось выгрузить HKLM\$HiveName — выгрузите вручную: reg unload HKLM\$HiveName" }
    }
    foreach ($ta in $tempAccess) {
        try {
            Remove-PartitionAccessPath -DiskNumber $ta.Disk -PartitionNumber $ta.Part -AccessPath $ta.Path
            Remove-Item -LiteralPath $ta.Path -Force -ErrorAction SilentlyContinue
        } catch { }
    }
    if ($mounted) { Dismount-DiskImage -ImagePath $resolved | Out-Null }
}
