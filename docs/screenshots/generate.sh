#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
output_dir="${SHARER_SCREENSHOT_OUTPUT_DIR:-$project_root/docs/screenshots}"
binary="${1:-}"

if [[ "$#" -gt 1 ]]; then
	echo "usage: $0 [existing-desktop-binary]" >&2
	exit 1
fi

for command_name in convert display identify import mogrify openbox python3 sha256sum sqlite3 timeout Xvfb xdotool; do
	if ! command -v "$command_name" >/dev/null 2>&1; then
		echo "missing required command: $command_name" >&2
		exit 1
	fi
done

if [[ -z "$binary" ]]; then
	if ! command -v cargo >/dev/null 2>&1; then
		echo "missing required command: cargo (or pass an existing desktop binary)" >&2
		exit 1
	fi
	# One compiler job is not a memory cap. Do not start an automatic build on
	# an unbounded workstation; a supplied binary never triggers Cargo.
	build_memory_limit=""
	if [[ -r /sys/fs/cgroup/memory.max ]]; then
		build_memory_limit="$(</sys/fs/cgroup/memory.max)"
	fi
	if [[ ! "$build_memory_limit" =~ ^[1-9][0-9]{0,9}$ ]] || ((10#$build_memory_limit > 6442450944)); then
		echo "automatic build requires a cgroup v2 memory cap of at most 6 GiB; pass a prebuilt desktop binary instead" >&2
		exit 1
	fi
	CARGO_INCREMENTAL=0 cargo build --manifest-path "$project_root/Cargo.toml" \
		--offline --locked --profile ci --jobs 1 --config profile.ci.package.sharer.opt-level=0 \
		--features desktop --bin ShareR
	binary="${CARGO_TARGET_DIR:-$project_root/target}/ci/ShareR"
fi
if [[ ! -x "$binary" ]]; then
	echo "desktop binary is not executable: $binary" >&2
	exit 1
fi

work_dir="$(mktemp -d "${TMPDIR:-/tmp}/sharer-screenshots.XXXXXX")"
gallery_dir="$work_dir/gallery"
app_pid=""
xvfb_pid=""
wm_pid=""
fixture_pid=""
latest_view=""

stop_process() {
	local process_id="$1"
	local signal
	for signal in TERM KILL; do
		kill -s "$signal" "$process_id" 2>/dev/null || true
		for _ in {1..20}; do
			if ! kill -0 "$process_id" 2>/dev/null; then
				wait "$process_id" 2>/dev/null || true
				return 0
			fi
			sleep 0.1
		done
	done
	echo "script-owned process did not terminate: $process_id" >&2
	return 1
}

stop_app() {
	if [[ -n "$app_pid" ]]; then
		stop_process "$app_pid" || return 1
		app_pid=""
	fi
}

cleanup() {
	local status=$?
	local diagnostics_failed=0
	# Failures preserve genuine available diagnostics, with logs first. A strict
	# exporter may then reject missing images; never fabricate a successful frame.
	mkdir -p "$output_dir" || diagnostics_failed=1
	for log_name in sharer sharer-fresh xvfb fixture openbox; do
		if [[ -f "$work_dir/$log_name.log" ]]; then
			cp -- "$work_dir/$log_name.log" "$output_dir/diagnostic-$log_name.log" || diagnostics_failed=1
		else
			: >"$output_dir/diagnostic-$log_name.log" || diagnostics_failed=1
		fi
	done
	if [[ -f "$gallery_dir/capture.png" ]]; then
		cp -- "$gallery_dir/capture.png" "$output_dir/diagnostic-capture.png" || diagnostics_failed=1
	else
		diagnostics_failed=1
	fi
	if [[ -n "$latest_view" && -f "$gallery_dir/$latest_view" ]]; then
		cp -- "$gallery_dir/$latest_view" "$output_dir/diagnostic-latest.png" || diagnostics_failed=1
	else
		diagnostics_failed=1
	fi
	if [[ -n "$xvfb_pid" ]] && kill -0 "$xvfb_pid" 2>/dev/null; then
		timeout --kill-after=1s 5s import -window root "$output_dir/diagnostic-root.png" 2>/dev/null || diagnostics_failed=1
		{
			timeout --kill-after=1s 3s xdotool getmouselocation --shell || true
			timeout --kill-after=1s 3s xdotool search --onlyvisible --name '.*' getwindowname %@ || true
			if [[ -n "${window_id:-}" ]]; then
				timeout --kill-after=1s 3s xdotool getwindowgeometry --shell "$window_id" || true
			fi
		} >"$output_dir/diagnostic-windows.txt" 2>&1 || diagnostics_failed=1
	else
		diagnostics_failed=1
	fi
	# A successful gallery must include every fixed diagnostic artifact declared
	# by the exporter. Empty process logs are valid; empty images/window data are not.
	if ((status == 0)); then
		if ((diagnostics_failed != 0)); then
			echo "failed to collect the required gallery diagnostic bundle" >&2
			status=1
		fi
		for diagnostic in capture.png latest.png root.png windows.txt; do
			if [[ ! -s "$output_dir/diagnostic-$diagnostic" ]]; then
				echo "missing required gallery diagnostic: $diagnostic" >&2
				status=1
			fi
		done
		for log_name in sharer sharer-fresh xvfb fixture openbox; do
			if [[ ! -f "$output_dir/diagnostic-$log_name.log" ]]; then
				echo "missing required gallery diagnostic log: $log_name" >&2
				status=1
			fi
		done
	fi
	stop_app || status=1
	for process_id in "$fixture_pid" "$wm_pid" "$xvfb_pid"; do
		if [[ -n "$process_id" ]]; then
			stop_process "$process_id" || status=1
		fi
	done
	rm -rf -- "$work_dir"
	exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Every image must come from the same full application, even if a developer
# rebuilds the source binary while this script is running.
source_hash="$(sha256sum <"$binary")"
source_hash="${source_hash%% *}"
cp -- "$binary" "$work_dir/ShareR"
chmod 500 "$work_dir/ShareR"
binary="$work_dir/ShareR"
frozen_hash="$(sha256sum <"$binary")"
frozen_hash="${frozen_hash%% *}"
if [[ "$frozen_hash" != "$source_hash" ]]; then
	echo "desktop binary changed while freezing it" >&2
	exit 1
fi
mkdir -p "$gallery_dir" "$work_dir/captures"

display_number=90
while [[ -e "/tmp/.X11-unix/X$display_number" || -e "/tmp/.X$display_number-lock" ]]; do
	display_number=$((display_number + 1))
done
export DISPLAY=":$display_number"
export SLINT_BACKEND=winit-software SLINT_SCALE_FACTOR=2
# Defense in depth only: the caller's network isolation is the hard boundary.
# All capture actions below are local-only; no upload action is invoked.
export HTTP_PROXY=http://127.0.0.1:9 HTTPS_PROXY=http://127.0.0.1:9 ALL_PROXY=http://127.0.0.1:9
export http_proxy="$HTTP_PROXY" https_proxy="$HTTPS_PROXY" all_proxy="$ALL_PROXY"
export NO_PROXY="" no_proxy=""

# Keep a full framebuffer below X11's request limit when shared-memory IPC is
# unavailable in the sandbox. The app still fits at 200% scale.
Xvfb "$DISPLAY" -screen 0 2200x1500x24 -nolisten tcp >"$work_dir/xvfb.log" 2>&1 &
xvfb_pid=$!
for _ in {1..50}; do
	if xdotool getmouselocation >/dev/null 2>&1; then
		break
	fi
	sleep 0.1
done
if ! xdotool getmouselocation >/dev/null 2>&1; then
	echo "Xvfb did not become ready" >&2
	exit 1
fi
# Match the window-manager/focus setup used by the full application QA harness.
# A bare X server does not establish the activation behavior of a desktop session.
# Copy the system defaults into this private session and remove only the WM's
# Print grabs, which would otherwise conflict with ShareR's default shortcuts.
python3 - "$work_dir/openbox.xml" <<'PY'
from pathlib import Path
import sys
import xml.etree.ElementTree as ET

source = Path('/etc/xdg/openbox/rc.xml')
if source.stat().st_size > 256 * 1024:
    raise RuntimeError('Unexpected oversized Openbox configuration')
root = ET.fromstring(source.read_bytes())
ET.register_namespace('', 'http://openbox.org/3.4/rc')
for parent in root.iter():
    for item in list(parent):
        if item.tag.rsplit('}', 1)[-1] == 'keybind' and 'Print' in item.get('key', ''):
            parent.remove(item)
Path(sys.argv[1]).write_bytes(ET.tostring(root, encoding='utf-8', xml_declaration=True))
PY
openbox --sm-disable --config-file "$work_dir/openbox.xml" >"$work_dir/openbox.log" 2>&1 &
wm_pid=$!
# xcap needs a primary output for the real region-capture path on Xvfb. Use the
# existing X11 libraries; minimal test images need not install the xrandr CLI.
python3 - <<'PY'
import ctypes
import ctypes.util
import os

xlib = ctypes.CDLL(ctypes.util.find_library('X11'))
randr = ctypes.CDLL(ctypes.util.find_library('Xrandr'))
xlib.XOpenDisplay.argtypes = [ctypes.c_char_p]
xlib.XOpenDisplay.restype = ctypes.c_void_p
xlib.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
xlib.XDefaultRootWindow.restype = ctypes.c_ulong
xlib.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
xlib.XCloseDisplay.argtypes = [ctypes.c_void_p]

class Resources(ctypes.Structure):
    _fields_ = [('timestamp', ctypes.c_ulong), ('configTimestamp', ctypes.c_ulong),
                ('ncrtc', ctypes.c_int), ('crtcs', ctypes.POINTER(ctypes.c_ulong)),
                ('noutput', ctypes.c_int), ('outputs', ctypes.POINTER(ctypes.c_ulong)),
                ('nmode', ctypes.c_int), ('modes', ctypes.c_void_p)]

randr.XRRGetScreenResourcesCurrent.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
randr.XRRGetScreenResourcesCurrent.restype = ctypes.POINTER(Resources)
randr.XRRSetOutputPrimary.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong]
randr.XRRSetOutputPrimary.restype = None
randr.XRRFreeScreenResources.argtypes = [ctypes.POINTER(Resources)]
randr.XRRFreeScreenResources.restype = None
connection = xlib.XOpenDisplay(os.environ['DISPLAY'].encode())
if not connection:
    raise RuntimeError('Cannot connect to private X server')
resources = None
try:
    root = xlib.XDefaultRootWindow(connection)
    resources = randr.XRRGetScreenResourcesCurrent(connection, root)
    if not resources or resources.contents.noutput < 1:
        raise RuntimeError('Private X server has no display output')
    randr.XRRSetOutputPrimary(connection, root, resources.contents.outputs[0])
    xlib.XSync(connection, 0)
finally:
    if resources:
        randr.XRRFreeScreenResources(resources)
    xlib.XCloseDisplay(connection)
PY

config_root="$work_dir/config"
export XDG_CONFIG_HOME="$config_root" XDG_DATA_HOME="$work_dir/data"
export SHARER_CONFIG_DIR="$config_root/sharer"
"$binary" --init uguu >/dev/null
database="$SHARER_CONFIG_DIR/sharer.sqlite3"
capture_directory_sql="${work_dir//\'/\'\'}/captures"

sqlite3 "$database" <<SQL
UPDATE settings
SET value = CAST(json_set(
    CAST(value AS TEXT),
    '$.capture_directory', '$capture_directory_sql',
    '$.capture_naming.mode', 'custom',
    '$.capture_naming.custom_name', 'design-review',
    '$.auto_upload_captures', json('false'),
    '$.save_captures', json('true'),
    '$.completion_sound', json('false'),
    '$.copy_link_after_upload', json('false'),
    '$.minimize_to_tray', json('false'),
    '$.start_at_login', json('false'),
    '$.settings_mode', 'simple',
    '$.onboarding_complete', json('true')
) AS BLOB)
WHERE key = 'application';
DELETE FROM upload_history;
INSERT INTO upload_history (created_at, original_name, size_bytes, link, delete_url, expires_at)
VALUES (1792690200, 'release-notes.pdf', 2411725, 'https://files.example/release-notes.pdf', 'https://files.example/private-delete/fixture-only', 'Retention set by host');
INSERT INTO upload_history (created_at, original_name, size_bytes, link, delete_url, expires_at)
VALUES (1792686600, 'capture-hdr.jpg', 7340032, 'https://files.example/capture-hdr.jpg', '', '2026-10-25 19:30 UTC');
SQL
if [[ "$(sqlite3 "$database" "SELECT json_extract(value, '$.auto_upload_captures') || ':' || json_extract(value, '$.save_captures') FROM settings WHERE key = 'application';")" != '0:1' ]]; then
	echo "screenshot profile did not retain local-only capture settings" >&2
	exit 1
fi

# A synthetic document, shown on this private X server, is captured by ShareR
# itself. The resulting preview is not injected into a component or screenshot.
convert -size 600x400 xc:'#F2F5F7' \
	-font "$project_root/assets/fonts/IBMPlexSans-SemiBold.ttf" \
	-fill '#17372F' -draw 'roundrectangle 24,24 576,105 12,12' \
	-fill '#82E0BF' -pointsize 28 -annotate +48+75 'Release checklist' \
	-fill '#203C38' -pointsize 20 -annotate +82+162 'Capture the important details' \
	-annotate +82+224 'Review before sharing' -annotate +82+286 'Keep a local copy' \
	-fill '#4FA98A' -draw 'roundrectangle 46,144 62,160 3,3' \
	-draw 'roundrectangle 46,206 62,222 3,3' -draw 'roundrectangle 46,268 62,284 3,3' \
	-font "$project_root/assets/fonts/IBMPlexSans-Regular.ttf" \
	-fill '#637B8D' -pointsize 16 -annotate +46+350 'Synthetic screenshot fixture · no personal data' \
	"$work_dir/document.png"
display -title 'ShareR synthetic fixture' -geometry +1200+180 "$work_dir/document.png" >"$work_dir/fixture.log" 2>&1 &
fixture_pid=$!
fixture_window=""
for _ in {1..100}; do
	fixture_windows="$(xdotool search --onlyvisible --name '^ShareR synthetic fixture$' 2>/dev/null || true)"
	if [[ -n "$fixture_windows" ]]; then
		fixture_window="${fixture_windows%%$'\n'*}"
		break
	fi
	if ! kill -0 "$fixture_pid" 2>/dev/null; then
		echo "synthetic fixture exited before opening a window" >&2
		sed -n '1,160p' "$work_dir/fixture.log" >&2
		exit 1
	fi
	sleep 0.1
done
if [[ -z "$fixture_window" ]]; then
	echo "synthetic fixture window did not appear" >&2
	exit 1
fi
xdotool windowmove "$fixture_window" 1200 180 windowsize "$fixture_window" 600 400
sleep 0.25
fixture_x=""
fixture_y=""
fixture_width=""
fixture_height=""
while IFS='=' read -r property value; do
	case "$property" in
	X) fixture_x="$value" ;;
	Y) fixture_y="$value" ;;
	WIDTH) fixture_width="$value" ;;
	HEIGHT) fixture_height="$value" ;;
	esac
done < <(xdotool getwindowgeometry --shell "$fixture_window")
if [[ ! "$fixture_x" =~ ^[0-9]+$ || ! "$fixture_y" =~ ^[0-9]+$ || "$fixture_width:$fixture_height" != 600:400 ]]; then
	echo "cannot determine the synthetic fixture's client coordinates" >&2
	exit 1
fi

window_id=""
launch_app() {
	local log_file="$1"
	"$binary" >"$log_file" 2>&1 &
	app_pid=$!
	window_id=""
	for _ in {1..100}; do
		local window_ids
		window_ids="$(xdotool search --onlyvisible --name '^ShareR$' 2>/dev/null || true)"
		if [[ -n "$window_ids" ]]; then
			window_id="${window_ids%%$'\n'*}"
			break
		fi
		if ! kill -0 "$app_pid" 2>/dev/null; then
			echo "ShareR exited before opening a window" >&2
			sed -n '1,160p' "$log_file" >&2
			exit 1
		fi
		sleep 0.1
	done
	if [[ -z "$window_id" ]]; then
		echo "ShareR window did not appear" >&2
		exit 1
	fi
	xdotool windowsize "$window_id" 1840 1240 windowmove "$window_id" 0 0
	timeout 5s xdotool windowactivate --sync "$window_id"
	sleep 0.5
}

# Input coordinates are logical pixels at the app's default 920×620 size.
click_at() {
	timeout 5s xdotool windowactivate --sync "$window_id"
	xdotool mousemove --sync --window "$window_id" "$((2 * $1))" "$((2 * $2))" click 1
	sleep 0.25
}

scroll_page() {
	local count="$1"
	local button="${2:-5}"
	xdotool mousemove --sync --window "$window_id" 1764 1020 click --repeat "$count" --delay 25 "$button"
	sleep 0.3
}

capture_view() {
	local filename="$1"
	xdotool mousemove --window "$window_id" 2 2
	sleep 0.2
	import -window "$window_id" "$gallery_dir/$filename"
	mogrify -strip "$gallery_dir/$filename"
	if [[ "$(identify -format '%w %h' "$gallery_dir/$filename")" != '1840 1240' ]]; then
		echo "unexpected screenshot size: $filename" >&2
		exit 1
	fi
	latest_view="$filename"
}

launch_app "$work_dir/sharer.log"
capture_view capture.png

click_at 284 150
selector_stable=0
for _ in {1..100}; do
	selector_windows="$(timeout --kill-after=1s 3s xdotool search --onlyvisible --name '^Select a region$' 2>/dev/null || true)"
	if [[ -n "$selector_windows" ]]; then
		selector_window="${selector_windows%%$'\n'*}"
		selector_geometry="$(timeout --kill-after=1s 3s xdotool getwindowgeometry --shell "$selector_window" 2>/dev/null || true)"
		selector_geometry=$'\n'"$selector_geometry"$'\n'
		if [[ "$selector_geometry" == *$'\nX=0\n'* && "$selector_geometry" == *$'\nY=0\n'* &&
			"$selector_geometry" == *$'\nWIDTH=2200\n'* && "$selector_geometry" == *$'\nHEIGHT=1500\n'* ]]; then
			selector_stable=$((selector_stable + 1))
			if ((selector_stable >= 3)); then
				break
			fi
		else
			selector_stable=0
		fi
	else
		selector_stable=0
	fi
	sleep 0.1
done
if ((selector_stable < 3)); then
	echo "region selector did not reach stable full-screen bounds" >&2
	exit 1
fi
# Capture the document content, leaving the displayed fixture's bottom edge out.
# The final text baseline is y=350; the inset keeps a clear lower margin.
# ShareR hides its window during region selection, revealing the fixture below.
xdotool mousemove "$((fixture_x + 1))" "$((fixture_y + 1))" mousedown 1 \
	mousemove "$((fixture_x + 599))" "$((fixture_y + 375))" mouseup 1
local_capture=""
for _ in {1..150}; do
	local_capture="$(sqlite3 -cmd '.timeout 1000' "$database" "SELECT local_path FROM upload_history WHERE link = '' AND local_path != '' ORDER BY id DESC LIMIT 1;")"
	if [[ -n "$local_capture" && -s "$local_capture" ]]; then
		break
	fi
	sleep 0.1
done
if [[ -z "$local_capture" || ! -s "$local_capture" ]]; then
	echo "ShareR did not save the local fixture capture" >&2
	exit 1
fi
sleep 1
capture_view capture-result.png

click_at 80 142
capture_view history.png

click_at 80 188
click_at 214 136
capture_view settings-general.png
click_at 289 136
capture_view settings-capture.png
click_at 326 88
if [[ "$(sqlite3 -cmd '.timeout 1000' "$database" "SELECT json_extract(value, '$.settings_mode') FROM settings WHERE key = 'application';")" != advanced ]]; then
	echo "Advanced mode did not become active" >&2
	exit 1
fi
scroll_page 60 4
# Keep the Recording heading and its complete card in the featured view.
scroll_page 3
capture_view settings-recording.png
click_at 227 88
if [[ "$(sqlite3 -cmd '.timeout 1000' "$database" "SELECT json_extract(value, '$.settings_mode') FROM settings WHERE key = 'application';")" != simple ]]; then
	echo "Simple mode did not become active" >&2
	exit 1
fi
click_at 359 136
# Whole wheel steps overshoot this short pane. Use the observed thumb position.
scroll_page 60
xdotool mousemove --sync --window "$window_id" 1782 790 mousedown 1 \
	mousemove --sync --window "$window_id" 1782 750 mouseup 1
sleep 0.3
capture_view settings-upload.png
click_at 429 136
capture_view settings-privacy.png

# First run has a separate empty profile; do not complete setup or capture here.
stop_app
export XDG_CONFIG_HOME="$work_dir/fresh-config" XDG_DATA_HOME="$work_dir/fresh-data"
export SHARER_CONFIG_DIR="$XDG_CONFIG_HOME/sharer"
launch_app "$work_dir/sharer-fresh.log"
capture_view onboarding.png
stop_app

final_hash="$(sha256sum <"$binary")"
if [[ "${final_hash%% *}" != "$source_hash" ]]; then
	echo "frozen binary changed during screenshot generation" >&2
	exit 1
fi
mkdir -p "$output_dir"
for screenshot in capture.png capture-result.png history.png settings-general.png \
	settings-capture.png settings-recording.png settings-upload.png settings-privacy.png onboarding.png; do
	if [[ ! -s "$gallery_dir/$screenshot" ]]; then
		echo "failed to create $screenshot" >&2
		exit 1
	fi
	cp -- "$gallery_dir/$screenshot" "$output_dir/$screenshot"
done
echo "Generated full-application screenshots in $output_dir"
echo "Binary SHA-256: $source_hash"
echo "Linux/Xvfb, 920x620 logical pixels, 200% scale; independent visual review still required."
