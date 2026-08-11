use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Debug, Parser)]
#[command(about = "Deterministic local HTTP upstream for edgeproxy benchmarks")]
struct Args {
    #[arg(long)]
    listen: SocketAddr,
    #[arg(long)]
    id: String,
    #[arg(long, default_value_t = 128)]
    body_bytes: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let listener = TcpListener::bind(args.listen).await?;
    let prefix = format!("upstream={};", args.id);
    let mut body = prefix.into_bytes();
    body.resize(args.body_bytes.max(body.len()), b'x');
    let body = Arc::new(body);

    loop {
        let (mut stream, _) = listener.accept().await?;
        let body = Arc::clone(&body);
        tokio::spawn(async move {
            let mut request = [0_u8; 4096];
            if stream.read(&mut request).await.unwrap_or(0) == 0 {
                return;
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\n\
                 content-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            if stream.write_all(head.as_bytes()).await.is_ok() {
                let _ = stream.write_all(&body).await;
            }
        });
    }
}
