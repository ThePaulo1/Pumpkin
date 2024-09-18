use std::{collections::HashMap, sync::Arc};

pub mod player_chunker;

use mio::Token;
use num_traits::ToPrimitive;
use parking_lot::Mutex;
use pumpkin_config::BasicConfiguration;
use pumpkin_core::math::vector2::Vector2;
use pumpkin_entity::{entity_type::EntityType, EntityId};
use pumpkin_protocol::{
    client::play::{
        CChunkData, CGameEvent, CLogin, CPlayerAbilities, CPlayerInfoUpdate, CRemoveEntities,
        CRemovePlayerInfo, CSetEntityMetadata, CSpawnEntity, GameEvent, Metadata, PlayerAction,
    },
    ClientPacket, VarInt,
};
use pumpkin_world::level::Level;
use tokio::sync::mpsc;

use crate::{
    client::Client,
    entity::{player::Player, Entity},
};

/// Represents a Minecraft world, containing entities, players, and the underlying level data.
///
/// Each dimension (Overworld, Nether, End) typically has its own `World`.
///
/// **Key Responsibilities:**
///
/// - Manages the `Level` instance for handling chunk-related operations.
/// - Stores and tracks active `Player` entities within the world.
/// - Provides a central hub for interacting with the world's entities and environment.
pub struct World {
    /// The underlying level, responsible for chunk management and terrain generation.
    pub level: Arc<Mutex<Level>>,
    /// A map of active players within the world, keyed by their unique id.
    pub current_players: Arc<Mutex<HashMap<u32, Arc<Player>>>>,
    // TODO: entities
}

impl World {
    pub fn load(level: Level) -> Self {
        Self {
            level: Arc::new(Mutex::new(level)),
            current_players: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Broadcasts a packet to all connected players within the world.
    ///
    /// Sends the specified packet to every player currently logged in to the server.
    ///
    /// **Note:** This function acquires a lock on the `current_players` map, ensuring thread safety.
    pub fn broadcast_packet_all<P>(&self, packet: &P)
    where
        P: ClientPacket,
    {
        let current_players = self.current_players.lock();
        for (_, player) in current_players.iter() {
            player.client.send_packet(packet);
        }
    }

    /// Broadcasts a packet to all connected players within the world, excluding the specified players.
    ///
    /// Sends the specified packet to every player currently logged in to the server, excluding the players listed in the `except` parameter.
    ///
    /// **Note:** This function acquires a lock on the `current_players` map, ensuring thread safety.
    pub fn broadcast_packet_expect<P>(&self, except: &[Token], packet: &P)
    where
        P: ClientPacket,
    {
        let current_players = self.current_players.lock();
        for (_, player) in current_players.iter().filter(|c| !except.contains(c.0)) {
            player.client.send_packet(packet);
        }
    }

    pub async fn spawn_player(&self, base_config: &BasicConfiguration, player: Arc<Player>) {
        // This code follows the vanilla packet order
        let entity_id = player.entity_id();
        let gamemode = player.gamemode.load();
        log::debug!("spawning player, entity id {}", entity_id);

        // login packet for our new player
        player.client.send_packet(&CLogin::new(
            entity_id,
            base_config.hardcore,
            &["minecraft:overworld"],
            base_config.max_players.into(),
            base_config.view_distance.into(), //  TODO: view distance
            base_config.simulation_distance.into(), // TODO: sim view dinstance
            false,
            false,
            false,
            0.into(),
            "minecraft:overworld",
            0, // seed
            gamemode.to_u8().unwrap(),
            base_config.default_gamemode.to_i8().unwrap(),
            false,
            false,
            None,
            0.into(),
            false,
        ));
        dbg!("sending abilities");
        // player abilities
        // TODO: this is for debug purpose, remove later
        player
            .client
            .send_packet(&CPlayerAbilities::new(0x02, 0.4, 0.1));

        // teleport
        let x = 10.0;
        let y = 120.0;
        let z = 10.0;
        let yaw = 10.0;
        let pitch = 10.0;
        player.teleport(x, y, z, 10.0, 10.0);
        let gameprofile = &player.gameprofile;
        // first send info update to our new player, So he can see his Skin
        // also send his info to everyone else
        self.broadcast_packet_all(&CPlayerInfoUpdate::new(
            0x01 | 0x08,
            &[pumpkin_protocol::client::play::Player {
                uuid: gameprofile.id,
                actions: vec![
                    PlayerAction::AddPlayer {
                        name: gameprofile.name.clone(),
                        properties: gameprofile.properties.clone(),
                    },
                    PlayerAction::UpdateListed(true),
                ],
            }],
        ));

        // here we send all the infos of already joined players
        let mut entries = Vec::new();
        for (_, playerr) in self
            .current_players
            .lock()
            .iter()
            .filter(|(c, _)| **c != player.client.token)
        {
            let gameprofile = &playerr.gameprofile;
            entries.push(pumpkin_protocol::client::play::Player {
                uuid: gameprofile.id,
                actions: vec![
                    PlayerAction::AddPlayer {
                        name: gameprofile.name.clone(),
                        properties: gameprofile.properties.clone(),
                    },
                    PlayerAction::UpdateListed(true),
                ],
            })
        }
        player
            .client
            .send_packet(&CPlayerInfoUpdate::new(0x01 | 0x08, &entries));

        let gameprofile = &player.gameprofile;

        // spawn player for every client
        self.broadcast_packet_expect(
            &[player.client.token],
            // TODO: add velo
            &CSpawnEntity::new(
                entity_id.into(),
                gameprofile.id,
                (EntityType::Player as i32).into(),
                x,
                y,
                z,
                pitch,
                yaw,
                yaw,
                0.into(),
                0.0,
                0.0,
                0.0,
            ),
        );
        // spawn players for our client
        let token = player.client.token;
        for (_, existing_player) in self.current_players.lock().iter().filter(|c| c.0 != &token) {
            let entity = &existing_player.entity;
            let pos = entity.pos.load();
            let gameprofile = &existing_player.gameprofile;
            player.client.send_packet(&CSpawnEntity::new(
                existing_player.entity_id().into(),
                gameprofile.id,
                (EntityType::Player as i32).into(),
                pos.x,
                pos.y,
                pos.z,
                entity.yaw.load(),
                entity.pitch.load(),
                entity.head_yaw.load(),
                0.into(),
                0.0,
                0.0,
                0.0,
            ))
        }
        // entity meta data
        // set skin parts
        if let Some(config) = player.client.config.lock().as_ref() {
            let packet = CSetEntityMetadata::new(
                entity_id.into(),
                Metadata::new(17, VarInt(0), config.skin_parts),
            );
            self.broadcast_packet_all(&packet)
        }

        // Start waiting for level chunks, Sets the "Loading Terrain" screen
        player
            .client
            .send_packet(&CGameEvent::new(GameEvent::StartWaitingChunks, 0.0));

        // Spawn in inital chunks
        player_chunker::player_join(self, player.clone()).await;
    }

    async fn spawn_world_chunks(&self, client: &Client, chunks: Vec<Vector2<i32>>, distance: i32) {
        let inst = std::time::Instant::now();
        let (sender, mut chunk_receiver) = mpsc::channel(distance as usize);

        let level = self.level.clone();
        let closed = client.closed.load(std::sync::atomic::Ordering::Relaxed);
        let chunks = Arc::new(chunks);
        tokio::task::spawn_blocking(move || level.lock().fetch_chunks(&chunks, sender, closed));

        while let Some(chunk_data) = chunk_receiver.recv().await {
            // dbg!(chunk_pos);
            let chunk_data = match chunk_data {
                Ok(d) => d,
                Err(_) => continue,
            };
            #[cfg(debug_assertions)]
            if chunk_data.position == (0, 0).into() {
                use pumpkin_protocol::bytebuf::ByteBuffer;
                let mut test = ByteBuffer::empty();
                CChunkData(&chunk_data).write(&mut test);
                let len = test.buf().len();
                log::debug!(
                    "Chunk packet size: {}B {}KB {}MB",
                    len,
                    len / 1024,
                    len / (1024 * 1024)
                );
            }
            if !client.closed.load(std::sync::atomic::Ordering::Relaxed) {
                client.send_packet(&CChunkData(&chunk_data));
            }
        }
        dbg!("DONE CHUNKS", inst.elapsed());
    }

    /// Gets a Player by entity id
    pub fn get_player_by_entityid(&self, id: EntityId) -> Option<Arc<Player>> {
        for (_, player) in self.current_players.lock().iter() {
            if player.entity_id() == id {
                return Some(player.clone());
            }
        }
        None
    }

    /// Gets a Player by name
    pub fn get_player_by_name(&self, name: &str) -> Option<Arc<Player>> {
        for (_, player) in self.current_players.lock().iter() {
            if player.gameprofile.name == name {
                return Some(player.clone());
            }
        }
        None
    }

    pub fn add_player(&self, id: u32, player: Arc<Player>) {
        self.current_players.lock().insert(id, player);
    }

    pub fn remove_player(&self, player: &Player) {
        self.current_players
            .lock()
            .remove(&player.client.id)
            .unwrap();
        let uuid = player.gameprofile.id;
        self.broadcast_packet_expect(
            &[player.client.id],
            &CRemovePlayerInfo::new(1.into(), &[uuid]),
        );
        self.remove_entity(&player.entity);
    }

    pub fn remove_entity(&self, entity: &Entity) {
        self.broadcast_packet_all(&CRemoveEntities::new(&[entity.entity_id.into()]))
    }
}
