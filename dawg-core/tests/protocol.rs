use dawg_core::{CreateServer, Request, Response, ServerKind, ServerStatus};

#[test]
fn every_server_kind_round_trips_in_a_create_request() {
    for kind in ServerKind::ALL {
        let request = Request::CreateServer(CreateServer {
            name: "Friends \"and\" family 🐾".to_owned(),
            kind,
            minecraft_version: "1.21.4".to_owned(),
            memory_mb: 2048,
            port: 25565,
            java_path: "/a path/java".to_owned(),
            eula_accepted: true,
            import_jar: None,
            network: Default::default(),
        });
        let bytes = serde_json::to_vec(&request).unwrap();
        assert!(!bytes.contains(&b'\n'), "a request must fit one JSON frame");
        let decoded: Request = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            serde_json::to_value(decoded).unwrap(),
            serde_json::to_value(request).unwrap()
        );
    }
}

#[test]
fn create_request_without_optional_import_remains_compatible() {
    let request: Request = serde_json::from_str(r#"{"CreateServer":{"name":"Survival","kind":"Paper","minecraft_version":"1.21.4","memory_mb":4096,"port":25565,"java_path":"java","eula_accepted":false}}"#).unwrap();
    let Request::CreateServer(config) = request else {
        panic!("wrong request")
    };
    assert_eq!(config.kind, ServerKind::Paper);
    assert!(!config.eula_accepted);
    assert!(config.import_jar.is_none());
}

#[test]
fn invalid_and_unknown_requests_are_not_silently_accepted() {
    for input in [
        "not json",
        r#""LaunchEverything""#,
        r#"{"SendCommand":{"id":"1"}}"#,
        r#"{"CreateServer":{"name":"incomplete"}}"#,
    ] {
        assert!(serde_json::from_str::<Request>(input).is_err(), "{input}");
    }
}

#[test]
fn multiline_console_errors_still_fit_a_single_json_frame() {
    let response = Response::Error("First line\nSecond line\r\n\"quoted\"".into());
    let frame = serde_json::to_vec(&response).unwrap();
    assert!(!frame.contains(&b'\n'));
    let Response::Error(message) = serde_json::from_slice::<Response>(&frame).unwrap() else {
        panic!("error response lost its type")
    };
    assert_eq!(message, "First line\nSecond line\r\n\"quoted\"");
}

#[test]
fn only_live_process_states_are_active() {
    for status in [
        ServerStatus::Starting,
        ServerStatus::Running,
        ServerStatus::Stopping,
    ] {
        assert!(status.is_active());
    }
    for status in [
        ServerStatus::Stopped,
        ServerStatus::Installing,
        ServerStatus::Crashed,
    ] {
        assert!(!status.is_active());
    }
}
