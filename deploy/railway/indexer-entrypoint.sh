#!/bin/sh
# Railway entrypoint: the indexer reads pure YAML (no env overrides), so
# materialize the config from env at container start. DATABASE_URL and
# ACKI_NACKI_ENDPOINT come from Railway service variables.
set -eu
: "${DATABASE_URL:?DATABASE_URL must be set}"
: "${ACKI_NACKI_ENDPOINT:=https://shellnet.ackinacki.org/graphql}"
cat > /app/config/indexer.runtime.yaml <<YAML
app:
  env: railway
  log_level: ${LOG_LEVEL:-info}

database:
  url: ${DATABASE_URL}
  max_connections: ${DB_MAX_CONNECTIONS:-10}
  min_connections: 1
  connect_timeout_ms: 3000

graphql:
  endpoint: ${ACKI_NACKI_ENDPOINT}
  page_size: 100
  request_timeout_ms: 10000

indexer:
  polling_interval_ms: 3000
  depth_refresh_interval_ms: 5000
  reconciliation_interval_ms: 60000
  reprojection_batch_size: 500
  oracle_event_list_reconciliation_interval_ms: 60000
YAML
export APP_CONFIG=/app/config/indexer.runtime.yaml
exec dodex-indexer
