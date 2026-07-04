# XBoot Patcher — подготовка Windows-образа к бездисковой загрузке

## Зачем это нужно (симптом: дисконнект на логотипе Windows)

Загрузка по PXE/iSCSI идёт в два этапа:

1. **iPXE-этап.** iPXE подключает iSCSI-диск и отдаёт его Windows через iBFT.
   Ранний загрузчик Windows (winload) читает диск через firmware-интерфейс —
   сеть держит сам iPXE. На этом этапе всё работает: BIOS/UEFI → iPXE → логотип Windows.
2. **Ядро Windows.** В какой-то момент ядро отключает firmware-путь и переходит на
   **свои** драйверы: PCI → сетевой драйвер (NIC) → NDIS → TCP/IP → iScsiPrt
   (Microsoft iSCSI Initiator). Если родной драйвер сетевой карты **не помечен как
   boot-start** (`Start=0`) и **не привязан к устройству** в реестре образа — ядру не
   на чем продолжить iSCSI-сессию. Соединение с XBoot обрывается, клиент виснет на
   логотипе или падает в BSOD `0x7B INACCESSIBLE_BOOT_DEVICE`.

CCBoot решает это своей базой PnP-драйверов: при «снятии» образа он инжектит драйвер
сетевухи и правит реестр. XBoot Patcher делает то же самое, но открыто и офлайн —
патчит смонтированный VHD/VHDX без загрузки Windows.

## Состав

| Файл | Платформа | Назначение |
|------|-----------|------------|
| `Patch-XbootImage.ps1` | Windows (PowerShell 5.1+, админ) | Основной патчер: монтирует VHD/VHDX, правит реестр SYSTEM, при необходимости инжектит драйвер |
| `Harvest-NicDriver.ps1` | Windows (админ) | «Снимает» PnP-состояние сетевого драйвера с эталонной машины → driverpack (аналог записи в базе драйверов CCBoot) |
| `patch-image-linux.sh` | Linux (guestfish + python3-hivex) | Профиль AutoFix без Windows — удобно на dev/боевом Linux-сервере |
| `lib/autofix_hive.py` | Linux | Правка куста SYSTEM напрямую (используется скриптом выше, можно и отдельно) |
| `driverpacks/` | — | Сюда складываются driverpack'и, снятые Harvest-скриптом |

## Профили патча (варианты — пробовать по порядку)

Собирать/грузить образ сейчас негде, поэтому патчер реализует **несколько стратегий**;
на месте пробуйте в этом порядке.

### 1. `AutoFix` (по умолчанию) — если образ снят с той же машины (или драйвер уже установлен)

Ничего не инжектит, только чинит реестр образа:

- Находит в `Enum\PCI` образа устройства `VEN_10EC&DEV_8168` (RTL8168/8111, включая 8168H),
  читает их `Service` → ставит службе драйвера `Start=0` (boot-start);
- **Удаляет `StartOverride`** у службы драйвера и у `iScsiPrt` — на Win8+ это значение
  тихо перекрывает `Start` и является самой частой причиной «всё выставил, а не работает»;
- `iScsiPrt` (Microsoft iSCSI Initiator, msiscsi.sys) → `Start=0`;
- `Tcpip` → `Start=0` (нужен в boot-группе для iSCSI; отключается ключом `-NoTcpipBootStart`);
- `CriticalDeviceDatabase`: `pci#ven_10ec&dev_8168` → служба драйвера
  (работает на Win7; на Win10/11 игнорируется, но не мешает);
- `Tcpip\Parameters\DisableDHCPMediaSense=1` — чтобы моргание линка не рвало стек;
- Отключает Fast Startup / гибернацию (`HiberbootEnabled=0`, `HibernateEnabled=0`) —
  hiberfile на бездисковой машине с volatile-writeback ломает загрузку;
- `ConfigFlags=0` на инстансах устройства (сброс флагов «переустановить»);
- Realtek-твики против обрывов линка: `PnPCapabilities=0x118` (запрет отключения
  питания карты), `*EEE=0`, `EnableGreenEthernet=0`, `AdvancedEEE=0`, `GigaLite=0`
  (отключается ключом `-NoNicTweaks`);
- Опционально `-DisablePagefile` — файл подкачки в volatile-writeback только зря
  забивает оверлей.

Это почти наверняка ваш случай: образ «чистой винды» снят с машины, где RTL8168H уже
стоял — драйвер в образе есть, он просто `Start=3` (demand) и умирает на передаче эстафеты.

### 2. `InjectPnp` — если образ никогда не видел эту сетевуху

Полный аналог базы драйверов CCBoot. Сначала на **эталонной машине того же железа**
(любой клиентский ПК клуба, загруженный с обычного диска) снимаете driverpack:

```powershell
.\Harvest-NicDriver.ps1 -OutDir .\driverpacks\rtl8168h
```

Harvest сохраняет: файлы пакета драйвера из DriverStore, `oemNN.inf`, `.sys`, и
reg-фрагменты — `Services\<драйвер>`, `Enum\PCI\...` (инстанс устройства),
`Control\Class\{4d36e972-...}\00NN` (привязка + параметры адаптера),
`Control\Network\...\Connection`, `DriverDatabase` (DriverPackages / DriverInfFiles /
DeviceIds). Затем патчер реплицирует всё это в офлайн-образ и поверх прогоняет AutoFix:

```powershell
.\Patch-XbootImage.ps1 -ImagePath D:\images\win11.vhd -Profile InjectPnp -DriverPack .\driverpacks\rtl8168h
```

Важно: PCI-путь инстанса (`4&2f8f4c1&0&00E0`) зависит от машины. На **одинаковом
железе** клуба он совпадает — снимайте driverpack именно с клиентской машины.

### 3. `DismStage` — экспериментальный запасной вариант (только Win8+)

`dism /Add-Driver` кладёт пакет в DriverStore офлайн; при загрузке с iBFT PnP-менеджер
умеет ставить boot-critical драйвер из DriverStore. Требует `dism.exe` на машине, где
запускается патчер. Поверх также прогоняется AutoFix.

```powershell
.\Patch-XbootImage.ps1 -ImagePath D:\images\win11.vhd -Profile DismStage -DriverPack .\driverpacks\rtl8168h
```

Профили можно комбинировать: `-Profile InjectPnp,DismStage` (AutoFix выполняется всегда, последним).

## Использование

### Windows (основной сценарий)

```powershell
# ВСЕГДА патчим копию, не боевой мастер:
Copy-Item D:\images\win11.vhd D:\images\win11-patched.vhd

# запуск от администратора
.\Patch-XbootImage.ps1 -ImagePath D:\images\win11-patched.vhd                # AutoFix
.\Patch-XbootImage.ps1 -ImagePath D:\images\win11-patched.vhd -DisablePagefile
.\Patch-XbootImage.ps1 -ImagePath D:\images\win11-patched.vhd -WhatIfOnly    # показать, ничего не менять
```

Перед правкой скрипт сохраняет резервную копию куста:
`\Windows\System32\config\SYSTEM.xboot-bak` внутри образа.

### Linux

```bash
sudo apt install libguestfs-tools python3-hivex   # Debian/Ubuntu
./patch-image-linux.sh /data/xboot/win11-patched.vhd
./patch-image-linux.sh --disable-pagefile /data/xboot/win11-patched.vhd
```

Linux-вариант реализует только профиль AutoFix (для InjectPnp нужен Windows).

### Другая сетевая карта

Везде можно передать другой hardware ID:

```powershell
.\Patch-XbootImage.ps1 -ImagePath ... -HardwareId 'VEN_10EC&DEV_8125'   # RTL8125 2.5G
.\Harvest-NicDriver.ps1 -OutDir ... -HardwareId 'VEN_8086&DEV_15B8'     # Intel I219
```

```bash
./patch-image-linux.sh --hwid 'VEN_10EC&DEV_8125' image.vhd
```

Так со временем набирается своя «база драйверов» в `driverpacks/` — по одному паку
на модель карты.

## Что делает XBoot-сервер (и почему его почти не пришлось менять)

Обрыв происходит **внутри Windows**, сервер помочь не может — поэтому фикс целиком в
образе. Единственное изменение на сервере: boot-скрипт iPXE теперь явно выставляет
`set keep-san 1`, чтобы iSCSI-диск гарантированно оставался зарегистрированным в iBFT
при любых путях выхода из `sanboot`.

## Диагностика после патча

| Симптом | Причина | Что делать |
|---------|---------|------------|
| Всё так же виснет на логотипе | Драйвера нет в образе / не привязан | Профиль `InjectPnp` с driverpack с эталонной машины |
| BSOD `0x7B INACCESSIBLE_BOOT_DEVICE` | Драйвер загрузился, но iSCSI-стек нет | Проверить, что `iScsiPrt` реально `Start=0` и `StartOverride` удалён (лог патчера) |
| BSOD `0xC000000E` / `boot device inaccessible` сразу | Повреждён образ / не тот раздел пропатчен | Лог патчера: какой раздел он нашёл как Windows |
| Загрузилась, но сеть отваливается под нагрузкой | Green Ethernet / EEE / энергосбережение | Убедиться, что NIC-твики применены (не указан `-NoNicTweaks`) |
| Загружается раз через раз | Медиасенс/линк | `DisableDHCPMediaSense` применён? Смотрите также порт коммутатора (STP → portfast) |

Проверить содержимое куста после патча можно не загружаясь: `-WhatIfOnly` печатает
все шаги; на Linux — `hivexregedit --export SYSTEM 'ControlSet001\Services\iScsiPrt'`.
