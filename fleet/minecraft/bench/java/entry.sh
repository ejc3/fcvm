#!/bin/sh
# The same world and rules as a Pumpkin tile (fleet/minecraft/src/pumpkin.rs): offline, creative,
# no spawn protection, the same seed and view distance.
set -eu
cd /srv/mc
[ "${MCBENCH_EULA:-}" = "true" ] || { echo "MCBENCH_EULA=true is required (the bench box only)"; exit 2; }
echo "eula=true" > eula.txt
cat > server.properties <<PROPS
server-port=${MC_PORT}
online-mode=false
gamemode=creative
force-gamemode=true
spawn-protection=0
level-seed=${MC_SEED}
view-distance=${MC_VIEW_DISTANCE}
simulation-distance=${MC_VIEW_DISTANCE}
max-players=20
white-list=false
enforce-whitelist=false
enable-query=false
enable-rcon=false
sync-chunk-writes=false
motd=fcvm density bench
PROPS
# Paper (Bukkit) refuses a second connection from one address within 4 s; every bench bot
# comes from the host's one address. Vanilla reads no bukkit.yml.
printf 'settings:\n  connection-throttle: -1\n' > bukkit.yml
exec java -Xms${MC_HEAP} -Xmx${MC_HEAP} -XX:+UseG1GC -jar /srv/server.jar nogui
