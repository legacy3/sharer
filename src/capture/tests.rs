use super::*;

#[test]
fn every_resize_quality_produces_the_requested_dimensions() {
    let image = RgbaImage::from_fn(7, 5, |x, y| {
        image::Rgba([
            u8::try_from(x * 31).unwrap(),
            u8::try_from(y * 47).unwrap(),
            u8::try_from((x + y) * 19).unwrap(),
            255,
        ])
    });

    for quality in [
        ResizeQuality::Fast,
        ResizeQuality::Balanced,
        ResizeQuality::Sharp,
    ] {
        let resized = resize_rgba(&image, 3, 2, quality);

        assert_eq!(resized.dimensions(), (3, 2));
    }
}

#[cfg(not(windows))]
#[test]
fn selector_preview_is_reduced_to_its_pixel_allowance() {
    let screen = CapturedScreen {
        preview: RgbaImage::new(8, 4),
        desktop_bounds: DesktopBounds {
            x: 0,
            y: 0,
            width: 4,
            height: 2,
        },
        window_regions: Vec::new(),
    };
    let preview = bounded_selector_preview(screen.preview, 8);

    assert_eq!(preview.dimensions(), (4, 2));
}

#[test]
fn selector_previews_are_bounded_across_four_4k_displays() {
    let per_display = selector_preview_pixel_budget(4);
    let dimensions = bounded_selector_dimensions(3840, 2160, per_display);
    let total_pixels = u64::from(dimensions.0) * u64::from(dimensions.1) * 4;
    let owned_bytes = selector_owned_bytes(total_pixels);

    assert!(total_pixels <= SELECTOR_SOURCE_PREVIEW_BUDGET_BYTES / 4);
    assert!(owned_bytes <= SELECTOR_SOURCE_PREVIEW_BUDGET_BYTES * 2);
}

#[test]
fn five_k_selector_downscale_allocates_only_the_bounded_output() {
    let max_pixels = selector_preview_pixel_budget(1);
    let image = RgbaImage::new(5120, 2880);
    let mut preview = None;
    let allocations = allocation_counter::measure(|| {
        preview = Some(bounded_selector_preview(image, max_pixels));
    });
    let preview = preview.unwrap();
    let output_bytes =
        u64::from(preview.width()) * u64::from(preview.height()) * RGBA8_BYTES_PER_PIXEL;

    assert!(output_bytes <= SELECTOR_SOURCE_PREVIEW_BUDGET_BYTES);
    assert!(allocations.bytes_max <= SELECTOR_SOURCE_PREVIEW_BUDGET_BYTES);
}

mod regions {
    use super::*;

    #[test]
    fn normalized_region_is_cropped_to_expected_dimensions() {
        let image = RgbaImage::new(200, 100);
        let region = NormalizedRegion::new([0.25, 0.2, 0.5, 0.4]);
        let payload = crop_region(&image, region).unwrap();
        let decoded = image::load_from_memory(payload.bytes().unwrap()).unwrap();

        assert_eq!((decoded.width(), decoded.height()), (100, 40));
        assert_eq!(payload.filename, "region.png");
    }

    #[test]
    fn normalized_region_maps_to_offset_desktop_coordinates() {
        let desktop = DesktopBounds {
            x: -1920,
            y: 100,
            width: 1920,
            height: 1080,
        };
        let region = NormalizedRegion::new([0.25, 0.5, 0.5, 0.25]);
        let bounds = normalized_desktop_bounds(desktop, region).unwrap();

        assert_eq!(
            bounds,
            DesktopRegion {
                x: -1440,
                y: 640,
                width: 960,
                height: 270,
            }
        );
    }

    #[test]
    fn hover_chooses_first_topmost_overlapping_window() {
        let top = [0.2, 0.2, 0.4, 0.4];
        let windows = [
            WindowRegion {
                bounds: top,
                desktop_bounds: DesktopBounds {
                    x: 384,
                    y: 216,
                    width: 768,
                    height: 432,
                },
                title: "Top".to_owned(),
            },
            WindowRegion {
                bounds: [0.0, 0.0, 1.0, 1.0],
                desktop_bounds: DesktopBounds {
                    x: 0,
                    y: 0,
                    width: 1920,
                    height: 1080,
                },
                title: "Bottom".to_owned(),
            },
        ];

        assert_eq!(window_region_at(&windows, [0.3, 0.3]), Some(top));
        assert_eq!(window_region_at(&windows, [1.1, 0.5]), None);
        let selected = selection_region_at(&windows, [1.1, 0.5]);
        let full_display = [0.0, 0.0, 1.0, 1.0];

        assert!(
            selected
                .iter()
                .zip(full_display)
                .all(|(actual, expected)| (*actual - expected).abs() < f32::EPSILON)
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn nvidia_overlay_windows_are_not_snap_candidates() {
        assert!(!is_snap_candidate_title("NVIDIA GeForce Overlay DT"));
        assert!(!is_snap_candidate_title("NVIDIA GeForce Overlay"));
        assert!(is_snap_candidate_title("Visual Studio Code"));
    }

    #[cfg(windows)]
    #[test]
    fn nonactivating_tool_windows_are_overlays() {
        use windows::Win32::UI::WindowsAndMessaging::{
            WINDOW_EX_STYLE, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        };

        assert!(is_overlay_style(WINDOW_EX_STYLE(
            WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0
        )));
        assert!(!is_overlay_style(WS_EX_LAYERED));
        assert!(!is_overlay_style(WS_EX_NOACTIVATE));
        assert!(!is_overlay_style(WS_EX_TOOLWINDOW));
    }

    #[cfg(windows)]
    #[test]
    fn nvidia_overlay_class_is_ignored() {
        assert!(is_ignored_overlay_class("CEF-OSC-WIDGET"));
        assert!(is_ignored_overlay_class("cef-osc-widget"));
        assert!(!is_ignored_overlay_class("Chrome_WidgetWin_1"));
    }

    #[test]
    fn snapped_window_bounds_preserve_exact_desktop_pixels() {
        let desktop = DesktopBounds {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let window = DesktopBounds {
            x: 17,
            y: 31,
            width: 901,
            height: 503,
        };
        let bounds = exact_window_bounds(desktop, window, 1920, 1080).unwrap();

        assert_eq!((bounds.x, bounds.y), (17, 31));
        assert_eq!((bounds.width, bounds.height), (901, 503));
    }
}

mod sdr {
    use super::*;

    #[test]
    fn generated_png_has_native_dimensions_and_no_optional_metadata() {
        let image = RgbaImage::from_pixel(17, 9, image::Rgba([12, 34, 56, 255]));
        let payload = encode_sdr_png(image, "private.png").unwrap();
        let bytes = payload.bytes().unwrap();
        let decoded = image::load_from_memory(bytes).unwrap();
        let mut chunks = Vec::new();
        let mut offset = 8;

        while offset + 12 <= bytes.len() {
            let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap());
            let end = offset + 12 + length as usize;

            if end > bytes.len() {
                break;
            }

            chunks.push(&bytes[offset + 4..offset + 8]);
            offset = end;
        }

        assert_eq!((decoded.width(), decoded.height()), (17, 9));

        for forbidden in [b"tEXt", b"zTXt", b"iTXt", b"eXIf"] {
            assert!(!chunks.contains(&forbidden.as_slice()));
        }
    }

    #[test]
    fn generated_filename_preserves_automatic_hdr_marker() {
        assert_eq!(
            named_capture_filename("terminal", "region-hdr.jpg"),
            "terminal-hdr.jpg"
        );
        assert_eq!(
            named_capture_filename("terminal", "region.png"),
            "terminal.png"
        );
    }

    #[test]
    fn scrgb_reference_white_is_normalized_for_ultra_hdr() {
        let normalized = scrgb_to_ultra_hdr_linear(1.0);

        assert!((normalized - 80.0 / 203.0).abs() < f32::EPSILON);
        assert!((scrgb_to_ultra_hdr_linear(203.0 / 80.0) - 1.0).abs() < f32::EPSILON);
    }
}
