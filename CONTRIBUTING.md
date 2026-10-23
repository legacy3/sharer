# Contributing to ShareR

Whether you are fixing a typo, adding a provider, or telling us the capture math is wrong, all of it helps. If you disagree with how something is done, that is even better. Open an issue or a PR.

ShareR is small enough that you can understand the part you care about without learning the whole application. UI work mostly lives in `ui/` and `src/views/`. Capture work lives in `src/capture/`. Upload work lives in `src/upload/`. Start there and follow the types outward only when you need to.

## Getting set up

ShareR requires Rust 1.95 or newer. The default build is headless and avoids graphical dependencies:

```bash
cargo build
```

Build the desktop application with:

```bash
cargo build --features desktop
```

Ubuntu needs the desktop development libraries first:

```bash
sudo apt install libasound2-dev libayatana-appindicator3-dev libgbm-dev libgtk-3-dev libpipewire-0.3-dev libwayland-dev libxdo-dev libxkbcommon-dev
```

On Windows, use the MSVC Rust toolchain and install Visual Studio Build Tools with the **Desktop development with C++** workload and a Windows SDK. The desktop build also needs:

- CMake on `PATH` for the native UltraHDR and libjpeg-turbo build.
- LLVM/libclang for `bindgen`; set `LIBCLANG_PATH` if `libclang.dll` is not found automatically.
- Either an MSYS `patch` on `PATH` or `ULTRAHDR_SKIP_PATCHES=1`. The Windows CI sets the variable because the crate's patch only adds an opt-in threadless build, which the desktop build does not use.

The headless build needs none of these.

The macOS desktop build targets macOS 14 and is compiled separately for Apple silicon and Intel Macs.

Some checks use small standalone tools:

| Tool                                                      | Used for                        |
| --------------------------------------------------------- | ------------------------------- |
| Prettier                                                  | Markdown formatting             |
| Rulewright                                                | Repository-specific Rust rules  |
| shfmt                                                     | Screenshot generator formatting |
| ShellCheck                                                | Screenshot generator linting    |
| actionlint                                                | GitHub Actions workflow linting |
| ImageMagick, Python 3, SQLite, Xvfb, Openbox, and xdotool | Screenshot generation           |

## Day-to-day commands

```bash
cargo build --features desktop              # build the app
cargo test --lib --bins --tests             # test the headless build
cargo test --features desktop --lib --bins --tests
cargo clippy --all-targets                   # lint the headless build
cargo clippy --features desktop --all-targets
cargo bench --bench memory                   # check allocation boundaries
rulewright --strict                          # repository rules
```

For memory-constrained machines and WSL, use the existing CI profile and one compiler job for desktop builds and tests:

```bash
cargo build --profile ci --jobs 1 --config profile.ci.package.sharer.opt-level=0 --features desktop --bin ShareR
cargo test --profile ci --jobs 1 --config profile.ci.package.sharer.opt-level=0 --features desktop --lib --bins --tests
```

The CI profile disables debug symbols and incremental compilation; the override also disables ShareR's development optimization. A job limit reduces concurrent compiler work but is not a memory cap; use an OS or container memory limit when sharing RAM with other applications.

Run `cargo fmt` before committing. The full gate used for a final check is listed under [Before you open a PR](#before-you-open-a-pr).

## Where things live

| Path                | What it is                                                              |
| ------------------- | ----------------------------------------------------------------------- |
| `src/capture/`      | Platform capture, coordinates, color conversion, encoding, and resizing |
| `src/app/`          | Desktop coordination, jobs, UI lifecycle, and region selection          |
| `src/views/`        | Rust callbacks and view models for Slint                                |
| `src/upload/`       | Streaming payloads, metadata removal, and uploader providers            |
| `src/storage.rs`    | SQLite settings and paginated upload history                            |
| `ui/`               | Slint views, shared components, theme, and models                       |
| `assets/fonts/`     | Bundled IBM Plex Sans fonts, source checksums, and license              |
| `tests/fixtures/`   | Licensed reference images used by transformation tests                  |
| `docs/screenshots/` | README screenshots and their generator                                  |

## A few rules that matter

**Keep headless builds headless.** Desktop dependencies stay behind the `desktop` feature and platform-specific code stays behind target configuration.

**Do not hide copies.** Ordinary files stream from disk. Generated captures move into the upload body. A change that silently duplicates a large image or recording is a regression even if it looks fast in a small test.

**Do not invent another control style.** Shared controls and typography belong in `ui/components.slint`. Colors and shared measurements belong in `ui/theme.slint`. Render UI changes in a desktop session because compiling Slint is not a visual test.

**Keep view preferences separate from settings drafts.** Simple and Advanced disclose controls for the same configuration. Switching views persists only that preference immediately; it must preserve the draft, exact hidden values, and active privacy requirements. Save and Discard still control operational settings.

**Keep first-run upload consent explicit.** Fresh desktop setup starts with Local only selected, local copies on, and automatic uploads off. Selecting a provider must not enable automatic uploads. Preserve completed configurations and begin setup replay with their saved choices; the first-run staging rule must not silently rewrite existing settings or redefine explicit Reset behavior.

**Redistribute bundled font licenses.** The desktop app embeds unmodified IBM Plex Sans. Its provenance and checksums live in `assets/fonts/README.md` in the source tree. Keep `IBM-PLEX-SANS-OFL.txt` in Linux, Windows, and macOS packages, including test artifacts, and inside the macOS app's Resources directory so the notice stays with a moved app bundle.

**Treat deletion URLs as secrets.** They are capabilities, not ordinary public links. Do not put real endpoints, credentials, deletion URLs, or user paths in tests, screenshots, logs, or documentation.

## Capture backends

macOS uses ScreenCaptureKit for full-display pixels, window metadata, and persistent recording streams. A static screen may produce idle callbacks without a new image, so recording retains the latest native sample until a new frame arrives. Interactive region screenshots use the system `screencapture` picker and currently ignore logical-size and resize-quality preferences. The recording selector currently presents the primary display; logical recording size uses ScreenCaptureKit's scaling rather than the screenshot resize filter.

Windows uses Windows Graphics Capture. HDR detection combines the display's Advanced Color state with the captured pixel range before choosing SDR or Ultra HDR output.

Linux uses xcap under X11 and screenshot portals under supported Wayland compositors. Native Wayland region recording is intentionally unavailable because repeatedly opening an interactive screenshot portal is not a recording pipeline.

## Memory contracts

These are tested boundaries, not aspirations:

| Operation                                   | Boundary                                           |
| ------------------------------------------- | -------------------------------------------------- |
| Validate an upload lifetime                 | 0 allocations                                      |
| Hand an owned 4 MiB capture to the uploader | 0 allocations                                      |
| Wrap an owned 4 MiB recording for upload    | 1 allocation under 128 bytes, buffer is not copied |
| Prepare a sparse 64 MiB file for streaming  | Less than 16 KiB allocated                         |
| Clean a 32 MiB file-backed image            | Peak heap below 128 KiB; source stays unchanged    |
| Stream a 16 MiB multipart upload            | Peak heap below 2 MiB                              |
| Move a receipt into history                 | 0 allocations                                      |
| Read an uploader response                   | Hard limit of 64 KiB                               |
| Publish upload progress                     | At most 101 observations                           |
| SQLite page cache                           | 512 KiB target                                     |
| Render upload history                       | 25 entries per page                                |

Closing to the tray drops the Slint component and rendered history model when no settings draft or setup session is open. Unsaved settings and setup stay in a hidden UI session so reopening preserves the user's work. Actually quitting with unsaved settings asks whether to discard them; cancelling returns to Settings. Allocator and operating-system working-set pages may remain resident after UI objects are freed.

Animated recording encodes one bounded frame at a time into a temporary file. RGBA input is limited to 32 MiB per frame. Encoded output is configurable from 1 to 3 GiB, below WebP's 32-bit container-size limit; this does not promise a fixed process RSS ceiling.

The latest-result preview uses one background decoder worker and bounded request/result queues. It accepts regular PNG and JPEG files up to 32 MiB, limits either image dimension to 8192 pixels, limits total pixels to 16,777,216, and rejects decoded image data above 64 MiB. The retained RGBA thumbnail is at most 640 × 360. The decoder receives a 128 MiB allocation budget; this is a best-effort decoder limit, not a guarantee about process RSS or every codec allocation. Stale results must never replace a newer artifact, and oversized or unsupported files must retain their ordinary file actions.

## UI screenshots

Generate the README gallery from an existing full desktop binary built under the desired resource limits:

```bash
./docs/screenshots/generate.sh /absolute/path/to/ShareR
```

The generator needs Python 3, Xvfb, Openbox, xdotool, SQLite, and ImageMagick, including `display` and `convert`. It freezes and hashes one full application binary, renders the primary views at 920 × 620 logical pixels and 2x scale, and strips image metadata. A private X server and temporary database contain only synthetic files and links. The result preview comes from an actual local-only region capture; no upload action is invoked. Simple settings and a separate Advanced recording view demonstrate both disclosure levels. Runs preserve available diagnostic frames and application/window-manager logs beside the output, including when an interaction fails.

Without a binary argument, the script permits an automatic build only inside a cgroup v2 memory limit of at most 6 GiB. That build runs offline with the CI profile, one Cargo job, and ShareR optimization level 0. A supplied binary never triggers Cargo. Run the generator in a bounded, network-isolated validation environment; its proxy environment variables are only a fallback, not network isolation. Set `SHARER_SCREENSHOT_OUTPUT_DIR` to write the staged gallery to a scratch directory when the source checkout is read-only.

Regenerate the full set whenever a visible UI change lands, then inspect every image independently. A completed generator run establishes that files were produced, not that the design or interactions passed review.

Judge composition as well as containment: related fields need consistent label placement, aligned controls, and deliberate spacing. Readable controls can still form an incoherent layout. Frame a featured section with its heading and complete card whenever it fits; use overlapping views when scrolling is necessary. A composition failure blocks visual acceptance even when builds and interaction checks pass.

## Image fixtures

The EXIF and HDR fixtures come from [Google libultrahdr](https://github.com/google/libultrahdr) commit `7d51b4cfacb8187ab74c34189db6ebdb895962d8` under [CC BY 4.0](https://github.com/google/libultrahdr/blob/7d51b4cfacb8187ab74c34189db6ebdb895962d8/tests/data/LICENSE).

| Fixture                    | Upstream file                           | SHA-256                                                            |
| -------------------------- | --------------------------------------- | ------------------------------------------------------------------ |
| `minnie_sdr_with_exif.jpg` | `tests/data/minnie-320x240-yuv-icc.jpg` | `171bc422d337d8ba3bd3935cc489c0f755f83687ea7bb39337a7d407b657b08f` |
| `apple_gainmap_new.jpg`    | `tests/data/apple_gainmap_new.jpg`      | `492a94bd0636bcf15b3f560c68142a7783936c1597e78b0d0461d8be7fddc078` |

Tests verify that identifying metadata removal preserves SDR pixels, ICC profiles, HDR gain-map pixels and rendering XMP, and the sizes and offsets in JPEG MPF tables. Metadata-free inspection and owned PNG cleaning do not buffer the image. File and reader cleaning streams through a private temporary file. JPEG XML inspection is limited to a 64 KiB packet and 4096 XML nodes; MPF rewriting supports up to 32 images. Unsupported or mixed HDR metadata is rejected before upload rather than retaining identifying fields or changing HDR rendering. Google gain-map image-length references are retained only when they match the unchanged second JPEG; cleaning a referenced secondary image, primary-length references, and padding references are rejected before mutation. They also cover every public resize quality and generated SDR PNG metadata. Do not replace a fixture without updating its provenance and checksum here.

## Uploaders

The public Custom uploader contract lives in [Custom uploaders](docs/custom-uploaders.md). Built-in providers each have their own implementation under `src/upload/providers/` and must not reuse credentials belonging to another provider.

Keep response bodies bounded, validate every returned link, and add parser tests using fake domains and fake credentials.

## Before you open a PR

Run the same checks the repository expects:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo clippy --features desktop --all-targets -- -D warnings
cargo test --lib --bins --tests
cargo test --features desktop --lib --bins --tests
cargo bench --bench memory
rulewright --llm
rulewright --strict
shfmt -d docs/screenshots/generate.sh
shellcheck docs/screenshots/generate.sh
actionlint
git ls-files -z -- '*.md' | xargs -0 prettier --no-config --no-editorconfig --ignore-path /dev/null --parser markdown --check
```

The Markdown check uses an existing Prettier 3.8.3 installation, only tracked Markdown files, and no repository configuration or EditorConfig. It does not install packages or load plugins.

Keep LF line endings in Rust and shell files, as specified by `.gitattributes`. CRLF can produce false unused-dependency warnings in Rulewright and break shell scripts.

For UI changes, regenerate the screenshots and inspect every affected view at the default and minimum supported window sizes.

Use separate acceptance gates for layout, interaction, and state handling. Layout review must inspect actual renders at 100%, 150%, and 200%, including intermediate scroll positions: labels must fit, related controls must align, and scrollbars must have clear space beside content. Interaction review must exercise mouse and keyboard focus, popup selection, cancellation, and reopening. State review must verify saved settings and files after success, failure, and cancellation. Ask a reviewer who did not implement the view to judge the screenshots; a successful build or screenshot command does not pass the visual gate. Record the tested binary, fixtures, and any platform-specific gaps before publishing the gallery.

For the capture workspace, also inspect PNG and JPEG previews, unsupported formats, missing or oversized files, and a newer result arriving while an older preview is loading. Copy/open feedback must preserve the capture or upload outcome. A failed new attempt must clearly distinguish its error from the retained previous artifact, and cancellation must not look like failure. Check effective forced-proxy messaging and the no-output/unconfigured-uploader readiness states. Stop recording and Cancel upload must remain reachable at the minimum window size. Exercise Simple/Advanced switching with unsaved edits and exact custom values, then Save, Discard, close, and reopen. History review must combine search, filters, sort order, pagination, and deletion; use fake deletion links to verify that their secret values cannot produce search matches. Include a large history with missing paths to measure event-loop responsiveness while the background worker reconciles it.

Record design, interaction, feature/state, and security verdicts separately, with unresolved issues and the evidence each reviewer actually inspected. Linux/Xvfb screenshots establish Linux layout evidence only. They do not establish native Windows or macOS behavior, screen-reader compatibility, working system capture permissions, or a passed security review. Keep those gaps explicit until their corresponding checks run.

Windows CI runs the complete headless and desktop test suites, including redirected CLI output, per-user startup registration in a disposable non-startup registry subtree, and single-instance coordination. For native UI checks, use an isolated `SHARER_CONFIG_DIR` and a local test uploader; do not change the user's login registration or upload their captures.

macOS CI runs the headless and desktop suites on both Apple silicon and Intel runners. Its capture tests cover retained static frames and Retina coordinate calculations without requiring screen-recording permission. The plist checks include native `plutil` and `PlistBuddy` round trips. These checks do not replace a real Mac session: verify permissions, a recording of an unchanged desktop longer than five seconds, Retina and secondary-display selection, tray behavior, and an actual sign-in. The startup switch currently checks the LaunchAgent file, not external `launchd` disable overrides.

Check first-run, replay/cancel, reset, long error messages, and all history deletion choices at 100%, 150%, and 200% scale. A screenshot of a clean default page is not enough: exercise the result card after successful upload, local-save failure, and upload failure, and verify that settings retain exact lifetimes and credentials.

Exercise History uploads against a local test server: success must update the original row without a duplicate, and failure or cancellation must preserve the original file and its result actions. Open the actual popup menus, including their conditional upload and deletion items; directly calling Rust callbacks does not test whether a Slint popup dispatches its action after closing. Check keyboard focus through setup and help popups on an interactive desktop.
