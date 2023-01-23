use std::collections::HashMap;
use std::fmt::Formatter;
use std::path::Path;
use fiberplane::provider_bindings::Error;
use fiberplane::provider_bindings::host::errors::InvocationError;
use fiberplane::provider_runtime::spec::Runtime;
use tokio::fs;
use tokio::runtime::Builder;
use tokio::sync::mpsc::{Sender};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{LocalSet};
use tracing::error;

#[derive(Debug, Default)]
pub struct Providers {
    providers: HashMap<String, Provider>
}

impl Providers {
    pub async fn from_dir(wasm_dir: &Path, provider_types: Vec<String>) -> Result<Self, Error> {
        Ok(Self {
            providers: provider_types
                .into_iter()
                .map(|provider_type| {
                    let provider = Provider::new(wasm_dir, &provider_type);
                    (provider_type, provider)
                })
                .collect()
        })
    }

    pub async fn invoke_raw(&self, provider_type: &str, request: Vec<u8>, config: Vec<u8>) -> Result<Vec<u8>, InvocationError> {
        if let Some(provider) = self.providers.get(provider_type) {
            provider
                .send(ProviderMessage::InvokeRaw {
                    request,
                    config
                })
                .await
        } else {
            // todo: change to appropriate error
            Err(InvocationError::UnexpectedReturnType)
        }
    }

    pub async fn invoke2_raw(&self, provider_type: &str, request: Vec<u8>) -> Result<Vec<u8>, InvocationError> {
        if let Some(provider) = self.providers.get(provider_type) {
            provider
                .send(ProviderMessage::Invoke2Raw {
                    request,
                })
                .await
        } else {
            // todo: change to appropriate error
            Err(InvocationError::UnexpectedReturnType)
        }
    }
}

struct Provider {
    request_tx: Sender<(ProviderMessage, ProviderReplySender)>,
    handler: std::thread::JoinHandle<()>,
}

type ProviderReplySender = oneshot::Sender<Result<Vec<u8>, InvocationError>>;

#[derive(Debug)]
enum ProviderMessage {
    InvokeRaw {
        request: Vec<u8>,
        config: Vec<u8>,
    },
    Invoke2Raw {
        request: Vec<u8>,
    },
}

impl Provider {
    fn new(wasm_dir: &Path, provider_type: &str) -> Self {
        let wasm_dir = wasm_dir.to_path_buf();
        let provider_type = provider_type.to_owned();
        let (request_tx, mut request_rx) = mpsc::channel::<(ProviderMessage, ProviderReplySender)>(256);

        let handler = std::thread::spawn(move || {
            let rt = Builder::new_current_thread().enable_all().build().unwrap();
            let local = LocalSet::new();
            local.block_on(&rt, async move {
                // Each provider's wasm module is found in the wasm_dir as data_source_type.wasm
                let wasm_path = &wasm_dir.join(&format!("{}.wasm", &provider_type));
                let wasm_module = fs::read(wasm_path).await.map_err(|err| {
                    error!("Error reading wasm file: {} {}", wasm_path.display(), err);
                    Error::Invocation {
                        message: format!("Error reading wasm file: {}", err),
                    }
                }).unwrap();
                let mut runtime = Runtime::new(wasm_module).map_err(|err| {
                    error!("Error compiling wasm module: {}", err);
                    Error::Invocation {
                        message: format!("Error compiling wasm module: {}", err),
                    }
                }).unwrap();

                while let Some((message, reply_tx)) = request_rx.recv().await {
                    let result = match message {
                        ProviderMessage::InvokeRaw { request, config } => {
                            runtime.invoke_raw(request, config).await
                        }
                        ProviderMessage::Invoke2Raw { request } => {
                            runtime.invoke2_raw(request).await
                        }
                    };
                    reply_tx.send(result).unwrap();
                }
            });
        });

        Self {
            request_tx,
            handler,
        }
    }

    async fn send(&self, message: ProviderMessage) -> Result<Vec<u8>, InvocationError> {
        let (response_tx, response_rx) = oneshot::channel();
        self.request_tx
            .send((message, response_tx))
            .await
            .expect("oops");
        response_rx.await.unwrap()
    }
}

impl std::fmt::Debug for Provider {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("[Provider]")
    }
}
