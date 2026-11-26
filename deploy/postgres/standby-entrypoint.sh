#!/bin/bash
# Bootstraps a hot standby from the primary via pg_basebackup -R
# (F043), then hands over to the stock postgres entrypoint. First
# boot only: a PGDATA that already holds a cluster (seeded or
# restored volume) starts directly, so a restarted standby never
# re-copies the primary.
set -eu

if [ ! -s "$PGDATA/PG_VERSION" ]; then
    echo "bootstrapping standby from postgres-primary ..."
    until pg_isready -q -h postgres-primary -U fishpond -d fishpond; do
        sleep 1
    done
    # The password is only ever passed through the environment of
    # this bootstrap step; -R persists primary_conninfo (including
    # it) inside PGDATA on the standby's own volume — never in the
    # compose file, the environment of the running service, or logs.
    PGPASSWORD="$(cat /run/secrets/replication_password)"
    export PGPASSWORD
    gosu postgres pg_basebackup \
        -h postgres-primary -U replicator -D "$PGDATA" -R -P -X stream
    unset PGPASSWORD
fi

exec docker-entrypoint.sh postgres
