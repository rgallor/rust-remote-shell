use clap::{Parser, Subcommand};

use std::net::SocketAddr;

use color_eyre::Result;

use tracing::{debug, Level};
use tracing_subscriber::FmtSubscriber;

use rust_remote_shell::device::Device;
use rust_remote_shell::host::Host;

/// CLI for a rust remote shell
#[derive(Debug, Parser)]
struct Cli {
    #[clap(subcommand)]
    command: Commands,
}

// these commands can be called from the CLI using lowercase Commands name
#[derive(Subcommand, Debug)]
enum Commands {
    /// Host waiting for a device connection
    Host {
        addr: SocketAddr,
        #[clap(long, requires("server-cert-file"), requires("privkey-file"))]
        tls_enabled: bool,
        #[clap(long)]
        server_cert_file: Option<String>, // "certs/localhost.local.der"
        #[clap(long)]
        privkey_file: Option<String>, // "certs/localhost.local.key.der"
    },
    /// Device capable of receiving commands and sending their output
    Device {
        device_cfg_path: String,
        #[clap(long, requires("ca-cert-file"))]
        tls_enabled: bool,
        #[clap(long)]
        ca_cert_file: Option<String>, // "certs/CA.der"
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    // define a subscriber for logging purposes
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::TRACE)
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    let cli = Cli::parse();

    debug!(?cli);

    match cli.command {
        Commands::Host {
            addr,
            tls_enabled,
            server_cert_file,
            privkey_file,
        } => {
            let builder = Host::bind(addr).await?;

            if tls_enabled {
                println!("TLS");

                // retrieve certificates from the file names given in input and pass them as argument to with_tls()
                let cert = tokio::fs::read(
                    server_cert_file.expect("expected to be called with --tls-enabled option"),
                )
                .await
                .expect("error while reading server certificate");

                let privkey = tokio::fs::read(
                    privkey_file.expect("expected to be called with --tls-enabled option"),
                )
                .await
                .expect("error while reading server private key");

                builder.with_tls(cert, privkey).await?.serve().await?;
            } else {
                builder.serve().await?;
            }
        }
        Commands::Device {
            device_cfg_path,
            tls_enabled,
            ca_cert_file,
        } => {
            // To make comminicate a device with Astarte use the following command
            // astartectl appengine --appengine-url http://localhost:4002/ --realm-management-url http://localhost:4000/ --realm-key test_private.pem --realm-name test devices send-data 2TBn-jNESuuHamE2Zo1anA org.astarte-platform.rust-remote-shell.ConnectToHost /host '{"scheme" : "ws", "ip" : "127.0.0.1", "port" : 8080}'
            let mut device = Device::new(device_cfg_path.as_str()).await?;

            if tls_enabled {
                let ca_cert = tokio::fs::read(
                    ca_cert_file.expect("expected to be called with --tls-enabled option"),
                )
                .await
                .expect("error while reading server certificate");

                device.connect_tls(ca_cert).await?;
            } else {
                device.connect().await?;
            }
        }
    }

    Ok(())
}
