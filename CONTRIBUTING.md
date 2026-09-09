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

On Windows, use the MSVC Rust toolchain and install Visual Studio Build Tools with the **Desktop development with C++** workload and a Windows SDK.

The macOS desktop build targets macOS 14 and is compiled separately for Apple silicon and Intel Macs.

Some checks use small standalone tools:

| Tool                                   | Used for                        |
| -------------------------------------- | ------------------------------- |
| Prettier                               | Markdown formatting             |
| Rulewright                             | Repository-specific Rust rules  |
| shfmt                                  | Screenshot generator formatting |
| ShellCheck                             | Screenshot generator linting    |
| actionlint                             | GitHub Actions workflow linting |
| ImageMagick, SQLite, Xvfb, and xdotool | Screenshot generation           |

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
| `tests/fixtures/`   | Licensed reference images used by transformation tests                  |
| `docs/screenshots/` | README screenshots and their generator                                  |

## A few rules that matter

**Keep headless builds headless.** Desktop dependencies stay behind the `desktop` feature and platform-specific code stays behind target configuration.

**Do not hide copies.** Ordinary files stream from disk. Generated captures move into the upload body. A change that silently duplicates a large image or recording is a regression even if it looks fast in a small test.

**Do not invent another control style.** Shared controls and typography belong in `ui/components.slint`. Colors and shared measurements belong in `ui/theme.slint`. Render UI changes in a desktop session because compiling Slint is not a visual test.

**Treat deletion URLs as secrets.** They are capabilities, not ordinary public links. Do not put real endpoints, credentials, deletion URLs, or user paths in tests, screenshots, logs, or documentation.

## Capture backends

macOS uses ScreenCaptureKit for pixels, window metadata, and persistent recording streams. Region selection uses logical desktop coordinates, while capture output keeps native Retina pixels unless the user asks for logical size.

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
| Stream a 16 MiB multipart upload            | Peak heap below 2 MiB                              |
| Move a receipt into history                 | 0 allocations                                      |
| Read an uploader response                   | Hard limit of 64 KiB                               |
| Publish upload progress                     | At most 101 observations                           |
| SQLite page cache                           | 512 KiB target                                     |
| Render upload history                       | 25 entries per page                                |

Closing to the tray drops the Slint component and rendered history model. Allocator and operating-system working-set pages may remain resident after those objects are freed.

## UI screenshots

The README gallery is generated from the real desktop application against an isolated temporary database:

```bash
./docs/screenshots/generate.sh
```

The generator needs Xvfb, xdotool, SQLite, and ImageMagick. It renders every primary view at 2x scale, strips image metadata, and never reads the user's ShareR configuration. Regenerate the full set whenever a visible UI change lands.

## Image fixtures

The EXIF and HDR fixtures come from [Google libultrahdr](https://github.com/google/libultrahdr) commit `7d51b4cfacb8187ab74c34189db6ebdb895962d8` under [CC BY 4.0](https://github.com/google/libultrahdr/blob/7d51b4cfacb8187ab74c34189db6ebdb895962d8/tests/data/LICENSE).

| Fixture                    | Upstream file                           | SHA-256                                                            |
| -------------------------- | --------------------------------------- | ------------------------------------------------------------------ |
| `minnie_sdr_with_exif.jpg` | `tests/data/minnie-320x240-yuv-icc.jpg` | `171bc422d337d8ba3bd3935cc489c0f755f83687ea7bb39337a7d407b657b08f` |
| `apple_gainmap_new.jpg`    | `tests/data/apple_gainmap_new.jpg`      | `492a94bd0636bcf15b3f560c68142a7783936c1597e78b0d0461d8be7fddc078` |

Tests verify that EXIF removal preserves SDR pixels, ICC profiles, and HDR gain-map structure. They also cover every public resize quality and generated SDR PNG metadata. Do not replace a fixture without updating its provenance and checksum here.

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
prettier --check "**/*.md"
```

For UI changes, regenerate the screenshots and inspect every affected view at the default and minimum supported window sizes.
