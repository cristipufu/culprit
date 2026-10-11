# SQL Server demo

A disposable SQL Server 2022 container (`culprit-mssql-demo`, port 14333) with a synthetic online shop (`culprit_demo`, schema `shop`) and planted problems: a varchar column queried with nvarchar parameters, missing indexes, an unindexed foreign key, duplicate, redundant and unused indexes, an N+1 lookup, and a parameter-sensitive key-lookup plan. See the header of `demo.sql`.

```sh
dev/mssql/setup.sh                 # start, seed (a few minutes under emulation), run the workload
culprit conn add mssql-demo --env local --password-env MSSQL_SA_PASSWORD \
  --url 'mssql://sa@127.0.0.1:14333/culprit_demo?encrypt=false&trustServerCertificate=true'
MSSQL_SA_PASSWORD='Culprit-Demo-2026!' culprit scan mssql-demo
dev/mssql/setup.sh down            # remove it
```

The SA password is a throwaway for this local container; override it with `MSSQL_SA_PASSWORD`. On Apple silicon the image runs under emulation.
