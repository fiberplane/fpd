use crate::service::{parse_data_sources_yaml, DataSources, ProxyService, WasmModules};
use fiberplane::protocols::core::{
    DataSource, DataSourceType, ElasticsearchDataSource, PrometheusDataSource, ProxyDataSource,
};
use fp_provider_runtime::spec::types::{
    Error as ProviderError, HttpRequestError, LegacyProviderRequest, LegacyProviderResponse,
    QueryInstant, QueryTimeRange, TimeRange,
};
use futures::{select, FutureExt, SinkExt, StreamExt};
use http::{Request, Response, StatusCode};
use httpmock::prelude::*;
use hyper::header::HeaderValue;
use proxy_types::{
    DataSourceDetails, DataSourceDetailsOrType, DataSourceStatus, InvokeProxyMessage, RelayMessage,
    ServerMessage, Uuid,
};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use test_log::test;
use tokio::join;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_tungstenite::{accept_hdr_async, tungstenite::Message};

#[test]
fn parses_data_sources_from_yaml() {
    let yaml = "
Production Prometheus:
  type: prometheus
  url: http://localhost:9090
Dev Elasticsearch:
  type: elasticsearch
  url: http://localhost:9200";
    let data_sources: DataSources = parse_data_sources_yaml(yaml).unwrap();
    assert_eq!(data_sources.len(), 2);
    match &data_sources["Production Prometheus"] {
        DataSource::Prometheus(prometheus) => {
            assert_eq!(prometheus.url, "http://localhost:9090");
        }
        _ => panic!("Expected Prometheus data source"),
    }
    match &data_sources["Dev Elasticsearch"] {
        DataSource::Elasticsearch(elasticsearch) => {
            assert_eq!(elasticsearch.url, "http://localhost:9200");
        }
        _ => panic!("Expected Elasticsearch data source"),
    }
}

#[test]
fn parses_old_data_sources_format() {
    let yaml = "
Production Prometheus:
  type: prometheus
  options:
    url: http://localhost:9090
Dev Elasticsearch:
  type: elasticsearch
  options:
    url: http://localhost:9200";
    let data_sources: DataSources = parse_data_sources_yaml(yaml).unwrap();
    assert_eq!(data_sources.len(), 2);
    match &data_sources["Production Prometheus"] {
        DataSource::Prometheus(prometheus) => {
            assert_eq!(prometheus.url, "http://localhost:9090");
        }
        _ => panic!("Expected Prometheus data source"),
    }
    match &data_sources["Dev Elasticsearch"] {
        DataSource::Elasticsearch(elasticsearch) => {
            assert_eq!(elasticsearch.url, "http://localhost:9200");
        }
        _ => panic!("Expected Elasticsearch data source"),
    }
}

#[test(tokio::test)]
async fn sends_auth_token_in_header() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let service = ProxyService::new(
        format!("ws://{}/api/proxies/ws", addr).parse().unwrap(),
        "auth token".to_string(),
        HashMap::new(),
        DataSources::new(),
        5,
        None,
        Duration::from_secs(300),
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
    let connected_prometheus = MockServer::start();
    let connected_prometheus_mock = connected_prometheus.mock(|when, then| {
        when.method("GET").path("/api/v1/query");
        then.status(200).body("{}");
    });
    let connected_elasticsearch = MockServer::start();
    let connected_elasticsearch_mock = connected_elasticsearch.mock(|when, then| {
        when.method("GET").path("/_xpack");
        then.status(200).body("{}");
    });
    let disconnected_prometheus = MockServer::start();
    let disconnected_prometheus_mock = disconnected_prometheus.mock(|when, then| {
        when.method("GET").path("/api/v1/query");
        then.status(500).body("Some Error");
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let data_sources = HashMap::from([
        (
            "data source 1".to_string(),
            DataSource::Prometheus(PrometheusDataSource {
                url: connected_prometheus.url(""),
            }),
        ),
        (
            "data source 2".to_string(),
            DataSource::Elasticsearch(ElasticsearchDataSource {
                url: connected_elasticsearch.url(""),
                body_field_names: Vec::new(),
                timestamp_field_names: Vec::new(),
            }),
        ),
        (
            "data source 3".to_string(),
            DataSource::Prometheus(PrometheusDataSource {
                url: disconnected_prometheus.url(""),
            }),
        ),
        // We don't have the proxy provider wasm module so this tests
        // what happens if you specify a provider that we don't have
        (
            "data source 4".to_string(),
            DataSource::Proxy(ProxyDataSource {
                data_source_name: "something else".to_string(),
                data_source_type: DataSourceType::Elasticsearch,
                proxy_id: "proxy id".to_string(),
            }),
        ),
    ]);
    let service = ProxyService::init(
        format!("ws://{}/api/proxies/ws", addr).parse().unwrap(),
        "auth token".to_string(),
        Path::new("../providers"),
        data_sources,
        5,
        None,
        Duration::from_secs(300),
    )
    .await;

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
                assert_eq!(data_sources.len(), 4);
                assert_eq!(
                    data_sources.get("data source 1").unwrap(),
                    &DataSourceDetailsOrType::Details(DataSourceDetails {
                        ty: DataSourceType::Prometheus,
                        status: DataSourceStatus::Connected,
                    })
                );
                assert_eq!(
                    data_sources.get("data source 2").unwrap(),
                    &DataSourceDetailsOrType::Details(DataSourceDetails {
                        ty: DataSourceType::Elasticsearch,
                        status: DataSourceStatus::Connected,
                    })
                );
                assert_eq!(
                    data_sources.get("data source 3").unwrap(),
                    &DataSourceDetailsOrType::Details(DataSourceDetails {
                        ty: DataSourceType::Prometheus,
                        status: DataSourceStatus::Error(
                            "Provider returned HTTP error: status=500, response=Some Error"
                                .to_string()
                        ),
                    })
                );
                let proxy_data_source = if let DataSourceDetailsOrType::Details(details) =
                    &data_sources["data source 4"]
                {
                    details
                } else {
                    panic!("wrong type");
                };
                if let DataSourceStatus::Error(message) = &proxy_data_source.status {
                    assert!(message.starts_with("Error invoking provider: Error loading wasm module ../providers/proxy.wasm: No such file or directory"));
                } else {
                    panic!("Should have errored");
                }
            }
            _ => panic!(),
        };
    };

    let (tx, _) = broadcast::channel(3);
    select! {
      result = service.connect(tx).fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }
    connected_prometheus_mock.assert();
    connected_elasticsearch_mock.assert();
    disconnected_prometheus_mock.assert();
}

#[test(tokio::test)]
async fn checks_data_source_status_on_interval() {
    let mock_server = MockServer::start();
    let mut connected_prometheus_mock = mock_server.mock(|when, then| {
        when.method("GET").path("/api/v1/query");
        then.status(200).body("{}");
    });
    let disconnected_prometheus_mock = mock_server.mock(|when, then| {
        when.method("GET").path("/api/v1/query");
        then.status(500).body("Internal Server Error");
    });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let data_sources = HashMap::from([(
        "data source 1".to_string(),
        DataSource::Prometheus(PrometheusDataSource {
            url: mock_server.url(""),
        }),
    )]);
    let service = ProxyService::init(
        format!("ws://{}/api/proxies/ws", addr).parse().unwrap(),
        "auth token".to_string(),
        Path::new("../providers"),
        data_sources,
        5,
        None,
        Duration::from_millis(200),
    )
    .await;

    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_hdr_async(stream, |_req: &Request<()>, mut res: Response<()>| {
            res.headers_mut()
                .insert("x-fp-conn-id", HeaderValue::from_static("conn-id"));
            Ok(res)
        })
        .await
        .unwrap();
        for _i in 0..2 {
            let message = ws.next().await.unwrap().unwrap();
            let message = match message {
                Message::Binary(message) => RelayMessage::deserialize_msgpack(message).unwrap(),
                _ => panic!("wrong type"),
            };
            if let RelayMessage::SetDataSources(data_sources) = message {
                assert_eq!(
                    data_sources["data source 1"],
                    DataSourceDetailsOrType::Details(DataSourceDetails {
                        ty: DataSourceType::Prometheus,
                        status: DataSourceStatus::Connected,
                    })
                );
            } else {
                panic!();
            };
        }
        connected_prometheus_mock.assert_hits(2);
        connected_prometheus_mock.delete();

        let message = ws.next().await.unwrap().unwrap();
        let message = match message {
            Message::Binary(message) => RelayMessage::deserialize_msgpack(message).unwrap(),
            _ => panic!("wrong type"),
        };
        if let RelayMessage::SetDataSources(data_sources) = message {
            assert_eq!(
                data_sources["data source 1"],
                DataSourceDetailsOrType::Details(DataSourceDetails {
                    ty: DataSourceType::Prometheus,
                    status: DataSourceStatus::Error(
                        "Provider returned HTTP error: status=500, response=Internal Server Error"
                            .to_string()
                    ),
                })
            );
        } else {
            panic!();
        };
    };

    let (tx, _) = broadcast::channel(3);
    select! {
      result = service.connect(tx).fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }

    disconnected_prometheus_mock.assert();
}

#[test(tokio::test)]
async fn sends_pings() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = ProxyService::new(
        format!("ws://{}/api/proxies/ws", addr).parse().unwrap(),
        "auth token".to_string(),
        WasmModules::new(),
        DataSources::new(),
        5,
        None,
        Duration::from_secs(300),
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
async fn health_check_endpoints() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let service_addr = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();

    let service = ProxyService::new(
        format!("ws://{}/api/proxies/ws", addr).parse().unwrap(),
        "auth token".to_string(),
        HashMap::new(),
        HashMap::new(),
        5,
        Some(service_addr),
        Duration::from_secs(300),
    );

    let handle_connection = async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_hdr_async(stream, |req: &Request<()>, mut res: Response<()>| {
            assert_eq!(req.headers().get("fp-auth-token").unwrap(), "auth token");
            res.headers_mut()
                .insert("x-fp-conn-id", HeaderValue::from_static("conn-id"));
            Ok(res)
        })
        .await
        .unwrap();
        ws.next().await.unwrap().unwrap();

        let check_endpoint = |path: &'static str| async move {
            reqwest::get(format!("http://{}{}", service_addr, path))
                .await
                .unwrap()
                .status()
        };

        // Check status while connected
        assert_eq!(StatusCode::OK, check_endpoint("").await);
        assert_eq!(StatusCode::OK, check_endpoint("/health").await);

        // Check status after disconnect
        drop(ws);
        assert_eq!(StatusCode::OK, check_endpoint("").await);
        assert_eq!(StatusCode::BAD_GATEWAY, check_endpoint("/health").await);
    };

    let connect = async move {
        let (tx, _) = broadcast::channel(3);
        service.connect(tx).await.unwrap();
    };

    select! {
      _ = connect.fuse() => {}
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
        WasmModules::new(),
        DataSources::new(),
        5,
        None,
        Duration::from_secs(300),
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
        let message = ServerMessage::InvokeProxy(InvokeProxyMessage {
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
    let prometheus = MockServer::start();
    prometheus.mock(|when, then| {
        when.method("GET").path("/api/v1/status/buildinfo");
        then.status(200).body("{}");
    });
    let query_mock = prometheus.mock(|when, then| {
        when.method("POST").path("/api/v1/query");
        then.status(200).body(
            r#"{
                   "status" : "success",
                   "data" : {
                      "resultType" : "vector",
                      "result" : [
                         {
                            "metric" : {
                               "__name__" : "up",
                               "job" : "prometheus",
                               "instance" : "localhost:9090"
                            },
                            "value": [ 1435781451.781, "1" ]
                         }
                      ]
                   }
                }"#,
        );
    });

    // Create a websocket listener for the proxy to connect to
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut data_sources = HashMap::new();
    data_sources.insert(
        "data source 1".to_string(),
        DataSource::Prometheus(PrometheusDataSource {
            url: prometheus.url(""),
        }),
    );
    let service = ProxyService::init(
        format!("ws://{}/api/proxies/ws", addr).parse().unwrap(),
        "auth token".to_string(),
        Path::new("../providers"),
        data_sources,
        5,
        None,
        Duration::from_secs(300),
    )
    .await;

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
        let request = LegacyProviderRequest::Instant(QueryInstant {
            query: "test query".to_string(),
            timestamp: 0.0,
        });
        let message = ServerMessage::InvokeProxy(InvokeProxyMessage {
            op_id,
            data_source_name: "data source 1".to_string(),
            data: rmp_serde::to_vec(&request).unwrap(),
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
            RelayMessage::InvokeProxyResponse(message) => message,
            other => panic!("wrong message type: {:?}", other),
        };
        assert_eq!(result.op_id, op_id);
        match rmp_serde::from_slice(&result.data) {
            Ok(LegacyProviderResponse::Instant { instants }) => {
                assert_eq!(instants[0].metric.name, "up");
            }
            Err(e) => panic!(
                "error deserializing provider repsonse: {:?} {:?}",
                e, result.data
            ),
            Ok(response) => panic!("wrong response {:?}", response),
        }
    };

    let (tx, _) = broadcast::channel(3);
    select! {
      result = service.connect(tx).fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }

    query_mock.assert();
}

#[test(tokio::test)]
async fn handles_multiple_concurrent_messages() {
    let prometheus = MockServer::start();
    prometheus.mock(|when, then| {
        when.method("GET").path("/api/v1/status/buildinfo");
        then.status(200).body("{}");
    });
    // Delay the first response so the second response definitely comes back first
    let query_instant_mock = prometheus.mock(|when, then| {
        when.method("POST").path("/api/v1/query");
        then.delay(Duration::from_secs(4)).status(200).body(
            r#"{
                   "status" : "success",
                   "data" : {
                      "resultType" : "vector",
                      "result" : []
                   }
                }"#,
        );
    });
    let query_series_mock = prometheus.mock(|when, then| {
        when.method("POST").path("/api/v1/query_range");
        then.status(200).body(
            r#"{
                   "status" : "success",
                   "data" : {
                      "resultType" : "vector",
                      "result" : []
                   }
                }"#,
        );
    });

    // Create a websocket listener for the proxy to connect to
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut data_sources = DataSources::new();
    data_sources.insert(
        "data source 1".to_string(),
        DataSource::Prometheus(PrometheusDataSource {
            url: prometheus.url(""),
        }),
    );
    let service = ProxyService::init(
        format!("ws://{}/api/proxies/ws", addr).parse().unwrap(),
        "auth token".to_string(),
        Path::new("../providers"),
        data_sources,
        5,
        None,
        Duration::from_secs(300),
    )
    .await;

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
        // Ignore the data sources message
        ws.next().await.unwrap().unwrap();

        // Send two queries
        let op_1 = Uuid::parse_str("10000000-0000-0000-0000-000000000000").unwrap();
        let message_1 = ServerMessage::InvokeProxy(InvokeProxyMessage {
            op_id: op_1,
            data_source_name: "data source 1".to_string(),
            data: rmp_serde::to_vec(&LegacyProviderRequest::Instant(QueryInstant {
                query: "query 1".to_string(),
                timestamp: 0.0,
            }))
            .unwrap(),
        })
        .serialize_msgpack();
        ws.send(Message::Binary(message_1)).await.unwrap();

        let op_2 = Uuid::parse_str("20000000-0000-0000-0000-000000000000").unwrap();
        let message_2 = ServerMessage::InvokeProxy(InvokeProxyMessage {
            op_id: op_2,
            data_source_name: "data source 1".to_string(),
            data: rmp_serde::to_vec(&LegacyProviderRequest::Series(QueryTimeRange {
                query: "query 2".to_string(),
                time_range: TimeRange { from: 0.0, to: 1.0 },
            }))
            .unwrap(),
        })
        .serialize_msgpack();
        ws.send(Message::Binary(message_2)).await.unwrap();

        // Parse the query result
        if let Message::Binary(message) = ws.next().await.unwrap().unwrap() {
            if let RelayMessage::InvokeProxyResponse(message) =
                RelayMessage::deserialize_msgpack(message).unwrap()
            {
                // Check that the second query comes back first
                assert_eq!(message.op_id, op_2);

                // Now we will wait for the first query
                if let Message::Binary(message) = ws.next().await.unwrap().unwrap() {
                    if let RelayMessage::InvokeProxyResponse(message) =
                        RelayMessage::deserialize_msgpack(message).unwrap()
                    {
                        assert_eq!(message.op_id, op_1);
                        return;
                        // Everything is fine so just stop handle_connection
                    }
                }
            }
        }

        panic!("received the wrong response type or wrong order");
    };

    let (tx, _) = broadcast::channel(3);
    select! {
      result = service.connect(tx).fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }
    query_instant_mock.assert();
    query_series_mock.assert();
}

#[test(tokio::test)]
async fn calls_provider_with_query_and_sends_error() {
    let prometheus = MockServer::start();
    prometheus.mock(|when, then| {
        when.method("GET").path("/api/v1/status/buildinfo");
        then.status(200).body("{}");
    });
    let query_mock = prometheus.mock(|when, then| {
        when.method("POST").path("/api/v1/query");
        then.status(418).body("Some error");
    });

    // Create a websocket listener for the proxy to connect to
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut data_sources = DataSources::new();
    data_sources.insert(
        "data source 1".to_string(),
        DataSource::Prometheus(PrometheusDataSource {
            url: prometheus.url(""),
        }),
    );
    let service = ProxyService::init(
        format!("ws://{}/api/proxies/ws", addr).parse().unwrap(),
        "auth token".to_string(),
        Path::new("../providers"),
        data_sources,
        5,
        None,
        Duration::from_secs(300),
    )
    .await;

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
        let request = LegacyProviderRequest::Instant(QueryInstant {
            query: "test query".to_string(),
            timestamp: 0.0,
        });
        let message = ServerMessage::InvokeProxy(InvokeProxyMessage {
            op_id,
            data_source_name: "data source 1".to_string(),
            data: rmp_serde::to_vec(&request).unwrap(),
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
            RelayMessage::InvokeProxyResponse(message) => message,
            other => panic!("wrong message type: {:?}", other),
        };
        assert_eq!(result.op_id, op_id);
        match rmp_serde::from_slice(&result.data) {
            Ok(LegacyProviderResponse::Error {
                error:
                    ProviderError::Http {
                        error:
                            HttpRequestError::ServerError {
                                status_code,
                                response: _,
                            },
                    },
            }) => {
                assert_eq!(status_code, 418);
            }
            Err(e) => panic!(
                "error deserializing provider repsonse: {:?} {:?}",
                e, result.data
            ),
            Ok(response) => panic!("wrong response {:?}", response),
        }
    };

    let (tx, _) = broadcast::channel(3);
    select! {
      result = service.connect(tx).fuse() => result.unwrap(),
      _ = handle_connection.fuse() => {}
    }

    query_mock.assert();
}

#[test(tokio::test)]
async fn reconnects_if_websocket_closes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = ProxyService::new(
        format!("ws://{}/api/proxies/ws", addr).parse().unwrap(),
        "auth token".to_string(),
        WasmModules::new(),
        DataSources::new(),
        1,
        None,
        Duration::from_secs(300),
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
        "HTTP error: 500 Internal Server Error"
    );
}

#[test(tokio::test)]
async fn service_shutdown() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = ProxyService::new(
        format!("ws://{}/api/proxies/ws", addr).parse().unwrap(),
        "auth token".to_string(),
        WasmModules::new(),
        DataSources::new(),
        1,
        None,
        Duration::from_secs(300),
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
