#!/bin/bash
# Creates the streaming-replication role on the primary's first boot
# (F043). Mounted into docker-entrypoint-initdb.d; the password comes
# from a compose file-secret — never from the compose file, the
# environment, or any log line.
set -eu

REPL_PASSWORD="$(cat /run/secrets/replication_password)"
psql -v on_error_stop=1 --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" <<-EOSQL
    DO \$do\$ BEGIN
        IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'replicator') THEN
            CREATE ROLE replicator WITH REPLICATION LOGIN PASSWORD '$REPL_PASSWORD';
        END IF;
    END \$do\$;
EOSQL
