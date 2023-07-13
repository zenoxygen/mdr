use std::fs::{metadata, File};
use std::io::prelude::*;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use anyhow::{anyhow, Result};
use askama::Template;
use axum::{
    extract::{
        ws::{Message, WebSocket},
        Extension, WebSocketUpgrade,
    },
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use clap::{crate_name, crate_version, App, Arg, ArgMatches};
use tokio::{
    sync::watch::{channel, Receiver, Sender},
    task,
    time::{interval, Duration},
};

const INTERVAL_WATCH_MSEC: u64 = 100;

#[derive(Clone)]
struct Config {
    filename: String,
    ip: String,
    port: String,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    filename: String,
    ip: String,
    port: String,
}

impl IntoResponse for IndexTemplate {
    fn into_response(self) -> Response {
        match self.render() {
            Ok(html) => Html(html).into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Error: could not render template: {e}"),
            )
                .into_response(),
        }
    }
}

async fn index_route(config: Extension<Config>) -> impl IntoResponse {
    IndexTemplate {
        filename: config.filename.to_string(),
        ip: config.ip.to_string(),
        port: config.port.to_string(),
    }
}

async fn websocket_route(
    ws: WebSocketUpgrade,
    chan_rx: Extension<Receiver<String>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |ws| handle_websocket(ws, chan_rx))
}

async fn handle_websocket(mut ws: WebSocket, mut chan_rx: Extension<Receiver<String>>) {
    while chan_rx.changed().await.is_ok() {
        let html = chan_rx.borrow().clone();

        if let Err(e) = ws.send(Message::Text(html)).await {
            eprintln!("Error: could not send text message to websocket: {e}");
        }
    }
}

async fn render_markdown(file_path: &PathBuf, chan_tx: &Sender<String>) -> Result<()> {
    let mut file = File::open(file_path)?;
    let mut markdown = String::new();
    file.read_to_string(&mut markdown)?;

    let parser = pulldown_cmark::Parser::new(&markdown);
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);

    chan_tx.send(html)?;

    Ok(())
}

async fn check_file(file_path: &PathBuf, chan_tx: &Sender<String>) -> Result<()> {
    let mut interval = interval(Duration::from_millis(INTERVAL_WATCH_MSEC));
    let mut previous_mtime = SystemTime::UNIX_EPOCH;

    loop {
        let metadata = metadata(file_path)?;
        let last_mtime = metadata.modified()?;

        if previous_mtime != last_mtime {
            render_markdown(file_path, chan_tx).await?;
            previous_mtime = last_mtime;
        }

        interval.tick().await;
    }
}

fn parse_args<'a>() -> ArgMatches<'a> {
    App::new(crate_name!())
        .version(crate_version!())
        .arg(
            Arg::with_name("file")
                .index(1)
                .required(true)
                .help("The path to the markdown file to render"),
        )
        .arg(
            Arg::with_name("ip")
                .short("i")
                .long("ip")
                .help("The ip to serve the file from")
                .number_of_values(1)
                .default_value("127.0.0.1"),
        )
        .arg(
            Arg::with_name("port")
                .short("p")
                .long("port")
                .help("The port to serve the file from")
                .number_of_values(1)
                .default_value("8080"),
        )
        .get_matches()
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args();

    let file = args.value_of("file").unwrap();
    let ip = args.value_of("ip").unwrap();
    let port = args.value_of("port").unwrap();

    let host = format!("{ip}:{port}");
    let host: SocketAddr = match host.parse() {
        Ok(host) => host,
        Err(_) => return Err(anyhow!("could not parse ip/port")),
    };

    if !Path::new(&file).exists() {
        return Err(anyhow!("file does not exist"));
    }

    let file_path = PathBuf::from(file);

    let (chan_tx, chan_rx) = channel(String::new());

    task::spawn(async move {
        if let Err(e) = check_file(&file_path, &chan_tx).await {
            eprintln!("{e}");
            std::process::exit(1);
        }
    });

    let config = Config {
        filename: file.to_string(),
        ip: ip.to_string(),
        port: port.to_string(),
    };

    let app = Router::new()
        .route("/", get(index_route))
        .route("/websocket", get(websocket_route))
        .layer(Extension(config))
        .layer(Extension(chan_rx));

    println!("Serving file on http://{host}");

    let mut cmd = Command::new("xdg-open");
    cmd.arg(&format!("http://{host}"));
    cmd.spawn()?;

    axum::Server::bind(&host)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}
