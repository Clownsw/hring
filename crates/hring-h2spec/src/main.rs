#![allow(incomplete_features)]
#![feature(async_fn_in_trait)]

use std::{collections::VecDeque, path::PathBuf};

use hring::buffet::RollMut;
use hring::{
    http::{StatusCode, Version},
    Body, BodyChunk, Encoder, ExpectResponseHeaders, Headers, Request, Responder, Response,
    ResponseDone, ServerDriver,
};
use maybe_uring::{io::IntoHalves, net::TcpListener};
use tokio::process::Command;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use std::{net::SocketAddr, rc::Rc};

fn main() {
    color_eyre::install().unwrap();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|e| {
            eprintln!("Couldn't parse RUST_LOG: {e}");
            EnvFilter::try_new("info").unwrap()
        }))
        .init();

    let h2spec_binary = match which::which("h2spec") {
        Ok(h2spec_binary) => {
            info!("Using h2spec binary from {}", h2spec_binary.display());
            h2spec_binary
        }
        Err(_) => {
            error!("Couldn't find h2spec binary in PATH, see https://github.com/summerwind/h2spec");
            std::process::exit(1);
        }
    };

    maybe_uring::start(async move { real_main(h2spec_binary).await.unwrap() });
}

struct SDriver;

impl ServerDriver for SDriver {
    async fn handle<E: Encoder>(
        &self,
        req: Request,
        req_body: &mut impl Body,
        respond: Responder<E, ExpectResponseHeaders>,
    ) -> color_eyre::Result<Responder<E, ResponseDone>> {
        tracing::info!(
            "Handling {:?} {}, content_len = {:?}",
            req.method,
            req.uri,
            req_body.content_len()
        );

        let res = Response {
            version: Version::HTTP_2,
            status: StatusCode::OK,
            headers: Headers::default(),
        };
        let mut body = TestBody::default();
        respond.write_final_response_with_body(res, &mut body).await
    }
}

#[derive(Default, Debug)]
struct TestBody {
    eof: bool,
}

impl TestBody {
    const CONTENTS: &str = "I am a test body";
}

impl Body for TestBody {
    fn content_len(&self) -> Option<u64> {
        Some(Self::CONTENTS.len() as _)
    }

    fn eof(&self) -> bool {
        self.eof
    }

    async fn next_chunk(&mut self) -> color_eyre::eyre::Result<BodyChunk> {
        if self.eof {
            Ok(BodyChunk::Done { trailers: None })
        } else {
            self.eof = true;
            Ok(BodyChunk::Chunk(Self::CONTENTS.as_bytes().into()))
        }
    }
}

async fn real_main(h2spec_binary: PathBuf) -> color_eyre::Result<()> {
    let addr = spawn_server("127.0.0.1:0".parse()?).await?;

    let mut args = std::env::args().skip(1).collect::<VecDeque<_>>();
    if matches!(args.get(0).map(|s| s.as_str()), Some("--")) {
        args.pop_front();
    }
    tracing::info!("Custom args: {args:?}");

    Command::new(h2spec_binary)
        .arg("-p")
        .arg(&format!("{}", addr.port()))
        .arg("-o")
        .arg("1")
        .args(&args)
        .spawn()?
        .wait()
        .await?;

    Ok(())
}

pub(crate) async fn spawn_server(addr: SocketAddr) -> color_eyre::Result<SocketAddr> {
    let ln = TcpListener::bind(addr).await?;
    let addr = ln.local_addr()?;
    tracing::info!("Listening on {}", ln.local_addr()?);

    let _task = tokio::task::spawn_local(async move { run_server(ln).await.unwrap() });

    Ok(addr)
}

pub(crate) async fn run_server(ln: TcpListener) -> color_eyre::Result<()> {
    loop {
        let (stream, addr) = ln.accept().await?;
        tracing::info!(%addr, "Accepted connection from");
        let conf = Rc::new(hring::h2::ServerConf::default());
        let client_buf = RollMut::alloc()?;
        let driver = Rc::new(SDriver);

        tokio::task::spawn_local(async move {
            if let Err(e) = hring::h2::serve(stream.into_halves(), conf, client_buf, driver).await {
                tracing::error!("error serving client {}: {}", addr, e);
            }
        });
    }
}
