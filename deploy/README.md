# Deploy

F029 deployment face: PostgreSQL + one-shot `migrate` job + read-only
`doctor` check, with every credential as a compose file-secret. The
long-running app services (`fishpond-server`, `fishpond-worker`) join
this stack in a later task, when their binaries ship in the image.

## One-time setup

Create the secret files (they are git-ignored; `secrets/.gitignore`
keeps them out of version control):

```sh
cd deploy
mkdir -p secrets

# Database password.
openssl rand -base64 24 > secrets/postgres_password.txt

# Full database URL — the password must match the file above. The URL
# is mounted as a secret and resolved via FISHPOND_DATABASE_URL_FILE;
# it never appears in environment variables, logs or CLI output.
printf 'postgres://fishpond:%s@postgres:5432/fishpond' "$(cat secrets/postgres_password.txt)" \
  > secrets/database_url.txt
```

## Run

```sh
docker compose -f deploy/docker-compose.yml up --build
```

Startup order is enforced by conditions, not by sleep:

1. `postgres` — healthy when `pg_isready` passes.
2. `migrate` — applies pending migrations (idempotent, exit 0).
3. `doctor` — read-only checks: URL source named, `SELECT 1`
   round-trip, every embedded migration applied. Exit 0 healthy,
   1 unhealthy, 2 usage error.

`migrate` and `doctor` are one-shot (`restart: "no"`); to re-check a
running stack: `docker compose -f deploy/docker-compose.yml run --rm doctor`.

## Multi-node topology (F037)

`docker-compose.multi.yml` is the fleet variant: the same base face
(postgres + `migrate` + `doctor`) plus N worker services — every node
shares ONE database, so placement, leases and drain coordinate through
it and nodes need no network between each other. Each node mounts its
own profile volume (`profiles-node-N`): browser profiles are
node-local and never travel implicitly — moving one between nodes is
the F037 secure migration flow (`ProfileManager::migrate`: verified
snapshot, byte-checked destination copy, then the row swap, rollback
artifact kept under the backup root).

Node identity is the worker **name** (`command: ["worker", "--name",
"node-N"]`; one host may run several worker processes under one name).
The operator face for taking a machine down is the node drain:
every worker registered under that name drains together (marked
draining first, so migrations never land on a not-yet-drained
sibling), its live leases migrate to surviving nodes, and an unknown
or foreign-tenant name is refused rather than silently draining
nothing. The drain is idempotent — a second pass reports what was
already draining and moves nothing.

Worker services join the stack once the `worker` entry point ships in
the image (the engine-face task); today the binary is a stub, so the
worker services exit right after start. The same credential rules as
the base stack apply: secrets are compose file-secrets, never
environment variables, never logs.

## Backup and restore verification (F039)

`backup.sh` produces a consistent, compressed, custom-format
`pg_dump` (`fishpond-<utc-stamp>.dump`) and proves it parses before
calling it a backup. `restore-verify.sh` closes the loop: it restores
a dump into a scratch database (objects re-owned by the app role via
`--role`), runs the read-only `fishpond doctor` against the scratch
url, and drops the scratch database unless `--keep`. Both resolve the
url like every fishpond command (`--database-url`, then
`FISHPOND_DATABASE_URL`, then the secret file named by
`FISHPOND_DATABASE_URL_FILE`) and never echo the url — errors name
the source, not the value.

**The dump role must bypass row-level security.** The migrations set
`FORCE ROW LEVEL SECURITY` on every tenant table, which bars even the
table owner from reading rows — so the app role cannot `pg_dump` its
own database (it fails on the first tenant table). Dump and
restore-verify therefore connect as a role that bypasses RLS
(superuser in the compose stack; locally: `sudo -u postgres` over the
unix socket). Objects are restored with `--no-owner --role=<app
role>`, so the app role owns them again in the scratch database and
the restored policies make sense.

Drilled locally (2026-11-27): backup of the test database
(936 KB custom dump) → restore into a scratch database → doctor
healthy (reachable, schema current) → scratch dropped. In compose, run
both scripts inside the `postgres` service (or a sidecar with the
same socket access), pointing `FISHPOND_DATABASE_URL` at the
superuser url.
