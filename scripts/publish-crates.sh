#!/usr/bin/env bash
# Publishes the workspace to crates.io, and finishes what an earlier run
# started when it is run again.
#
#     CARGO_REGISTRY_TOKEN=... scripts/publish-crates.sh
#
# `cargo publish --workspace` does the work: it skips every crate marked
# `publish = false`, puts the rest in dependency order and waits for the
# index between them. It does not cope with being rate limited or with
# being run a second time. A 429 halfway through stops it, and a second
# call then stops at the first crate that is already up. So this asks
# the index what is there, excludes it, and waits when the registry says
# to wait. tamnd/rudb learned all of this on its own releases, and this
# is its script with zu's names in it.
#
# crates.io has three limits, and only two of them can be waited out
# inside one job:
#
#   a crate it has never seen     a burst of 5, then one every ten minutes
#   a new version of a crate      a burst of 30, then one a minute
#   a new version of a crate      twenty in any twenty four hours
#
# The first release here is fourteen crates the registry has never seen,
# so it is about an hour and a half of mostly waiting, once. Every
# release after it takes a couple of minutes. The third limit is waited
# out in hours, so when it is the one in play this stops and says so,
# and a re-run once a slot frees finishes the release.
#
# Running it when everything is already up is a no-op that exits zero,
# which is what makes re-running the release job the way to recover from
# a partial upload. Nothing it prints contains the token.

set -euo pipefail

if [ -z "${CARGO_REGISTRY_TOKEN:-}" ]; then
	echo "CARGO_REGISTRY_TOKEN is empty, so nothing can be published" >&2
	echo "it is a secret of the crates-io environment on tamnd/zu" >&2
	exit 1
fi

# The new crate limit plus slack, so a clock that disagrees with the
# registry by a few seconds does not cost a whole extra round.
new_pause=610
# The same for the one a minute limit on a crate that already exists.
existing_pause=70

metadata=$(cargo metadata --format-version 1 --no-deps)
version=$(echo "$metadata" | jq -r '.packages[] | select(.name == "zudb") | .version')
# `publish = false` comes out of cargo metadata as an empty list.
crates=$(echo "$metadata" | jq -r '.packages[] | select(.publish != []) | .name' | sort)
total=$(echo "$crates" | wc -w | tr -d ' ')
echo "publishing $total crates at $version:" $crates

# Where a crate lives in the sparse index, which is by the length of its
# name and is the rule every registry client implements.
index_path() {
	local name=$1
	case ${#name} in
	1) echo "1/$name" ;;
	2) echo "2/$name" ;;
	3) echo "3/${name:0:1}/$name" ;;
	*) echo "${name:0:2}/${name:2:2}/$name" ;;
	esac
}

# The index rather than the API, because the index is what cargo reads
# and a crate never published is a 404 there rather than an empty
# answer. That 404 is what decides which rate limit applies.
index_entry() {
	curl --silent --fail "https://index.crates.io/$(index_path "$1")" 2>/dev/null || true
}

log=$(mktemp)
trap 'rm -f "$log"' EXIT

# One attempt per crate still to do, plus a few. An attempt that uploads
# nothing is the only kind worth budgeting for, and no shape of rate
# limit makes two of those in a row.
attempts=$((total + 5))
remaining=$total

for attempt in $(seq 1 "$attempts"); do
	exclude=()
	up=0
	brand_new=0
	for crate in $crates; do
		entry=$(index_entry "$crate")
		if echo "$entry" | grep -q "\"vers\":\"$version\""; then
			exclude+=(--exclude "$crate")
			up=$((up + 1))
		elif [ -z "$entry" ]; then
			brand_new=$((brand_new + 1))
		fi
	done

	if [ "$up" -eq "$total" ]; then
		echo "all $total crates are on crates.io at $version"
		exit 0
	fi

	remaining=$((total - up))
	if [ "$brand_new" -gt 0 ]; then
		pause=$new_pause
		echo "attempt $attempt: $up of $total up, $remaining to go, $brand_new never published before"
	else
		pause=$existing_pause
		echo "attempt $attempt: $up of $total up, $remaining to go, all of them existing crates"
	fi

	# `${x[@]+"${x[@]}"}` because bash 3.2, which is the bash on a Mac,
	# calls an empty array unbound under `set -u`, and a Mac is where
	# this gets run by hand when the runner is not cooperating.
	if cargo publish --workspace --locked ${exclude[@]+"${exclude[@]}"} 2>&1 | tee "$log"; then
		echo "published $remaining crates at $version"
		exit 0
	fi

	# Something else published a crate while this ran, which is the
	# normal case when the workflow and a hand run race. The index is
	# read again at the top of the loop and that read decides.
	if grep -q "already exists on crates.io index" "$log"; then
		echo "a crate went up from somewhere else while this ran, reading the index again"
		sleep 10
		continue
	fi

	if grep -q "too many versions of this crate in the last 24 hours" "$log"; then
		echo "crates.io allows twenty versions of a crate a day and this one is at it" >&2
		echo "$up of $total are up at $version, and a re-run carries on from there" >&2
		echo "a slot frees when the oldest upload of the day ages out" >&2
		exit 1
	fi

	if ! grep -q "429 Too Many Requests" "$log"; then
		echo "the publish failed for a reason that waiting will not fix" >&2
		exit 1
	fi
	echo "crates.io asked for a slower pace, waiting ${pause}s and carrying on"
	sleep "$pause"
done

echo "gave up after $attempts attempts with $((total - remaining)) of $total up at $version" >&2
echo "nothing is lost: re-run the job and it carries on from here" >&2
exit 1
