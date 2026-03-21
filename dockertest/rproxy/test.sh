#!/bin/sh
set -e

echo "Waiting for gatel..."
sleep 3

echo "Test 1: direct response"
BODY=$(curl -sf http://gatel:8080/)
[ "$BODY" = "gatel-direct" ] || { echo "FAIL: expected 'gatel-direct', got '$BODY'"; exit 1; }
echo "PASS: direct response"

echo "Test 2: proxy to backend"
BODY=$(curl -sf http://gatel:8080/api/test)
[ "$BODY" = "backend-ok" ] || { echo "FAIL: expected 'backend-ok', got '$BODY'"; exit 1; }
echo "PASS: proxy to backend"

echo "Test 3: proxy preserves status"
STATUS=$(curl -s -o /dev/null -w '%{http_code}' http://gatel:8080/api/test)
[ "$STATUS" = "200" ] || { echo "FAIL: expected 200, got $STATUS"; exit 1; }
echo "PASS: proxy preserves status"

echo ""
echo "All reverse proxy tests passed."
