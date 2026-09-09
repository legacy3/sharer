#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
output_dir="$project_root/docs/screenshots"

for command_name in cargo import mogrify sqlite3 Xvfb xdotool; do
	if ! command -v "$command_name" >/dev/null 2>&1; then
		echo "missing required command: $command_name" >&2
		exit 1
	fi
done

work_dir="$(mktemp -d "${TMPDIR:-/tmp}/sharer-screenshots.XXXXXX")"
app_pid=""
xvfb_pid=""

cleanup() {
	if [[ -n "$app_pid" ]]; then
		kill "$app_pid" 2>/dev/null || true
		wait "$app_pid" 2>/dev/null || true
	fi
	if [[ -n "$xvfb_pid" ]]; then
		kill "$xvfb_pid" 2>/dev/null || true
		wait "$xvfb_pid" 2>/dev/null || true
	fi
	rm -rf -- "$work_dir"
}
trap cleanup EXIT INT TERM

display_number=90
while [[ -e "/tmp/.X11-unix/X$display_number" ]]; do
	display_number=$((display_number + 1))
done
display_value=":$display_number"

Xvfb "$display_value" -screen 0 2048x1400x24 -nolisten tcp >"$work_dir/xvfb.log" 2>&1 &
xvfb_pid=$!

for _ in {1..50}; do
	if DISPLAY="$display_value" xdotool getmouselocation >/dev/null 2>&1; then
		break
	fi
	sleep 0.1
done
if ! DISPLAY="$display_value" xdotool getmouselocation >/dev/null 2>&1; then
	echo "Xvfb did not become ready" >&2
	exit 1
fi

cargo build --manifest-path "$project_root/Cargo.toml" --features desktop --bin ShareR
binary="$project_root/target/debug/ShareR"
config_root="$work_dir/config"

XDG_CONFIG_HOME="$config_root" "$binary" --init https://uploads.example/upload >/dev/null
database="$config_root/sharer/sharer.sqlite3"

sqlite3 "$database" <<'SQL'
UPDATE settings
SET value = CAST(json_set(CAST(value AS TEXT), '$.capture_directory', 'Pictures/ShareR') AS BLOB)
WHERE key = 'application';
DELETE FROM upload_history;
INSERT INTO upload_history (original_name, size_bytes, link, delete_url, expires_at)
VALUES ('design-review.png', 184320, 'https://files.example/a1B2c3.png', 'https://files.example/delete/a1B2c3/token', '2026-09-16 20:14 UTC');
INSERT INTO upload_history (original_name, size_bytes, link, delete_url, expires_at)
VALUES ('release-notes.pdf', 2411725, 'https://files.example/d4E5f6.pdf', 'https://files.example/delete/d4E5f6/token', '2026-09-16 19:42 UTC');
INSERT INTO upload_history (original_name, size_bytes, link, delete_url, expires_at)
VALUES ('capture-hdr.jpg', 7340032, 'https://files.example/g7H8i9.jpg', 'https://files.example/delete/g7H8i9/token', '2026-09-16 18:03 UTC');
SQL

DISPLAY="$display_value" \
	XDG_CONFIG_HOME="$config_root" \
	SLINT_BACKEND=winit-software \
	SLINT_SCALE_FACTOR=2 \
	HTTPS_PROXY=http://192.0.2.1:9 \
	"$binary" >"$work_dir/sharer.log" 2>&1 &
app_pid=$!

window_id=""
for _ in {1..100}; do
	window_ids="$(DISPLAY="$display_value" xdotool search --onlyvisible --name '^ShareR$' 2>/dev/null || true)"
	if [[ -n "$window_ids" ]]; then
		window_id="${window_ids%%$'\n'*}"
		break
	fi
	if ! kill -0 "$app_pid" 2>/dev/null; then
		echo "ShareR exited before opening a window" >&2
		sed -n '1,160p' "$work_dir/sharer.log" >&2
		exit 1
	fi
	sleep 0.1
done
if [[ -z "$window_id" ]]; then
	echo "ShareR window did not appear" >&2
	exit 1
fi

mkdir -p "$output_dir"

click_at() {
	DISPLAY="$display_value" xdotool mousemove --sync --window "$window_id" "$1" "$2" click 1
	sleep 0.2
}

capture_view() {
	local filename="$1"
	DISPLAY="$display_value" import -window "$window_id" "$output_dir/$filename"
	mogrify -strip "$output_dir/$filename"
}

sleep 0.3
capture_view capture.png

click_at 140 278
capture_view history.png

click_at 140 366
click_at 568 156
capture_view settings-general.png

click_at 920 156
capture_view settings-capture.png

click_at 1272 156
capture_view settings-upload.png

click_at 1624 156
capture_view settings-privacy.png

for screenshot in \
	capture.png \
	history.png \
	settings-upload.png \
	settings-capture.png \
	settings-privacy.png \
	settings-general.png; do
	if [[ ! -s "$output_dir/$screenshot" ]]; then
		echo "failed to create $screenshot" >&2
		exit 1
	fi
done

echo "Generated screenshots in $output_dir"
