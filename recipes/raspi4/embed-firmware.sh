#!/usr/bin/env bash
#
# bootcher [hooks.disk] post recipe — Raspberry Pi 4
#
# Embeds the Raspberry Pi 4 UEFI firmware (pftf/RPi4) into the ESP of the built
# aarch64 raw image, so the board's bootloader can hand off to the bootc disk.
# This is exactly the kind of privileged, board-specific disk edit that has no
# business living in the Containerfile — so it lives here, in a disk.post hook.
#
# pftf's firmware has no battery-backed NVRAM, so its EFI variable store lives
# inside RPI_EFI.fd, and the published build ships that store *empty* — every
# setting falls back to a compiled-in default. Two of those defaults cripple a
# real Pi 4 running Linux: the system table defaults to ACPI (no BCM2711 clock/
# thermal drivers → the SoC is stuck at its boot clock, no cpufreq) and RAM is
# capped at 3 GB. Both are only changeable from the UEFI setup menu, which
# persists the change back into RPI_EFI.fd's store — there is no config.txt knob.
# The variable names, GUID, and value encodings seeded below come from pftf's
# upstream settings reference:
# https://github.com/tianocore/edk2-platforms/blob/master/Platform/RaspberryPi/RPi4/Readme.md
#
# The 3 GB cap isn't gratuitous — pftf sets it because the xHCI USB3 (front)
# ports have >3 GB DMA constraints that can otherwise break under Linux. We lift
# it by default (full RAM); a board that needs those USB3 ports can keep the cap
# with BOOTCHER_RASPI_KEEP_3GB_CAP=1 (see below).
#
# Rather than vendor a whole hand-configured RPI_EFI.fd (4 MB blob, pinned to one
# firmware version), we pre-seed just those variables into the *stock* store
# below: it's an authenticated-variable store, empty (all 0xFF) on a fresh build,
# so we append well-formed VAR_ADDED records exactly as the firmware would have
# written them from the menu. Boot order is deliberately NOT seeded — Boot####
# entries reference partition UUIDs that don't match a freshly-flashed card, so
# they misbehave; the firmware's own fallback (efi/boot/bootaa64.efi) is used.
#
# Wire it into bootcher.toml (see this directory's README.md):
#
#   [targets]
#   aarch64 = "raw"
#
#   [hooks.disk]
#   post = "recipes/raspi4/embed-firmware.sh"
#
# bootcher runs this from the project root with the BOOTCHER_METADATA env var set
# (JSON). We read the disks it just built straight from there — no need to know
# bootcher's output layout.
#
# Requires: jq, curl, unzip, python3, sfdisk, and sudo (loopback mount of the raw
# image's ESP).

set -euo pipefail

# Pin the Raspberry Pi UEFI firmware release to embed.
# Check https://github.com/pftf/RPi4/releases for the latest.
FIRMWARE_URL="https://github.com/pftf/RPi4/releases/download/v1.51/RPi4_UEFI_Firmware_v1.51.zip"

# By default we lift pftf's 3 GB RAM cap so the board sees all its memory. Set
# BOOTCHER_RASPI_KEEP_3GB_CAP=1 to leave the cap in place — do this if you need
# the xHCI USB3 (front) ports to work under Linux, which is exactly what pftf caps
# RAM to protect (those ports have >3 GB DMA constraints). See the upstream
# settings reference:
# https://github.com/tianocore/edk2-platforms/blob/master/Platform/RaspberryPi/RPi4/Readme.md
KEEP_3GB_CAP="${BOOTCHER_RASPI_KEEP_3GB_CAP:-0}"

# `EFI` partition type GUID — marks the ESP in a GPT.
EFI_PART_GUID="c12a7328-f81f-11d2-ba4b-00a0c93ec93b"

# Fail early with one clear message if any required tool is missing, rather than
# aborting mid-run (possibly after a download or a loop attach) on a raw
# "command not found".
require() {
	local bin missing=()
	for bin in "$@"; do
		command -v "$bin" >/dev/null 2>&1 || missing+=("$bin")
	done
	if [ "${#missing[@]}" -gt 0 ]; then
		echo "raspi4: missing required tool(s): ${missing[*]}" >&2
		return 1
	fi
}

# Locate the single EFI System Partition in a raw disk image and print its byte
# extent as "<offset> <sizelimit>", ready to hand to a `mount -o loop,offset=,
# sizelimit=`. Bails on 0 or >1 ESP matches.
#
# Everything is read straight from the GPT with `sfdisk -d`, which parses the
# on-disk table itself — no udev, no loop device, no partition scanning. That's
# deliberate: a privileged CI job container has a private tmpfs /dev, so the
# kernel's partition scan (`losetup --partscan`) never materialises /dev/loopNpM
# nodes there (lsblk's PARTTYPE is likewise empty without udevd). Computing the
# byte range here lets the caller loop-mount that slice of the image directly,
# using only the main loop node — which the container *does* get. sfdisk emits
# the per-partition start/size in sectors and GUIDs uppercase, so we scale by the
# reported sector size and case-fold both sides before comparing. sfdisk
# space-pads the numeric values (`start=        2048`), so we pull each field out
# with a regex over the whole line rather than by column.
find_esp_extent() {
	local img="$1"
	# Pass the GPT dump as an argument, not on stdin: `python3 -` already reads
	# the script itself from stdin (the heredoc), so the two would collide.
	python3 - "$EFI_PART_GUID" "$(sudo sfdisk -d "$img")" <<-'PY'
		import re, sys
		guid = sys.argv[1].upper()
		secsz = 512
		matches = []
		for line in sys.argv[2].splitlines():
		    m = re.match(r'sector-size:\s*(\d+)', line)
		    if m:
		        secsz = int(m.group(1))
		        continue
		    if 'start=' not in line:
		        continue
		    t = re.search(r'type=([0-9A-Fa-f-]+)', line)
		    if not t or t.group(1).upper() != guid:
		        continue
		    start = re.search(r'start=\s*(\d+)', line)
		    size  = re.search(r'size=\s*(\d+)', line)
		    matches.append((int(start.group(1)) * secsz, int(size.group(1)) * secsz))
		if len(matches) != 1:
		    sys.exit("raspi4: expected exactly one EFI System Partition, "
		             "found %d" % len(matches))
		print("%d %d" % matches[0])
	PY
}

# Pre-seed the pftf UEFI variable store inside a freshly extracted RPI_EFI.fd ($1)
# so a real Pi 4 boots fast and with all its RAM, without vendoring a whole
# hand-configured firmware image.
#
# pftf's store is an EDK2 *authenticated* variable store: a VARIABLE_STORE_HEADER
# (signature = the auth-variable GUID) followed by 0x55AA-tagged
# AUTHENTICATED_VARIABLE_HEADER records. On a stock build the store body is fully
# erased (0xFF), so seeding a variable is just appending a VAR_ADDED record — byte
# for byte what the firmware writes when you toggle the option in the menu (this
# encoder was diffed against a hand-configured card and matches exactly).
#
# We only set the ConfigDxe options we care about; everything else keeps its
# compiled-in default. Each record holds one UINT32 (attr NV|BS|RT), so we bail
# loudly if the store can't be found/validated, if it isn't empty where we mean
# to write, or if the values don't read back. $2 = "1" keeps pftf's 3 GB RAM cap
# (leaves RamLimitTo3GB unseeded at its firmware default) for boards that need
# the xHCI USB3 ports.
preseed_efi_vars() {
	python3 - "$1" "$2" <<-'PY'
	import struct, sys, uuid

	# EDK2 VARIABLE_STORE_HEADER signature for an authenticated store, and the
	# pftf ConfigDxe formset GUID that owns the options below. The names, GUID,
	# and value encodings are from pftf's upstream settings reference:
	# https://github.com/tianocore/edk2-platforms/blob/master/Platform/RaspberryPi/RPi4/Readme.md
	STORE_SIG = uuid.UUID("aaf32c78-947b-439a-a180-2e144ec37792").bytes_le
	CONFIG_GUID = uuid.UUID("cd7cc258-31db-22e6-9f22-63b0b8eed6b5")

	keep_3gb_cap = sys.argv[2] == "1"

	# name -> UINT32 value, each verbatim from the upstream reference:
	#   SystemTableMode 2 = Devicetree (0=ACPI, 1=ACPI+DT) -> real cpufreq
	WANT = {"SystemTableMode": 2}
	# RamLimitTo3GB 0 = use all RAM (firmware default 1 caps at 3 GB). Left
	# unseeded when the caller keeps the cap, so the firmware default stands.
	if not keep_3gb_cap:
	    WANT["RamLimitTo3GB"] = 0

	VAR_ADDED = 0x3F       # State: header + name + data all valid
	ATTR = 0x7             # NV | BS | RT
	HDR = 60               # AUTHENTICATED_VARIABLE_HEADER size

	def align4(n):
	    return (n + 3) & ~3

	def record(name, value):
	    nb = (name + "\0").encode("utf-16-le")
	    h = struct.pack("<HBBIQ", 0x55AA, VAR_ADDED, 0, ATTR, 0)  # id,state,rsvd,attr,mono
	    h += b"\x00" * 16                                         # TimeStamp
	    h += struct.pack("<III", 0, len(nb), 4)                  # PubKeyIndex,NameSize,DataSize
	    h += CONFIG_GUID.bytes_le
	    return h + nb + struct.pack("<I", value)

	path = sys.argv[1]
	data = bytearray(open(path, "rb").read())

	off = data.find(STORE_SIG)
	if off < 0:
	    sys.exit("raspi4: no UEFI variable store found in RPI_EFI.fd")
	size, fmt, state = struct.unpack_from("<IBB", data, off + 16)
	if fmt != 0x5A or state != 0xFE:
	    sys.exit("raspi4: variable store not formatted/healthy "
	             "(fmt=0x%x state=0x%x)" % (fmt, state))
	store_end = off + size

	# Walk existing records to find where free (erased) space begins — and refuse
	# if a target var is already set, so re-running never writes a second (invalid)
	# VAR_ADDED for the same name. A fresh pftf extract has an empty store.
	p = align4(off + 28)
	while p < store_end and struct.unpack_from("<H", data, p)[0] == 0x55AA:
	    st = data[p + 2]
	    namesz, datasz = struct.unpack_from("<II", data, p + 36)
	    g = uuid.UUID(bytes_le=bytes(data[p + 44:p + 60]))
	    nm = data[p + 60:p + 60 + namesz].decode("utf-16-le").rstrip("\0")
	    if st == VAR_ADDED and g == CONFIG_GUID and nm in WANT:
	        sys.exit("raspi4: %r already set in the store — not a fresh firmware "
	                 "extract; refusing to duplicate" % nm)
	    p = align4(p + HDR + namesz + datasz)

	blob = bytearray()
	for name, value in WANT.items():
	    while len(blob) % 4:            # 4-byte-align each record start
	        blob += b"\xFF"
	    blob += record(name, value)
	if p + len(blob) > store_end:
	    sys.exit("raspi4: not enough room in variable store to seed vars")
	if any(b != 0xFF for b in data[p:p + len(blob)]):
	    sys.exit("raspi4: intended seed region is not erased -- refusing to clobber")
	data[p:p + len(blob)] = blob

	# Read the seeded vars straight back to prove the store parses as intended.
	got = {}
	q = align4(off + 28)
	while q < store_end and struct.unpack_from("<H", data, q)[0] == 0x55AA:
	    st = data[q + 2]
	    namesz, datasz = struct.unpack_from("<II", data, q + 36)
	    g = uuid.UUID(bytes_le=bytes(data[q + 44:q + 60]))
	    nm = data[q + 60:q + 60 + namesz].decode("utf-16-le").rstrip("\0")
	    if g == CONFIG_GUID and nm in WANT and st == VAR_ADDED:
	        got[nm] = struct.unpack_from("<I", data, q + 60 + namesz)[0]
	    q = align4(q + HDR + namesz + datasz)
	if got != WANT:
	    sys.exit("raspi4: seeded vars did not read back (got %r)" % got)

	open(path, "wb").write(data)
	print("raspi4: seeded UEFI vars %s" % ", ".join("%s=%d" % kv for kv in WANT.items()))
	PY
}

require jq curl unzip python3 sudo sfdisk mount umount

# The aarch64 raw disk bootcher just built. `.file` is the resolved disk.<ext>
# path (present at disk.post). `[targets]` maps each arch to a single
# disk type, so this selects 0 or 1 target — the recipe is inert (empty `img`)
# for a project that doesn't build such a target.
img="$(printf '%s' "$BOOTCHER_METADATA" \
	| jq -r 'first(.targets[] | select(.arch == "aarch64" and .disk_type == "raw") | .file) // empty')"

if [ -z "$img" ]; then
	echo "raspi4: no aarch64 raw target in this build — nothing to do"
	exit 0
fi

echo "raspi4: embedding UEFI firmware into $img"

# A subshell scopes the temp dir and cleanup trap to this image.
(
	set -euo pipefail
	work="$(mktemp -d)"
	trap '
		sudo umount "$work/esp" 2>/dev/null || true
		rm -rf "$work"
	' EXIT

	mkdir -p "$work/fw" "$work/esp"
	curl -fSL --retry 3 "$FIRMWARE_URL" -o "$work/fw.zip"
	unzip -q "$work/fw.zip" -d "$work/fw"

	# Seed the NV variable store in the stock RPI_EFI.fd (Devicetree, plus full
	# RAM unless the cap is kept) before it gets overlaid onto the ESP.
	preseed_efi_vars "$work/fw/RPI_EFI.fd" "$KEEP_3GB_CAP"

	# Locate the ESP by GPT type and loop-mount that byte slice of the image
	# directly — no partition scanning, so it works in a container whose private
	# /dev never gets /dev/loopNpM nodes. The loop device auto-detaches on umount.
	extent="$(find_esp_extent "$img")"
	sudo mount -o "loop,offset=${extent%% *},sizelimit=${extent##* }" "$img" "$work/esp"

	# Overlay the firmware onto the ESP, overwriting the stock boot files.
	sudo cp -r "$work/fw"/. "$work/esp"/
	sudo sync
)

echo "raspi4: done -> $img"
