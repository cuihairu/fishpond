#!/usr/bin/env sh
# fishpond restore verification (F039): restores a backup dump into a
# scratch database and proves it serves: pg_restore (custom format,
# exit-on-error) then the read-only `fishpond doctor` against the
# scratch url. The scratch database is dropped afterwards unless
# --keep is passed.
#
# usage: restore-verify.sh [--database-url <url>] [--fishpond <bin>] [--role <owner>] [--keep] <dump> <scratch-db-name>
#
# The dump restores with --no-owner --role=<owner role>, so the app
# role ends up owning the objects (RLS policies restored under it make
# sense again). Run as a superuser or the database owner; --role
# defaults to the user in the url. Note that FORCE ROW LEVEL SECURITY
# (baked into the migrations) bars the app role itself from reading
# its tables with plain COPY, so the dump and this script must connect
# as a role that bypasses RLS. URLs follow the fishpond resolution
# rules and are never echoed.
set -eu

url_arg=""
fishpond_bin="fishpond"
role_arg=""
keep=""
dump=""
scratch=""
while [ $# -gt 0 ]; do
    case "$1" in
        --database-url)
            [ $# -ge 2 ] || { echo "error: --database-url needs a value" >&2; exit 2; }
            url_arg="$2"
            shift 2
            ;;
        --database-url=*)
            url_arg="${1#*=}"
            shift
            ;;
        --fishpond)
            [ $# -ge 2 ] || { echo "error: --fishpond needs a value" >&2; exit 2; }
            fishpond_bin="$2"
            shift 2
            ;;
        --role)
            [ $# -ge 2 ] || { echo "error: --role needs a value" >&2; exit 2; }
            role_arg="$2"
            shift 2
            ;;
        --keep)
            keep="1"
            shift
            ;;
        *)
            if [ -z "$dump" ]; then
                dump="$1"
            elif [ -z "$scratch" ]; then
                scratch="$1"
            else
                echo "error: unexpected argument $1" >&2
                exit 2
            fi
            shift
            ;;
    esac
done
[ -n "$dump" ] && [ -n "$scratch" ] || {
    echo "usage: restore-verify.sh [--database-url <url>] [--fishpond <bin>] [--role <owner>] [--keep] <dump> <scratch-db-name>" >&2
    exit 2
}
[ -f "$dump" ] || { echo "error: dump not found: $dump" >&2; exit 2; }

url="$url_arg"
if [ -z "$url" ] && [ -n "${FISHPOND_DATABASE_URL:-}" ]; then
    url="$FISHPOND_DATABASE_URL"
fi
[ -n "$url" ] || {
    echo "error: no scratch database url (pass --database-url or set FISHPOND_DATABASE_URL)" >&2
    exit 2
}

role="$role_arg"
if [ -z "$role" ]; then
    role="$(echo "$url" | sed -n 's|.*://\([^:@/]*\).*|\1|p')"
fi
echo "restoring into scratch database $scratch (owned by $role) ..."
pg_restore --dbname="$url" --no-owner --role="$role" \
    --exit-on-error "$dump"
echo "restore complete; running doctor against the scratch database ..."
FISHPOND_DATABASE_URL="$url" "$fishpond_bin" doctor
echo "restore verified: $dump serves in $scratch"
if [ -z "$keep" ]; then
    base="${url%%\?*}"
    dbname="$(basename "$base")"
    query=""
    case "$url" in
        *\?*) query="?${url#*\?}" ;;
    esac
    psql "${base%/*}/postgres$query" -c "DROP DATABASE IF EXISTS $dbname" >/dev/null
    echo "scratch database dropped"
fi
