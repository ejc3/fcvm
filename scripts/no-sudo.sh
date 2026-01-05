#!/bin/bash
# Prevent sudo access for unprivileged tests
#
# This script shadows the real sudo with a stub that errors.
# Used to ensure unprivileged tests can't accidentally escalate to root.

set -e

NOSUDO_DIR=$(mktemp -d)
trap "rm -rf $NOSUDO_DIR" EXIT

cat > "$NOSUDO_DIR/sudo" << 'EOF'
#!/bin/bash
echo "ERROR: sudo is not allowed in unprivileged tests" >&2
echo "This test should work without root access." >&2
exit 1
EOF
chmod +x "$NOSUDO_DIR/sudo"

export PATH="$NOSUDO_DIR:$PATH"
"$@"
