#!/bin/sh
# Readies BuildKit's persistent state volume (build.stateVolume.persistent) for a build, then
# runs the build: `state.sh <state dir> <pvc> <keep|fresh> <reserved> <max used> <min free>
# -- <command...>`, sizes in bytes.
#
# BuildKit's own garbage collection is not enough here: it runs a minute after buildkitd starts
# or a build ends, but every build gets its own buildkitd, stopped right after the build. So:
#
# 1. A short-lived buildkitd prunes the least recently used cache until at most <max used> is
#    used and enough is free for this build: <min free>, or more if recent builds grew the state
#    by more (their peaks are kept in .gitimage-peaks). Never below <reserved>.
# 2. With under 5% free after that (buildkitd cannot start on a full volume, or files BuildKit
#    does not track fill it), or in `fresh` mode (the controller's retry after a build ran out
#    of space), the volume is emptied: it holds only a cache.
# 3. While the build runs, a background loop records how much it grew the state. A build that
#    fails with the volume full ends with a "no space left on device" line for the controller.
#
# It prints one `gitimage-state {json}` line for the controller, which shows it in the
# GitImage's status and the UI.
set -u
d=$1 claim=$2 mode=$3 reserved=$4 max_used=$5 min_free=$6
shift 6
[ "${1-}" = -- ] && shift

peaks=$d/.gitimage-peaks
# The running build's growth so far: left behind by a build that was killed.
now=$d/.gitimage-peak-now
log() { echo "gitimage: $*" >&2; }
# Size, used and available bytes of the volume (df columns 2, 3, 4).
df_bytes() { df -Pk "$d" | awk -v c="$1" 'NR == 2 { printf "%.0f\n", $c * 1024 }'; }
human() { awk -v b="$1" 'BEGIN { split("B KB MB GB TB", u); i = 1; while (b >= 1000 && i < 5) { b /= 1000; i++ } printf (i == 1 ? "%d%s" : "%.1f%s"), b, u[i] }'; }
low() { [ "$(df_bytes 4)" -lt "$(($(df_bytes 2) / 20))" ]; }

emptied=
empty() {
	emptied=$1
	log "emptying the BuildKit state volume: $1"
	set -- find "$d" -mindepth 1 -maxdepth 1 -exec rm -rf {} +
	# Rootless BuildKit's files belong to subordinate uids: delete them in its user namespace.
	if [ "$(id -u)" = 0 ]; then "$@"; else rootlesskit "$@" || "$@"; fi
}

# The growth of the last 5 builds; this build needs as much again. Read before emptying.
[ -r "$now" ] && cat "$now" >>"$peaks" 2>/dev/null
rm -f "$now"
hist=$(tail -n 4 "$peaks" 2>/dev/null | grep -E '^[0-9]+$')
peak=0
for p in $(tail -n 5 "$peaks" 2>/dev/null | grep -E '^[0-9]+$'); do
	[ "$p" -gt "$peak" ] && peak=$p
done

[ "$mode" = fresh ] && empty "the last build ran out of space"
size=$(df_bytes 2)
want=$min_free
[ $((peak * 5 / 4)) -gt "$want" ] && want=$((peak * 5 / 4))
[ "$want" -gt $((size * 9 / 10)) ] && want=$((size * 9 / 10))

before=$(df_bytes 3)
log "the state uses $(human "$before") of $(human "$size"); pruning to leave $(human "$want") free (recent builds grew it by up to $(human "$peak"))"
mb() { awk -v b="$1" 'BEGIN { printf "%.6f", b / 1e6 }'; }
pruned=/tmp/gitimage-pruned
if buildctl-daemonless.sh prune --all --format '{{json .}}' \
	--keep-storage-min="$(mb "$reserved")" --keep-storage="$(mb "$max_used")" \
	--free-storage="$(mb "$want")" >"$pruned"; then
	# What went, by kind: cache mounts going on every build mean the volume is too small.
	awk '
		match($0, /"size":[0-9]+/) { s = substr($0, RSTART + 7, RLENGTH - 7) } !RSTART { next }
		/"description":"cached mount/ { k = "cache mounts" }
		/"description":"local source/ { k = "build contexts" }
		/"description":"pulled from/ { k = "base images" }
		/"description":"mount \/ from exec/ { k = "layers" }
		!k { k = "other" }
		{ n[k]++; b[k] += s; k = "" }
		END { for (k in n) printf "gitimage: pruned %d %s, %.1fMB\n", n[k], k, b[k] / 1e6 }
	' "$pruned" >&2
else
	log "pruning the BuildKit state failed"
fi
rm -f "$pruned"
low && empty "under 5% of it was free after pruning"
if low; then
	log "could not free the BuildKit state volume; reset it: kubectl delete pvc $claim"
	exit 1
fi

used=$(df_bytes 3)
free=$(df_bytes 4)
freed=$((before - used))
[ "$freed" -lt 0 ] && freed=0
log "starting the build with $(human "$free") free"
printf 'gitimage-state {"sizeBytes":%s,"usedBytes":%s,"freeBytes":%s,"prunedBytes":%s,"wantFreeBytes":%s,"peakBytes":%s,"emptied":"%s"}\n' \
	"$size" "$used" "$free" "$freed" "$want" "$peak" "$emptied"

# How much the build grew the state: sampled while it runs, and once more when it ends.
(
	top=0
	while sleep 2; do
		g=$(($(df_bytes 3) - used))
		[ "$g" -le "$top" ] && continue
		top=$g
		echo "$top" >"$now.tmp" && mv "$now.tmp" "$now"
	done
) &
watcher=$!

# Not exec'd: a step that fills the volume fails with its tool's error ("exit code: 1"), often
# without saying why. A failed build that left under 2% free ran out of space: the last line
# says so, and the controller (which reads the last lines) retries it on an emptied volume.
# Stopping (a cancelled build, the deadline) is passed to the whole process group, so buildkitd
# shuts down cleanly and lets go of the state.
"$@" &
child=$!
trap 'trap "" TERM INT; kill -TERM 0 2>/dev/null' TERM INT
rc=0
wait "$child" || rc=$?
# A trapped signal interrupts `wait`; the build is still stopping.
while kill -0 "$child" 2>/dev/null; do wait "$child" || rc=$?; done
kill "$watcher" 2>/dev/null

top=$(($(df_bytes 3) - used))
[ -r "$now" ] && read -r w <"$now" && [ "$w" -gt "$top" ] && top=$w
[ "$top" -lt 0 ] && top=0
{ [ -n "$hist" ] && printf '%s\n' "$hist"; echo "$top"; } >"$peaks.tmp" 2>/dev/null &&
	mv "$peaks.tmp" "$peaks" && rm -f "$now"
log "the build grew the state by $(human "$top")"

if [ "$rc" -ne 0 ] && [ "$(df_bytes 4)" -lt $((size / 50)) ]; then
	log "no space left on device: the build filled the BuildKit state volume ($(human "$(df_bytes 4)") of $(human "$size") free)"
fi
exit "$rc"
