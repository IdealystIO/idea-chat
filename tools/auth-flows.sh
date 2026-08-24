#!/usr/bin/env bash
# Drive every authentication flow against a running Hearth server.
#
# Not a unit test: the things worth checking here are properties of the whole
# stack — that the session cookie is httpOnly, that a cross-origin request
# cannot use it, that both login failures are indistinguishable, that a
# non-member sees "no such space" rather than "forbidden". None of that is
# reachable from inside the process.
#
#   docker exec idea-chat-postgres-1 psql -U postgres -c 'CREATE DATABASE hearth_dev'
#   cargo build -p hearth --bin server --features server
#   DATABASE_URL=postgres://postgres:dev@localhost:55432/hearth_dev \
#     HEARTH_BIND=127.0.0.1:8090 REDIS_URL=redis://localhost:6399 \
#     ./target/debug/server | tee /tmp/hearth-server.log &
#   bash tools/auth-flows.sh
#
# Expects a FRESH database: it claims the instance, which only works once.
set -u

ORIGIN_URL=${ORIGIN_URL:-http://127.0.0.1:8090}
API=${API:-$ORIGIN_URL/_srv}
LOG=${LOG:-/tmp/hearth-server.log}
TMP=$(mktemp -d)
JAR=$TMP/admin.jar
JAR2=$TMP/member.jar
trap 'rm -rf "$TMP"' EXIT


PASS=0; FAIL=0
ok(){ if [ "$2" = "$3" ]; then echo "  ok   $1"; PASS=$((PASS+1)); else echo " FAIL  $1"; echo "        want: $3"; echo "        got : $2"; FAIL=$((FAIL+1)); fi; }
has(){ case "$2" in *"$3"*) echo "  ok   $1"; PASS=$((PASS+1));; *) echo " FAIL  $1"; echo "        got: $2"; FAIL=$((FAIL+1));; esac; }
no(){ case "$2" in *"$3"*) echo " FAIL  $1"; echo "        got: $2"; FAIL=$((FAIL+1));; *) echo "  ok   $1"; PASS=$((PASS+1));; esac; }
# The `#[server]` wire format is a positional array of the fn's arguments —
# `null` for a zero-arg fn — and every response is `{"Ok":…}` or `{"Err":…}`.
call(){ curl -sS -X POST "$API/$1" -H 'Content-Type: application/json' -H "Origin: $ORIGIN_URL" ${3:-} -d "$2"; }

TOKEN=$(grep -o 'claim/[a-f0-9]*' $LOG | head -1 | cut -d/ -f2)

echo "--- instance status ---"
R=$(call instance_status 'null')
has "an unclaimed instance says so" "$R" '"needs_claim":true'
no  "and never reveals the claim token" "$R" "$TOKEN"
has "a fresh instance is invite-only" "$R" '"registration_mode":"invite_only"'

echo "--- claiming ---"
R=$(call claim_instance "[{\"token\":\"wrong\",\"handle\":\"ana\",\"display_name\":\"Ana\",\"password\":\"a good passphrase\"}]")
has "a wrong claim token is refused" "$R" 'not valid'
R=$(call claim_instance "[{\"token\":\"$TOKEN\",\"handle\":\"ana\",\"display_name\":\"Ana Ruiz\",\"password\":\"a good passphrase\"}]" "-c $JAR")
has "the right token claims the instance" "$R" '"handle":"ana"'
has "the first account is instance admin" "$R" '"is_instance_admin":true'
no  "web callers get no bearer token" "$R" '"token":"'
grep -q 'session' $JAR && { echo "  ok   a session cookie was set"; PASS=$((PASS+1)); } || { echo " FAIL  no session cookie"; FAIL=$((FAIL+1)); }
grep -qi 'HttpOnly' $JAR && { echo "  ok   the cookie is httpOnly"; PASS=$((PASS+1)); } || { echo " FAIL  cookie is not httpOnly"; FAIL=$((FAIL+1)); }

R=$(call claim_instance "[{\"token\":\"$TOKEN\",\"handle\":\"thief\",\"display_name\":\"Thief\",\"password\":\"another passphrase\"}]")
has "the claim token cannot be reused" "$R" 'already been claimed'
R=$(call instance_status 'null')
has "a claimed instance stops advertising a claim" "$R" '"needs_claim":false'

echo "--- session ---"
R=$(curl -sS -X POST "$API/me" -H 'Content-Type: application/json' -H "Origin: $ORIGIN_URL" -b $JAR -d 'null')
has "the session identifies the user" "$R" '"handle":"ana"'
R=$(call me 'null')
no  "no session means no identity" "$R" '"handle":"ana"'

echo "--- spaces and invites ---"
R=$(curl -sS -X POST "$API/create_space" -H 'Content-Type: application/json' -H "Origin: $ORIGIN_URL" -b $JAR -d '["cabin","The Cabin"]')
has "a space can be created" "$R" '"slug":"cabin"'
has "the creator owns it" "$R" '"role":"owner"'
has "it is seeded with channels" "$R" '"name":"general"'
SPACE=$(echo "$R" | grep -o '"id":"[^"]*"' | head -1 | cut -d'"' -f4)

R=$(curl -sS -X POST "$API/create_invite" -H 'Content-Type: application/json' -H "Origin: $ORIGIN_URL" -b $JAR -d "[\"$SPACE\",1]")
CODE=$(echo "$R" | sed -n 's/.*"Ok":"\([^"]*\)".*/\1/p')
has "an owner can mint an invite" "$CODE" "-"

R=$(call peek_invite "[\"$CODE\"]")
has "the invite names its space" "$R" '"space_name":"The Cabin"'
has "and who sent it" "$R" '"invited_by":"Ana Ruiz"'

echo "--- registration ---"
R=$(call register "[{\"invite_code\":\"$CODE\",\"handle\":\"theo\",\"display_name\":\"Theo Lang\",\"password\":\"theos passphrase\"}]" "-c $JAR2")
has "an invite creates an account" "$R" '"handle":"theo"'
no  "a registered member is not instance admin" "$R" '"is_instance_admin":true'
R=$(curl -sS -X POST "$API/my_spaces" -H 'Content-Type: application/json' -H "Origin: $ORIGIN_URL" -b $JAR2 -d 'null')
has "and lands in the space they were invited to" "$R" '"slug":"cabin"'
has "as a member" "$R" '"role":"member"'

R=$(call register "[{\"invite_code\":\"$CODE\",\"handle\":\"gate\",\"display_name\":\"Gate\",\"password\":\"gates passphrase\"}]")
has "a single-use invite cannot be used twice" "$R" 'already been used'
R=$(call register '[{"invite_code":"no-such-code-00","handle":"nope","display_name":"Nope","password":"nopes passphrase"}]')
has "an unknown invite is refused" "$R" 'not valid'
R=$(curl -sS -X POST "$API/login" -H 'Content-Type: application/json' -H "Origin: $ORIGIN_URL" -d '[{"identifier":"gate","password":"gates passphrase"}]')
has "a rolled-back registration leaves no account behind" "$R" 'invalid handle or password'

echo "--- login ---"
R=$(call login '[{"identifier":"ana","password":"a good passphrase"}]')
has "a correct password signs in" "$R" '"handle":"ana"'
R=$(call login '[{"identifier":"ANA","password":"a good passphrase"}]')
has "the handle is case-insensitive" "$R" '"handle":"ana"'
W1=$(call login '[{"identifier":"ana","password":"wrong"}]')
W2=$(call login '[{"identifier":"nobody-at-all","password":"wrong"}]')
has "a wrong password is refused" "$W1" 'invalid handle or password'
ok  "an unknown account gives the SAME error" "$W2" "$W1"

echo "--- authorization ---"
R=$(curl -sS -X POST "$API/create_invite" -H 'Content-Type: application/json' -H "Origin: $ORIGIN_URL" -b $JAR2 -d "[\"$SPACE\",1]")
has "a plain member cannot mint invites" "$R" 'no such space'
R=$(curl -sS -X POST "$API/create_invite" -H 'Content-Type: application/json' -H "Origin: $ORIGIN_URL" -b $JAR2 -d '["01a03000-0000-7000-8000-0000000000ff",1]')
has "and a space they cannot see reads as absent, not forbidden" "$R" 'no such space'

echo "--- csrf ---"
R=$(curl -sS -X POST "$API/me" -H 'Content-Type: application/json' -H "Origin: https://evil.example" -b $JAR -d 'null')
no "a cross-origin request cannot use the cookie" "$R" '"handle":"ana"'

echo "--- logout ---"
curl -sS -X POST "$API/logout" -H 'Content-Type: application/json' -H "Origin: $ORIGIN_URL" -b $JAR -c $JAR -d 'null' >/dev/null
R=$(curl -sS -X POST "$API/me" -H 'Content-Type: application/json' -H "Origin: $ORIGIN_URL" -b $JAR -d 'null')
no "the session is dead after logout" "$R" '"handle":"ana"'

echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
