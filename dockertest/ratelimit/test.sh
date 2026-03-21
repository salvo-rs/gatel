#!/bin/sh
set -e

echo "Waiting for gatel..."
sleep 3

echo "Test 1: requests within limit succeed"
for i in 1 2 3; do
    STATUS=$(curl -s -o /dev/null -w '%{http_code}' http://gatel:8080/)
    [ "$STATUS" = "200" ] || { echo "FAIL: request $i returned $STATUS"; exit 1; }
done
echo "PASS: requests within limit"

echo "Test 2: exceeding limit returns 429"
STATUS=$(curl -s -o /dev/null -w '%{http_code}' http://gatel:8080/)
[ "$STATUS" = "429" ] || { echo "FAIL: expected 429, got $STATUS"; exit 1; }
echo "PASS: rate limit enforced"

echo "Test 3: after window expires, requests succeed again"
sleep 2
STATUS=$(curl -s -o /dev/null -w '%{http_code}' http://gatel:8080/)
[ "$STATUS" = "200" ] || { echo "FAIL: expected 200 after window, got $STATUS"; exit 1; }
echo "PASS: window reset"

echo ""
echo "All rate limit tests passed."
