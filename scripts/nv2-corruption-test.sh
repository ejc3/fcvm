#!/bin/bash
SIZE=$1
ATTEMPTS=$2

# First verify simple VM works
echo "=== Verifying simple VM works ==="
TMPDIR=$(mktemp -d)
RESULT=$(sudo RUST_LOG="fcvm=info" ./target/release/fcvm podman run \
  --name verify-$$ \
  --network bridged \
  --kernel-profile nested \
  --map "$TMPDIR:/mnt/test" \
  alpine:latest \
  sh -c "echo hello > /mnt/test/out.txt && cat /mnt/test/out.txt" 2>&1)

if grep -q "hello" "$TMPDIR/out.txt" 2>/dev/null; then
  echo "✓ Simple VM works"
  rm -rf "$TMPDIR"
else
  echo "✗ Simple VM FAILED"
  echo "$RESULT" | tail -20
  rm -rf "$TMPDIR"
  exit 1
fi

# Now run corruption tests
echo ""
echo "=== Testing $SIZE $ATTEMPTS times ==="
PASS=0
FAIL=0
for i in $(seq 1 $ATTEMPTS); do
  echo "--- Attempt $i ---"
  TMPDIR=$(mktemp -d)
  OUTPUT=$(RUST_LOG="fcvm=info,fuse-pipe::server=error" \
    sudo -E ./target/release/fcvm podman run \
      --name test-$SIZE-$i-$$ \
      --network bridged \
      --kernel-profile nested \
      --map "$TMPDIR:/mnt/fuse-test" \
      alpine:latest \
      sh -c "dd if=/dev/urandom of=/mnt/fuse-test/test.bin bs=$SIZE count=1 conv=fsync 2>&1" 2>&1)

  if echo "$OUTPUT" | grep -q "MISMATCH"; then
    echo "✗ CORRUPTION DETECTED"
    echo "$OUTPUT" | grep -E "MISMATCH|Error"
    ((FAIL++))
  else
    ACTUAL=$(ls -la "$TMPDIR/test.bin" 2>/dev/null | awk '{print $5}')
    echo "✓ OK - File size: $ACTUAL"
    ((PASS++))
  fi
  rm -rf "$TMPDIR"
done

echo ""
echo "=== Results: $PASS passed, $FAIL failed out of $ATTEMPTS ==="
