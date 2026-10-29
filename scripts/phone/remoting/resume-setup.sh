#!/bin/bash
# The rest of box-setup.sh after make setup-phone (the emulator build keeps running on its own).
set -uo pipefail
B=/mnt/fcvm-btrfs/bench
exec >> $B/logs/setup.log 2>&1
set -x
cd ~/fcvm-bench
./scripts/phone/aosp.sh > $B/logs/aosp.log 2>&1 || { echo "aosp.sh failed"; exit 1; }
ACCEPT_ANDROID_SDK_LICENSE=1 ./scripts/phone/sdk.sh > $B/logs/sdk.log 2>&1 || { echo "sdk.sh failed"; exit 1; }
./scripts/phone/app.sh > $B/logs/app.log 2>&1 || { echo "app.sh failed"; exit 1; }
IMAGE=$(./scripts/phone/run.sh --image)
make -s -C fleet/phone image IMAGE="$IMAGE" > $B/logs/image.log 2>&1 || { echo "image build failed"; exit 1; }
echo "image $IMAGE"
while ! grep -q "emulator exit" $B/logs/emulator.log; do sleep 10; done
tail -2 $B/logs/emulator.log
echo SETUP DONE
