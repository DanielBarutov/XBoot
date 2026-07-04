#Requires -RunAsAdministrator
<#
.SYNOPSIS
  XBoot Harvest: снимает PnP-состояние сетевого драйвера с эталонной машины в driverpack.

.DESCRIPTION
  Аналог записи в «базе драйверов» CCBoot. Запускается на машине ТОГО ЖЕ железа,
  что и бездисковые клиенты (загруженной с обычного диска, с рабочим драйвером).
  Собирает:
    - файлы пакета драйвера из DriverStore\FileRepository + oemNN.inf + .sys;
    - reg-фрагменты: Services\<драйвер>, Enum\PCI\<инстанс>, Control\Class\{net}\00NN,
      Control\Network\...\Connection, Tcpip\Parameters\Interfaces\<guid>,
      DriverDatabase (DriverInfFiles / DriverPackages / DeviceIds);
    - manifest.json с описанием пакета.
  Результат скармливается Patch-XbootImage.ps1 -Profile InjectPnp.

.PARAMETER OutDir
  Куда сложить driverpack (например .\driverpacks\rtl8168h).

.PARAMETER HardwareId
  Часть hardware ID карты. По умолчанию VEN_10EC&DEV_8168 (RTL8168/8111/8168H).

.EXAMPLE
  .\Harvest-NicDriver.ps1 -OutDir .\driverpacks\rtl8168h
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$OutDir,

    [string]$HardwareId = 'VEN_10EC&DEV_8168'
)

$ErrorActionPreference = 'Stop'
$NetClassGuid = '{4d36e972-e325-11ce-bfc1-08002be10318}'

function Write-Step([string]$Msg) { Write-Host "==> $Msg" -ForegroundColor Cyan }
function Write-Ok([string]$Msg) { Write-Host "    [+] $Msg" -ForegroundColor Green }

# ── экспорт ключа в .reg-текст (пропускает недоступные подключи вроде Enum\...\Properties) ──

function ConvertTo-HexList([byte[]]$Bytes) {
    if (-not $Bytes -or -not $Bytes.Length) { return '' }
    ($Bytes | ForEach-Object { $_.ToString('x2') }) -join ','
}

function Get-RegKeyDump([Microsoft.Win32.RegistryKey]$Key, [string]$Path) {
    $lines = New-Object System.Collections.Generic.List[string]
    $lines.Add("[HKEY_LOCAL_MACHINE\$Path]")
    foreach ($name in $Key.GetValueNames()) {
        $kind = $Key.GetValueKind($name)
        $raw = $Key.GetValue($name, $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        $lhs = if ($name -eq '') { '@' } else { '"' + $name.Replace('\', '\\').Replace('"', '\"') + '"' }
        $rhs = switch ("$kind") {
            'String'       { '"' + ([string]$raw).Replace('\', '\\').Replace('"', '\"') + '"' }
            'DWord'        { 'dword:{0:x8}' -f ([int64]$raw -band 0xFFFFFFFF) }
            'QWord'        { 'hex(b):' + (ConvertTo-HexList ([BitConverter]::GetBytes([int64]$raw))) }
            'ExpandString' { 'hex(2):' + (ConvertTo-HexList ([Text.Encoding]::Unicode.GetBytes([string]$raw + [char]0))) }
            'MultiString'  { 'hex(7):' + (ConvertTo-HexList ([Text.Encoding]::Unicode.GetBytes((([string[]]$raw) -join [char]0) + [char]0 + [char]0))) }
            'Binary'       { 'hex:' + (ConvertTo-HexList ([byte[]]$raw)) }
            default        { $null }
        }
        if ($null -eq $rhs) { $lines.Add("; пропущено значение '$name' (тип $kind)"); continue }
        $lines.Add("$lhs=$rhs")
    }
    $lines.Add('')
    foreach ($sub in $Key.GetSubKeyNames()) {
        $sk = $null
        try { $sk = $Key.OpenSubKey($sub) } catch { }
        if ($null -eq $sk) {
            $lines.Add("; пропущен подключ (нет доступа): $Path\$sub")
            $lines.Add('')
            continue
        }
        foreach ($l in (Get-RegKeyDump $sk "$Path\$sub")) { $lines.Add($l) }
        $sk.Close()
    }
    return $lines
}

function Export-RegKeyToFile([string]$LivePath, [string]$OutFile) {
    $k = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($LivePath)
    if (-not $k) { Write-Warning "ключ не найден, пропущен: HKLM\$LivePath"; return $false }
    $lines = @('Windows Registry Editor Version 5.00', '') + @(Get-RegKeyDump $k $LivePath)
    $k.Close()
    Set-Content -LiteralPath $OutFile -Value $lines -Encoding Unicode
    Write-Ok "$OutFile"
    return $true
}

function Get-PnpProp([string]$InstanceId, [string]$KeyName) {
    (Get-PnpDeviceProperty -InstanceId $InstanceId -KeyName $KeyName -ErrorAction SilentlyContinue).Data
}

# ────────────────────────────── основной поток ──────────────────────────────

Write-Step "поиск сетевых устройств с '$HardwareId'"
$devs = @(Get-PnpDevice -Class Net -PresentOnly -ErrorAction SilentlyContinue |
    Where-Object { $_.InstanceId -match [regex]::Escape($HardwareId) })
if (-not $devs.Count) {
    throw "устройство '$HardwareId' не найдено. Запускайте на машине с этой картой; список: Get-PnpDevice -Class Net"
}
foreach ($d in $devs) { Write-Ok "$($d.InstanceId) — $($d.FriendlyName) [$($d.Status)]" }

# берём первый инстанс как источник данных о драйвере (у одинаковых карт драйвер один)
$dev = $devs[0]
$service   = Get-PnpProp $dev.InstanceId 'DEVPKEY_Device_Service'
$driverKey = Get-PnpProp $dev.InstanceId 'DEVPKEY_Device_Driver'          # {classguid}\00NN
$infName   = Get-PnpProp $dev.InstanceId 'DEVPKEY_Device_DriverInfPath'   # oemNN.inf или inbox-имя
$classGuid = Get-PnpProp $dev.InstanceId 'DEVPKEY_Device_ClassGuid'
if (-not $service) { throw "у устройства нет службы драйвера (драйвер не установлен?)" }
Write-Step "драйвер: служба=$service, inf=$infName, driverKey=$driverKey"

# каталог пакета в DriverStore: сначала через DriverDatabase, затем через DISM
$pkgName = $null
$dif = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey("SYSTEM\DriverDatabase\DriverInfFiles\$infName")
if ($dif) {
    $pkgName = $dif.GetValue('Active')
    if (-not $pkgName) { $def = $dif.GetValue(''); if ($def) { $pkgName = @($def)[0] } }
    $dif.Close()
}
if (-not $pkgName) {
    $d = Get-WindowsDriver -Online -Driver $infName -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($d -and $d.OriginalFileName) { $pkgName = Split-Path (Split-Path $d.OriginalFileName -Parent) -Leaf }
}
if (-not $pkgName) { throw "не удалось определить каталог пакета в DriverStore для $infName" }
Write-Step "пакет DriverStore: $pkgName"

# ── файлы ──
$OutDir = New-Item -ItemType Directory -Force -Path $OutDir | ForEach-Object FullName
foreach ($sub in 'package', 'inf', 'drivers', 'registry\controlset', 'registry\driverdatabase') {
    New-Item -ItemType Directory -Force -Path (Join-Path $OutDir $sub) | Out-Null
}

Write-Step 'копирование файлов драйвера'
$repoSrc = Join-Path $env:windir "System32\DriverStore\FileRepository\$pkgName"
if (-not (Test-Path -LiteralPath $repoSrc)) { throw "нет каталога $repoSrc" }
Copy-Item -LiteralPath $repoSrc -Destination (Join-Path $OutDir "package\$pkgName") -Recurse -Force
Write-Ok "package\$pkgName"

$infSrc = Join-Path $env:windir "INF\$infName"
if (Test-Path -LiteralPath $infSrc) {
    Copy-Item -LiteralPath $infSrc -Destination (Join-Path $OutDir "inf\$infName") -Force
    Write-Ok "inf\$infName"
}

$sysFiles = @()
$imagePath = (Get-ItemProperty "HKLM:\SYSTEM\CurrentControlSet\Services\$service" -ErrorAction SilentlyContinue).ImagePath
if ($imagePath) {
    $resolvedSys = $imagePath -replace '^\\\?\?\\', '' -replace '^\\SystemRoot\\', "$env:windir\"
    if ($resolvedSys -notmatch '^[A-Za-z]:') { $resolvedSys = Join-Path $env:windir $resolvedSys }
    if (Test-Path -LiteralPath $resolvedSys) {
        $leaf = Split-Path $resolvedSys -Leaf
        Copy-Item -LiteralPath $resolvedSys -Destination (Join-Path $OutDir "drivers\$leaf") -Force
        $sysFiles += $leaf
        Write-Ok "drivers\$leaf"
    }
}

# ── реестр ──
Write-Step 'экспорт reg-фрагментов (CurrentControlSet)'
$csDir = Join-Path $OutDir 'registry\controlset'
[void](Export-RegKeyToFile "SYSTEM\CurrentControlSet\Services\$service" (Join-Path $csDir "10-service-$service.reg"))
if ($driverKey) {
    [void](Export-RegKeyToFile "SYSTEM\CurrentControlSet\Control\Class\$driverKey" (Join-Path $csDir '20-class-driver.reg'))
}

$instInfo = @()
$n = 0
foreach ($d in $devs) {
    [void](Export-RegKeyToFile "SYSTEM\CurrentControlSet\Enum\$($d.InstanceId)" (Join-Path $csDir ("30-enum-{0}.reg" -f $n)))
    $netCfg = $null
    $dk = Get-PnpProp $d.InstanceId 'DEVPKEY_Device_Driver'
    if ($dk) {
        $netCfg = (Get-ItemProperty "HKLM:\SYSTEM\CurrentControlSet\Control\Class\$dk" -ErrorAction SilentlyContinue).NetCfgInstanceId
    }
    if ($netCfg) {
        [void](Export-RegKeyToFile "SYSTEM\CurrentControlSet\Control\Network\$NetClassGuid\$netCfg" (Join-Path $csDir ("40-network-connection-{0}.reg" -f $n)))
        [void](Export-RegKeyToFile "SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces\$netCfg" (Join-Path $csDir ("50-tcpip-interface-{0}.reg" -f $n)))
    }
    $instInfo += [ordered]@{
        instanceId       = $d.InstanceId
        driverKey        = "$dk"
        netCfgInstanceId = "$netCfg"
        hardwareIds      = @(Get-PnpProp $d.InstanceId 'DEVPKEY_Device_HardwareIds')
    }
    $n++
}

Write-Step 'экспорт reg-фрагментов (DriverDatabase)'
$ddDir = Join-Path $OutDir 'registry\driverdatabase'
[void](Export-RegKeyToFile "SYSTEM\DriverDatabase\DriverInfFiles\$infName" (Join-Path $ddDir '10-inffiles.reg'))
[void](Export-RegKeyToFile "SYSTEM\DriverDatabase\DriverPackages\$pkgName" (Join-Path $ddDir '20-package.reg'))

$didRoot = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey('SYSTEM\DriverDatabase\DeviceIds\PCI')
if ($didRoot) {
    $n = 0
    foreach ($sub in $didRoot.GetSubKeyNames()) {
        if ($sub -match [regex]::Escape($HardwareId)) {
            [void](Export-RegKeyToFile "SYSTEM\DriverDatabase\DeviceIds\PCI\$sub" (Join-Path $ddDir ("30-deviceids-{0}.reg" -f $n)))
            $n++
        }
    }
    $didRoot.Close()
}

# ── manifest ──
Write-Step 'manifest.json'
$cv = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion'
$manifest = [ordered]@{
    hardwareIdFilter = $HardwareId
    service          = "$service"
    infName          = "$infName"
    packageDirName   = "$pkgName"
    classGuid        = "$classGuid"
    sysFiles         = $sysFiles
    instances        = $instInfo
    donor            = [ordered]@{
        computer = $env:COMPUTERNAME
        os       = "$($cv.ProductName) $($cv.DisplayVersion) build $($cv.CurrentBuildNumber)"
        captured = (Get-Date -Format 's')
    }
}
$manifest | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $OutDir 'manifest.json') -Encoding UTF8
Write-Ok 'manifest.json'

Write-Step "driverpack готов: $OutDir"
Write-Host @"

Дальше на машине с образом:
  .\Patch-XbootImage.ps1 -ImagePath <копия-образа>.vhd -Profile InjectPnp -DriverPack $OutDir
"@
