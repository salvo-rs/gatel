#!/bin/sh
set -e

echo "Waiting for gatel..."
sleep 3

echo "Test 1: serve index.html"
BODY=$(curl -sf http://gatel:8080/)
echo "$BODY" | grep -q "hello-static" || { echo "FAIL: index.html not served"; exit 1; }
echo "PASS: serve index.html"

echo "Test 2: serve JSON file"
BODY=$(curl -sf http://gatel:8080/data.json)
echo "$BODY" | grep -q '"key"' || { echo "FAIL: data.json not served"; exit 1; }
echo "PASS: serve JSON file"

echo "Test 3: 404 for missing file"
STATUS=$(curl -s -o /dev/null -w '%{http_code}' http://gatel:8080/missing.txt)
[ "$STATUS" = "404" ] || { echo "FAIL: expected 404, got $STATUS"; exit 1; }
echo "PASS: 404 for missing file"

echo "Test 4: Content-Type header"
CT=$(curl -sf -o /dev/null -w '%{content_type}' http://gatel:8080/data.json)
echo "$CT" | grep -q "json" || { echo "FAIL: expected json content-type, got '$CT'"; exit 1; }
echo "PASS: Content-Type header"

echo ""
echo "All static file tests passed."
