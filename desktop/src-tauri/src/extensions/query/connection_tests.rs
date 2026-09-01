use std::pin::Pin;
use std::task::{Context, Poll};

use super::*;

type DataRx = mpsc::Receiver<OutboundCommand>;

fn dummy(generation: u64, alive_now: bool) -> (Arc<Connection>, DataRx, DataRx) {
    let (outbound, data_rx) = mpsc::channel(1);
    let (control, control_rx) = mpsc::channel(1);
    let (cancel, _cancel_rx) = watch::channel(false);
    let key = ("ws://relay.test".to_string(), "identity".to_string());
    let alive = Arc::new(AtomicBool::new(alive_now));
    (
        Arc::new(Connection {
            outbound,
            control,
            cancel,
            witness: IdentityWitness::for_test(generation, "identity"),
            instance: ConnectionInstance { key, generation },
            alive,
        }),
        data_rx,
        control_rx,
    )
}

#[test]
fn reader_death_invalidates_reuse_while_the_writer_channel_is_open() {
    let (connection, _data, _control) = dummy(1, false);
    assert!(
        !connection.outbound.is_closed(),
        "positive precondition: a writer sender remains open"
    );
    assert!(
        !reusable(&connection),
        "reader liveness, not Sender::is_closed, owns reuse"
    );
}

#[tokio::test]
async fn g1_cleanup_cannot_remove_the_installed_g2_manager_entry() {
    let manager = ConnectionManager::default();
    let (g2, _data, _control) = dummy(2, true);
    manager
        .conns
        .lock()
        .await
        .insert(g2.instance.key.clone(), Arc::clone(&g2));

    let g1 = ConnectionInstance {
        key: g2.instance.key.clone(),
        generation: 1,
    };
    manager.remove_if_current(&g1).await;
    assert_eq!(manager.conns.lock().await.len(), 1);
    manager.remove_if_current(&g2.instance).await;
    assert!(manager.conns.lock().await.is_empty());
}

#[tokio::test]
async fn a_req_burst_is_one_queue_item_or_nothing() {
    let (connection, mut data, _control) = dummy(1, true);
    connection
        .send_reqs(vec!["REQ-a".into(), "REQ-b".into()])
        .expect("first burst");
    assert!(
        connection
            .send_reqs(vec!["REQ-c".into(), "REQ-d".into()])
            .is_err(),
        "one-slot queue refuses the complete second burst"
    );
    let OutboundCommand::Burst(first) = data.recv().await.expect("queued command");
    assert_eq!(first, ["REQ-a", "REQ-b"]);
    assert!(
        data.try_recv().is_err(),
        "no visible prefix of the refused burst"
    );
}

#[tokio::test]
async fn a_full_data_queue_cannot_starve_the_priority_close_queue() {
    let (connection, _data, mut control) = dummy(1, true);
    connection.send_reqs(vec!["REQ".into()]).expect("fill data");
    connection
        .send_closes(vec!["CLOSE-a".into(), "CLOSE-b".into()])
        .expect("control has independent capacity");
    let OutboundCommand::Burst(frames) = control.recv().await.expect("control burst");
    assert_eq!(frames, ["CLOSE-a", "CLOSE-b"]);
}

#[test]
fn both_tungstenite_decoder_ceilings_are_explicit() {
    let config = websocket_config();
    assert_eq!(config.max_message_size, Some(MAX_WS_MESSAGE_BYTES));
    assert_eq!(config.max_frame_size, Some(MAX_WS_FRAME_BYTES));
}

struct FailAfterOne {
    sent: usize,
}

impl futures_util::Sink<Message> for FailAfterOne {
    type Error = tokio_tungstenite::tungstenite::Error;

    fn poll_ready(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(mut self: Pin<&mut Self>, _: Message) -> Result<(), Self::Error> {
        if self.sent == 1 {
            return Err(Self::Error::ConnectionClosed);
        }
        self.sent += 1;
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn oversized_local_relay_input_is_rejected_before_text_reaches_the_pump() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut websocket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("upgrade");
        websocket
            .send(Message::Text("x".repeat(MAX_WS_MESSAGE_BYTES + 1).into()))
            .await
            .expect("send oversized");
    });
    let url = format!("ws://{address}");
    let (mut client, _) =
        tokio_tungstenite::connect_async_with_config(url, Some(websocket_config()), false)
            .await
            .expect("connect");
    let received = client.next().await.expect("decoder outcome");
    assert!(received.is_err(), "no oversized Message::Text reaches pump");
    server.await.expect("server");
}

#[tokio::test]
async fn exhausted_quota_prevents_the_network_closure_from_running() {
    let quota = super::super::subscription::SubscriptionQuota::new();
    let mut held = Vec::new();
    for _ in 0..(super::super::subscription::MAX_BRANCHES_PER_EXTENSION
        / super::super::subscription::MAX_BRANCHES_PER_SUB)
    {
        held.push(
            quota
                .reserve(
                    "identity",
                    "extension",
                    super::super::subscription::MAX_BRANCHES_PER_SUB,
                )
                .expect("fill quota"),
        );
    }
    let network_ran = std::cell::Cell::new(false);
    let result = reserve_before_network(&quota, "identity", "extension", 1, || async {
        network_ran.set(true);
        Ok(())
    })
    .await;
    assert_eq!(result.err(), Some(OpenFailure::QuotaExhausted));
    assert!(!network_ran.get(), "zero TCP/AUTH/network side effects");
    drop(held);
}

#[tokio::test]
async fn partial_socket_write_is_terminal_not_reported_as_an_atomic_success() {
    let mut sink = FailAfterOne { sent: 0 };
    assert!(send_burst(&mut sink, vec!["first".into(), "second".into()])
        .await
        .is_err());
    assert_eq!(sink.sent, 1, "the partial write is observed exactly");
}
