use super::*;

#[tokio::test]
async fn root_ownership_is_refused_before_delivery_for_every_inbound_path() {
    for case in ["result", "nested", "exception", "late", "unknown", "event"] {
        let (channel, events, mut browser) = fixture().await;
        let call = channel.submit(Command::Status, deadline()).unwrap();
        let receipt = call.receipt.clone();
        let sent = request(&mut browser).await;
        let root = json!({"type":"node","sharedId":"node","handle":"owned"});
        let value = match case {
            "exception" => {
                json!({"type":"success","id":sent["id"],"result":{"type":"exception","exceptionDetails":{"exception":root}}})
            }
            "nested" => {
                json!({"type":"success","id":sent["id"],"result":{"result":{"type":"array","value":[root]}}})
            }
            "event" => json!({"type":"event","method":"log.entryAdded","params":{"args":[root]}}),
            "unknown" => json!({"type":"success","id":99999,"result":{"result":root}}),
            _ => json!({"type":"success","id":sent["id"],"result":{"result":root}}),
        };
        let call = if case == "late" {
            drop(call);
            None
        } else {
            Some(call)
        };
        send(&mut browser, value).await;
        assert_eq!(stopped(&channel).await, Fault::NativeHandle);
        assert_eq!(receipt.status(), Settlement::Unknown(Fault::NativeHandle));
        if let Some(call) = call {
            assert!(matches!(
                call.wait().await,
                Err(Error::Transport(Fault::NativeHandle))
            ));
        }
        finish(&channel, events).await;
    }
}

#[tokio::test]
async fn shared_node_identities_do_not_acquire_root_ownership() {
    let (channel, events, mut browser) = fixture().await;
    let call = channel.submit(Command::Status, deadline()).unwrap();
    let sent = request(&mut browser).await;
    send(&mut browser,json!({"type":"success","id":sent["id"],"result":{"nodes":[{"type":"node","sharedId":"document-node"}]}})).await;
    assert_eq!(
        call.wait().await.unwrap().value["nodes"][0]["sharedId"],
        "document-node"
    );
    assert!(channel.connected());
    assert_eq!(
        channel.snapshot()["handle_inventory"]["root_handles_returned"],
        0
    );
    assert_eq!(
        channel.snapshot()["handle_inventory"]["shared_ids_returned"],
        1
    );
    finish(&channel, events).await;
}
