use super::{connect_async, Error, Message, ReconnectingWebSocket};
use futures::{future::join, select, FutureExt, SinkExt, StreamExt};
use http::header::HeaderName;
use http::{HeaderValue, Request, Response};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use test_env_log::test;
use tokio::net::TcpListener;
use tokio::time::sleep;
use tokio_tungstenite::{accept_async, accept_hdr_async};

#[test(tokio::test)]
async fn connects() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let accept_connection = async {
        let (stream, _) = server.accept().await.unwrap();
        accept_async(stream).await.unwrap();
    };
    let connect = async {
        let ws = connect_async(format!("ws://{}", addr)).await.unwrap();
        assert!(ws.is_connected());
    };
    join(accept_connection, connect).await;
}

#[test(tokio::test)]
async fn connect_response_handler() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let accept_connection = async {
        let (stream, _) = server.accept().await.unwrap();
        accept_hdr_async(
            stream,
            |_request: &Request<()>, mut response: Response<()>| {
                response.headers_mut().insert(
                    HeaderName::from_static("my-header"),
                    HeaderValue::from_static("hello"),
                );
                Ok(response)
            },
        )
        .await
        .unwrap();
    };
    let handler_called = Arc::new(AtomicBool::from(false));
    let handler_called_clone = handler_called.clone();
    let connect = async {
        let ws = ReconnectingWebSocket::builder(format!("ws://{}", addr))
            .unwrap()
            .connect_response_handler(move |response| {
                assert_eq!(
                    response.headers().get("my-header"),
                    Some(&HeaderValue::from_static("hello"))
                );
                handler_called_clone.store(true, Ordering::SeqCst);
            })
            .build();
        ws.connect().await.unwrap();
    };
    join(accept_connection, connect).await;
    assert_eq!(handler_called.load(Ordering::SeqCst), true);
}

#[test(tokio::test)]
async fn sends_custom_http_headers() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let accept_connection = async {
        let (stream, _) = server.accept().await.unwrap();
        accept_hdr_async(stream, |request: &Request<()>, response: Response<()>| {
            assert_eq!(
                request.headers().get("my-header"),
                Some(&HeaderValue::from_static("hello"))
            );
            Ok(response)
        })
        .await
        .unwrap();
    };
    let connect = async {
        let request = Request::builder()
            .method("GET")
            .uri(format!("ws://{}", addr))
            .header("my-header", "hello")
            .body(())
            .unwrap();
        connect_async(request).await.unwrap();
    };
    join(accept_connection, connect).await;
}

#[test(tokio::test)]
async fn sends_messages() {
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
        let ws = connect_async(format!("ws://{}", addr)).await.unwrap();

        // We can also send from other tasks
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
async fn receives_messages() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let accept_connection = async {
        let (stream, _) = server.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        ws.send(Message::Text("hello".to_string())).await.unwrap();
    };
    let connect = async {
        let ws = connect_async(format!("ws://{}", addr)).await.unwrap();
        let message = ws.recv().await.unwrap().unwrap();
        assert_eq!(message, Message::Text("hello".to_string()));
    };
    join(accept_connection, connect).await;
}

#[test(tokio::test)]
async fn close() {
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
        let ws = connect_async(format!("ws://{}", addr)).await.unwrap();

        // Clone the websocket and pass it to another task to ensure that it also exits
        let ws_clone = ws.clone();
        let handle = tokio::spawn(async move {
            assert!(ws_clone.recv().await.is_none());
        });

        ws.close().await;
        assert!(!ws.is_connected());

        // Clone's read loop should exit
        handle.await.unwrap();
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
        let ws = connect_async(format!("ws://{}", addr)).await.unwrap();
        let ws_clone = ws.clone();

        ws.close().await;

        match ws_clone.send(Message::Text("hello".to_string())).await {
            Ok(_) => panic!("sending after close should error"),
            Err(Error::AlreadyClosed) => {}
            Err(err) => panic!(
                "sending after close should cause AlreadyClosed error. got: {:?}",
                err
            ),
        }
    };

    join(accept_connection, connect).await;
}

#[test(tokio::test)]
async fn closes_connection_when_all_instances_dropped() {
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
        let ws = connect_async(format!("ws://{}", addr)).await.unwrap();
        drop(ws);
    };
    join(accept_connection, connect).await;
}

#[test(tokio::test)]
async fn reconnects_if_initial_connection_fails() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let accept_connection = async {
        for _i in 0..2 {
            let (stream, _) = server.accept().await.unwrap();
            accept_hdr_async(stream, |_req: &Request<()>, _res: Response<()>| {
                Ok(Response::builder().status(500).body(()).unwrap())
            })
            .await
            .unwrap();
        }

        let (stream, _) = server.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        let result = ws.next().await;
        if let Some(Ok(Message::Text(message))) = result {
            assert_eq!(message, "hello");
        } else {
            panic!("wrong response {:?}", result);
        }
    };
    let connect = async {
        let ws = connect_async(format!("ws://{}", addr)).await.unwrap();
        ws.send(Message::Text("hello".to_string())).await.unwrap();
    };
    join(accept_connection, connect).await;
}

#[test(tokio::test)]
async fn stops_reconnecting_after_the_specified_number_of_times() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let accept_connection = async {
        for _i in 0..3 {
            let (stream, _) = server.accept().await.unwrap();
            accept_hdr_async(stream, |_req: &Request<()>, _res: Response<()>| {
                Ok(Response::builder().status(500).body(()).unwrap())
            })
            .await
            .unwrap();
        }

        let (stream, _) = server.accept().await.unwrap();
        accept_async(stream).await.unwrap();
        panic!("shouldn't connect again");
    };
    let connect = async {
        let ws = ReconnectingWebSocket::builder(format!("ws://{}", addr))
            .unwrap()
            .max_retries(2)
            .build();

        match ws.connect().await {
            Err(Error::Http(response)) => assert_eq!(response.status(), 500),
            result => panic!("wrong result: {:?}", result),
        };
    };
    select! {
        _ = connect.fuse() => {},
        _ = accept_connection.fuse() => {}
    }
}

#[test(tokio::test)]
async fn does_not_reconnect_on_client_http_errors() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let accept_connection = async {
        let (stream, _) = server.accept().await.unwrap();
        accept_hdr_async(stream, |_req: &Request<()>, _res: Response<()>| {
            Ok(Response::builder().status(404).body(()).unwrap())
        })
        .await
        .unwrap();

        let (stream, _) = server.accept().await.unwrap();
        accept_async(stream).await.unwrap();
        panic!("should not reconnect");
    };
    let connect = async {
        let result = connect_async(format!("ws://{}", addr)).await;
        if let Err(Error::Http(response)) = result {
            assert_eq!(response.status(), 404);
        } else {
            panic!("wrong error");
        }
    };
    select! {
        _ = accept_connection.fuse() => {},
        _ = connect.fuse() => {}
    }
}

#[test(tokio::test)]
async fn reconnects_if_connection_drops() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let (mut server_ws, client_ws) = join(
        async {
            let (stream, _) = server.accept().await.unwrap();
            accept_async(stream).await.unwrap()
        },
        async { connect_async(format!("ws://{}", addr)).await.unwrap() },
    )
    .await;
    let accept_connection = async {
        let message = server_ws.next().await.unwrap().unwrap();
        assert_eq!(message, Message::Text("request 1".to_string()));
        server_ws
            .send(Message::Text("response 1".to_string()))
            .await
            .unwrap();

        // Server disconnect
        drop(server_ws);

        let (stream, _) = server.accept().await.unwrap();
        let mut server_ws = accept_async(stream).await.unwrap();
        let message = server_ws.next().await.unwrap().unwrap();
        assert_eq!(message, Message::Text("request 2".to_string()));
        server_ws
            .send(Message::Text("response 2".to_string()))
            .await
            .unwrap();
    };
    let connect = async {
        client_ws
            .send(Message::Text("request 1".to_string()))
            .await
            .unwrap();
        let response = client_ws.recv().await.unwrap().unwrap();
        assert_eq!(response, Message::Text("response 1".to_string()));
        client_ws
            .send(Message::Text("request 2".to_string()))
            .await
            .unwrap();
        let response = client_ws.recv().await.unwrap().unwrap();
        assert_eq!(response, Message::Text("response 2".to_string()));
    };
    join(accept_connection, connect).await;
}

#[ignore]
#[test(tokio::test)]
async fn does_not_lose_outgoing_messages_if_connection_drops() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let (server_ws, client_ws) = join(
        async {
            let (stream, _) = server.accept().await.unwrap();
            accept_async(stream).await.unwrap()
        },
        async { connect_async(format!("ws://{}", addr)).await.unwrap() },
    )
    .await;

    let accept_connection = async {
        drop(server_ws);

        let (stream, _) = server.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        let message = ws.next().await.unwrap().unwrap();
        assert_eq!(message, Message::Text("request 1".to_string()));
        let message = ws.next().await.unwrap().unwrap();
        assert_eq!(message, Message::Text("request 2".to_string()));
    };
    let connect = async {
        client_ws
            .send(Message::Text("request 1".to_string()))
            .await
            .unwrap();
        client_ws
            .send(Message::Text("request 2".to_string()))
            .await
            .unwrap();
    };
    join(accept_connection, connect).await;
}

#[test(tokio::test)]
async fn reconnects_while_waiting_to_receive_message() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    let accept_connection = async {
        let (stream, _) = server.accept().await.unwrap();
        let ws = accept_async(stream).await.unwrap();

        // Server disconnect
        drop(ws);

        let (stream, _) = server.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        ws.send(Message::Text("hello".to_string())).await.unwrap();
    };
    let connect = async {
        let ws = connect_async(format!("ws://{}", addr)).await.unwrap();
        let message = ws.recv().await.unwrap().unwrap();
        assert_eq!(message, Message::Text("hello".to_string()));
    };
    join(accept_connection, connect).await;
}

#[test(tokio::test)]
async fn does_not_reconnect_on_close_frame() {
    let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = server.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = server.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        ws.close(None).await.unwrap();

        let (stream, _) = server.accept().await.unwrap();
        accept_async(stream).await.unwrap();
        panic!("should not reconnect");
    });

    let ws = connect_async(format!("ws://{}", addr)).await.unwrap();
    ws.send(Message::Text("hello".to_string())).await.unwrap();
}

#[test(tokio::test)]
async fn sends_pings() {
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
        let ws = ReconnectingWebSocket::builder(format!("ws://{}", addr))
            .unwrap()
            .ping_timeout(Duration::from_millis(200))
            .build();
        ws.connect().await.unwrap();
        sleep(Duration::from_millis(250)).await;
    };
    join(accept_connection, connect).await;
}
