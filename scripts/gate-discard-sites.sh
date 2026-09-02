#!/usr/bin/env bash
# Enumerate every call in check-review-threads.sh's VERDICT_JQ that DISCARDS part of a
# comment body, and block on one that is neither the accounting primitive nor a listed
# normalization.
#
# Why this exists. Three rounds of one defect class landed on the same shape: content
# removed before classification is content the classifier did not read, so a finding placed
# in the removed region made a bot comment "a notice" and needed no disposition. Round 1
# was the notice payload (every line starting with ">" was accepted). Round 2 was the
# CodeRabbit tips block. Round 3 was the folded About Codex block, which also bound
# no-findings COVERAGE to the head. Fixing sites one at a time is what produced rounds 2
# and 3, so this pin fails on the NEXT one instead.
#
# The invariant: a body is exempt from dispositions only if the classifier accounted for
# every byte of it. A removal is accounted for when it goes through strip_accounted, which
# removes a declared region only after every non-blank line inside it matches one of that
# region's shapes, and returns null (body claimable) otherwise. Everything else must be a
# NORMALIZATION: it drops nothing a reader could have read, and what survives it is still
# shape-checked. The list below is the whole set, written out; a call that is not in it is
# reported and this script exits 1, whatever it is named.
#
# Run by both harnesses: scripts/test-check-review-threads.sh (finding 45) and
# tests/test_log_scan.rs (a_region_stripped_before_classification_is_validated_...), the
# second of which is the one CI runs.
set -uo pipefail

for tool in awk; do
  command -v "$tool" >/dev/null 2>&1 || { echo "BLOCKED: '$tool' missing" >&2; exit 2; }
done

GATE=${1:-"$(dirname "$0")/check-review-threads.sh"}
[ -r "$GATE" ] || { echo "BLOCKED: cannot read $GATE" >&2; exit 2; }

awk -v gate="$GATE" '
# Read the whole file, then pull out the single-quoted VERDICT_JQ program. A single-quoted
# shell string cannot contain a quote, so the first one after the opener ends it.
{ file = file $0 "\n" }
function callat(s, i,   d, q, c, j) {
  d = 0; q = 0
  for (j = i; j <= length(s); j++) {
    c = substr(s, j, 1)
    if (q) { if (c == "\\") j++; else if (c == "\"") q = 0; continue }
    if (c == "\"") { q = 1; continue }
    if (c == "(") d++
    else if (c == ")") { d--; if (d == 0) return substr(s, i, j - i + 1) }
  }
  return ""
}
function norm(t,   r) { r = t; gsub(/[ \t\n]+/, " ", r); gsub(/^ | $/, "", r); return r }
END {
  k = index(file, "VERDICT_JQ=\x27")
  if (k == 0) { print "BLOCKED: no VERDICT_JQ assignment in " gate > "/dev/stderr"; exit 2 }
  rest = substr(file, k + length("VERDICT_JQ=\x27"))
  e = index(rest, "\x27")
  if (e == 0) { print "BLOCKED: unterminated VERDICT_JQ in " gate > "/dev/stderr"; exit 2 }
  jq = substr(rest, 1, e - 1)

  # The accounting primitives: every discard inside one is that primitive doing its job.
  # strip_accounted removes a declared region after checking its content against the
  # shapes that region declares. strip_hidden_comments is the check the html_comment
  # region declares instead: it removes a comment only where CommonMark hides it, and
  # leaves every other one in the body.
  nprim = split("def strip_accounted(|def strip_hidden_comments:", prim, "|")
  for (i = 1; i <= nprim; i++) {
    p = index(jq, prim[i])
    if (p == 0) { print "BLOCKED: " prim[i] " is gone; nothing accounts for a removal" > "/dev/stderr"; exit 2 }
    pstart[i] = p
    tail = substr(jq, p + 1)
    q = index(tail, "\ndef ")
    pend[i] = (q == 0) ? length(jq) : p + q
  }

  # Normalizations. Each drops nothing a reader could read, and what survives is still
  # shape-checked by the caller.
  ok[norm("gsub(\"^[[:space:]]+|[[:space:]]+$\"; \"\")")] = "trim a line"
  ok[norm("sub(\"^[[:space:]]+\"; \"\")")] = "trim the head of a body before a startswith test"
  ok[norm("sub(\"^>[[:space:]]*\"; \"\")")] = "drop a blockquote marker; the rest is shape-checked"
  ok[norm("select(length > 0 and . != cr_note)")] = "drop blank lines and the one exact note line"
  ok[norm("select(length > 0)")] = "drop blank lines"
  ok[norm("select(startswith(\"|\"))")] = "keep table rows (coverage side; every kept row is parsed)"
  ok[norm("select((((.[0] // \"\") == \"Review\") and ((.[1] // \"\") == \"Status\")) | not)")] = "drop the table header row"
  ok[norm("select(((length > 0) and all(.[]; test(\"^:?-{3,}:?$\"))) | not)")] = "drop the table separator row"
  ok[norm("select([scan(cr_marker_re)] == [cr_summarize_marker])")] = "whole-comment guard: the summarize marker is the only marker"
  ok[norm("select(test(\"(?m)^>[[:space:]]*##\") | not)")] = "whole-comment guard: no blockquoted notice heading"
  ok[norm("select(test(\"No actionable comments were generated in the recent review\"))")] = "whole-block guard: the review finished clean"
  ok[norm("select(.name == \"f\")")] = "pick a named capture out of a match; no body text is touched"
  ok[norm("gsub(\"\\t\"; \"    \")")] = "count a tab as four columns while measuring indentation; no body text is touched"

  bad = 0
  split("gsub( sub( select(", kw, " ")
  for (n = 1; n <= 3; n++) {
    key = kw[n]
    pos = 0
    while (1) {
      hit = index(substr(jq, pos + 1), key)
      if (hit == 0) break
      pos = pos + hit
      if (key == "sub(" && pos > 1 && substr(jq, pos - 1, 1) == "g") continue
      call = callat(jq, pos + length(key) - 1)
      if (call == "") { print "BLOCKED: unbalanced parentheses at offset " pos > "/dev/stderr"; exit 2 }
      text = norm(substr(jq, pos, length(key) - 1) call)
      inside = 0
      for (i = 1; i <= nprim; i++) if (pos >= pstart[i] && pos <= pend[i]) inside = 1
      total++
      if (inside) { printf "  primitive     %s\n", text }
      else if (text in ok) { printf "  %-13s %s\n", "normalization", text }
      else { printf "  UNACCOUNTED   %s\n", text; bad++ }
    }
  }
  printf "%d discarding call(s) in VERDICT_JQ, %d unaccounted\n", total, bad
  if (bad > 0) {
    print "" > "/dev/stderr"
    print "A call that discards part of a body must remove a region declared in" > "/dev/stderr"
    print "accounted_regions (through strip_accounted, which checks the region content" > "/dev/stderr"
    print "against that region\x27s line shapes) or be one of the normalizations listed in" > "/dev/stderr"
    print "this script. An undeclared removal is a region nobody read, which is how a" > "/dev/stderr"
    print "finding becomes exempt from dispositions." > "/dev/stderr"
    exit 1
  }
  exit 0
}
' "$GATE"
