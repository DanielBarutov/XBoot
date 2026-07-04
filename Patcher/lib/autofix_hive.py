#!/usr/bin/env python3
"""XBoot AutoFix: офлайн-правка куста SYSTEM для бездисковой iSCSI-загрузки.

Linux-эквивалент профиля AutoFix из Patch-XbootImage.ps1 (см. Patcher/README.md):
boot-start драйвера сетевой карты и iSCSI-стека, CriticalDeviceDatabase,
анти-дисконнект твики. Обычно вызывается через patch-image-linux.sh, но можно
и напрямую по извлечённому файлу куста.
"""
import argparse
import struct
import sys

try:
    import hivex
except ImportError:
    sys.exit("нужен python3-hivex: apt install python3-hivex")

REG_SZ = 1
REG_EXPAND_SZ = 2
REG_DWORD = 4
REG_MULTI_SZ = 7

NET_CLASS_GUID = "{4d36e972-e325-11ce-bfc1-08002be10318}"
# Известные службы драйверов Realtek PCIe GbE (Win7..Win11) — запасной вариант,
# если инстанса устройства нет в Enum, но драйвер когда-то ставился.
KNOWN_NIC_SERVICES = ["rt640x64", "rt68cx21x64", "rt25cx21x64", "rtcx21x64",
                      "rt630x64", "rt64win7", "RTL8167"]

changes = 0


def log(msg):
    global changes
    changes += 1
    print(f"    [+] {msg}")


def skip(msg):
    print(f"    [-] {msg}")


def warn(msg):
    print(f"    [!] {msg}")


class Hive:
    """Тонкая обёртка над hivex: регистронезависимая навигация + типизированные значения."""

    def __init__(self, path):
        self.h = hivex.Hivex(path, write=True)

    def child(self, node, name):
        if node is None:
            return None
        low = name.lower()
        for c in self.h.node_children(node):
            if self.h.node_name(c).lower() == low:
                return c
        return None

    def walk(self, node, *names):
        for name in names:
            node = self.child(node, name)
            if node is None:
                return None
        return node

    def ensure_child(self, node, name):
        c = self.child(node, name)
        if c is None:
            c = self.h.node_add_child(node, name)
            log(f"создан ключ …\\{name}")
        return c

    def _value(self, node, name):
        low = name.lower()
        for v in self.h.node_values(node):
            if self.h.value_key(v).lower() == low:
                return v
        return None

    def get_sz(self, node, name):
        v = self._value(node, name)
        if v is None:
            return None
        t, data = self.h.value_value(v)
        if t not in (REG_SZ, REG_EXPAND_SZ):
            return None
        return data.decode("utf-16-le", "ignore").rstrip("\x00")

    def get_dword(self, node, name):
        v = self._value(node, name)
        if v is None:
            return None
        t, data = self.h.value_value(v)
        if t != REG_DWORD or len(data) < 4:
            return None
        return struct.unpack("<I", data[:4])[0]

    def value_names(self, node):
        return {self.h.value_key(v) for v in self.h.node_values(node)}

    def set_dword(self, node, name, val, where=""):
        if self.get_dword(node, name) == val:
            skip(f"{where}{name} уже = {val}")
            return
        self.h.node_set_value(node, {"key": name, "t": REG_DWORD,
                                     "value": struct.pack("<I", val)})
        log(f"{where}{name} = {val}")

    def set_sz(self, node, name, s, where=""):
        if self.get_sz(node, name) == s:
            skip(f"{where}{name} уже = '{s}'")
            return
        self.h.node_set_value(node, {"key": name, "t": REG_SZ,
                                     "value": (s + "\x00").encode("utf-16-le")})
        log(f"{where}{name} = '{s}'")

    def set_empty_multi(self, node, name, note, where=""):
        empty = "\x00\x00".encode("utf-16-le")
        v = self._value(node, name)
        if v is not None:
            t, data = self.h.value_value(v)
            if t == REG_MULTI_SZ and data.strip(b"\x00") == b"":
                skip(f"{where}{name} уже пуст")
                return
        self.h.node_set_value(node, {"key": name, "t": REG_MULTI_SZ, "value": empty})
        log(f"{where}{name} = <пусто> ({note})")

    def delete_key(self, parent, name, where=""):
        c = self.child(parent, name)
        if c is not None:
            self.h.node_delete_child(c)
            log(f"удалён ключ {where}{name}")


def set_boot_start(hv, cs_node, cs_name, svc, ensure_ndis_group=False):
    node = hv.walk(cs_node, "Services", svc)
    if node is None:
        warn(f"служба {svc} не найдена в {cs_name}")
        return False
    where = f"{cs_name}\\Services\\{svc}\\"
    hv.set_dword(node, "Start", 0, where)
    # StartOverride на Win8+ тихо перекрывает Start — обязательно удалить
    hv.delete_key(node, "StartOverride", where)
    if ensure_ndis_group and hv.get_sz(node, "Group") is None:
        hv.set_sz(node, "Group", "NDIS", where)
    return True


def autofix_controlset(hv, cs_node, cs_name, args):
    print(f"==> AutoFix: {cs_name}")
    hwid = args.hwid.lower()

    # 1. Инстансы сетевой карты в Enum\PCI образа
    instances = []  # (node, service, driver)
    pci = hv.walk(cs_node, "Enum", "PCI")
    if pci is not None:
        for dev in hv.h.node_children(pci):
            dev_name = hv.h.node_name(dev)
            if hwid not in dev_name.lower():
                continue
            for inst in hv.h.node_children(dev):
                svc = hv.get_sz(inst, "Service")
                drv = hv.get_sz(inst, "Driver")
                print(f"    найден инстанс {dev_name}\\{hv.h.node_name(inst)} (Service={svc})")
                instances.append((inst, svc, drv))

    # 2. Служба драйвера → boot-start
    services = sorted({svc for _, svc, _ in instances if svc})
    if not services:
        services = [s for s in KNOWN_NIC_SERVICES
                    if hv.walk(cs_node, "Services", s) is not None]
        if services:
            warn(f"инстансы {args.hwid} не найдены в Enum; беру известные службы: {services}")
        else:
            warn(f"в {cs_name} нет ни устройства {args.hwid}, ни известной службы драйвера — "
                 "нужен InjectPnp (Windows-патчер Patch-XbootImage.ps1)")
    for svc in services:
        set_boot_start(hv, cs_node, cs_name, svc, ensure_ndis_group=True)

    control = hv.child(cs_node, "Control")

    # 3. Твики инстанса и параметров адаптера (анти-дисконнект)
    for inst, _svc, drv in instances:
        hv.set_dword(inst, "ConfigFlags", 0, "Enum-инстанс: ")
        if drv and not args.no_nic_tweaks and control is not None:
            guid, _, subkey = drv.partition("\\")
            class_node = hv.walk(control, "Class", guid, subkey)
            if class_node is not None:
                where = f"Class\\{drv}\\"
                hv.set_dword(class_node, "PnPCapabilities", 0x118, where)
                existing = hv.value_names(class_node)
                for name in ("*EEE", "EnableGreenEthernet", "AdvancedEEE", "GigaLite"):
                    if name in existing:
                        hv.set_sz(class_node, name, "0", where)

    # 4. CriticalDeviceDatabase (Win7; безвредно на Win10+)
    if services and control is not None:
        cdd = hv.ensure_child(control, "CriticalDeviceDatabase")
        key = hv.ensure_child(cdd, "pci#" + hwid)
        hv.set_sz(key, "Service", services[0], "CDD: ")
        hv.set_sz(key, "ClassGUID", NET_CLASS_GUID, "CDD: ")

    # 5. iSCSI-стек в boot-группу
    set_boot_start(hv, cs_node, cs_name, "iScsiPrt")
    if not args.no_tcpip_boot_start:
        set_boot_start(hv, cs_node, cs_name, "Tcpip")
    tcpip_params = hv.walk(cs_node, "Services", "Tcpip", "Parameters")
    if tcpip_params is not None:
        hv.set_dword(tcpip_params, "DisableDHCPMediaSense", 1, "Tcpip\\Parameters\\")

    # 6. Fast Startup / гибернация несовместимы с volatile-writeback
    if control is not None:
        sm = hv.child(control, "Session Manager")
        if sm is not None:
            hv.set_dword(hv.ensure_child(sm, "Power"), "HiberbootEnabled", 0,
                         "Session Manager\\Power\\")
        power = hv.child(control, "Power")
        if power is not None:
            hv.set_dword(power, "HibernateEnabled", 0, "Control\\Power\\")

        # 7. Файл подкачки (опционально)
        if args.disable_pagefile and sm is not None:
            mm = hv.child(sm, "Memory Management")
            if mm is not None:
                hv.set_empty_multi(mm, "PagingFiles", "файл подкачки отключён",
                                   "Memory Management\\")


def main():
    ap = argparse.ArgumentParser(
        description="XBoot AutoFix: правка куста SYSTEM для iSCSI-загрузки")
    ap.add_argument("hive", help="путь к извлечённому файлу куста SYSTEM")
    ap.add_argument("--hwid", default="VEN_10EC&DEV_8168",
                    help="часть hardware ID сетевой карты (default: %(default)s)")
    ap.add_argument("--disable-pagefile", action="store_true",
                    help="отключить файл подкачки")
    ap.add_argument("--no-nic-tweaks", action="store_true",
                    help="не трогать параметры адаптера (EEE/Green Ethernet/питание)")
    ap.add_argument("--no-tcpip-boot-start", action="store_true",
                    help="не переводить Tcpip в boot-start")
    ap.add_argument("--dry-run", action="store_true",
                    help="показать изменения, файл не записывать")
    args = ap.parse_args()

    hv = Hive(args.hive)
    root = hv.h.root()
    control_sets = [(hv.h.node_name(c), c) for c in hv.h.node_children(root)
                    if hv.h.node_name(c).lower().startswith("controlset")]
    if not control_sets:
        sys.exit("в кусте нет ControlSetNNN — это точно куст SYSTEM?")

    for name, node in control_sets:
        autofix_controlset(hv, node, name, args)

    if args.dry_run:
        print(f"==> dry-run: изменения ({changes}) НЕ записаны")
    else:
        hv.h.commit(None)
        print(f"==> записано изменений: {changes}")


if __name__ == "__main__":
    main()
