#!/bin/bash
# Box setup for the phone remoting measurement (run as ubuntu, detached). Logs in $B/logs.
set -uo pipefail
B=/mnt/fcvm-btrfs/bench
mkdir -p $B/logs $B/apk $B/media
exec > $B/logs/setup.log 2>&1
set -x
cd ~
if [ ! -d fcvm-bench ]; then
  git clone -q --branch phone-gpu-remoting-bench https://github.com/ejc3/fcvm.git fcvm-bench || exit 1
fi
cd ~/fcvm-bench
git fetch -q origin && git checkout -q origin/phone-gpu-remoting-bench
git log --oneline -1

# the emulator from source, in parallel with the fcvm build
( EMULATOR_KEEP_SOURCES=1 ./scripts/phone/emulator.sh > $B/logs/emulator.log 2>&1; echo "emulator exit $?" >> $B/logs/emulator.log ) &
emu=$!

# APKs and the video, in parallel too
(
  while IFS=$'\t' read -r pkg ver vc url sha; do
    f=$B/apk/$pkg.apk
    [ -f "$f" ] && echo "$sha  $f" | sha256sum -c --status && continue
    curl -fsSL --retry 3 -o "$f.tmp" "$url" && echo "$sha  $f.tmp" | sha256sum -c --status && mv "$f.tmp" "$f" || echo "APK FAILED $pkg"
  done < $B/apks.tsv
  if [ ! -f $B/media/bbb720.mp4 ]; then
    curl -fsSL --retry 3 -o $B/media/bbb.mov https://download.blender.org/peach/bigbuckbunny_movies/big_buck_bunny_720p_h264.mov \
      && ffmpeg -y -loglevel error -ss 60 -t 100 -i $B/media/bbb.mov -map 0:v:0 -c copy -movflags +faststart $B/media/bbb720.mp4 \
      && rm -f $B/media/bbb.mov
  fi
  ffprobe -v error -show_entries stream=codec_name,width,height,r_frame_rate,bit_rate -of compact $B/media/bbb720.mp4
  echo "downloads done"
) > $B/logs/downloads.log 2>&1 &

make build > $B/logs/build.log 2>&1 || { echo "make build failed"; exit 1; }
make setup-phone > $B/logs/setup-phone.log 2>&1 || { echo "setup-phone failed"; exit 1; }
./scripts/phone/aosp.sh > $B/logs/aosp.log 2>&1 || { echo "aosp.sh failed"; exit 1; }
ACCEPT_ANDROID_SDK_LICENSE=1 ./scripts/phone/sdk.sh > $B/logs/sdk.log 2>&1 || { echo "sdk.sh failed"; exit 1; }
./scripts/phone/app.sh > $B/logs/app.log 2>&1 || { echo "app.sh failed"; exit 1; }
IMAGE=$(./scripts/phone/run.sh --image)
make -s -C fleet/phone image IMAGE="$IMAGE" > $B/logs/image.log 2>&1 || { echo "image build failed"; exit 1; }
echo "image $IMAGE"
wait $emu
tail -2 $B/logs/emulator.log
echo SETUP DONE
