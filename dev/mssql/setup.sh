#!/usr/bin/env bash
# A disposable SQL Server with the synthetic `shop` demo (demo.sql) and a workload that exercises it.
#   dev/mssql/setup.sh            # start the container if needed, seed culprit_demo, run the workload
#   dev/mssql/setup.sh workload   # run the workload again
#   dev/mssql/setup.sh down       # remove the container
set -euo pipefail
: "${MSSQL_SA_PASSWORD:=Culprit-Demo-2026!}"
export MSSQL_SA_PASSWORD
CONTAINER="${CULPRIT_MSSQL_CONTAINER:-culprit-mssql-demo}"
PORT="${CULPRIT_MSSQL_PORT:-14333}"
IMAGE="mcr.microsoft.com/mssql/server:2022-latest"
here="$(cd "$(dirname "$0")" && pwd)"

sqlcmd() {
  docker exec -i -e SQLCMDPASSWORD="$MSSQL_SA_PASSWORD" "$CONTAINER" \
    /opt/mssql-tools18/bin/sqlcmd -S localhost -U sa -C -I -b "$@"
}

if [[ "${1:-all}" == "down" ]]; then
  docker rm -f "$CONTAINER"
  exit 0
fi

if ! docker ps -a --format '{{.Names}}' | grep -qx "$CONTAINER"; then
  docker run -d --name "$CONTAINER" --platform linux/amd64 \
    -e ACCEPT_EULA=Y -e MSSQL_SA_PASSWORD="$MSSQL_SA_PASSWORD" -e MSSQL_MEMORY_LIMIT_MB=2048 \
    -p "$PORT:1433" "$IMAGE" >/dev/null
fi
docker start "$CONTAINER" >/dev/null

echo "waiting for SQL Server…"
for _ in $(seq 1 90); do
  sqlcmd -Q "SELECT 1" >/dev/null 2>&1 && break
  sleep 2
done

if [[ "${1:-all}" != "workload" ]]; then
  echo "seeding culprit_demo (a few minutes)…"
  sqlcmd -i /dev/stdin < "$here/demo.sql"
fi

echo "running workload…"
CULPRIT_MSSQL_ADO="Server=127.0.0.1,$PORT;Database=culprit_demo;User ID=sa;Password=$MSSQL_SA_PASSWORD;Encrypt=false;TrustServerCertificate=true" \
  cargo run -q --release -p culprit-mssql --example workload
