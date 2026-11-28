#!/bin/bash
# Differential test of the model's kernel rules against the kernel's own functions.
#
# Fetches the pinned files of Linux v6.14 and v7.3.3 (checked against SHA256SUMS), cuts the
# functions the model has rules for out of them verbatim (cut.py), compiles them with stubs for
# everything they call (stubs.h, harness.c), and feeds them the operations `lake exe hostops`
# prints: every rule on every input, then every recorded run of WakeVerify/Breaks.lean. Any
# difference between what the kernel's functions do and what the model expects fails.
#
# Cut: the halt (__kvm_emulate_halt, kvm_vcpu_has_events, kvm_arch_vcpu_runnable, vcpu_block),
# the kick (__apic_accept_irq), KVM_GET_MP_STATE and KVM_SET_MP_STATE, and the whole path of a
# completion: queued by the MSR write (kvm_pv_enable_async_pf, kvm_async_pf_wakeup_all), dequeued
# on entry (kvm_check_async_pf_completion, kvm_arch_can_dequeue_async_page_present,
# apf_pageready_slot_free) and delivered (kvm_arch_async_page_present, apf_put_user_ready,
# kvm_apic_set_irq). Not cut, and set by hand where a run needs it: vcpu_enter_guest itself,
# the async page fault worker, the guest's interrupt handler, the LAPIC's own state.
#
# The kernel source is fetched, not kept in this repository.
#
# Exit status: 0 every kernel agrees, compares the number of operations operations.expected
# gives, and the self-test found its differences; 1 a kernel disagrees with its rule book, or a
# count differs from operations.expected; 2 BLOCKED: a step could not be carried out (a tool
# missing, a fetch, the patch, the cut, the compiler, the generation of the operations, the
# comparison itself). harness/test-run.sh tests each BLOCKED path and the count.
#
# The build directory is harness/build, or $HARNESS_BUILD. One run at a time uses it: a second
# run on the same directory waits for the first.
#
# usage: harness/run.sh            from verify/, or through `make verify-harness`
set -Eeuo pipefail

blocked() {
  echo "BLOCKED: $*" >&2
  exit 2
}
# Any command that fails where its failure is not asked about ends the run as BLOCKED, never
# as a disagreement (exit 1 is a verdict about the kernel's functions).
trap 'blocked "harness/run.sh line $LINENO failed"' ERR

here=$(cd "$(dirname "$0")" && pwd)
verify=$(dirname "$here")
repo=$(dirname "$verify")
build=${HARNESS_BUILD:-$here/build}
expected_counts=${HARNESS_EXPECTED:-$here/operations.expected}
base=https://raw.githubusercontent.com/gregkh/linux
patch_file="$repo/kernel/patches-x86-host/0003-kvm-x86-keep-a-pending-pv-kick-across-async-page-ready.patch"

for tool in curl sha256sum python3 cc lake diff patch grep sed paste head wc flock awk; do
  command -v "$tool" >/dev/null 2>&1 || { echo "BLOCKED: '$tool' missing" >&2; exit 2; }
done
[ -s "$expected_counts" ] || blocked "$expected_counts is missing or empty"

# Every run of Breaks.lean is in `recordedRuns`, the hand-kept list `lake exe hostops` replays:
# a run left out of it would be recorded and never driven through the kernel's functions.
missing=$(python3 - "$verify/WakeVerify/Breaks.lean" <<'PY'
import re, sys
text = open(sys.argv[1]).read()
defined = re.findall(r"^def (Run\.\w+) :", text, re.M)
block = text[text.index("def recordedRuns"):]
listed = set(re.findall(r"Run\.\w+", block))
if not defined or not listed:
    sys.exit("no runs found in Breaks.lean")
print(" ".join(r for r in defined if r not in listed))
PY
)
[ -z "$missing" ] || blocked "Breaks.lean defines $missing and recordedRuns does not list it"

mkdir -p "$build/src"
exec 9> "$build/.lock"
flock 9
while read -r sum path; do
  tag=${path%%/*}
  file=${path#*/}
  dest="$build/src/$path"
  if ! { [ -f "$dest" ] && echo "$sum  $dest" | sha256sum --check --status; }; then
    mkdir -p "$(dirname "$dest")"
    curl -fsSL "$base/$tag/$file" -o "$dest"
    echo "$sum  $dest" | sha256sum --check --status ||
      { echo "BLOCKED: $path does not match its pinned checksum" >&2; exit 2; }
  fi
done < "$here/SHA256SUMS"

# v7.3.3 with the #2130 patch: the repository's own patch file when it is there, and otherwise
# the patch's one changed statement, replaced in place.
rm -rf "$build/src/v7.3.3-patched"
cp -r "$build/src/v7.3.3" "$build/src/v7.3.3-patched"
if [ -f "$patch_file" ]; then
  patch -s -p1 -d "$build/src/v7.3.3-patched" < "$patch_file"
  echo "v7.3.3-patched: applied $(basename "$patch_file")"
else
  python3 - "$build/src/v7.3.3-patched/arch/x86/kvm/x86.c" <<'PY'
import sys
path = sys.argv[1]
text = open(path).read()
head, sep, tail = text.partition("\nvoid kvm_arch_async_page_present(")
old = "\tkvm_set_mp_state(vcpu, KVM_MP_STATE_RUNNABLE);\n}\n"
new = "\tvcpu->arch.mp_state = KVM_MP_STATE_RUNNABLE;\n}\n"
if not sep or old not in tail:
    sys.exit("BLOCKED: kvm_arch_async_page_present does not end as the #2130 patch expects")
open(path, "w").write(head + sep + tail.replace(old, new, 1))
PY
  echo "v7.3.3-patched: the #2130 patch file is not in this tree; its one statement was replaced in place"
fi

x86_functions="kvm_vcpu_running kvm_vcpu_has_events kvm_arch_vcpu_runnable vcpu_block
  __kvm_emulate_halt kvm_arch_vcpu_ioctl_get_mpstate kvm_arch_vcpu_ioctl_set_mpstate
  apf_put_user_ready apf_pageready_slot_free kvm_arch_async_page_present
  kvm_arch_async_page_present_queued kvm_arch_can_dequeue_async_page_present"
async_functions="kvm_flush_and_free_async_pf_work kvm_check_async_pf_completion
  kvm_async_pf_wakeup_all"

# The functions are cut in an order that defines each before its first use. Two of them live in
# different files in the two releases: kvm_pv_async_pf_enabled (x86.c in v6.14, x86.h with its
# helper in v7.3.3) and kvm_pv_enable_async_pf (x86.c in v6.14, msrs.c in v7.3.3).
build_one() {
  local variant=$1 src="$build/src/$1" out="$build/$1"
  local kvm="$src/arch/x86/kvm"
  mkdir -p "$out"
  {
    if [ -f "$kvm/x86.h" ]; then
      python3 "$here/cut.py" "$kvm/x86.h" kvm_set_mp_state __kvm_pv_async_pf_enabled \
        kvm_pv_async_pf_enabled
    else
      python3 "$here/cut.py" "$kvm/x86.c" kvm_pv_async_pf_enabled
    fi
    python3 "$here/cut.py" "$kvm/lapic.c" __apic_accept_irq kvm_apic_set_irq
    # shellcheck disable=SC2086
    python3 "$here/cut.py" "$kvm/x86.c" $x86_functions
    # shellcheck disable=SC2086
    python3 "$here/cut.py" "$src/virt/kvm/async_pf.c" $async_functions
    if [ -f "$kvm/msrs.c" ]; then
      python3 "$here/cut.py" "$kvm/msrs.c" kvm_pv_enable_async_pf
    else
      python3 "$here/cut.py" "$kvm/x86.c" kvm_pv_enable_async_pf
    fi
  } > "$out/cut.c"
  cc -std=gnu11 -O1 -Wall -Werror -Wno-unused-function -Wno-unused-value -Wno-unused-variable \
    -DCUT="\"$out/cut.c\"" -I"$here" -o "$out/harness" "$here/harness.c"
}

# Run one variant's functions against one rule book's expectations. Returns 0 when they agree and
# 1 when they differ, with the differences on stdout. Both callers use it as a condition, where
# `set -e` does not apply, so every step is checked here: a step that could not be carried out
# ends the run as BLOCKED (exit 2) and is never taken for agreement or for a difference.
compare() {
  local variant=$1 book=$2 out="$build/$1" rc=0
  (cd "$verify" && lake exe hostops "$book") > "$out/expect-$book.txt" ||
    blocked "lake exe hostops $book failed"
  [ -s "$out/expect-$book.txt" ] || blocked "no operations for $book"
  sed 's/ | .*//' "$out/expect-$book.txt" > "$out/ops-$book.txt" || blocked "sed failed for $book"
  sed 's/.* | //' "$out/expect-$book.txt" > "$out/want-$book.txt" || blocked "sed failed for $book"
  "$out/harness" < "$out/ops-$book.txt" > "$out/got-$book.txt" ||
    blocked "the $variant functions failed on the operations of $book"
  paste -d'|' "$out/ops-$book.txt" "$out/want-$book.txt" > "$out/want-lines-$book.txt" ||
    blocked "paste failed for $book"
  paste -d'|' "$out/ops-$book.txt" "$out/got-$book.txt" > "$out/got-lines-$book.txt" ||
    blocked "paste failed for $book"
  diff "$out/want-lines-$book.txt" "$out/got-lines-$book.txt" || rc=$?
  # diff: 0 no difference, 1 differences, 2 or more it could not compare
  [ "$rc" -le 1 ] || blocked "diff failed (status $rc) comparing $variant with the rule book $book"
  return "$rc"
}

# The number of operations operations.expected gives for a name, or nothing.
expected_count() {
  awk -v name="$1" '$1 == name { print $2 }' "$expected_counts"
}
# check_count NAME FOUND: the count is a number and is the one the file gives.
check_count() {
  local name=$1 found=$2 want
  want=$(expected_count "$name")
  case "$want" in '' | *[!0-9]*) blocked "$expected_counts has no count for $name" ;; esac
  if [ "$found" != "$want" ]; then
    echo "$name: $found operations, and operations.expected says $want: the documents quote the second (update the file and them together)" >&2
    status=1
  fi
}

status=0
for variant in v6.14 v7.3.3 v7.3.3-patched; do
  build_one "$variant"
  if compare "$variant" "$variant" > "$build/$variant/diff.txt"; then
    count=$(wc -l < "$build/$variant/ops-$variant.txt")
    case "$count" in '' | *[!0-9]*) blocked "the operations of $variant could not be counted" ;; esac
    echo "$variant: $count operations, the kernel's functions agree with the model"
    check_count "$variant" "$count"
  else
    echo "$variant: the kernel's functions DISAGREE with the model:" >&2
    head -20 "$build/$variant/diff.txt" >&2
    status=1
  fi
done

# The comparison must be able to fail: the unpatched functions against the patched rule book
# differ exactly where #2125 is. Only a completed comparison that found differences passes.
self=0
compare v7.3.3 v7.3.3-patched > "$build/v7.3.3/self-test.txt" || self=$?
differing=$(grep -c '^<' "$build/v7.3.3/self-test.txt" || true)
case "$differing" in
  '' | *[!0-9]*) blocked "the self-test's count of differing operations could not be read: '$differing'" ;;
esac
if [ "$self" = 0 ]; then
  blocked "the harness cannot tell v7.3.3 from v7.3.3 with the #2130 patch"
elif [ "$self" != 1 ] || [ "$differing" -lt 1 ]; then
  blocked "the self-test did not complete (status $self, $differing differing operations)"
fi
echo "self-test: v7.3.3's functions against the patched rule book differ in $differing operations, as they must"
check_count self-test "$differing"
exit $status
