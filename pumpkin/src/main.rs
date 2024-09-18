#![expect(clippy::await_holding_lock)]

#[cfg(target_os = "wasi")]
compile_error!("Compiling for WASI targets is not supported!");

use std::collections::HashMap;
use std::io::{self, Read};
use std::sync::Mutex;

use client::{interrupted, Client};
use server::Server;
use tokio::net::TcpListener;

// Setup some tokens to allow us to identify which event is for which socket.

pub mod client;
pub mod commands;
pub mod entity;
pub mod proxy;
pub mod rcon;
pub mod server;
pub mod util;
pub mod world;

#[tokio::main]
async fn main() -> io::Result<()> {
    use std::sync::Arc;

    use entity::player::Player;
    use pumpkin_config::{ADVANCED_CONFIG, BASIC_CONFIG};
    use pumpkin_core::text::{color::NamedColor, TextComponent};
    use rcon::RCONServer;

    simple_logger::SimpleLogger::new()
        .with_level(log::LevelFilter::Info)
        .init()
        .unwrap();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    ctrlc::set_handler(|| {
        log::warn!(
            "{}",
            TextComponent::text("Stopping Server")
                .color_named(NamedColor::Red)
                .to_pretty_console()
        );
        std::process::exit(0);
    })
    .unwrap();
    // ensure rayon is built outside of tokio scope
    rayon::ThreadPoolBuilder::new().build_global().unwrap();
    rt.block_on(async {
        use std::time::Instant;

        let time = Instant::now();

        // Setup the TCP server socket.
        let addr = BASIC_CONFIG.server_address;
        let listener = TcpListener::bind(addr).await?;

        // Unique token for each incoming connection.
        let mut unique_token = 0;

        let use_console = ADVANCED_CONFIG.commands.use_console;
        let rcon = ADVANCED_CONFIG.rcon.clone();

        let clients: Arc<Mutex<HashMap<u32, Arc<Client>>>> = Arc::new(Mutex::new(HashMap::new()));
        let players: Arc<Mutex<HashMap<u32, Arc<Player>>>> = Arc::new(Mutex::new(HashMap::new()));

        let server = Arc::new(Server::new());
        log::info!("Started Server took {}ms", time.elapsed().as_millis());
        log::info!("You now can connect to the server, Listening on {}", addr);

        if use_console {
            let server = server.clone();
            tokio::spawn(async move {
                let stdin = std::io::stdin();
                loop {
                    let mut out = String::new();
                    stdin
                        .read_line(&mut out)
                        .expect("Failed to read console line");

                    if !out.is_empty() {
                        let dispatcher = server.command_dispatcher.clone();
                        dispatcher.handle_command(
                            &mut commands::CommandSender::Console,
                            &server,
                            &out,
                        );
                    }
                }
            });
        }
        if rcon.enabled {
            let server = server.clone();
            tokio::spawn(async move {
                RCONServer::new(&rcon, server).await.unwrap();
            });
        }

        loop {
            let (socket, address) = listener.accept().await?;

            // Received an event for the TCP server socket, which
            // indicates we can accept an connection.

            if let Err(e) = socket.set_nodelay(true) {
                log::warn!("failed to set TCP_NODELAY {e}");
            }

            log::info!("Accepted connection from: {}", address);

            unique_token += 1;
            let token = unique_token;
            let client = Client::new(token, socket, addr);
            let mut clients = clients.lock().unwrap();
            clients.insert(token, Arc::new(client));
            let players = players.clone();

            tokio::spawn(async move {
                // Poll Players
                let players = players.clone();
                if let Some(player) = players.lock().unwrap().get_mut(&token) {
                    player.client.poll().await;
                    let closed = player
                        .client
                        .closed
                        .load(std::sync::atomic::Ordering::Relaxed);
                    if !closed {
                        player.process_packets(&server).await;
                    }
                    if closed {
                        if let Some(player) = players.lock().unwrap().remove(&token) {
                            player.remove().await;
                        }
                    }
                };

                // Poll current Clients (non players)
                // Maybe received an event for a TCP connection.
                let (done, make_player) = if let Some(client) = clients.get_mut(&token) {
                    client.poll().await;
                    let closed = client.closed.load(std::sync::atomic::Ordering::Relaxed);
                    if !closed {
                        client.process_packets(&server).await;
                    }
                    (
                        closed,
                        client
                            .make_player
                            .load(std::sync::atomic::Ordering::Relaxed),
                    )
                } else {
                    // Sporadic events happen, we can safely ignore them.
                    (false, false)
                };
                if done || make_player {
                    if let Some(client) = clients.remove(&token) {
                        if done {
                        } else if make_player {
                            let token = client.id;
                            let (player, world) = server.add_player(token, *client).await;
                            players.lock().unwrap().insert(token, player.clone());
                            world.spawn_player(&BASIC_CONFIG, player).await;
                        }
                    }
                }
            });
        }
    })
}
