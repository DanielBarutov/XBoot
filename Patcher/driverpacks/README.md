# driverpacks

Сюда складываются пакеты драйверов, снятые с эталонных машин скриптом
`../Harvest-NicDriver.ps1` — по одному каталогу на модель сетевой карты, например:

```
driverpacks/
  rtl8168h/            ← Harvest-NicDriver.ps1 -OutDir .\driverpacks\rtl8168h
    manifest.json
    package/<имя-пакета-DriverStore>/...
    inf/oemNN.inf
    drivers/*.sys
    registry/controlset/*.reg
    registry/driverdatabase/*.reg
```

Содержимое используется `../Patch-XbootImage.ps1 -Profile InjectPnp`.
Это аналог «базы сетевых драйверов» CCBoot, только собираемый самостоятельно.

Бинарные пакеты в git не коммитим — храните их рядом с образами.
