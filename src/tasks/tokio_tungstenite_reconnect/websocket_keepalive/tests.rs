use super::{Error, Message, WebSocketKeepAlive};
use futures::{future::join, SinkExt, StreamExt};
use std::time::Duration;
use test_log::test;
use tokio::net::TcpListener;
use tokio::time::sleep;
use tokio_tungstenite::{accept_async, connect_async};

#[test(tokio::test)]
async fn client_sends_messages() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let accept_connection = async {
        let (stream, _) = server.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        let message = ws.next().await.unwrap().unwrap();
        assert_eq!(message, Message::Text("hello".to_string()));

        let message = ws.next().await.unwrap().unwrap();
        assert_eq!(message, Message::Text("hello".to_string()));
    };
    let connect = async {
        let (ws, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        let ws = WebSocketKeepAlive::new(ws);

        // We can also send a message from another task
        let ws_clone = ws.clone();
        let task = tokio::spawn(async move {
            ws_clone
                .send(Message::Text("hello".to_string()))
                .await
                .unwrap();
        });

        ws.send(Message::Text("hello".to_string())).await.unwrap();
        task.await.unwrap();
    };
    join(accept_connection, connect).await;
}

#[test(tokio::test)]
async fn server_sends_messages() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let accept_connection = async {
        let (stream, _) = server.accept().await.unwrap();
        let ws = accept_async(stream).await.unwrap();
        let ws = WebSocketKeepAlive::new(ws);

        // We can also send a message from another task
        let ws_clone = ws.clone();
        let task = tokio::spawn(async move {
            ws_clone
                .send(Message::Text("hello".to_string()))
                .await
                .unwrap();
        });

        ws.send(Message::Text("hello".to_string())).await.unwrap();
        task.await.unwrap();
    };
    let connect = async {
        let (mut ws, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        let message = ws.next().await.unwrap().unwrap();
        assert_eq!(message, Message::Text("hello".to_string()));
        let message = ws.next().await.unwrap().unwrap();
        assert_eq!(message, Message::Text("hello".to_string()));
    };
    join(accept_connection, connect).await;
}

#[test(tokio::test)]
async fn client_receives_messages() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let accept_connection = async {
        let (stream, _) = server.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        ws.send(Message::Text("request 1".to_string()))
            .await
            .unwrap();
        ws.send(Message::Text("request 2".to_string()))
            .await
            .unwrap();
    };
    let connect = async {
        let (ws, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        let ws = WebSocketKeepAlive::new(ws);
        assert_eq!(
            ws.recv().await.unwrap().unwrap(),
            Message::Text("request 1".to_string())
        );
        assert_eq!(
            ws.recv().await.unwrap().unwrap(),
            Message::Text("request 2".to_string())
        );
    };
    join(accept_connection, connect).await;
}

#[test(tokio::test)]
async fn server_receives_messages() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let accept_connection = async {
        let (stream, _) = server.accept().await.unwrap();
        let ws = accept_async(stream).await.unwrap();
        let ws = WebSocketKeepAlive::new(ws);
        let results = vec![
            ws.recv().await.unwrap().unwrap(),
            ws.recv().await.unwrap().unwrap(),
            ws.recv().await.unwrap().unwrap(),
        ];
        assert_eq!(
            results,
            vec![
                Message::Text("request 1".to_string()),
                Message::Text("request 2".to_string()),
                Message::Text("request 3".to_string()),
            ]
        );
    };
    let connect = async {
        let (mut ws, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        ws.send(Message::Text("request 1".to_string()))
            .await
            .unwrap();
        // sleep(Duration::from_millis(1)).await;
        ws.send(Message::Text("request 2".to_string()))
            .await
            .unwrap();
        ws.send(Message::Text("request 3".to_string()))
            .await
            .unwrap();
        sleep(Duration::from_millis(10)).await;
        let _ws = ws;
    };
    join(accept_connection, connect).await;
}

#[test(tokio::test)]
async fn client_close() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let accept_connection = async {
        let (stream, _) = server.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        let result = ws.next().await;
        if let Some(Ok(Message::Close(None))) = result {
        } else {
            panic!("wrong response {:?}", result);
        }
    };
    let connect = async {
        let (ws, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        let ws = WebSocketKeepAlive::new(ws);

        // Clone the websocket and pass it to another task to ensure that it also exits
        let ws_clone = ws.clone();
        let handle = tokio::spawn(async move {
            assert!(ws_clone.recv().await.is_none());
        });

        assert!(ws.is_connected());
        ws.close().await;
        assert!(!ws.is_connected());

        // Clone's read loop should exit
        handle.await.unwrap();
    };

    join(accept_connection, connect).await;
}

#[test(tokio::test)]
async fn server_close() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let accept_connection = async {
        let (stream, _) = server.accept().await.unwrap();
        let ws = accept_async(stream).await.unwrap();
        let ws = WebSocketKeepAlive::new(ws);

        // Clone the websocket and pass it to another task to ensure that it also exits
        let ws_clone = ws.clone();
        let handle = tokio::spawn(async move {
            assert!(ws_clone.recv().await.is_none());
        });

        assert!(ws.is_connected());
        ws.close().await;
        assert!(!ws.is_connected());

        // Clone's read loop should exit
        handle.await.unwrap();
    };
    let connect = async {
        let (mut ws, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        let result = ws.next().await;
        if let Some(Ok(Message::Close(None))) = result {
        } else {
            panic!("wrong response {:?}", result);
        }
    };

    join(accept_connection, connect).await;
}

#[test(tokio::test)]
async fn send_after_close_returns_error() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let accept_connection = async {
        let (stream, _) = server.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        ws.next().await;
    };
    let connect = async {
        let (ws, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        let ws = WebSocketKeepAlive::new(ws);
        let ws_clone = ws.clone();

        ws.close().await;

        match ws_clone.send(Message::Text("hello".to_string())).await {
            Ok(_) => panic!("sending after close should error"),
            Err(Error::ConnectionClosed) => {}
            Err(err) => panic!(
                "sending after close should cause AlreadyClosed error. got: {:?}",
                err
            ),
        }
    };

    join(accept_connection, connect).await;
}

#[test(tokio::test)]
async fn client_closes_connection_when_all_instances_dropped() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let accept_connection = async {
        let (stream, _) = server.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        let result = ws.next().await;
        if let Some(Ok(Message::Close(None))) = result {
        } else {
            panic!("wrong response {:?}", result);
        }
    };
    let connect = async {
        let (ws, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        let ws = WebSocketKeepAlive::new(ws);

        drop(ws);
    };
    join(accept_connection, connect).await;
}

#[test(tokio::test)]
async fn server_closes_connection_when_all_instances_dropped() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let accept_connection = async {
        let (stream, _) = server.accept().await.unwrap();
        let ws = accept_async(stream).await.unwrap();
        let ws = WebSocketKeepAlive::new(ws);

        drop(ws);
    };
    let connect = async {
        let (mut ws, _) = connect_async(format!("ws://{addr}")).await.unwrap();

        let result = ws.next().await;
        if let Some(Ok(Message::Close(None))) = result {
        } else {
            panic!("wrong response {:?}", result);
        }
    };
    join(accept_connection, connect).await;
}

#[test(tokio::test)]
async fn client_sends_pings() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let accept_connection = async {
        let (stream, _) = server.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        let message = ws.next().await.unwrap().unwrap();
        if let Message::Ping(_) = message {
        } else {
            panic!("wrong message type {:?}", message);
        }
    };
    let connect = async {
        let (ws, _) = connect_async(format!("ws://{addr}")).await.unwrap();
        let ws = WebSocketKeepAlive::new_with_ping_timeout(ws, Duration::from_millis(100));
        sleep(Duration::from_millis(150)).await;
        let _ws = ws;
    };
    join(accept_connection, connect).await;
}

#[test(tokio::test)]
async fn server_sends_pings() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let accept_connection = async {
        let (stream, _) = server.accept().await.unwrap();
        let ws = accept_async(stream).await.unwrap();
        let ws = WebSocketKeepAlive::new_with_ping_timeout(ws, Duration::from_millis(100));
        sleep(Duration::from_millis(150)).await;
        let _ws = ws;
    };
    let connect = async {
        let (mut ws, _) = connect_async(format!("ws://{addr}")).await.unwrap();

        let message = ws.next().await.unwrap().unwrap();
        if let Message::Ping(_) = message {
        } else {
            panic!("wrong message type {:?}", message);
        }
    };
    join(accept_connection, connect).await;
}
