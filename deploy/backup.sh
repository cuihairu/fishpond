#!/usr/bin/env sh
# fishpond backup (F039): a consistent pg_dump of the deployment
# database in custom format (compressed, restorable selectively).
#
# usage: backup.sh [--database-url <url>] <output-dir>
#
# The URL resolves like every fishpond command: --database-url, then
# FISHPOND_DATABASE_URL, then the secret file named by
# FISHPOND_DATABASE_URL_FILE. The URL is never echoed; failures name
# the source, not the value.
set -eu

url_arg=""
out_dir=""
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
        *)
            if [ -n "$out_dir" ]; then
                echo "error: unexpected argument $1" >&2
                exit 2
            fi
            out_dir="$1"
            shift
            ;;
    esac
done
[ -n "$out_dir" ] || { echo "usage: backup.sh [--database-url <url>] <output-dir>" >&2; exit 2; }
mkdir -p "$out_dir"

url="$url_arg"
source_name="argument --database-url"
if [ -z "$url" ] && [ -n "${FISHPOND_DATABASE_URL:-}" ]; then
    url="$FISHPOND_DATABASE_URL"
    source_name="environment variable FISHPOND_DATABASE_URL (value withheld)"
fi
if [ -z "$url" ] && [ -n "${FISHPOND_DATABASE_URL_FILE:-}" ]; then
    url="$(cat "$FISHPOND_DATABASE_URL_FILE")"
    source_name="secret file from FISHPOND_DATABASE_URL_FILE (value withheld)"
fi
[ -n "$url" ] || {
    echo "error: no database url (pass --database-url, set FISHPOND_DATABASE_URL, or mount FISHPOND_DATABASE_URL_FILE)" >&2
    exit 2
}

stamp="$(date -u +%Y%m%dT%H%M%SZ)"
dump="$out_dir/fishpond-$stamp.dump"
if ! pg_dump --format=custom --file="$dump" "$url"; then
    echo "error: pg_dump failed (url source: $source_name)" >&2
    rm -f "$dump"
    exit 1
fi
if ! pg_restore --list "$dump" >/dev/null 2>&1; then
    echo "error: the dump does not parse as a custom-format archive" >&2
    exit 1
fi
echo "backup written: $dump ($(du -h "$dump" | cut -f1))"
