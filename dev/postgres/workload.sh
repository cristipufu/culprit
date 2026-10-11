#!/bin/sh
# Runs a mixed workload for about 30 seconds: an N+1 lookup, unindexed foreign-key lookups,
# a dashboard scan, a sort that spills past work_mem, and reads on a table with stale statistics.
set -e
cd /demo/workload
pgbench -U postgres -n -c 4 -j 2 -T "${DURATION:-30}" \
  -f lookup_customer.sql@40 \
  -f orders_by_customer.sql@4 \
  -f customer_activity.sql@8 \
  -f product_sales.sql@2 \
  -f dashboard.sql@1 \
  -f export_notes.sql@1 \
  demo
