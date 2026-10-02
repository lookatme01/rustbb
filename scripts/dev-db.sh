#!/usr/bin/env bash
# Starts a project-local PostgreSQL cluster in ./data/pg (port 5433) and creates the `rbb` database.
set -euo pipefail
cd "$(dirname "$0")/.."
PGBIN="${PGBIN:-$(brew --prefix postgresql@17 2>/dev/null)/bin}"
[ -x "$PGBIN/pg_ctl" ] || PGBIN="$(dirname "$(command -v pg_ctl)")"
PGDATA="${PGDATA:-$PWD/data/pg}"
PORT="${PGPORT:-5433}"
if [ ! -f "$PGDATA/PG_VERSION" ]; then
  mkdir -p "$PGDATA"
  "$PGBIN/initdb" -D "$PGDATA" -U rbb --auth=trust -E UTF8 --locale=C >/dev/null
  cat >> "$PGDATA/postgresql.conf" <<CONF
port = $PORT
listen_addresses = '127.0.0.1'
unix_socket_directories = '$PGDATA'
max_connections = 200
shared_buffers = 512MB
effective_cache_size = 2GB
work_mem = 16MB
maintenance_work_mem = 256MB
synchronous_commit = off   # development only: never in production
wal_compression = on
checkpoint_timeout = 15min
max_wal_size = 4GB
random_page_cost = 1.1
CONF
fi
if ! "$PGBIN/pg_ctl" -D "$PGDATA" status >/dev/null 2>&1; then
  "$PGBIN/pg_ctl" -D "$PGDATA" -l "$PGDATA/server.log" -w start >/dev/null
fi
"$PGBIN/psql" -h 127.0.0.1 -p "$PORT" -U rbb -d postgres -tAc "SELECT 1 FROM pg_database WHERE datname='rbb'" | grep -q 1 \
  || "$PGBIN/createdb" -h 127.0.0.1 -p "$PORT" -U rbb rbb
echo "PostgreSQL running: postgres://rbb@127.0.0.1:$PORT/rbb"
