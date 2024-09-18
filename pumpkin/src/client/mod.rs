use std::{
    io::{self, Write},
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicI32},
        Arc,
    },
};

use crate::{
    entity::player::{ChatMode, Hand},
    server::Server,
};

use authentication::GameProfile;
use crossbeam::atomic::AtomicCell;
use parking_lot::Mutex;
use pumpkin_core::text::TextComponent;
use pumpkin_protocol::{
    bytebuf::{packet_id::Packet, DeserializerError},
    client::{config::CConfigDisconnect, login::CLoginDisconnect, play::CPlayDisconnect},
    packet_decoder::PacketDecoder,
    packet_encoder::PacketEncoder,
    server::{
        config::{SAcknowledgeFinishConfig, SClientInformationConfig, SKnownPacks, SPluginMessage},
        handshake::SHandShake,
        login::{SEncryptionResponse, SLoginAcknowledged, SLoginPluginResponse, SLoginStart},
        status::{SStatusPingRequest, SStatusRequest},
    },
    ClientPacket, ConnectionState, PacketError, RawPacket, ServerPacket,
};
use tokio::{
    io::{AsyncReadExt, ReadHalf},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpStream,
    },
};

use std::io::Read;
use thiserror::Error;

pub mod authentication;
mod client_packet;
mod container;
pub mod player_packet;

/// Represents a player's configuration settings.
///
/// This struct contains various options that can be customized by the player, affecting their gameplay experience.
///
/// **Usage:**
///
/// This struct is typically used to store and manage a player's preferences. It can be sent to the server when a player joins or when they change their settings.
#[derive(Clone)]
pub struct PlayerConfig {
    /// The player's preferred language.
    pub locale: String, // 16
    /// The maximum distance at which chunks are rendered.
    pub view_distance: i8,
    /// The player's chat mode settings
    pub chat_mode: ChatMode,
    /// Whether chat colors are enabled.
    pub chat_colors: bool,
    /// The player's skin configuration options.
    pub skin_parts: u8,
    /// The player's dominant hand (left or right).
    pub main_hand: Hand,
    /// Whether text filtering is enabled.
    pub text_filtering: bool,
    /// Whether the player wants to appear in the server list.
    pub server_listing: bool,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            locale: "en_us".to_string(),
            view_distance: 2,
            chat_mode: ChatMode::Enabled,
            chat_colors: true,
            skin_parts: 0,
            main_hand: Hand::Main,
            text_filtering: false,
            server_listing: false,
        }
    }
}

/// Everything which makes a Conection with our Server is a `Client`.
/// Client will become Players when they reach the `Play` state
pub struct Client {
    /// The client's game profile information.
    pub gameprofile: Mutex<Option<GameProfile>>,
    /// The client's configuration settings, Optional
    pub config: Mutex<Option<PlayerConfig>>,
    /// The client's brand or modpack information, Optional.
    pub brand: Mutex<Option<String>>,
    /// The minecraft protocol version used by the client.
    pub protocol_version: AtomicI32,
    /// The current connection state of the client (e.g., Handshaking, Status, Play).
    pub connection_state: AtomicCell<ConnectionState>,
    /// Whether encryption is enabled for the connection.
    pub encryption: AtomicBool,
    /// Indicates if the client connection is closed.
    pub closed: AtomicBool,
    /// A unique id identifying the client.
    pub id: u32,
    pub connection_writer: Mutex<OwnedWriteHalf>,
    pub connection_reader: Mutex<OwnedReadHalf>,
    /// The client's IP address.
    pub address: Mutex<SocketAddr>,
    /// The packet encoder for outgoing packets.
    enc: Arc<Mutex<PacketEncoder>>,
    /// The packet decoder for incoming packets.
    dec: Arc<Mutex<PacketDecoder>>,
    /// A queue of raw packets received from the client, waiting to be processed.
    pub client_packets_queue: Arc<Mutex<Vec<RawPacket>>>,

    /// Indicates whether the client should be converted into a player.
    pub make_player: AtomicBool,
}

impl Client {
    pub fn new(id: u32, connection: TcpStream, address: SocketAddr) -> Self {
        let (connection_reader, connection_writer) = connection.into_split();
        Self {
            protocol_version: AtomicI32::new(0),
            gameprofile: Mutex::new(None),
            config: Mutex::new(None),
            brand: Mutex::new(None),
            id,
            address: Mutex::new(address),
            connection_state: AtomicCell::new(ConnectionState::HandShake),
            enc: Arc::new(Mutex::new(PacketEncoder::default())),
            dec: Arc::new(Mutex::new(PacketDecoder::default())),
            encryption: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            client_packets_queue: Arc::new(Mutex::new(Vec::new())),
            make_player: AtomicBool::new(false),
            connection_reader: Mutex::new(connection_reader),
            connection_writer: Mutex::new(connection_writer),
        }
    }

    /// Adds a Incoming packet to the queue
    pub fn add_packet(&self, packet: RawPacket) {
        let mut client_packets_queue = self.client_packets_queue.lock();
        client_packets_queue.push(packet);
    }

    /// Enables encryption
    pub fn enable_encryption(
        &self,
        shared_secret: &[u8], // decrypted
    ) -> Result<(), EncryptionError> {
        self.encryption
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let crypt_key: [u8; 16] = shared_secret
            .try_into()
            .map_err(|_| EncryptionError::SharedWrongLength)?;
        self.dec.lock().enable_encryption(&crypt_key);
        self.enc.lock().enable_encryption(&crypt_key);
        Ok(())
    }

    /// Compression threshold, Compression level
    pub fn set_compression(&self, compression: Option<(u32, u32)>) {
        self.dec.lock().set_compression(compression.map(|v| v.0));
        self.enc.lock().set_compression(compression);
    }

    /// Send a Clientbound Packet to the Client
    pub fn send_packet<P: ClientPacket>(&self, packet: &P) {
        // assert!(!self.closed);
        let mut enc = self.enc.lock();
        enc.append_packet(packet)
            .unwrap_or_else(|e| self.kick(&e.to_string()));
        self.connection
            .lock()
            .write_all(&enc.take())
            .map_err(|_| PacketError::ConnectionWrite)
            .unwrap_or_else(|e| self.kick(&e.to_string()));
    }

    pub fn try_send_packet<P: ClientPacket>(&self, packet: &P) -> Result<(), PacketError> {
        // assert!(!self.closed);

        let mut enc = self.enc.lock();
        enc.append_packet(packet)?;
        self.connection
            .lock()
            .write_all(&enc.take())
            .map_err(|_| PacketError::ConnectionWrite)?;
        Ok(())
    }

    /// Processes all packets send by the client
    pub async fn process_packets(&self, server: &Arc<Server>) {
        while let Some(mut packet) = self.client_packets_queue.lock().pop() {
            match self.handle_packet(server, &mut packet).await {
                Ok(_) => {}
                Err(e) => {
                    let text = format!("Error while reading incoming packet {}", e);
                    log::error!("{}", text);
                    self.kick(&text)
                }
            };
        }
    }

    /// Handles an incoming decoded not Play state Packet
    pub async fn handle_packet(
        &self,
        server: &Arc<Server>,
        packet: &mut RawPacket,
    ) -> Result<(), DeserializerError> {
        // TODO: handle each packet's Error instead of calling .unwrap()
        let bytebuf = &mut packet.bytebuf;
        match self.connection_state.load() {
            pumpkin_protocol::ConnectionState::HandShake => match packet.id.0 {
                SHandShake::PACKET_ID => {
                    self.handle_handshake(server, SHandShake::read(bytebuf)?);
                    Ok(())
                }
                _ => {
                    log::error!(
                        "Failed to handle packet id {} while in Handshake state",
                        packet.id.0
                    );
                    Ok(())
                }
            },
            pumpkin_protocol::ConnectionState::Status => match packet.id.0 {
                SStatusRequest::PACKET_ID => {
                    self.handle_status_request(server, SStatusRequest::read(bytebuf)?);
                    Ok(())
                }
                SStatusPingRequest::PACKET_ID => {
                    self.handle_ping_request(server, SStatusPingRequest::read(bytebuf)?);
                    Ok(())
                }
                _ => {
                    log::error!(
                        "Failed to handle packet id {} while in Status state",
                        packet.id.0
                    );
                    Ok(())
                }
            },
            // TODO: Check config if transfer is enabled
            pumpkin_protocol::ConnectionState::Login
            | pumpkin_protocol::ConnectionState::Transfer => match packet.id.0 {
                SLoginStart::PACKET_ID => {
                    self.handle_login_start(server, SLoginStart::read(bytebuf)?);
                    Ok(())
                }
                SEncryptionResponse::PACKET_ID => {
                    self.handle_encryption_response(server, SEncryptionResponse::read(bytebuf)?)
                        .await;
                    Ok(())
                }
                SLoginPluginResponse::PACKET_ID => {
                    self.handle_plugin_response(server, SLoginPluginResponse::read(bytebuf)?);
                    Ok(())
                }
                SLoginAcknowledged::PACKET_ID => {
                    self.handle_login_acknowledged(server, SLoginAcknowledged::read(bytebuf)?);
                    Ok(())
                }
                _ => {
                    log::error!(
                        "Failed to handle packet id {} while in Login state",
                        packet.id.0
                    );
                    Ok(())
                }
            },
            pumpkin_protocol::ConnectionState::Config => match packet.id.0 {
                SClientInformationConfig::PACKET_ID => {
                    self.handle_client_information_config(
                        server,
                        SClientInformationConfig::read(bytebuf)?,
                    );
                    Ok(())
                }
                SPluginMessage::PACKET_ID => {
                    self.handle_plugin_message(server, SPluginMessage::read(bytebuf)?);
                    Ok(())
                }
                SAcknowledgeFinishConfig::PACKET_ID => {
                    self.handle_config_acknowledged(
                        server,
                        SAcknowledgeFinishConfig::read(bytebuf)?,
                    )
                    .await;
                    Ok(())
                }
                SKnownPacks::PACKET_ID => {
                    self.handle_known_packs(server, SKnownPacks::read(bytebuf)?);
                    Ok(())
                }
                _ => {
                    log::error!(
                        "Failed to handle packet id {} while in Config state",
                        packet.id.0
                    );
                    Ok(())
                }
            },
            _ => {
                log::error!("Invalid Connection state {:?}", self.connection_state);
                Ok(())
            }
        }
    }

    /// Reads the connection until our buffer of len 4096 is full, then decode
    /// Close connection when an error occurs or when the Client closed the connection
    pub async fn poll(&self) {
        let mut received_data = vec![0; 4096];
        // We can (maybe) read from the connection.
        while !self.closed.load(std::sync::atomic::Ordering::Relaxed) {
            // self.connection.readable().await.expect(":c");
            match self.connection_reader.lock().read(&mut received_data).await {
                Ok(0) => {
                    // Reading 0 bytes means the other side has closed the
                    // connection or is done writing, then so are we.
                    self.close();
                    break;
                }
                Ok(n) => {
                    dbg!(n);
                    received_data.extend(&vec![0; n]);
                    let mut dec = self.dec.lock();
                    dec.queue_slice(&received_data);
                    match dec.decode() {
                        Ok(packet) => {
                            if let Some(packet) = packet {
                                self.add_packet(packet);
                            }
                        }
                        Err(err) => self.kick(&err.to_string()),
                    }
                    dec.clear();
                }
                // Would block "errors" are the OS's way of saying that the
                // connection is not actually ready to perform this I/O operation.
                Err(ref err) if would_block(err) => break,
                Err(ref err) if interrupted(err) => continue,
                // Other errors we'll consider fatal.
                Err(_) => self.close(),
            }
        }
    }

    /// Kicks the Client with a reason depending on the connection state
    pub fn kick(&self, reason: &str) {
        dbg!(reason);
        match self.connection_state.load() {
            ConnectionState::Login => {
                self.try_send_packet(&CLoginDisconnect::new(
                    &serde_json::to_string_pretty(&reason).unwrap_or("".into()),
                ))
                .unwrap_or_else(|_| self.close());
            }
            ConnectionState::Config => {
                self.try_send_packet(&CConfigDisconnect::new(reason))
                    .unwrap_or_else(|_| self.close());
            }
            // So we can also kick on errors, but generally should use Player::kick
            ConnectionState::Play => {
                self.try_send_packet(&CPlayDisconnect::new(&TextComponent::text(reason)))
                    .unwrap_or_else(|_| self.close());
            }
            _ => {
                log::warn!("Can't kick in {:?} State", self.connection_state)
            }
        }
        self.close()
    }

    /// You should prefer to use `kick` when you can
    pub fn close(&self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[derive(Error, Debug)]
pub enum EncryptionError {
    #[error("failed to decrypt shared secret")]
    FailedDecrypt,
    #[error("shared secret has the wrong length")]
    SharedWrongLength,
}

fn would_block(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::WouldBlock
}

pub fn interrupted(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::Interrupted
}
