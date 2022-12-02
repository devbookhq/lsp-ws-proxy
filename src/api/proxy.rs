use std::{convert::Infallible, process::Stdio, str::FromStr};

use futures_util::{
    future::{select, Either},
    stream, SinkExt, StreamExt,
};
use tokio::{
    fs,
    process::{Child, Command},
    sync::Mutex,
};
use url::Url;
use warp::{Filter, Rejection, Reply};

use crate::lsp;

use super::with_context;

#[derive(Debug, Clone)]
pub struct Context {
    /// One or more commands to start a Language Server.
    pub commands: Vec<Vec<String>>,
    /// Write file on save.
    pub sync: bool,
    /// Remap relative `source://` to absolute `file://`.
    pub remap: bool,
    /// Project root.
    pub cwd: Url,
    /// Cached language servers
    pub db: Db,
}

use std::collections::HashMap;
use std::sync::Arc;

// https://tokio.rs/tokio/tutorial/shared-state
pub type Db = Arc<Mutex<HashMap<String, Child>>>;

#[derive(Clone, Debug, serde::Deserialize)]
struct Query {
    /// The command name of the Language Server to start.
    /// If not specified, the first one is started.
    name: String,
}

fn with_optional_query() -> impl Filter<Extract = (Option<Query>,), Error = Infallible> + Clone {
    warp::query::<Query>()
        .map(Some)
        .or_else(|_| async { Ok::<(Option<Query>,), Infallible>((None,)) })
}

/// Handler for WebSocket connection.
pub fn handler(ctx: Context) -> impl Filter<Extract = impl Reply, Error = Rejection> + Clone {
    warp::path::end()
        .and(warp::ws())
        .and(with_context(ctx))
        .and(with_optional_query())
        .map(|ws: warp::ws::Ws, ctx, query| {
            ws.with_compression()
                .on_upgrade(move |socket| on_upgrade(socket, ctx, query))
        })
}

#[tracing::instrument(level = "debug", err, skip(msg))]
async fn maybe_write_text_document(msg: &lsp::Message) -> Result<(), std::io::Error> {
    if let lsp::Message::Notification(lsp::Notification::DidSave { params }) = msg {
        if let Some(text) = &params.text {
            let uri = &params.text_document.uri;
            if uri.scheme() == "file" {
                if let Ok(path) = uri.to_file_path() {
                    if let Some(parent) = path.parent() {
                        tracing::debug!("writing to {:?}", path);
                        fs::create_dir_all(parent).await?;
                        fs::write(&path, text.as_bytes()).await?;
                    }
                }
            }
        }
    }
    Ok(())
}

async fn on_upgrade(socket: warp::ws::WebSocket, ctx: Context, query: Option<Query>) {
    tracing::info!("connected");
    if let Err(err) = connected(socket, ctx, query).await {
        tracing::error!("connection error: {}", err);
    }
    tracing::info!("disconnected");
}

#[tracing::instrument(level = "debug", skip(ws, ctx), fields(remap = %ctx.remap, sync = %ctx.sync))]
async fn connected(
    ws: warp::ws::WebSocket,
    ctx: Context,
    query: Option<Query>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ls_name = query.unwrap().name;
    let ls_name_2 = ls_name.clone();

    let maybe_command = ctx.commands.iter().find(|v| v[0] == ls_name);

    if maybe_command.is_none() {
        return Ok(());
    }

    let command = maybe_command.unwrap();

    tracing::info!("trying to retrieve cached {} in {}", command[0], ctx.cwd);
    let mut db = ctx.db.lock().await;
    
    let cached_server = db.get_mut(&ls_name);
    
    if cached_server.is_none() {
        tracing::info!("creating new {} in {}", command[0], ctx.cwd);
        let new_server = Command::new(&command[0])
            .args(&command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(false)
            .spawn()
            .unwrap();

        db.insert(ls_name, new_server);
    }

    let server = db.get_mut(&ls_name_2).unwrap();

    let server_in = server.stdin.as_mut().unwrap();
    let server_out = server.stdout.as_mut().unwrap();

    tracing::debug!("running {}", command[0]);
    let mut server_send = lsp::framed::writer(server_in);
    let mut server_recv = lsp::framed::reader(server_out);

    let (mut client_send, client_recv) = ws.split();
    let client_recv = client_recv
        .filter_map(filter_map_warp_ws_message)
        // Chain this with `Done` so we know when the client disconnects
        .chain(stream::once(async { Ok(Message::Done) }));
    // Tick every 30s so we can ping the client to keep the connection alive
    let ticks = stream::unfold(
        tokio::time::interval(std::time::Duration::from_secs(30)),
        |mut interval| async move {
            interval.tick().await;
            Some((Ok(Message::Tick), interval))
        },
    );
    let mut client_recv = stream::select(client_recv, ticks).boxed();

    let mut client_msg = client_recv.next();
    let mut server_msg = server_recv.next();
    // Keeps track if `pong` was received since sending the last `ping`.
    let mut is_alive = true;

    loop {
        match select(client_msg, server_msg).await {
            // From Client
            Either::Left((from_client, p_server_msg)) => {
                match from_client {
                    // Valid LSP message
                    Some(Ok(Message::Message(mut msg))) => {
                        if ctx.remap {
                            lsp::ext::remap_relative_uri(&mut msg, &ctx.cwd)?;
                            tracing::debug!("remapped relative URI from client");
                        }
                        if ctx.sync {
                            maybe_write_text_document(&msg).await?;
                        }
                        let text = serde_json::to_string(&msg)?;
                        tracing::debug!("-> {}", text);
                        server_send.send(text).await?;
                    }

                    // Invalid JSON body
                    Some(Ok(Message::Invalid(text))) => {
                        tracing::warn!("-> {}", text);
                        // Just forward it to the server as is.
                        server_send.send(text).await?;
                    }

                    // Close message
                    Some(Ok(Message::Close)) => {
                        // The connection will terminate when None is received.
                        tracing::info!("received Close message");
                    }

                    // Ping the client to keep the connection alive
                    Some(Ok(Message::Tick)) => {
                        // Terminate if we haven't heard back from the previous ping.
                        if !is_alive {
                            tracing::warn!("terminating unhealthy connection");
                            break;
                        }

                        is_alive = false;
                        tracing::debug!("pinging the client");
                        client_send.send(warp::ws::Message::ping(vec![])).await?;
                    }

                    // Mark the connection as alive on any pong.
                    Some(Ok(Message::Pong)) => {
                        tracing::debug!("received pong");
                        is_alive = true;
                    }

                    // Connection closed
                    Some(Ok(Message::Done)) => {
                        tracing::info!("connection closed");
                        break;
                    }

                    // WebSocket Error
                    Some(Err(err)) => {
                        tracing::error!("websocket error: {}", err);
                    }

                    None => {
                        // Unreachable because of the interval stream
                        unreachable!("should never yield None");
                    }
                }

                client_msg = client_recv.next();
                server_msg = p_server_msg;
            }

            // From Server
            Either::Right((from_server, p_client_msg)) => {
                match from_server {
                    // Serialized LSP Message
                    Some(Ok(text)) => {
                        if ctx.remap {
                            if let Ok(mut msg) = lsp::Message::from_str(&text) {
                                lsp::ext::remap_relative_uri(&mut msg, &ctx.cwd)?;
                                tracing::debug!("remapped relative URI from server");
                                let text = serde_json::to_string(&msg)?;
                                tracing::debug!("<- {}", text);
                                client_send.send(warp::ws::Message::text(text)).await?;
                            } else {
                                tracing::warn!("<- {}", text);
                                client_send.send(warp::ws::Message::text(text)).await?;
                            }
                        } else {
                            tracing::debug!("<- {}", text);
                            client_send.send(warp::ws::Message::text(text)).await?;
                        }
                    }

                    // Codec Error
                    Some(Err(err)) => {
                        tracing::error!("{}", err);
                    }

                    // Server exited
                    None => {
                        tracing::error!("server process exited unexpectedly");
                        client_send.send(warp::ws::Message::close()).await?;
                        break;
                    }
                }

                client_msg = p_client_msg;
                server_msg = server_recv.next();
            }
        }
    }
    Ok(())
}

// Type to describe a message from the client conveniently.
#[allow(clippy::large_enum_variant)]
#[allow(clippy::enum_variant_names)]
enum Message {
    // Valid LSP message
    Message(lsp::Message),
    // Invalid JSON
    Invalid(String),
    // Close message
    Close,
    // Ping the client to keep the connection alive.
    // Note that this is from the interval stream and not actually from client.
    Tick,
    // Client disconnected. Necessary because the combined stream is infinite.
    Done,
    // A reply for ping or heartbeat from client.
    Pong,
}

// Parse the message and ignore anything we don't care.
async fn filter_map_warp_ws_message(
    wsm: Result<warp::ws::Message, warp::Error>,
) -> Option<Result<Message, warp::Error>> {
    match wsm {
        Ok(msg) => {
            if msg.is_close() {
                Some(Ok(Message::Close))
            } else if msg.is_text() {
                let text = msg.to_str().expect("text");
                match lsp::Message::from_str(text) {
                    Ok(msg) => Some(Ok(Message::Message(msg))),
                    Err(_) => Some(Ok(Message::Invalid(text.to_owned()))),
                }
            } else if msg.is_pong() {
                Some(Ok(Message::Pong))
            } else {
                // Ignore any other message types
                None
            }
        }

        Err(err) => Some(Err(err)),
    }
}
