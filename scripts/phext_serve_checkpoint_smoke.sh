#!/bin/bash
# phext-serve checkpoint smoke: WAL growth, POST /checkpoint compaction,
# restart persistence, and graceful SIGTERM checkpointing.
BIN=${PHEXT_SERVE_BIN:-/home/wbic16/phext-lattice-fold/target/release/phext-serve}
DATA=/home/wbic16/serve-ckpt-smoke-data
PORT=19732
B=http://127.0.0.1:$PORT/api/v2
rm -rf $DATA; mkdir -p $DATA
cd $DATA || exit 1
P=0; F=0
chk() { if [ "$2" = "$3" ]; then echo "PASS $1"; P=$((P+1)); else echo "FAIL $1: want [$3] got [$2]"; F=$((F+1)); fi }
contains() { case "$2" in *"$3"*) echo "PASS $1"; P=$((P+1));; *) echo "FAIL $1: [$2] lacks [$3]"; F=$((F+1));; esac }
gt() { if [ "$2" -gt "$3" ] 2>/dev/null; then echo "PASS $1"; P=$((P+1)); else echo "FAIL $1: $2 not > $3"; F=$((F+1)); fi }
lt() { if [ "$2" -lt "$3" ] 2>/dev/null; then echo "PASS $1"; P=$((P+1)); else echo "FAIL $1: $2 not < $3"; F=$((F+1)); fi }
fsize() { stat -c %s "$1" 2>/dev/null || echo 0; }
jget() { printf '%s' "$1" | grep -o "\"$2\":[0-9]*" | head -1 | cut -d: -f2; }

$BIN $PORT > server1.log 2>&1 &
SRV=$!
sleep 1.5

# Several txns, including repeated rewrites of the SAME scroll, so the
# WAL accumulates superseded versions while the main file stays small.
BODY1=$(printf 'version one %.0s' $(seq 1 200))
BODY2=$(printf 'version two is longer %.0s' $(seq 1 300))
BODY3=$(printf 'version three final %.0s' $(seq 1 250))
curl -s -X POST "$B/update?c=1.1.1/1.1.1/1.1.1&p=ckpt" --data-binary "$BODY1" > /dev/null
curl -s -X POST "$B/update?c=1.1.1/1.1.1/1.1.1&p=ckpt" --data-binary "$BODY2" > /dev/null
curl -s -X POST "$B/update?c=1.1.1/1.1.1/1.1.1&p=ckpt" --data-binary "$BODY3" > /dev/null
curl -s -X POST "$B/txn?p=ckpt" -H 'Content-Type: application/json' --data-binary '{"writes":[["1.1.1/1.1.1/1.1.2","Second scroll"],["1.1.1/1.1.1/1.1.3","Third scroll"]]}' > /dev/null
curl -s -X POST "$B/update?c=1.1.1/1.1.1/1.1.2&p=ckpt" --data-binary 'Second scroll rewritten' > /dev/null

SEQ_PRE=$(curl -s "$B/seq?p=ckpt")
contains "seq pre" "$SEQ_PRE" '"seq":5'
MAIN_PRE=$(fsize ckpt.phext); WAL_PRE=$(fsize ckpt.wal.phext)
echo "pre-checkpoint: main=$MAIN_PRE wal=$WAL_PRE"
gt "wal exceeds main pre-checkpoint" "$WAL_PRE" "$MAIN_PRE"

# Checkpoint via the endpoint.
RESP=$(curl -s -X POST "$B/checkpoint?p=ckpt")
echo "checkpoint response: $RESP"
contains "checkpoint seq" "$RESP" '"seq":5'
MAIN_POST=$(jget "$RESP" main_bytes); WAL_POST=$(jget "$RESP" wal_bytes)
gt "main grew at checkpoint" "$MAIN_POST" "$MAIN_PRE"
lt "wal shrank at checkpoint" "$WAL_POST" "$WAL_PRE"
chk "stat main matches response" "$(fsize ckpt.phext)" "$MAIN_POST"
chk "stat wal matches response" "$(fsize ckpt.wal.phext)" "$WAL_POST"

# Restart (plain kill): content and seq survive, now served from main.
kill $SRV; sleep 1
$BIN $PORT > server2.log 2>&1 &
SRV=$!
sleep 1.5
chk "restart select rewritten scroll" "$(curl -s "$B/select?c=1.1.1/1.1.1/1.1.1&p=ckpt")" "$BODY3"
chk "restart select txn scroll" "$(curl -s "$B/select?c=1.1.1/1.1.1/1.1.2&p=ckpt")" 'Second scroll rewritten'
chk "restart seq preserved" "$(curl -s "$B/seq?p=ckpt")" '{"seq":5}'

# One more write, then SIGTERM: graceful stop must checkpoint it.
curl -s -X POST "$B/update?c=1.1.1/1.1.1/1.1.4&p=ckpt" --data-binary 'Written before SIGTERM' > /dev/null
WAL_SIG_PRE=$(fsize ckpt.wal.phext)
kill -TERM $SRV
wait $SRV; EXIT=$?
chk "SIGTERM exit code 0" "$EXIT" '0'
WAL_SIG_POST=$(fsize ckpt.wal.phext)
echo "sigterm: wal before=$WAL_SIG_PRE after=$WAL_SIG_POST"
lt "wal compacted by graceful stop" "$WAL_SIG_POST" "$WAL_SIG_PRE"

# Fresh start after SIGTERM still reads everything.
$BIN $PORT > server3.log 2>&1 &
SRV=$!
sleep 1.5
chk "post-SIGTERM select 1" "$(curl -s "$B/select?c=1.1.1/1.1.1/1.1.1&p=ckpt")" "$BODY3"
chk "post-SIGTERM select pre-term write" "$(curl -s "$B/select?c=1.1.1/1.1.1/1.1.4&p=ckpt")" 'Written before SIGTERM'
chk "post-SIGTERM seq" "$(curl -s "$B/seq?p=ckpt")" '{"seq":6}'
kill $SRV; sleep 1
echo "CKPT-SMOKE: $P passed, $F failed"
