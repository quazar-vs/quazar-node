#!/bin/bash
# Smoke: state-mediated exchange (sell → SB verify → buy → SB confirm)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
if [ -f "$SCRIPT_DIR/.env" ]; then
  set -a
  # shellcheck disable=SC1091
  source "$SCRIPT_DIR/.env"
  set +a
fi

BASE_URL="${BASE_URL:-http://localhost:8080}"
# Prefer separate keys if set; else same key for all steps (must be Guardian/Aiya capable).
SELLER_KEY="${QUAZAR_SELLER_API_KEY:-${QUAZAR_API_KEY:?QUAZAR_API_KEY not set}}"
BUYER_KEY="${QUAZAR_BUYER_API_KEY:-$SELLER_KEY}"
SB_KEY="${QUAZAR_SB_API_KEY:-$SELLER_KEY}"
AIYA_KEY="${QUAZAR_AIYA_API_KEY:-$SELLER_KEY}"

auth() { echo "Authorization: Bearer $1"; }

echo "=== Quazar exchange (state-mediated) ==="

echo "1. Seed inventory for seller (requires SB/Aiya)"
curl -sS -X POST "$BASE_URL/exchange/inventory" \
  -H "$(auth "$AIYA_KEY")" -H "Content-Type: application/json" \
  -d "{\"citizen_id\": \"${SELLER_CITIZEN_ID:-testcitizen}\", \"svod_code\": \"WEB_DEV\", \"quantity\": 100}" | jq .

echo "2. Fund buyer"
curl -sS -X POST "$BASE_URL/exchange/balance/add" \
  -H "$(auth "$AIYA_KEY")" -H "Content-Type: application/json" \
  -d "{\"citizen_id\": \"${BUYER_CITIZEN_ID:-buyercitizen}\", \"amount\": 20000}" | jq .

echo "3. Seller creates sell request qty=100"
REQ=$(curl -sS -X POST "$BASE_URL/exchange/requests" \
  -H "$(auth "$SELLER_KEY")" -H "Content-Type: application/json" \
  -d '{"side":"sell","svod_code":"WEB_DEV","quantity":100}')
echo "$REQ" | jq .
REQ_ID=$(echo "$REQ" | jq -r '.data.id')

echo "4. SB verifies request"
VER=$(curl -sS -X POST "$BASE_URL/exchange/requests/$REQ_ID/sb-verify" \
  -H "$(auth "$SB_KEY")" -H "Content-Type: application/json" \
  -d '{"approve":true}')
echo "$VER" | jq .
LISTING_ID=$(echo "$VER" | jq -r '.data.listing_id')

echo "5. Buyer takes 15 units"
DEAL=$(curl -sS -X POST "$BASE_URL/exchange/listings/$LISTING_ID/buy" \
  -H "$(auth "$BUYER_KEY")" -H "Content-Type: application/json" \
  -d '{"quantity":15}')
echo "$DEAL" | jq .
TASK_ID=$(echo "$DEAL" | jq -r '.data.security_task_id')

echo "6. SB confirms delivery"
curl -sS -X POST "$BASE_URL/exchange/security/tasks/$TASK_ID/confirm" \
  -H "$(auth "$SB_KEY")" -H "Content-Type: application/json" \
  -d '{}' | jq .

echo "7. Balances"
curl -sS "$BASE_URL/exchange/balance" -H "$(auth "$BUYER_KEY")" | jq .
curl -sS "$BASE_URL/exchange/balance" -H "$(auth "$SELLER_KEY")" | jq .

echo "=== done ==="
