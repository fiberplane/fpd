use crate::data_sources::{DataSource, DataSources};
use crate::service::ProxyService;
use fp_provider_runtime::spec::types::{PrometheusDataSource, RequestError};
use futures::{select, FutureExt, SinkExt, StreamExt};
use http::{Request, Response};
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Server};
use proxy_types::{
    DataSourceType, FetchDataMessage, QueryResult, QueryType, RelayMessage, ServerMessage, Uuid,
};
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::Path;
use tokio::net::TcpListener;
use tokio_tungstenite::{accept_async, accept_hdr_async, tungstenite::Message};

#[tokio::test]
async fn sends_auth_token_in_header() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let service = ProxyService::new(
        format!("ws://{}/api/proxies/ws", addr).parse().unwrap(),
        "auth token".to_string(),
        HashMap::new(),
        DataSources(HashMap::new()),
    );

    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        accept_hdr_async(stream, |request: &Request<()>, response: Response<()>| {
            assert_eq!(
                request.headers().get("fp-auth-token").unwrap(),
                "auth token"
            );
            Ok(response)
        })
        .await
        .unwrap();
    };
    select! {
      result = service.connect().fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }
}

#[tokio::test]
async fn sends_data_sources_on_connect() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut data_sources = HashMap::new();
    data_sources.insert(
        "data source 1".to_string(),
        DataSource::Prometheus(PrometheusDataSource {
            url: "prometheus.example".to_string(),
        }),
    );
    data_sources.insert(
        "data source 2".to_string(),
        DataSource::Prometheus(PrometheusDataSource {
            url: "prometheus.example".to_string(),
        }),
    );
    let service = ProxyService::new(
        format!("ws://{}/api/proxies/ws", addr).parse().unwrap(),
        "auth token".to_string(),
        HashMap::new(),
        DataSources(data_sources),
    );

    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
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
    select! {
      result = service.connect().fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }
}

#[tokio::test]
async fn sends_pings() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = ProxyService::new(
        format!("ws://{}/api/proxies/ws", addr).parse().unwrap(),
        "auth token".to_string(),
        HashMap::new(),
        DataSources(HashMap::new()),
    );

    tokio::time::pause();

    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
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

    select! {
      result = service.connect().fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }
}

#[tokio::test]
async fn returns_error_for_query_to_unknown_provider() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = ProxyService::new(
        format!("ws://{}/api/proxies/ws", addr).parse().unwrap(),
        "auth token".to_string(),
        HashMap::new(),
        DataSources(HashMap::new()),
    );

    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        // first message is data sources
        ws.next().await.unwrap().unwrap();

        let op_id = Uuid::new_v4();
        let message = ServerMessage::FetchData(FetchDataMessage {
            op_id,
            data_source_name: "data source 1".to_string(),
            query: "test query".to_string(),
            query_type: QueryType::Instant(0.0),
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
            RelayMessage::FetchDataResult(result) => result,
            _ => panic!("wrong message type"),
        };
        assert_eq!(result.op_id, op_id);
        match result.result {
            QueryResult::Instant(Err(proxy_types::FetchError::Other { message })) => {
                assert!(message.contains("unknown data source"))
            }
            _ => panic!("wrong response"),
        }
    };

    select! {
      result = service.connect().fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }
}

#[tokio::test]
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
        DataSource::Prometheus(PrometheusDataSource {
            url: format!("http://{}", fake_prometheus_addr),
        }),
    );
    let service = ProxyService::init(
        format!("ws://{}/api/proxies/ws", addr).parse().unwrap(),
        "auth token".to_string(),
        Path::new("../providers"),
        DataSources(data_sources),
    )
    .await
    .unwrap();

    // After the proxy connects, send it a query
    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        ws.next().await.unwrap().unwrap();

        // Send query
        let op_id = Uuid::new_v4();
        let message = ServerMessage::FetchData(FetchDataMessage {
            op_id,
            data_source_name: "data source 1".to_string(),
            query: "test query".to_string(),
            query_type: QueryType::Instant(0.0),
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
            RelayMessage::FetchDataResult(result) => result,
            _ => panic!("wrong message type"),
        };
        assert_eq!(result.op_id, op_id);
        match result.result {
            QueryResult::Instant(Err(proxy_types::FetchError::RequestError {
                payload:
                    RequestError::ServerError {
                        status_code,
                        response: _,
                    },
            })) => assert_eq!(status_code, 418),
            _ => panic!("wrong response"),
        }
    };
    select! {
      result = service.connect().fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }
}
