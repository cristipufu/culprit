#!/usr/bin/env bash
# Local MySQL demo with planted performance problems. Throwaway credentials for a disposable container.
#   dev/mysql/setup.sh            # start the container (if needed), seed it, run the workload
#   dev/mysql/setup.sh workload   # only run the workload again
#   dev/mysql/setup.sh down       # remove the container
set -euo pipefail
: "${MYSQL_DEMO_PASSWORD:=culprit-demo}"
export MYSQL_DEMO_PASSWORD
CONTAINER="${CULPRIT_MYSQL_CONTAINER:-culprit-mysql-demo}"
PORT="${CULPRIT_MYSQL_PORT:-33306}"
IMAGE="${CULPRIT_MYSQL_IMAGE:-mysql:8.4}"
here="$(cd "$(dirname "$0")" && pwd)"

mysql_in_container() {
  docker exec -i -e MYSQL_PWD="$MYSQL_DEMO_PASSWORD" "$CONTAINER" mysql -uroot "$@"
}

case "${1:-all}" in
  down)
    docker rm -f "$CONTAINER" >/dev/null && echo "removed $CONTAINER"
    exit 0
    ;;
  workload) ;;
  all)
    if ! docker ps -a --format '{{.Names}}' | grep -qx "$CONTAINER"; then
      echo "starting $CONTAINER on port ${PORT}…"
      # Planted: a 32 MB buffer pool for ~150 MB of data, and 1 MB in-memory temporary tables (MEMORY engine) so big
      # GROUP BYs spill to on-disk temporary tables.
      docker run -d --name "$CONTAINER" -p "$PORT:3306" \
        -e MYSQL_ROOT_PASSWORD="$MYSQL_DEMO_PASSWORD" -e MYSQL_ROOT_HOST=% \
        "$IMAGE" --performance-schema=ON --innodb-buffer-pool-size=33554432 \
        --internal-tmp-mem-storage-engine=MEMORY --tmp-table-size=1048576 --max-heap-table-size=1048576 \
        --max-allowed-packet=67108864 >/dev/null
    else
      docker start "$CONTAINER" >/dev/null
    fi
    echo -n "waiting for MySQL"
    until mysql_in_container -e "SELECT 1" >/dev/null 2>&1; do echo -n "."; sleep 2; done
    echo
    echo "seeding shop…"
    mysql_in_container < "$here/seed.sql"
    ;;
  *) echo "usage: $0 [all|workload|down]" >&2; exit 2 ;;
esac

echo "resetting statement statistics and running the workload…"
mysql_in_container -e "TRUNCATE performance_schema.events_statements_summary_by_digest; TRUNCATE performance_schema.table_io_waits_summary_by_index_usage; TRUNCATE performance_schema.table_io_waits_summary_by_table;"
CULPRIT_MYSQL_URL="mysql://root:${MYSQL_DEMO_PASSWORD}@127.0.0.1:${PORT}/shop" \
  cargo run -q -p culprit-mysql --example workload
