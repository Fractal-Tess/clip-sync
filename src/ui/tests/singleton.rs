use super::*;

#[test]
fn unified_ui_singleton_delivers_quick_and_management_intents() {
    use std::os::unix::fs::PermissionsExt as _;

    let temporary = tempfile::tempdir().expect("temporary directory");
    let mut first = UiInstance::acquire(temporary.path(), UiSignal::OpenQuick)
        .expect("acquire first instance")
        .expect("first instance owns lock");
    let (signal_tx, signal_rx) = std_mpsc::channel();
    first
        .start_signal_listener(egui::Context::default(), signal_tx)
        .expect("start signal listener");

    let second = UiInstance::acquire(temporary.path(), UiSignal::OpenManagement)
        .expect("signal first instance");

    assert!(second.is_none());
    assert_eq!(
        signal_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("management intent"),
        UiSignal::OpenManagement
    );
    assert!(
        UiInstance::acquire(temporary.path(), UiSignal::OpenQuick)
            .expect("signal quick intent")
            .is_none()
    );
    assert_eq!(
        signal_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("quick intent"),
        UiSignal::OpenQuick
    );
    assert!(temporary.path().join("switcher.lock").exists());
    assert!(!temporary.path().join("control.lock").exists());
    assert_eq!(
        std::fs::metadata(temporary.path())
            .expect("private runtime directory")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for path in [
        temporary.path().join("switcher.lock"),
        temporary.path().join("switcher.sock"),
    ] {
        assert_eq!(
            std::fs::metadata(path)
                .expect("private singleton resource")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    drop(first);
    assert!(!temporary.path().join("switcher.sock").exists());
}

#[test]
fn ui_instance_does_not_replace_regular_signal_path() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let signal_socket = temporary.path().join("switcher.sock");
    std::fs::write(&signal_socket, b"sentinel").expect("write sentinel");

    let Err(error) = UiInstance::acquire(temporary.path(), UiSignal::OpenManagement) else {
        panic!("regular signal path must be rejected");
    };

    assert!(error.contains("refusing to replace non-socket"));
    assert_eq!(
        std::fs::read(&signal_socket).expect("sentinel remains"),
        b"sentinel"
    );
}

#[test]
fn local_ui_signal_protocol_rejects_malformed_or_oversized_messages() {
    assert_eq!(UiSignal::parse(b"open-quick\n"), Some(UiSignal::OpenQuick));
    assert_eq!(
        UiSignal::parse(b"open-management\n"),
        Some(UiSignal::OpenManagement)
    );
    assert_eq!(
        UiSignal::parse(b"close-quick\n"),
        Some(UiSignal::CloseQuick)
    );
    for malformed in [
        b"close\n".as_slice(),
        b"close-quick".as_slice(),
        b"close-quick\nextra".as_slice(),
        &[b'x'; 33],
    ] {
        assert_eq!(UiSignal::parse(malformed), None);
    }
}

#[test]
fn global_close_signal_is_a_no_op_without_a_ui_and_creates_nothing() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let runtime = temporary.path().join("missing-runtime");
    let paths = AppPaths {
        config: temporary.path().join("config.toml"),
        state_dir: temporary.path().join("state"),
        socket: runtime.join("daemon.sock"),
        runtime_dir: runtime.clone(),
    };

    close_quick(&paths).expect("absent UI is a no-op");
    assert!(!runtime.exists());
}

#[test]
fn unsafe_local_signal_path_is_rejected_without_replacement() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let signal_path = temporary.path().join("switcher.sock");
    std::fs::write(&signal_path, b"sentinel").expect("write sentinel");

    let error = request_existing_signal(&signal_path, UiSignal::CloseQuick, true)
        .expect_err("regular file must be rejected");
    assert!(error.contains("unsafe"));
    assert_eq!(std::fs::read(signal_path).expect("sentinel"), b"sentinel");

    let runtime_link = temporary.path().join("runtime-link");
    std::os::unix::fs::symlink(temporary.path(), &runtime_link).expect("runtime symlink");
    let paths = AppPaths {
        config: temporary.path().join("config.toml"),
        state_dir: temporary.path().join("state"),
        socket: runtime_link.join("daemon.sock"),
        runtime_dir: runtime_link,
    };
    assert!(close_quick(&paths).is_err());
}
