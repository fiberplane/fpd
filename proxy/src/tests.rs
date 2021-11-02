use crate::data_sources::{DataSource, DataSources};
use crate::service::ProxyService;
use fp_provider_runtime::spec::types::{
    Config, Error as ProviderError, HttpRequestError, ProviderRequest, ProviderResponse,
    QueryInstant,
};
use futures::{select, FutureExt, SinkExt, StreamExt};
use http::{Request, Response};
use hyper::service::{make_service_fn, service_fn};
use hyper::{header::HeaderValue, Body, Server};
use proxy_types::{DataSourceType, RelayMessage, RequestMessage, ServerMessage, Uuid};
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::Path;
use std::time::Duration;
use test_env_log::test;
use tokio::join;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_tungstenite::{accept_hdr_async, tungstenite::Message};

#[test(tokio::test)]
async fn sends_auth_token_in_header() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let service = ProxyService::new(
        format!("ws://{}/api/proxies/ws", addr).parse().unwrap(),
        "auth token".to_string(),
        HashMap::new(),
        DataSources(HashMap::new()),
        5,
    );

    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        accept_hdr_async(stream, |req: &Request<()>, mut res: Response<()>| {
            assert_eq!(req.headers().get("fp-auth-token").unwrap(), "auth token");
            res.headers_mut()
                .insert("x-fp-conn-id", HeaderValue::from_static("conn-id"));
            Ok(res)
        })
        .await
        .unwrap();
    };

    let (tx, _) = broadcast::channel(3);
    select! {
      result = service.connect(tx).fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }
}

#[test(tokio::test)]
async fn sends_data_sources_on_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut data_sources = HashMap::new();
    data_sources.insert(
        "data source 1".to_string(),
        DataSource::Prometheus(Config {
            url: Some("prometheus.example".to_string()),
        }),
    );
    data_sources.insert(
        "data source 2".to_string(),
        DataSource::Prometheus(Config {
            url: Some("prometheus.example".to_string()),
        }),
    );
    let service = ProxyService::new(
        format!("ws://{}/api/proxies/ws", addr).parse().unwrap(),
        "auth token".to_string(),
        HashMap::new(),
        DataSources(data_sources),
        5,
    );

    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_hdr_async(stream, |_req: &Request<()>, mut res: Response<()>| {
            res.headers_mut()
                .insert("x-fp-conn-id", HeaderValue::from_static("conn-id"));
            Ok(res)
        })
        .await
        .unwrap();
        let message = ws.next().await.unwrap().unwrap();
        let message = match message {
            Message::Binary(message) => RelayMessage::deserialize_msgpack(message).unwrap(),
            _ => panic!("wrong type"),
        };
        match message {
            RelayMessage::SetDataSources(data_sources) => {
                assert_eq!(data_sources.len(), 2);
                assert_eq!(
                    data_sources.get("data source 1").unwrap(),
                    &DataSourceType::Prometheus
                );
                assert_eq!(
                    data_sources.get("data source 2").unwrap(),
                    &DataSourceType::Prometheus
                );
            }
            _ => panic!(),
        };
    };

    let (tx, _) = broadcast::channel(3);
    select! {
      result = service.connect(tx).fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }
}

#[test(tokio::test)]
async fn sends_pings() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = ProxyService::new(
        format!("ws://{}/api/proxies/ws", addr).parse().unwrap(),
        "auth token".to_string(),
        HashMap::new(),
        DataSources(HashMap::new()),
        5,
    );

    tokio::time::pause();

    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_hdr_async(stream, |_req: &Request<()>, mut res: Response<()>| {
            res.headers_mut()
                .insert("x-fp-conn-id", HeaderValue::from_static("conn-id"));
            Ok(res)
        })
        .await
        .unwrap();
        // first message is data sources
        ws.next().await.unwrap().unwrap();

        tokio::time::advance(tokio::time::Duration::from_secs(45)).await;
        tokio::time::resume();

        // Next should be ping
        let message = ws.next().await.unwrap().unwrap();
        match message {
            Message::Ping(_) => {}
            _ => panic!("expected ping"),
        };
    };

    let (tx, _) = broadcast::channel(3);
    select! {
      result = service.connect(tx).fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }
}

#[test(tokio::test)]
async fn returns_error_for_query_to_unknown_provider() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = ProxyService::new(
        format!("ws://{}/api/proxies/ws", addr).parse().unwrap(),
        "auth token".to_string(),
        HashMap::new(),
        DataSources(HashMap::new()),
        5,
    );

    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_hdr_async(stream, |_req: &Request<()>, mut res: Response<()>| {
            res.headers_mut()
                .insert("x-fp-conn-id", HeaderValue::from_static("conn-id"));
            Ok(res)
        })
        .await
        .unwrap();
        // first message is data sources
        ws.next().await.unwrap().unwrap();

        let op_id = Uuid::new_v4();
        let message = ServerMessage::Request(RequestMessage {
            op_id,
            data_source_name: "data source 1".to_string(),
            data: b"fake payload".to_vec(),
        });
        let message = message.serialize_msgpack();
        ws.send(Message::Binary(message)).await.unwrap();

        // Parse the query result
        let response = ws.next().await.unwrap().unwrap();
        let response = match response {
            Message::Binary(message) => RelayMessage::deserialize_msgpack(message).unwrap(),
            _ => panic!("wrong message type"),
        };
        let error = match response {
            RelayMessage::Error(error) => error,
            other => panic!("wrong message type {:?}", other),
        };
        assert_eq!(error.op_id, op_id);
        assert!(error.message.contains("unknown data source"));
    };

    let (tx, _) = broadcast::channel(3);
    select! {
      result = service.connect(tx).fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }
}

#[test(tokio::test)]
async fn calls_provider_with_query_and_sends_result() {
    // Note that the fake Prometheus returns an error just to test that
    // the error code is relayed back through the proxy
    // because we're not testing the Prometheus provider functionality here
    let fake_prometheus_server =
        Server::bind(&"127.0.0.1:0".parse().unwrap()).serve(make_service_fn(|_| async {
            Ok::<_, Infallible>(service_fn(|req: Request<Body>| async move {
                assert_eq!(req.uri(), "/api/v1/query");
                Ok::<_, Infallible>(Response::builder().status(418).body(Body::empty()).unwrap())
            }))
        }));
    let fake_prometheus_addr = fake_prometheus_server.local_addr();

    tokio::spawn(async move {
        fake_prometheus_server.await.unwrap();
    });

    // Create a websocket listener for the proxy to connect to
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut data_sources = HashMap::new();
    data_sources.insert(
        "data source 1".to_string(),
        DataSource::Prometheus(Config {
            url: Some(format!("http://{}", fake_prometheus_addr)),
        }),
    );
    let service = ProxyService::init(
        format!("ws://{}/api/proxies/ws", addr).parse().unwrap(),
        "auth token".to_string(),
        Path::new("../providers"),
        DataSources(data_sources),
        5,
    )
    .await
    .unwrap();

    // After the proxy connects, send it a query
    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_hdr_async(stream, |_req: &Request<()>, mut res: Response<()>| {
            res.headers_mut()
                .insert("x-fp-conn-id", HeaderValue::from_static("conn-id"));
            Ok(res)
        })
        .await
        .unwrap();
        ws.next().await.unwrap().unwrap();

        // Send query
        let op_id = Uuid::new_v4();
        let request = ProviderRequest::Instant(QueryInstant {
            query: "test query".to_string(),
            timestamp: 0.0,
        });
        let message = ServerMessage::Request(RequestMessage {
            op_id,
            data_source_name: "data source 1".to_string(),
            // TODO serializing this with to_vec fails while to_vec_named works. Why?
            data: rmp_serde::to_vec_named(&request).unwrap(),
        });
        let message = message.serialize_msgpack();
        ws.send(Message::Binary(message)).await.unwrap();

        // Parse the query result
        let response = ws.next().await.unwrap().unwrap();
        let response = match response {
            Message::Binary(message) => RelayMessage::deserialize_msgpack(message).unwrap(),
            _ => panic!("wrong message type"),
        };
        let result = match response {
            RelayMessage::Response(message) => message,
            other => panic!("wrong message type: {:?}", other),
        };
        assert_eq!(result.op_id, op_id);
        match rmp_serde::from_slice(&result.data) {
            Ok(ProviderResponse::Error(ProviderError::Http(HttpRequestError::ServerError {
                status_code,
                response: _,
            }))) => {
                assert_eq!(status_code, 418);
            }
            _ => panic!("wrong response"),
        }
    };

    let (tx, _) = broadcast::channel(3);
    select! {
      result = service.connect(tx).fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }
}

#[test(tokio::test)]
async fn reconnects_if_websocket_closes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = ProxyService::new(
        format!("ws://{}/api/proxies/ws", addr).parse().unwrap(),
        "auth token".to_string(),
        HashMap::new(),
        DataSources(HashMap::new()),
        1,
    );

    tokio::time::pause();

    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let ws = accept_hdr_async(stream, |_req: &Request<()>, _res: Response<()>| {
            Err(Response::builder().status(500).body(None).unwrap())
        })
        .await
        .ok();

        drop(ws);

        tokio::time::advance(tokio::time::Duration::from_secs(45)).await;
        tokio::time::resume();

        // Proxy should try to reconnect
        let (stream, _) = listener.accept().await.unwrap();
        let ws = accept_hdr_async(stream, |_req: &Request<()>, _res: Response<()>| {
            Err(Response::builder().status(500).body(None).unwrap())
        })
        .await
        .ok();

        drop(ws);

        let (_stream, _) = listener.accept().await.unwrap();
        panic!("should not get here because it should not try again");
    };

    let (tx, _) = broadcast::channel(3);
    let result = select! {
      result = service.connect(tx).fuse() => result,
      _ = handle_connection.fuse() => unreachable!()
    };
    assert_eq!(
        format!("{}", result.unwrap_err()),
        "unable to connect, exceeded max tries"
    );
}

#[test(tokio::test)]
async fn service_shutdown() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = ProxyService::new(
        format!("ws://{}/api/proxies/ws", addr).parse().unwrap(),
        "auth token".to_string(),
        HashMap::new(),
        DataSources(HashMap::new()),
        1,
    );

    let (tx, _) = broadcast::channel(3);
    let tx_clone = tx.clone();
    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_hdr_async(stream, |_req: &Request<()>, mut res: Response<()>| {
            res.headers_mut()
                .insert("x-fp-conn-id", HeaderValue::from_static("conn-id"));

            Ok(res)
        })
        .await
        .unwrap();

        // Signal the service to actually shutdown (the sleep is to ensure that
        // the service is able to spawn the read/write loops).
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(tx_clone.send(()).is_ok());

        // Read any message sent from the service, until it gets closed, which
        // will indicate that the service has shutdown.
        loop {
            if let None = ws.next().await {
                break;
            };
        }
    };

    // Wait for both the service and our test handle_connection are stopped
    let (_, result) = join!(handle_connection, service.connect(tx));
    if let Err(err) = result {
        panic!("unexpected error occurred: {:?}", err);
    }
}
