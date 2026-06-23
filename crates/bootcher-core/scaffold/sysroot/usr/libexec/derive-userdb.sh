#!/bin/sh
set -eu
cd /usr/lib/userdb
for f in *.user; do
    # Process only canonical <username>.user records, never the <uid>.user
    # alias symlinks this script creates: on a rerun (e.g. a derived image
    # re-deriving on top of base's layer) an alias would set u=uid, making
    # `ln -sf "$u.user" "$uid.user"` link a path to itself (ELOOP).
    [ -f "$f" ] && [ ! -L "$f" ] || continue
    u=${f%.user}
    uid=$(jq -r '.uid' "$u.user")
    gid=$(jq -r '.gid' "$u.user")
    ln -sf "$u.user"  "$uid.user"
    ln -sf "$u.group" "$gid.group"
    if [ -f "$u.user-privileged" ]; then
        chmod 0600 "$u.user-privileged"
        ln -sf "$u.user-privileged" "$uid.user-privileged"
    fi
    jq -r '[.userName] + (.memberOf // []) | .[]' "$u.user" \
        | while read -r g; do printf '{}\n' > "$u:$g.membership"; done
done
