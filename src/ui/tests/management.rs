use super::*;
#[test]
fn management_grids_are_responsive() {
    assert_eq!(management_grid_columns(688.0), 2);
    assert_eq!(management_grid_columns(448.0), 1);
}

#[test]
fn management_cards_fit_supported_viewports() {
    for size in [Vec2::new(480.0, 300.0), Vec2::new(720.0, 480.0)] {
        let context = egui::Context::default();
        configure_style(&context);
        let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        let peer = PeerItem {
            hostname: "very-long-authenticated-peer-name.netbird.cloud".to_owned(),
            address: "100.127.255.255".to_owned(),
            connected: true,
            stats: Some(PeerStats {
                shared_items: u64::MAX,
                shared_bytes: u64::MAX,
                pinned_items: u64::MAX,
                last_shared_millis: Some(1),
            }),
        };
        let check = DiagnosticCheck {
            name: "encrypted_storage_with_a_long_check_name".to_owned(),
            ok: true,
            detail: "A long diagnostic detail that must wrap without widening the management route beyond its supported viewport.".to_owned(),
        };
        let mut peer_rect = egui::Rect::NOTHING;
        let mut diagnostic_rect = egui::Rect::NOTHING;
        let mut header_name_rect = egui::Rect::NOTHING;
        let mut header_status_rect = egui::Rect::NOTHING;
        let _ = context.run_ui(egui_input(size, None), |ui| {
            Frame::new().inner_margin(Margin::same(16)).show(ui, |ui| {
                let columns = management_grid_columns(ui.available_width());
                ui.columns(columns, |uis| {
                    peer_rect = peer_card(&mut uis[0], &peer).rect;
                });
                ui.columns(columns, |uis| {
                    (header_name_rect, header_status_rect) = {
                        let (name, status) = peer_card_header(&mut uis[0], &peer, SUCCESS);
                        (name.rect, status.rect)
                    };
                });
                ui.columns(columns, |uis| {
                    diagnostic_rect = diagnostic_card(&mut uis[0], &check).rect;
                });
            });
        });
        for rect in [
            peer_rect,
            diagnostic_rect,
            header_name_rect,
            header_status_rect,
        ] {
            assert!(rect.left() >= viewport.left());
            assert!(rect.right() <= viewport.right());
            assert!(rect.width() > 0.0);
        }
        assert!(
            !header_name_rect.intersects(header_status_rect),
            "peer name and status overlap at {size:?}: {header_name_rect:?} / {header_status_rect:?}"
        );
    }
}

#[test]
fn mesh_quota_accepts_human_readable_binary_units() {
    assert_eq!(parse_byte_size("1 GiB"), Some(1_073_741_824));
    assert_eq!(parse_byte_size("512 MiB"), Some(536_870_912));
    assert_eq!(parse_byte_size("1.5 GiB"), Some(1_610_612_736));
    assert_eq!(parse_byte_size("1073741824"), Some(1_073_741_824));
    assert_eq!(parse_byte_size("0.1 KiB"), None);
    assert_eq!(parse_byte_size("1.9 B"), None);
    assert_eq!(parse_byte_size("1. B"), None);
    assert_eq!(parse_byte_size("1.b"), None);
    assert_eq!(parse_byte_size("1 nonsense"), None);
    assert_eq!(parse_byte_size(""), None);
}

#[test]
fn editable_byte_sizes_round_trip_without_lowering_quota() {
    for bytes in [1, 1_024, 1_073_741_824, 2_136_748_011, u64::MAX] {
        assert_eq!(parse_byte_size(&format_bytes_input(bytes)), Some(bytes));
    }
    assert!(format_bytes_exact(2_136_748_011).contains("2136748011 B exact"));
}
