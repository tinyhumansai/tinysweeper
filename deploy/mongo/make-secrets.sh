#!/bin/sh
# Generate the shared secrets mongod and mongot need, once.
#
# Generated rather than committed: a keyfile in the repository is a keyfile
# everyone who clones the repository has. Idempotent, so restarting the stack
# does not invalidate a running deployment's credentials.
set -eu

secrets=/run/secrets/mongo
mkdir -p "$secrets"

if [ ! -s "$secrets/keyfile" ]; then
    openssl rand -base64 756 > "$secrets/keyfile"
fi
# mongod refuses to start if the keyfile is group- or world-readable, and it
# must be owned by the uid mongod runs as inside the community-server image.
chmod 400 "$secrets/keyfile"
chown 1000:1000 "$secrets/keyfile"

if [ ! -s "$secrets/mongot-password" ]; then
    # No trailing newline: mongot reads the file verbatim.
    openssl rand -hex 24 | tr -d '\n' > "$secrets/mongot-password"
fi
# mongot refuses anything more permissive than owner-read, and its image runs
# as root — so root must own this one, unlike the keyfile.
chmod 400 "$secrets/mongot-password"
chown 0:0 "$secrets/mongot-password"

# A second copy for mongod's first-boot script, which creates the user and runs
# as uid 1000. Two files rather than one group-readable file because mongot
# rejects any mode more permissive than owner-read outright, so there is no
# single set of permissions that satisfies both readers.
cp "$secrets/mongot-password" "$secrets/mongot-password.initdb"
chmod 400 "$secrets/mongot-password.initdb"
chown 1000:1000 "$secrets/mongot-password.initdb"
