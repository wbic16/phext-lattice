#!/bin/bash
# phext-serve curl smoke: scratch data dir, all endpoints, restart persistence.
BIN=/home/wbic16/phext-lattice-vc/target/release/phext-serve
DATA=/home/wbic16/serve-smoke-data
PORT=19731
B=http://127.0.0.1:$PORT/api/v2
rm -rf $DATA; mkdir -p $DATA
cd $DATA || exit 1
$BIN $PORT > server1.log 2>&1 &
SRV=$!
sleep 1.5
P=0; F=0
chk() { if [ "$2" = "$3" ]; then echo "PASS $1"; P=$((P+1)); else echo "FAIL $1: want [$3] got [$2]"; F=$((F+1)); fi }
contains() { case "$2" in *"$3"*) echo "PASS $1"; P=$((P+1));; *) echo "FAIL $1: [$2] lacks [$3]"; F=$((F+1));; esac }

# create soul via two updates (2 commits)
curl -s -X POST "$B/update?c=1.1.1/1.1.1/1.1.1&p=alice" --data-binary 'Alpha scroll
body a' > /dev/null
curl -s -X POST "$B/update?c=1.1.1/1.1.1/1.1.2&p=alice" --data-binary 'Beta scroll' > /dev/null
# insert appends as one commit
curl -s -X POST "$B/insert?c=1.1.1/1.1.1/1.1.2&p=alice" --data-binary '+tail' > /dev/null
# txn: three writes, ONE commit
RESP=$(curl -s -X POST "$B/txn?p=alice" -H 'Content-Type: application/json' --data-binary '{"writes":[["1.1.1/1.1.1/1.2.1","Gamma"],["1.1.1/1.1.1/1.2.2","Delta"],["1.1.1/1.1.1/1.2.3","Epsilon"]]}')
contains "txn response seq" "$RESP" '"seq":4'
# selects
chk "select 1" "$(curl -s "$B/select?c=1.1.1/1.1.1/1.1.1&p=alice")" 'Alpha scroll
body a'
chk "select 2 (update+insert)" "$(curl -s "$B/select?c=1.1.1/1.1.1/1.1.2&p=alice")" 'Beta scroll+tail'
chk "select txn scroll" "$(curl -s "$B/select?c=1.1.1/1.1.1/1.2.2&p=alice")" 'Delta'
chk "select absent empty" "$(curl -s "$B/select?c=9.9.9/9.9.9/9.9.9&p=alice")" ''
# toc: all five coords
TOC=$(curl -s "$B/toc?p=alice")
for c in 1.1.1/1.1.1/1.1.1 1.1.1/1.1.1/1.1.2 1.1.1/1.1.1/1.2.1 1.1.1/1.1.1/1.2.2 1.1.1/1.1.1/1.2.3; do contains "toc has $c" "$TOC" "$c"; done
contains "toc title" "$TOC" 'Alpha scroll'
# seq: exactly 4 commits (2 update + 1 insert + 1 txn)
chk "seq == 4" "$(curl -s "$B/seq?p=alice")" '{"seq":4}'
# updates since 0 lists 4 txns
UPD=$(curl -s "$B/updates?p=alice&since=0")
NUPD=$(printf '%s' "$UPD" | grep -o '"seq":' | wc -l)
chk "updates count" "$NUPD" '4'
contains "updates txn coords" "$UPD" '1.1.1/1.1.1/1.2.3'
# second soul isolation
curl -s -X POST "$B/update?c=1.1.1/1.1.1/1.1.1&p=bob" --data-binary 'Bob only' > /dev/null
chk "soul isolation seq" "$(curl -s "$B/seq?p=bob")" '{"seq":1}'
chk "soul isolation select" "$(curl -s "$B/select?c=1.1.1/1.1.1/1.1.1&p=alice")" 'Alpha scroll
body a'
# quads replace: toy 4-quad set
QR=$(curl -s -X POST "$B/quads?p=alice" -H 'Content-Type: application/json' --data-binary '{"replace":["<http://ex.org/a> <http://ex.org/name> \"Alice\" .","<http://ex.org/a> <http://ex.org/knows> <http://ex.org/b> .","<http://ex.org/b> <http://ex.org/name> \"Bob\" .","<http://ex.org/b> <http://ex.org/knows> <http://ex.org/a> ."]}')
chk "quads count" "$QR" '{"quads":4}'
# SELECT query
QH=$(curl -s -D - -X POST "$B/query?p=alice&base=http://ex.org/" --data-binary 'SELECT ?s ?n WHERE { ?s <http://ex.org/name> ?n }' -o /tmp/qbody.txt)
contains "query X-Exit 0" "$QH" 'X-Exit: 0'
QB=$(cat /tmp/qbody.txt)
contains "query SOLUTIONS" "$QB" 'SOLUTIONS'
contains "query row Alice" "$QB" '"Alice"'
contains "query row Bob" "$QB" '"Bob"'
# ASK
AH=$(curl -s -D - -X POST "$B/query?p=alice&base=http://ex.org/" --data-binary 'ASK { <http://ex.org/a> <http://ex.org/knows> <http://ex.org/b> }' -o /tmp/abody.txt)
contains "ask X-Exit 0" "$AH" 'X-Exit: 0'
chk "ask BOOLEAN true" "$(cat /tmp/abody.txt)" 'BOOLEAN	true'
# refused construct: SERVICE
RH=$(curl -s -D - -X POST "$B/query?p=alice&base=http://ex.org/" --data-binary 'SELECT ?s WHERE { SERVICE <http://ex.org/sparql> { ?s ?p ?o } }' -o /tmp/rbody.txt)
contains "refused X-Exit 3" "$RH" 'X-Exit: 3'
contains "refused line" "$(cat /tmp/rbody.txt)" 'REFUSED	SERVICE'
# bad request does not kill the server
curl -s "$B/select?c=banana&p=alice" > /dev/null
chk "server survives bad coord" "$(curl -s "$B/seq?p=alice")" '{"seq":4}'
# emptied scroll leaves toc
curl -s -X POST "$B/update?c=1.1.1/1.1.1/1.2.3&p=alice" --data-binary '' > /dev/null
TOC2=$(curl -s "$B/toc?p=alice")
case "$TOC2" in *"1.2.3"*) echo "FAIL emptied scroll still in toc"; F=$((F+1));; *) echo "PASS emptied scroll leaves toc"; P=$((P+1));; esac

# restart persistence (WAL recovery)
kill $SRV; sleep 1
cd $DATA || exit 1
$BIN $PORT > server2.log 2>&1 &
SRV=$!
sleep 1.5
chk "restart select 1" "$(curl -s "$B/select?c=1.1.1/1.1.1/1.1.1&p=alice")" 'Alpha scroll
body a'
chk "restart select txn" "$(curl -s "$B/select?c=1.1.1/1.1.1/1.2.1&p=alice")" 'Gamma'
chk "restart seq" "$(curl -s "$B/seq?p=alice")" '{"seq":5}'
kill $SRV
echo "SMOKE: $P passed, $F failed"
