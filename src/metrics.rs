use once_cell::sync::Lazy;
use prometheus::{
    register_histogram_vec, register_int_counter_vec, register_int_gauge_vec, Error, HistogramVec,
    IntCounterVec, IntGaugeVec, TextEncoder,
};

static LABELS: [&str; 3] = ["protocol_version", "provider_type", "data_source_name"];

pub static QUERIES_TOTAL: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!("proxy_queries_total", "Number of queries executed", &LABELS).unwrap()
});

pub static QUERIES_DURATION_SECONDS: Lazy<HistogramVec> = Lazy::new(|| {
    register_histogram_vec!(
        "proxy_queries_duration_seconds",
        "Duration of queries in seconds",
        &LABELS
    )
    .unwrap()
});

pub static CONCURRENT_QUERIES: Lazy<IntGaugeVec> = Lazy::new(|| {
    register_int_gauge_vec!(
        "proxy_concurrent_queries",
        "Number of concurrent queries",
        &LABELS
    )
    .unwrap()
});

pub fn metrics_export() -> Result<String, Error> {
    let encoder = TextEncoder::new();
    let metrics = prometheus::gather();
    encoder.encode_to_string(&metrics)
}
