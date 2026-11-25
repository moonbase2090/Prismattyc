#!/bin/bash
# Model-check specs/mail-claim/MailClaim.tla.
# The code contract must pass. Each broken variant must fail on the named
# property. The three wordings that the store does not implement must fail
# on the real steps (Defect "none").

set -euo pipefail

root=$(cd "$(dirname "$0")" && pwd)
cd "$root"

jar_sha=fdffef247f563ef6edbfd44958d2a82db79e7564e07cb673262d896353fa6713
jar_url=https://github.com/tlaplus/tlaplus/releases/download/v1.8.0/tla2tools.jar
jar="$root/.cache/tla2tools.jar"

find_java() {
  local candidate
  if command -v java >/dev/null 2>&1 && java -version >/dev/null 2>&1; then
    echo java
    return 0
  fi
  for candidate in \
    /opt/homebrew/opt/openjdk/bin/java \
    /usr/lib/jvm/java-21-openjdk-amd64/bin/java \
    /usr/lib/jvm/java-21-openjdk-arm64/bin/java \
    /usr/lib/jvm/java-17-openjdk-amd64/bin/java \
    /usr/lib/jvm/default-java/bin/java
  do
    if [ -x "$candidate" ] && "$candidate" -version >/dev/null 2>&1; then
      echo "$candidate"
      return 0
    fi
  done
  echo "Java 11 or newer is required to run the mail model check." >&2
  return 1
}

java_bin=$(find_java)

sha256_of() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1"
  else
    sha256sum "$1"
  fi
}

if [ ! -f "$jar" ] || ! sha256_of "$jar" | grep -q "$jar_sha"; then
  mkdir -p "$root/.cache"
  curl -fsSL -o "$jar" "$jar_url"
fi
if ! sha256_of "$jar" | grep -q "$jar_sha"; then
  echo "tla2tools.jar checksum does not match the pinned v1.8.0 jar." >&2
  exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

write_cfg() {
  local defect=$1
  local body=$2
  local cfg=$3
  cat > "$cfg" <<EOF
CONSTANTS
  a = a
  b = b
  d = d
  c1 = c1
  c2 = c2
  c3 = c3
  l1 = l1
  l2 = l2
  none = none
  Agents = {a, b, d}
  Clients = {c1, c2, c3}
  LetterIds = {l1, l2}
  Defect = "${defect}"

SPECIFICATION Spec
${body}
EOF
}

run_tlc() {
  local cfg=$1
  local out=$2
  set +e
  (
    cd "$work"
    "$java_bin" -XX:+UseParallelGC -jar "$jar" -workers 1 -config "$cfg" "$root/MailClaim.tla" >"$out" 2>&1
  )
  local status=$?
  set -e
  return "$status"
}

pass_out=$work/pass.out
if ! run_tlc "$root/MailClaim.cfg" "$pass_out"; then
  echo "Code contract failed." >&2
  cat "$pass_out" >&2
  exit 1
fi
echo "code contract: pass"
grep -E 'Model checking completed|states generated|distinct states found' "$pass_out" || true

# defect, property keyword, INVARIANT or PROPERTY
defects=(
  'restart_drops_held|NoLetterLost|INVARIANT'
  'claim_includes_committed|CommittedStaysGone|PROPERTY'
  'commit_ignores_recipient|CommitOnlyByRecipient|PROPERTY'
  'commit_ignores_held|CommitOnlyByRecipient|PROPERTY'
  'send_duplicates|SendStoresOne|PROPERTY'
  'restart_drops_held|RestartKeepsMailbox|PROPERTY'
  'claim_leaves_open|ClaimHoldsOpen|PROPERTY'
  'broadcast_drops_rest|BroadcastOneEach|PROPERTY'
  'release_deletes|ReleaseReturnsToOpen|PROPERTY'
)

for row in "${defects[@]}"; do
  defect=${row%%|*}
  rest=${row#*|}
  property=${rest%%|*}
  kind=${rest#*|}
  cfg=$work/${defect}-${property}.cfg
  out=$work/${defect}-${property}.out
  write_cfg "$defect" "${kind} ${property}" "$cfg"
  if run_tlc "$cfg" "$out"; then
    echo "Variant ${defect} did not fail ${property}." >&2
    cat "$out" >&2
    exit 1
  fi
  if ! grep -q "$property" "$out"; then
    echo "Variant ${defect} failed, but the trace does not name ${property}." >&2
    cat "$out" >&2
    exit 1
  fi
  echo "variant ${defect}: ${property} failed, as required"
done

# Wordings the current steps do not satisfy. Each must fail on Defect none.
ticket=(
  'RestartReturnsHeldToOpen|PROPERTY'
  'CommitOnlySameSession|PROPERTY'
  'AtMostOnceClaim|INVARIANT'
)

for row in "${ticket[@]}"; do
  property=${row%%|*}
  kind=${row#*|}
  cfg=$work/ticket-${property}.cfg
  out=$work/ticket-${property}.out
  write_cfg "none" "${kind} ${property}" "$cfg"
  if run_tlc "$cfg" "$out"; then
    echo "Wording ${property} now holds. Read the store before treating that as the contract." >&2
    cat "$out" >&2
    exit 1
  fi
  if ! grep -q "$property" "$out"; then
    echo "Wording ${property} failed without naming itself." >&2
    cat "$out" >&2
    exit 1
  fi
  echo "wording ${property}: fails on the current steps"
done

echo "mail claim model: code contract holds; known wordings still fail"
