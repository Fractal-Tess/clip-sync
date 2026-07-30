use super::*;

#[test]
fn unified_window_identity_and_size_contract_are_stable() {
    assert_eq!(APP_ID, "clip-sync-switcher");
    assert_eq!(WINDOW_TITLE, "ClipSync");
    assert_eq!(IPC_PROTOCOL_VERSION, 5);
    assert!(
        WindowGeometry {
            x: None,
            y: None,
            width: 720,
            height: 480,
        }
        .is_valid()
    );
    assert!(
        WindowGeometry {
            x: None,
            y: None,
            width: 480,
            height: 300,
        }
        .is_valid()
    );
    assert!(
        !WindowGeometry {
            x: None,
            y: None,
            width: 479,
            height: 300,
        }
        .is_valid()
    );
}

#[test]
fn window_geometry_is_private_and_round_trips() {
    use std::os::unix::fs::PermissionsExt as _;

    let temporary = tempfile::tempdir().expect("temporary directory");
    let path = window_state_path(temporary.path());
    let geometry = WindowGeometry {
        x: Some(-1200),
        y: Some(80),
        width: 860,
        height: 510,
    };

    save_window_geometry(&path, geometry).expect("save geometry");

    assert_eq!(load_window_geometry(&path), Some(geometry));
    assert_eq!(
        std::fs::metadata(&path)
            .expect("window state metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some("window.json")
    );
}

#[test]
fn switcher_geometry_migrates_first_to_the_only_geometry_file() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let [switcher, control] = legacy_window_state_paths(temporary.path());
    let switcher_geometry = WindowGeometry {
        x: Some(10),
        y: Some(20),
        width: 720,
        height: 480,
    };
    let control_geometry = WindowGeometry {
        x: Some(30),
        y: Some(40),
        width: 1040,
        height: 700,
    };
    std::fs::write(
        &switcher,
        serde_json::to_vec(&switcher_geometry).expect("switcher geometry"),
    )
    .expect("write switcher geometry");
    std::fs::write(
        &control,
        serde_json::to_vec(&control_geometry).expect("control geometry"),
    )
    .expect("write control geometry");

    assert_eq!(
        load_or_migrate_window_geometry(temporary.path()).expect("migrate geometry"),
        Some(switcher_geometry)
    );
    assert_eq!(
        load_window_geometry(&window_state_path(temporary.path())),
        Some(switcher_geometry)
    );
    assert!(!switcher.exists());
    assert!(!control.exists());
}

#[test]
fn invalid_window_geometry_is_ignored() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let path = window_state_path(temporary.path());
    std::fs::write(&path, br#"{"x":10,"y":null,"width":1040,"height":700}"#)
        .expect("write invalid geometry");

    assert_eq!(load_window_geometry(&path), None);
    assert!(
        save_window_geometry(
            &path,
            WindowGeometry {
                x: None,
                y: None,
                width: 120,
                height: 100,
            }
        )
        .is_err()
    );
}

#[test]
fn hyprland_client_geometry_deserializes() {
    let client = serde_json::from_str::<HyprlandClient>(
        r#"{"address":"0xabc","class":"clip-sync-switcher","at":[100,200],"size":[720,420]}"#,
    )
    .expect("Hyprland client");

    assert_eq!(client.address, "0xabc");
    assert_eq!(client.at, [100, 200]);
    assert_eq!(client.size, [720, 420]);
}
