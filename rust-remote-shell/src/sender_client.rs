use std::sync::Arc;

use futures::stream::SplitSink;
use futures::{StreamExt, TryStreamExt};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::{Sender, UnboundedReceiver};
use tokio::task::JoinHandle;
use tokio::{io::AsyncWriteExt, sync::Mutex};
use tokio_rustls::rustls;
use tokio_rustls::rustls::{Certificate, ClientConfig, RootCertStore};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{
    connect_async_tls_with_config, Connector, MaybeTlsStream, WebSocketStream,
};
use tracing::{debug, error, info, instrument, trace};
use url::Url;

use crate::io_handler::IOHandler;

// configuration options for TLS connection
async fn tls_client_config() -> ClientConfig {
    let mut root_certs = RootCertStore::empty();
    let cert_file = tokio::fs::read("certs/CA.der")
        .await
        .expect("no cert found");
    let cert = Certificate(cert_file);
    root_certs.add(&cert).unwrap();

    ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(root_certs)
        .with_no_client_auth()
}

#[derive(Error, Debug)]
pub enum SenderClientError {
    #[error("Error while trying to connect with server")]
    WebSocketConnect(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("IO error occurred while reading from stdin")]
    IORead(#[from] std::io::Error),
    #[error("IO error occurred while writing to stdout")]
    IOWrite {
        #[source]
        err: std::io::Error,
    },
    #[error("Error while trying to send the output of a command to the main task")]
    Channel(#[from] SendError<Message>),
    #[error("Error from Tungstenite while reading command")]
    TungsteniteReadData {
        #[source]
        err: tokio_tungstenite::tungstenite::Error,
    },
    #[error("Server disconnected")]
    Disconnected,
}

#[derive(Debug)]
pub struct SenderClient {
    listener_url: Url,
    tls_config: Arc<rustls::ClientConfig>,
}

impl SenderClient {
    pub async fn new(listener_url: Url) -> Self {
        let tls_config = Arc::new(tls_client_config().await);
        Self {
            listener_url,
            tls_config,
        }
    }

    #[instrument(skip(self))]
    pub async fn connect(&mut self) -> Result<(), SenderClientError> {
        // Websocket connection to an existing server
        let connector = Connector::Rustls(Arc::clone(&self.tls_config));
        let (ws_stream, _) =
            connect_async_tls_with_config(self.listener_url.clone(), None, Some(connector))
                .await
                .map_err(|err| {
                    error!("Websocket error: {:?}", err);
                    SenderClientError::WebSocketConnect(err)
                })?;

        // TODO: check that the TLS connection has been effectively established

        info!("WebSocket handshake has been successfully completed on a TLS protected stream");

        let (write, read) = ws_stream.split();

        let (tx_cmd_out, rx_cmd_out) = tokio::sync::mpsc::unbounded_channel::<Message>();
        let rx_cmd_out = Arc::new(Mutex::new(rx_cmd_out));
        let rx_cmd_out_clone = Arc::clone(&rx_cmd_out);

        let (tx_err, mut rx_err) = tokio::sync::mpsc::channel::<Result<(), SenderClientError>>(1);

        // handle stdin and stdout
        let handle_std_in_out =
            tokio::spawn(Self::read_write(write, rx_cmd_out_clone, tx_err.clone()));

        let handle_read = tokio::spawn(async move {
            let res = read
                .map_err(|err| SenderClientError::TungsteniteReadData { err })
                .try_for_each(|cmd_out| async {
                    tx_cmd_out.send(cmd_out).map_err(SenderClientError::Channel)
                })
                .await;

            if let Err(err) = res {
                tx_err.send(Err(err)).await.expect("channel error");
            }

            Ok(())
        });

        let mut handles = [handle_std_in_out, handle_read];

        match rx_err.recv().await.expect("channel error") {
            Ok(()) => {
                info!("Closing websocket connection");
                Self::close(&mut handles, rx_cmd_out).await
            }
            Err(err) => {
                error!("Fatal error: {}", err);
                Self::close(&mut handles, rx_cmd_out).await?;
                Err(err)
            }
        }
    }

    async fn read_write(
        write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
        rx: Arc<Mutex<UnboundedReceiver<Message>>>,
        tx_err: Sender<Result<(), SenderClientError>>,
    ) -> Result<(), SenderClientError> {
        let mut iohandler = IOHandler::new(write, tx_err);

        // read from stdin and, if messages are present on the channel (rx) print them to the stdout
        loop {
            iohandler.read_stdin().await?;
            iohandler.send_to_server().await?;
            iohandler.write_stdout(&rx).await?;
        }
    }

    #[instrument(skip_all)]
    async fn close(
        handles: &mut [JoinHandle<Result<(), SenderClientError>>],
        rx_cmd_out: Arc<Mutex<UnboundedReceiver<Message>>>,
    ) -> Result<(), SenderClientError> {
        // abort the current active tasks
        for h in handles.iter() {
            h.abort();
        }

        for h in handles {
            match h.await {
                Err(err) if !err.is_cancelled() => {
                    error!("Join failed: {}", err)
                }
                Err(_) => {
                    trace!("Task cancelled")
                }
                Ok(res) => {
                    debug!("Task joined with: {:?}", res)
                }
            }
        }

        // write the remaining elements from cmd out buffer to stdout
        let mut channel = rx_cmd_out.lock().await;
        let mut stdout = tokio::io::stdout();
        while let Ok(cmd_out) = channel.try_recv() {
            let data = cmd_out.into_data();
            stdout
                .write(&data)
                .await
                .map_err(|err| SenderClientError::IOWrite { err })?;
            stdout.flush().await.expect("writing stdout");
        }

        info!("EXIT");

        Ok(())
    }
}