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
# Requires: jq, curl, unzip, sfdisk, awk, and sudo (loopback mount of the raw
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

# Locate the single EFI System Partition on a (loop-attached) disk by GPT
# partition type. Prints the partition devnode; bails on 0 or >1 matches — more
# robust than assuming the ESP is partition 1.
#
# We read the partition type straight from the GPT with `sfdisk -d` rather than
# `lsblk`'s PARTTYPE column: lsblk sources PARTTYPE from the udev database, which
# is empty in a privileged CI job container (no running udevd), so every
# partition reports a blank type and the ESP is "found 0". sfdisk parses the
# on-disk table itself, so it works with or without udev. Its dump lines look
# like `/dev/loop1p1 : start=…, type=C12A7328-…, uuid=…`; GPT GUIDs are emitted
# uppercase, so we case-fold both sides before comparing.
find_esp() {
	local disk="$1"
	local matches
	matches="$(sudo sfdisk -d "$disk" | awk -v guid="$EFI_PART_GUID" '
		BEGIN { guid = toupper(guid) }
		/^\/dev\// {
			for (i = 1; i <= NF; i++) {
				t = $i
				sub(/,$/, "", t)
				if (t ~ /^type=/ && toupper(substr(t, 6)) == guid)
					print $1
			}
		}')"
	if [ "$(printf '%s' "$matches" | grep -c .)" -ne 1 ]; then
		echo "raspi4: expected exactly one EFI System Partition on $disk, found $(printf '%s' "$matches" | grep -c .)" >&2
		return 1
	fi
	printf '%s\n' "$matches"
}

require jq curl unzip sudo losetup sfdisk awk mount umount

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

# A subshell scopes the temp dir, loop device, and cleanup trap to this image.
(
	set -euo pipefail
	loop=""
	work="$(mktemp -d)"
	trap '
		[ -n "$loop" ] && sudo umount "$work/esp" 2>/dev/null || true
		[ -n "$loop" ] && sudo losetup -d "$loop" 2>/dev/null || true
		rm -rf "$work"
	' EXIT

	mkdir -p "$work/fw" "$work/esp"
	curl -fSL --retry 3 "$FIRMWARE_URL" -o "$work/fw.zip"
	unzip -q "$work/fw.zip" -d "$work/fw"

	# Attach the raw image and locate its ESP by GPT type, settling udev
	# first so the partition devnodes exist before we look for them.
	loop="$(sudo losetup --find --show --partscan "$img")"
	command -v udevadm >/dev/null && sudo udevadm settle || true
	esp="$(find_esp "$loop")"
	sudo mount "$esp" "$work/esp"

	# Overlay the firmware onto the ESP, overwriting the stock boot files.
	sudo cp -r "$work/fw"/. "$work/esp"/
	sudo sync
)

echo "raspi4: done -> $img"
