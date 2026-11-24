#!/bin/bash
# Print the release of the kernel GRUB boots by default, read from the generated grub.cfg.
#
# The runner image build (scripts/build-ami.sh) runs this after it installs the host kernel:
# the builder itself still runs the base image's kernel, so `uname -r` there says nothing about
# the image. /etc/default/grub is not the answer either: a file in /etc/default/grub.d can
# override it, and it changes nothing until update-grub has regenerated grub.cfg.
#
# The default is the `set default="..."` line grub.cfg carries, or grubenv's saved_entry when
# that line is `${saved_entry}`. It names a menu entry by index, title or id, with `>` between
# a submenu and the entry inside it. This prints the release of that entry's first `linux`
# line and fails when it cannot resolve the entry, when grubenv holds a next_entry (which
# would boot something else once), or when the entry's kernel or initrd is not in BOOT_DIR.
#
# Environment, for tests: GRUB_CFG (default /boot/grub/grub.cfg), BOOT_DIR (default /boot).
set -euo pipefail

grub_cfg="${GRUB_CFG:-/boot/grub/grub.cfg}"
boot_dir="${BOOT_DIR:-/boot}"

fail() {
  echo "ERROR: grub-boot-kernel: $*" >&2
  exit 1
}

[ -r "$grub_cfg" ] || fail "cannot read $grub_cfg"
grubenv=$(grub-editenv list) || fail "grub-editenv list failed"
next_entry=$(sed -n 's/^next_entry=//p' <<<"$grubenv")
[ -z "$next_entry" ] || fail "grubenv has next_entry=$next_entry, which the next boot uses instead of the default"

# shellcheck disable=SC2016  # the literal text grub.cfg carries, not a shell expansion
defaults=$(sed -n 's/^[[:space:]]*set default="\(.*\)"[[:space:]]*$/\1/p' "$grub_cfg" | grep -vxF '${next_entry}' || true)
[ -n "$defaults" ] || fail "$grub_cfg sets no default entry"
[ "$(wc -l <<<"$defaults")" -eq 1 ] || fail "$grub_cfg sets more than one default entry"
default="$defaults"
# shellcheck disable=SC2016
if [ "$default" = '${saved_entry}' ]; then
  default=$(sed -n 's/^saved_entry=//p' <<<"$grubenv")
  [ -n "$default" ] || fail "the default is the saved entry and grubenv has none"
fi

# Walk the menu: one path component per nesting level, matched against an entry's index
# among its siblings, its title or its id. Blocks open on a line ending in `{` and close on
# a line that is `}` alone, which is how grub-mkconfig writes them.
if ! lines=$(awk -v path="$default" '
  BEGIN { n = split(path, want, ">"); level = 0; depth = 0; want_depth = 0 }
  {
    line = $0
    sub(/^[ \t]+/, "", line); sub(/[ \t]+$/, "", line)
    if (line ~ /^(menuentry|submenu) / && line ~ /\{$/) {
      if (!found && !dead && depth == want_depth) {
        title = ""; id = ""
        if (match(line, /^(menuentry|submenu) \047[^\047]*\047/)) {
          title = substr(line, RSTART, RLENGTH); sub(/^[a-z]+ \047/, "", title); sub(/\047$/, "", title)
        }
        if (match(line, /(\$menuentry_id_option|--id) \047[^\047]*\047/)) {
          id = substr(line, RSTART, RLENGTH); sub(/^[^\047]*\047/, "", id); sub(/\047$/, "", id)
        }
        wanted = want[level + 1]
        if ((wanted ~ /^[0-9]+$/ && wanted + 0 == siblings[depth] + 0) || wanted == title || wanted == id) {
          level++
          if (level == n) {
            if (line !~ /^menuentry /) { dead = 1 } else { found = 1; in_entry = 1; entry_depth = depth + 1 }
          } else {
            if (line !~ /^submenu /) { dead = 1 } else { want_depth = depth + 1 }
          }
        }
        siblings[depth]++
      }
      depth++
      next
    }
    if (line ~ /\{$/) { depth++; next }
    if (line == "}") {
      depth--
      if (in_entry && depth < entry_depth) { in_entry = 0 }
      if (!found && depth < want_depth) { dead = 1 }
      next
    }
    if (in_entry && linux == "" && line ~ /^linux(efi|16)?[ \t]/) { split(line, field, /[ \t]+/); linux = field[2] }
    if (in_entry && initrd == "" && line ~ /^initrd(efi|16)?[ \t]/) { split(line, field, /[ \t]+/); initrd = field[2] }
  }
  END {
    if (!found || linux == "") { exit 1 }
    print linux
    print initrd
  }
' "$grub_cfg"); then
  fail "$grub_cfg has no menu entry '$default' with a linux line"
fi

linux=$(sed -n 1p <<<"$lines")
initrd=$(sed -n 2p <<<"$lines")
kernel_file=$(basename -- "$linux")
case "$kernel_file" in
  vmlinuz-?*) ;;
  *) fail "the default entry boots $linux, not a vmlinuz-<release> file" ;;
esac
[ -n "$initrd" ] || fail "the default entry '$default' has no initrd line"
initrd_file=$(basename -- "$initrd")
[ -e "$boot_dir/$kernel_file" ] || fail "the default entry boots $kernel_file, which is not in $boot_dir"
[ -e "$boot_dir/$initrd_file" ] || fail "the default entry loads $initrd_file, which is not in $boot_dir"

printf '%s\n' "${kernel_file#vmlinuz-}"
