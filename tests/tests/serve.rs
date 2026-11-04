use std::fs;

use insta::{assert_snapshot, assert_yaml_snapshot, with_settings};
use rbx_dom_weak::types::Ref;
use reqwest::StatusCode;
use tempfile::tempdir;

use crate::rojo_test::{
    internable::InternAndRedact,
    serve_util::{deserialize_msgpack, run_serve_test, serialize_to_xml_model},
};

use librojo::web_api::{
    ReadResponse, SerializeResponse, SocketPacketBody, SocketPacketType, SubscribeMessage,
};

#[test]
fn rejects_dns_rebinding_requests() {
    run_serve_test("empty", |session, _redactions| {
        let port = session.port();
        let local_host = format!("localhost:{port}");

        // A request carrying a local Host header is served normally.
        assert_eq!(
            session
                .api_rojo_response_with_headers(&[("host", &local_host)])
                .status(),
            reqwest::StatusCode::OK,
        );

        // A request whose Host is a foreign hostname, as a DNS-rebound page
        // would send, is rejected with a generic 404 that reveals nothing about
        // the server.
        assert_rejected(session.api_rojo_response_with_headers(&[("host", "evil.com")]));

        // Even with a local Host, a present-but-foreign Origin is rejected.
        let foreign_origin = format!("http://evil.com:{port}");
        assert_rejected(
            session.api_rojo_response_with_headers(&[
                ("host", &local_host),
                ("origin", &foreign_origin),
            ]),
        );
    });
}

/// Asserts that a Host/Origin rejection is a generic 404 whose body and
/// content-type do not identify the server as Rojo.
fn assert_rejected(response: reqwest::blocking::Response) {
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        !content_type.contains("msgpack"),
        "rejection should not use the msgpack API content-type, got {content_type:?}",
    );

    let body = response.text().expect("Failed to read response body");
    let body_lower = body.to_lowercase();
    assert!(
        !body_lower.contains("rojo") && !body_lower.contains("rebinding"),
        "rejection body should not identify the server, got {body:?}",
    );
}

#[test]
fn allows_api_open_from_loopback_peer() {
    run_serve_test("empty", |session, _redactions| {
        // The harness always connects over loopback, so the local-only gate on
        // /api/open must let the request through. A bogus instance id then fails
        // id parsing with 400, which confirms we got past the gate rather than
        // being rejected with 403.
        assert_eq!(
            session.api_open_status("not-a-real-ref"),
            reqwest::StatusCode::BAD_REQUEST,
        );
    });
}

#[test]
fn empty() {
    run_serve_test("empty", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("empty_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "empty_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn scripts() {
    run_serve_test("scripts", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("scripts_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        with_settings!({ sort_maps => true }, {
            assert_yaml_snapshot!(
                "scripts_all",
                read_response.intern_and_redact(&mut redactions, root_id)
            );
        });

        fs::write(session.path().join("src/foo.lua"), "Updated foo!").unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();
        assert_yaml_snapshot!(
            "scripts_subscribe",
            socket_packet.intern_and_redact(&mut redactions, ())
        );

        let read_response = session.get_api_read(root_id).unwrap();
        with_settings!({ sort_maps => true }, {
            assert_yaml_snapshot!(
                "scripts_all-2",
                read_response.intern_and_redact(&mut redactions, root_id)
            );
        });
    });
}

#[test]
fn add_folder() {
    run_serve_test("add_folder", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("add_folder_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "add_folder_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        fs::create_dir(session.path().join("src/my-new-folder")).unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();
        assert_yaml_snapshot!(
            "add_folder_subscribe",
            socket_packet.intern_and_redact(&mut redactions, ())
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "add_folder_all-2",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn remove_file() {
    run_serve_test("remove_file", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("remove_file_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "remove_file_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        fs::remove_file(session.path().join("src/hello.txt")).unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();
        assert_yaml_snapshot!(
            "remove_file_subscribe",
            socket_packet.intern_and_redact(&mut redactions, ())
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "remove_file_all-2",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn edit_init() {
    run_serve_test("edit_init", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("edit_init_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "edit_init_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        fs::write(session.path().join("src/init.lua"), b"-- Edited contents").unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();
        assert_yaml_snapshot!(
            "edit_init_subscribe",
            socket_packet.intern_and_redact(&mut redactions, ())
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "edit_init_all-2",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn edit_data_csv() {
    run_serve_test("edit_data_csv", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("edit_data_csv_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "edit_data_csv_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        fs::write(
            session.path().join("src/Items.data.csv"),
            "id,cost
sword,150
shield,50
",
        )
        .unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();
        assert_yaml_snapshot!(
            "edit_data_csv_subscribe",
            socket_packet.intern_and_redact(&mut redactions, ())
        );
    });
}

#[test]
fn move_folder_of_stuff() {
    run_serve_test("move_folder_of_stuff", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("move_folder_of_stuff_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "move_folder_of_stuff_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        // Create a directory full of stuff we can move in
        let src_dir = tempdir().unwrap();
        let stuff_path = src_dir.path().join("new-stuff");

        fs::create_dir(&stuff_path).unwrap();

        // Make a bunch of random files in our stuff folder
        for i in 0..10 {
            let file_name = stuff_path.join(format!("{}.txt", i));
            let file_contents = format!("File #{}", i);

            fs::write(file_name, file_contents).unwrap();
        }

        // We're hoping that this rename gets picked up as one event. This test
        // will fail otherwise.
        fs::rename(stuff_path, session.path().join("src/new-stuff")).unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();
        assert_yaml_snapshot!(
            "move_folder_of_stuff_subscribe",
            socket_packet.intern_and_redact(&mut redactions, ())
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "move_folder_of_stuff_all-2",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn empty_json_model() {
    run_serve_test("empty_json_model", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("empty_json_model_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "empty_json_model_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        fs::write(
            session.path().join("src/test.model.json"),
            r#"{"ClassName": "Model"}"#,
        )
        .unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();
        assert_yaml_snapshot!(
            "empty_json_model_subscribe",
            socket_packet.intern_and_redact(&mut redactions, ())
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "empty_json_model_all-2",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
#[ignore = "Rojo does not watch missing, optional files for changes."]
fn add_optional_folder() {
    run_serve_test("add_optional_folder", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("add_optional_folder", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "add_optional_folder_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        fs::create_dir(session.path().join("create-later")).unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();
        assert_yaml_snapshot!(
            "add_optional_folder_subscribe",
            socket_packet.intern_and_redact(&mut redactions, ())
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "add_optional_folder_all-2",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn sync_rule_alone() {
    run_serve_test("sync_rule_alone", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("sync_rule_alone_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "sync_rule_alone_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn sync_rule_complex() {
    run_serve_test("sync_rule_complex", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("sync_rule_complex_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "sync_rule_complex_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn sync_rule_no_extension() {
    run_serve_test("sync_rule_no_extension", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!(
            "sync_rule_no_extension_info",
            redactions.redacted_yaml(info)
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "sync_rule_no_extension_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn no_name_default_project() {
    run_serve_test("no_name_default_project", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!(
            "no_name_default_project_info",
            redactions.redacted_yaml(info)
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "no_name_default_project_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn no_name_project() {
    run_serve_test("no_name_project", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("no_name_project_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "no_name_project_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn no_name_top_level_project() {
    run_serve_test("no_name_top_level_project", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!(
            "no_name_top_level_project_info",
            redactions.redacted_yaml(info)
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "no_name_top_level_project_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        let project_path = session.path().join("default.project.json");
        let mut project_contents = fs::read_to_string(&project_path).unwrap();
        project_contents.push('\n');
        fs::write(&project_path, project_contents).unwrap();

        // The cursor shouldn't be changing so this snapshot is fine for testing
        // the response.
        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "no_name_top_level_project_all-2",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn sync_rule_no_name_project() {
    run_serve_test("sync_rule_no_name_project", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!(
            "sync_rule_no_name_project_info",
            redactions.redacted_yaml(info)
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "sync_rule_no_name_project_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn ref_properties() {
    run_serve_test("ref_properties", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("ref_properties_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "ref_properties_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        fs::write(
            session.path().join("ModelTarget.model.json"),
            r#"{
                "className": "Folder",
                "attributes": {
                    "Rojo_Id": "model target 2"
                },
                "children": [
                  {
                    "name": "ModelPointer",
                    "className": "Model",
                    "attributes": {
                      "Rojo_Target_PrimaryPart": "model target 2"
                    }
                  },
                  {
                    "name": "ProjectPointer",
                    "className": "Model",
                    "attributes": {
                      "Rojo_Target_PrimaryPart": "project target"
                    }
                  }
                ]
              }"#,
        )
        .unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();
        assert_yaml_snapshot!(
            "ref_properties_subscribe",
            socket_packet.intern_and_redact(&mut redactions, ())
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "ref_properties_all-2",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn ref_properties_remove() {
    run_serve_test("ref_properties_remove", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("ref_properties_remove_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "ref_properties_remove_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
        let target_id = read_response
            .instances
            .values()
            .find(|instance| instance.name == "target")
            .unwrap()
            .id;
        let pointer_id = read_response
            .instances
            .values()
            .find(|instance| instance.name == "pointer")
            .unwrap()
            .id;
        let initial_pointer = &read_response.instances[&pointer_id];
        let initial_root = &read_response.instances[&root_id];

        fs::remove_file(session.path().join("src/target.model.json")).unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, read_response.message_cursor)
            .unwrap();
        let SocketPacketBody::Messages(packet) = socket_packet.body;
        assert!(packet.messages.iter().any(|message| {
            message.removed.contains(&target_id)
                && message.updated.iter().any(|update| update.id == pointer_id)
        }));
        for message in packet.messages {
            assert!(message.added.is_empty());
            assert!(message.removed.iter().all(|id| *id == target_id));
            // Some backends also report a directory Write. Re-snapshotting
            // the directory must not produce anything beyond the cleared ref.
            for update in message.updated {
                assert_eq!(update.id, pointer_id);
                assert!(update.changed_name.is_none());
                assert!(update.changed_class_name.is_none());
                assert!(update.changed_metadata.is_none());
                assert_eq!(update.changed_properties.len(), 1);
                // Sent as an explicit null Ref because the plugin cannot see
                // a msgpack nil stored in a Luau table.
                assert_eq!(
                    update.changed_properties.get(&rbx_dom_weak::ustr("Value")),
                    Some(&Some(rbx_dom_weak::types::Variant::Ref(
                        rbx_dom_weak::types::Ref::none()
                    )))
                );
            }
        }

        let after_removal = session.get_api_read(root_id).unwrap();
        assert_eq!(after_removal.instances.len(), 2);
        assert!(!after_removal.instances.contains_key(&target_id));
        assert!(after_removal.instances.contains_key(&pointer_id));
        // Removing the target clears the pointer's resolved ref in the same
        // patch, so it never dangles regardless of which events the platform's
        // file watcher reports.
        assert_yaml_snapshot!(
            "ref_properties_remove_pointer_after_removal",
            redactions.redacted_yaml(&after_removal.instances[&pointer_id])
        );

        // A subsequent edit proves processing is still alive and that
        // re-snapshotting the pointer agrees with the cleared ref.
        fs::write(
            session.path().join("src/pointer.model.json"),
            r#"{
            "className": "ObjectValue",
            "attributes": { "Rojo_Target_Value": "test pointer", "AfterRemoval": true }
        }"#,
        )
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let final_read = loop {
            let read = session.get_api_read(root_id).unwrap();
            let pointer = &read.instances[&pointer_id];
            let updated = match pointer.properties.get(&rbx_dom_weak::ustr("Attributes")) {
                Some(value) => {
                    matches!(value.as_ref(), rbx_dom_weak::types::Variant::Attributes(attributes)
                    if attributes.get("AfterRemoval") == Some(&rbx_dom_weak::types::Variant::Bool(true)))
                }
                None => false,
            };
            if updated {
                break read;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "valid edit after target removal did not sync"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(final_read.instances.len(), 2);
        let root = &final_read.instances[&root_id];
        assert_eq!(root.name, initial_root.name);
        assert_eq!(root.class_name, initial_root.class_name);
        assert_eq!(root.parent, initial_root.parent);
        assert_eq!(root.properties, initial_root.properties);
        assert_eq!(root.children.as_ref(), &[pointer_id]);
        let pointer = &final_read.instances[&pointer_id];
        assert_eq!(pointer.name, initial_pointer.name);
        assert_eq!(pointer.class_name, initial_pointer.class_name);
        assert_eq!(pointer.parent, initial_pointer.parent);
        assert!(pointer.children.is_empty());
        assert!(!pointer
            .properties
            .contains_key(&rbx_dom_weak::ustr("Value")));
        assert_eq!(pointer.properties.len(), 1);
    });
}

/// A target that is removed and later re-created, as a branch switch or a
/// package reinstall can do in separate watcher events, must be pointed at
/// again without the pointer's own file changing.
#[test]
fn ref_properties_restore_after_target_returns() {
    run_serve_test("ref_properties_remove", |session, _redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;
        let value = rbx_dom_weak::ustr("Value");

        let read_response = session.get_api_read(root_id).unwrap();
        let pointer_id = read_response
            .instances
            .values()
            .find(|instance| instance.name == "pointer")
            .unwrap()
            .id;

        let target_path = session.path().join("src/target.model.json");
        let target_contents = fs::read_to_string(&target_path).unwrap();

        let wait_until = |what: &str, done: &dyn Fn(&ReadResponse<'_>) -> bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let read = session.get_api_read(root_id).unwrap();
                if done(&read) {
                    return;
                }
                assert!(std::time::Instant::now() < deadline, "timed out: {what}");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };

        fs::remove_file(&target_path).unwrap();
        wait_until("pointer cleared after target removal", &|read| {
            !read.instances[&pointer_id].properties.contains_key(&value)
        });

        fs::write(&target_path, target_contents).unwrap();
        wait_until("pointer restored after target returned", &|read| {
            let Some(target) = read
                .instances
                .values()
                .find(|instance| instance.name == "target")
            else {
                return false;
            };
            read.instances[&pointer_id]
                .properties
                .get(&value)
                .map(|value| value.as_ref())
                == Some(&rbx_dom_weak::types::Variant::Ref(target.id))
        });
    });
}

/// When Ref properties were first implemented, a mistake was made that resulted
/// in Ref properties defined via attributes not being included in patch
/// computation, which resulted in subsequent patches setting those properties
/// to `nil`.
///
/// See: https://github.com/rojo-rbx/rojo/issues/929
#[test]
fn ref_properties_patch_update() {
    // Reusing ref_properties is fun and easy.
    run_serve_test("ref_properties", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!(
            "ref_properties_patch_update_info",
            redactions.redacted_yaml(info)
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "ref_properties_patch_update_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        let target_path = session.path().join("ModelTarget.model.json");

        // Inserting scale just to force the change processor to run
        fs::write(
            target_path,
            r#"{
            "id": "model target",
            "className": "Folder",
            "children": [
                {
                    "name": "ModelPointer",
                    "className": "Model",
                    "attributes": {
                        "Rojo_Target_PrimaryPart": "model target"
                    },
                    "properties": {
                        "Scale": 1
                    }
                }
            ]
        }"#,
        )
        .unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();
        assert_yaml_snapshot!(
            "ref_properties_patch_update_subscribe",
            socket_packet.intern_and_redact(&mut redactions, ())
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "ref_properties_patch_update_all-2",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn model_pivot_migration() {
    run_serve_test("pivot_migration", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("pivot_migration_info", redactions.redacted_yaml(info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "pivot_migration_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        let project_path = session.path().join("default.project.json");

        fs::write(
            project_path,
            r#"{
            "name": "pivot_migration",
            "tree": {
                "$className": "DataModel",
                "Workspace": {
                    "Model": {
                        "$className": "Model"
                    },
                    "Tool": {
                        "$path": "Tool.model.json"
                    },
                    "Actor": {
                        "$className": "Actor"
                    }
                }
            }
        }"#,
        )
        .unwrap();

        let socket_packet = session
            .get_api_socket_packet(SocketPacketType::Messages, 0)
            .unwrap();
        assert_yaml_snapshot!(
            "model_pivot_migration_all",
            socket_packet.intern_and_redact(&mut redactions, ())
        );

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "model_pivot_migration_all-2",
            read_response.intern_and_redact(&mut redactions, root_id)
        );
    });
}

#[test]
fn meshpart_with_id() {
    run_serve_test("meshpart_with_id", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("meshpart_with_id_info", redactions.redacted_yaml(&info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "meshpart_with_id_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        // This is a bit awkward, but it's fine.
        let (meshpart, _) = read_response
            .instances
            .iter()
            .find(|(_, inst)| inst.class_name == "MeshPart")
            .unwrap();
        let (objectvalue, _) = read_response
            .instances
            .iter()
            .find(|(_, inst)| inst.class_name == "ObjectValue")
            .unwrap();

        let body = session
            .post_api_serialize(&[*meshpart, *objectvalue], info.session_id)
            .unwrap()
            .bytes()
            .unwrap();
        let serialize_response: SerializeResponse =
            deserialize_msgpack(&body).expect("Server returned malformed response");

        // We don't assert a snapshot on the SerializeResponse because the model includes the
        // Refs from the DOM as names, which means it will obviously be different every time
        // this code runs. Still, we ensure that the SessionId is right at least.
        assert_eq!(serialize_response.session_id, info.session_id);

        let model = serialize_to_xml_model(&serialize_response, &redactions);
        assert_snapshot!("meshpart_with_id_serialize_model", model);
    });
}

#[test]
fn serialize_skips_missing_ids() {
    run_serve_test("empty", |session, _| {
        let info = session.get_api_rojo().unwrap();
        let missing_id = Ref::new();

        // One stale ID in a fallback batch must not fail the instances that
        // still exist.
        let response = session
            .post_api_serialize(&[missing_id, info.root_instance_id], info.session_id)
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.bytes().unwrap();
        let serialize_response: SerializeResponse =
            deserialize_msgpack(&body).expect("Server returned malformed response");
        assert_eq!(serialize_response.missing_ids, vec![missing_id]);

        let model = rbx_binary::from_reader(serialize_response.model_contents.as_slice()).unwrap();
        let folder = model.get_by_ref(model.root().children()[0]).unwrap();
        let names: Vec<_> = folder
            .children()
            .iter()
            .map(|child| model.get_by_ref(*child).unwrap().name.clone())
            .collect();
        assert_eq!(names, vec![info.root_instance_id.to_string()]);
    });
}

/// Opens a subscription and returns the close frame the server answers with.
fn socket_close_frame(
    port: usize,
    path_and_query: &str,
) -> Option<hyper_tungstenite::tungstenite::protocol::CloseFrame<'static>> {
    use hyper_tungstenite::tungstenite::{connect, stream::MaybeTlsStream, Message};

    let (mut socket, _response) =
        connect(format!("ws://localhost:{port}{path_and_query}")).unwrap();
    if let MaybeTlsStream::Plain(stream) = socket.get_ref() {
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
    }

    loop {
        match socket.read().expect("the server should close the socket") {
            Message::Close(frame) => return frame.map(|frame| frame.into_owned()),
            Message::Binary(_) => panic!("a stale subscription must not receive messages"),
            _ => continue,
        }
    }
}

#[test]
fn subscription_from_a_previous_server_run_is_closed() {
    use hyper_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

    run_serve_test("empty", |session, _| {
        let info = session.get_api_rojo().unwrap();
        let cursor = session
            .get_api_read(info.root_instance_id)
            .unwrap()
            .message_cursor;

        // A cursor this server never issued means the client read from an
        // earlier run. Waiting for the queue to reach it would drop the first
        // changes on this run, so the client is told to resync instead.
        let frame = socket_close_frame(session.port(), &format!("/api/socket/{}", cursor + 5))
            .expect("the close frame should explain why");
        assert_eq!(frame.code, CloseCode::Restart);
        assert!(frame.reason.contains("restarted"), "{}", frame.reason);

        // A session ID naming another run is rejected even if the cursor
        // would be valid here.
        let other_session = uuid::Uuid::new_v4();
        let frame = socket_close_frame(
            session.port(),
            &format!("/api/socket/{cursor}?sessionId={other_session}"),
        )
        .expect("the close frame should explain why");
        assert_eq!(frame.code, CloseCode::Restart);
    });
}

#[test]
fn subscription_with_matching_session_id_receives_messages() {
    run_serve_test("empty", |session, _| {
        let info = session.get_api_rojo().unwrap();

        // Uppercase and percent-encoded (as Roblox's UrlEncode escapes "-") to
        // show the comparison doesn't depend on how the client formats the ID.
        let session_id = info
            .session_id
            .to_string()
            .to_uppercase()
            .replace('-', "%2D");
        let url = format!(
            "ws://localhost:{}/api/socket/0?sessionId={session_id}",
            session.port()
        );
        let (mut socket, _response) = hyper_tungstenite::tungstenite::connect(url).unwrap();
        if let hyper_tungstenite::tungstenite::stream::MaybeTlsStream::Plain(stream) =
            socket.get_ref()
        {
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
        }

        fs::write(
            session.path().join("default.project.json"),
            r#"{ "name": "empty", "tree": { "$className": "Folder", "Child": { "$className": "Folder" } } }"#,
        )
        .unwrap();

        loop {
            match socket.read().unwrap() {
                hyper_tungstenite::tungstenite::Message::Binary(_) => break,
                hyper_tungstenite::tungstenite::Message::Close(frame) => {
                    panic!("subscription with the right session ID was closed: {frame:?}")
                }
                _ => continue,
            }
        }
    });
}

#[test]
fn forced_parent() {
    run_serve_test("forced_parent", |session, mut redactions| {
        let info = session.get_api_rojo().unwrap();
        let root_id = info.root_instance_id;

        assert_yaml_snapshot!("forced_parent_info", redactions.redacted_yaml(&info));

        let read_response = session.get_api_read(root_id).unwrap();
        assert_yaml_snapshot!(
            "forced_parent_all",
            read_response.intern_and_redact(&mut redactions, root_id)
        );

        let body = session
            .post_api_serialize(&[root_id], info.session_id)
            .unwrap()
            .bytes()
            .unwrap();
        let serialize_response: SerializeResponse =
            deserialize_msgpack(&body).expect("Server returned malformed response");

        assert_eq!(serialize_response.session_id, info.session_id);

        let model = serialize_to_xml_model(&serialize_response, &redactions);
        assert_snapshot!("forced_parent_serialize_model", model);
    });
}

#[test]
fn changed_metadata() {
    run_serve_test("changed_metadata", |session, mut redactions| {
        let _ = session.get_api_rojo().unwrap();

        let project = |anchored: bool, ignore_unknown: Option<bool>, path: &str| {
            let ignore_unknown = match ignore_unknown {
                Some(value) => format!("\"$ignoreUnknownInstances\": {value},"),
                None => String::new(),
            };

            format!(
                r#"{{
  "name": "changed_metadata",
  "tree": {{
    "$className": "DataModel",
    "Workspace": {{
      "$className": "Workspace",
      "Baseplate": {{
        "$className": "Part",
        {ignore_unknown}
        "$properties": {{
          "Anchored": {anchored}
        }}
      }},
      "Thing": {{
        "$path": "{path}"
      }}
    }}
  }}
}}
"#
            )
        };

        let write_project = |contents: String| {
            fs::write(session.path().join("default.project.json"), contents).unwrap();
        };

        // A single write can reach the server as more than one watcher event,
        // and each event is its own patch. Rather than assuming one message per
        // write, collect messages until the expected change has arrived and
        // snapshot only the messages that carry changes.
        let mut cursor = 0;
        let mut wait_for = |done: &dyn Fn(&[SubscribeMessage<'static>]) -> bool| {
            // Packets that never satisfy `done` would otherwise keep this
            // waiting forever instead of failing the test.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            let mut received = Vec::new();
            loop {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for messages, received: {received:#?}"
                );
                let packet = session
                    .get_api_socket_packet(SocketPacketType::Messages, cursor)
                    .unwrap();
                let SocketPacketBody::Messages(packet) = packet.body;
                cursor = packet.message_cursor;
                received.extend(packet.messages);
                if done(&received) {
                    return received;
                }
            }
        };
        let any_update = |messages: &[SubscribeMessage<'static>]| {
            messages.iter().any(|message| !message.updated.is_empty())
        };

        write_project(project(false, None, "a"));
        let messages = wait_for(&any_update);
        assert_yaml_snapshot!(
            "changed_metadata_property_only",
            intern_messages(&mut redactions, &messages)
        );

        write_project(project(false, Some(false), "a"));
        let messages = wait_for(&any_update);
        assert_yaml_snapshot!(
            "changed_metadata_metadata_only",
            intern_messages(&mut redactions, &messages)
        );

        write_project(project(true, Some(true), "a"));
        let messages = wait_for(&any_update);
        assert_yaml_snapshot!(
            "changed_metadata_property_and_metadata",
            intern_messages(&mut redactions, &messages)
        );

        // Only internal metadata changes here, which the plugin never sees.
        // The packet still arrives so the cursor advances, but it carries no
        // messages that would make the plugin open an undo recording.
        write_project(project(true, Some(true), "b"));
        let messages = wait_for(&|_| true);
        assert!(messages.is_empty(), "{messages:#?}");

        // A later real change proves the cursor kept advancing.
        write_project(project(false, Some(true), "b"));
        wait_for(&any_update);
    });
}

fn intern_messages(
    redactions: &mut rojo_insta_ext::RedactionMap,
    messages: &[SubscribeMessage<'static>],
) -> serde_yaml::Value {
    for message in messages {
        for update in &message.updated {
            redactions.intern(update.id);
        }
    }
    redactions.redacted_yaml(messages)
}
