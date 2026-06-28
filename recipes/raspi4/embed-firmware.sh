#!/usr/bin/env bash
#
# bootcher [hooks.disk] post recipe — Raspberry Pi 4
#
# Embeds the Raspberry Pi 4 UEFI firmware (pftf/RPi4) into the ESP of the built
# aarch64 raw image, so the board's bootloader can hand off to the bootc disk.
# This is exactly the kind of privileged, board-specific disk edit that has no
# business living in the Containerfile — so it lives here, in a disk.post hook.
#
# Wire it into bootcher.toml (see this directory's README.md):
#
#   [general.disk_types]
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

require jq curl unzip python3 sudo sfdisk mount umount

# The aarch64 raw disk bootcher just built. `.file` is the resolved disk.<ext>
# path (present at disk.post). `[general.disk_types]` maps each arch to a single
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
