#!/usr/bin/env bash
# XBoot Patcher (Linux): профиль AutoFix для VHD/VHDX-образа без Windows.
# Извлекает куст SYSTEM через guestfish, правит его lib/autofix_hive.py и кладёт обратно.
# Зависимости: libguestfs-tools, python3-hivex. Подробности: Patcher/README.md.
#
# Использование:
#   ./patch-image-linux.sh [опции] <образ.vhd|.vhdx>
# Опции:
#   --hwid 'VEN_XXXX&DEV_YYYY'   hardware ID карты (default: VEN_10EC&DEV_8168)
#   --disable-pagefile           отключить файл подкачки
#   --no-nic-tweaks              не трогать параметры адаптера
#   --no-tcpip-boot-start        не переводить Tcpip в boot-start
#   --dry-run                    показать изменения, образ не менять
set -euo pipefail

usage() { sed -n '2,14p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

HWID='VEN_10EC&DEV_8168'
EXTRA=()
IMAGE=''
while [[ $# -gt 0 ]]; do
    case "$1" in
        --hwid) HWID=$2; shift 2 ;;
        --disable-pagefile|--no-nic-tweaks|--no-tcpip-boot-start|--dry-run) EXTRA+=("$1"); shift ;;
        -h|--help) usage; exit 0 ;;
        -*) echo "неизвестная опция: $1" >&2; usage >&2; exit 1 ;;
        *) IMAGE=$1; shift ;;
    esac
done

[[ -n $IMAGE ]] || { usage >&2; exit 1; }
[[ -f $IMAGE ]] || { echo "нет файла: $IMAGE" >&2; exit 1; }
command -v guestfish >/dev/null || { echo "нужен guestfish: apt install libguestfs-tools" >&2; exit 1; }
python3 -c 'import hivex' 2>/dev/null || { echo "нужен python3-hivex: apt install python3-hivex" >&2; exit 1; }

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
HIVE_PATH='/Windows/System32/config/SYSTEM'
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

echo "==> ищу раздел Windows в $IMAGE"
WINDEV=''
while read -r dev _; do
    dev=${dev%:}
    [[ $dev == /dev/* ]] || continue
    if [[ $(guestfish --ro -a "$IMAGE" run : mount-ro "$dev" / : exists "$HIVE_PATH" 2>/dev/null) == "true" ]]; then
        WINDEV=$dev
        break
    fi
done < <(guestfish --ro -a "$IMAGE" run : list-filesystems)
[[ -n $WINDEV ]] || { echo "не найден раздел с $HIVE_PATH" >&2; exit 1; }
echo "==> раздел Windows: $WINDEV"

echo "==> выгружаю куст SYSTEM"
guestfish --ro -a "$IMAGE" run : mount-ro "$WINDEV" / : download "$HIVE_PATH" "$TMP/SYSTEM"
cp "$TMP/SYSTEM" "$TMP/SYSTEM.orig"

echo "==> патчу куст (hwid=$HWID)"
python3 "$SCRIPT_DIR/lib/autofix_hive.py" --hwid "$HWID" ${EXTRA[@]+"${EXTRA[@]}"} "$TMP/SYSTEM"

if [[ " ${EXTRA[*]-} " == *" --dry-run "* ]]; then
    echo "==> dry-run: образ не изменён"
    exit 0
fi
if cmp -s "$TMP/SYSTEM" "$TMP/SYSTEM.orig"; then
    echo "==> куст не изменился — записывать нечего"
    exit 0
fi

echo "==> записываю куст обратно (в образе останется копия SYSTEM.xboot-bak)"
guestfish --rw -a "$IMAGE" run : mount "$WINDEV" / \
    : cp "$HIVE_PATH" "$HIVE_PATH.xboot-bak" \
    : upload "$TMP/SYSTEM" "$HIVE_PATH" \
    : umount-all
echo "==> готово"
