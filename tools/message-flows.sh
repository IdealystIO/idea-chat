#!/usr/bin/env bash
# Drive the message endpoints against a running Hearth server.
#
# Complements tools/auth-flows.sh: that one proves who you are, this one proves
# what that lets you read and write. Run it after auth-flows.sh against the
# same server — it signs in as the accounts that one created.
set -u
ORIGIN_URL=${ORIGIN_URL:-http://127.0.0.1:8091}
API=${API:-$ORIGIN_URL/_srv}
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
A=$TMP/ana.jar; T=$TMP/theo.jar
PASS=0; FAIL=0
has(){ case "$2" in *"$3"*) echo "  ok   $1"; PASS=$((PASS+1));; *) echo " FAIL  $1"; echo "        got: $2"; FAIL=$((FAIL+1));; esac; }
no(){ case "$2" in *"$3"*) echo " FAIL  $1"; echo "        got: $2"; FAIL=$((FAIL+1));; *) echo "  ok   $1"; PASS=$((PASS+1));; esac; }
as(){ jar=$1; shift; curl -sS -X POST "$API/$1" -H 'Content-Type: application/json' -H "Origin: $ORIGIN_URL" -b $jar -d "$2"; }
# A UUIDv7 with a current timestamp — the server rejects a skewed one.
v7(){ python3 - <<'PY'
import os,time
ms=int(time.time()*1000); r=os.urandom(10)
b=ms.to_bytes(6,'big')+bytes([0x70|(r[0]&0x0f),r[1],0x80|(r[2]&0x3f)])+r[3:10]
h=b.hex(); print(f"{h[:8]}-{h[8:12]}-{h[12:16]}-{h[16:20]}-{h[20:32]}")
PY
}

curl -sS -X POST "$API/login" -H 'Content-Type: application/json' -H "Origin: $ORIGIN_URL" -c $A -d '[{"identifier":"ana","password":"password123"}]' >/dev/null
curl -sS -X POST "$API/login" -H 'Content-Type: application/json' -H "Origin: $ORIGIN_URL" -c $T -d '[{"identifier":"theo","password":"password123"}]' >/dev/null
SPACES=$(as $A my_spaces 'null')
CH=$(echo "$SPACES" | grep -o '"id":"[^"]*","kind":"text"[^}]*"name":"general"' | grep -o '^"id":"[^"]*"' | cut -d'"' -f4)
[ -z "$CH" ] && CH=$(echo "$SPACES" | python3 -c 'import sys,json; d=json.load(sys.stdin)["Ok"]; print([c["id"] for c in d[0]["channels"] if c["name"]=="general"][0])')

echo "--- posting ---"
M1=$(v7)
R=$(as $A send_message "[\"$M1\",\"$CH\",\"first message\",null]")
has "a member can post" "$R" '"body":"first message"'
has "the author is named in the response" "$R" '"author_name":"Ana Ruiz"'
R=$(as $A send_message "[\"$M1\",\"$CH\",\"first message\",null]")
has "resending the same id is idempotent" "$R" "\"id\":\"$M1\""
R=$(as $A channel_page "[\"$CH\",null]")
C=$(echo "$R" | grep -o '"id":"' | wc -l)
has "and did not post twice" "$C" "1"

echo "--- threads ---"
R1=$(v7); as $A send_message "[\"$R1\",\"$CH\",\"a reply\",\"$M1\"]" >/dev/null
R=$(as $A thread "[\"$M1\"]")
has "a thread returns its root and replies" "$R" '"a reply"'
R=$(as $A channel_page "[\"$CH\",null]")
has "the reply count lands on the root" "$R" '"reply_count":1'
no  "and the reply stays out of the channel view" "$R" '"body":"a reply"'

echo "--- authorization ---"
R=$(as $T send_message "[\"$(v7)\",\"$CH\",\"me too\",null]")
has "another member of the space can post" "$R" '"body":"me too"'
R=$(as $T edit_message "[\"$M1\",\"rewritten\"]")
has "but cannot edit someone else's message" "$R" 'not yours to edit'
R=$(as $T delete_message "[\"$M1\",\"$CH\"]")
has "nor delete it" "$R" 'not yours to delete'
R=$(as $A edit_message "[\"$M1\",\"revised\"]")
has "the author can edit their own" "$R" '"body":"revised"'

echo "--- unknown channel ---"
R=$(as $T channel_page '["01a03000-0000-7000-8000-0000000000ff",null]')
has "a channel you cannot see reads as absent" "$R" 'no such channel'

echo "--- read state ---"
R=$(as $T unread 'null')
has "a channel with new messages is unread" "$R" "$CH"
# Mark up to the channel's NEWEST message, not the first one — messages were
# posted after M1, so marking there correctly leaves the channel unread.
LAST=$(as $T channel_page "[\"$CH\",null]" | python3 -c 'import sys,json;m=json.load(sys.stdin)["Ok"]["messages"];print(m[-1]["id"])')
as $T mark_read "[\"$CH\",\"$LAST\"]" >/dev/null
R=$(as $T unread 'null')
no "and is clear once marked" "$R" "$CH"

echo "--- direct messages ---"
ANA=$(as $A me 'null' | python3 -c 'import sys,json;print(json.load(sys.stdin)["Ok"]["id"])')
R=$(as $T open_dm "[\"$ANA\"]")
has "a DM can be opened" "$R" '"kind":"dm"'
DM=$(echo "$R" | python3 -c 'import sys,json;print(json.load(sys.stdin)["Ok"]["id"])')
R2=$(as $A open_dm "[$(as $T me 'null' | python3 -c 'import sys,json;print(chr(34)+json.load(sys.stdin)["Ok"]["id"]+chr(34))')]")
has "and is the same channel from the other side" "$R2" "\"id\":\"$DM\""
R=$(as $A send_message "[\"$(v7)\",\"$DM\",\"just us\",null]")
has "both parties can post in it" "$R" '"body":"just us"'

echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
