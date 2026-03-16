use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Song {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub image: String,
}

#[derive(Clone, Debug)]
pub struct RichPresenceConfig {
    pub logo_url: String,
    pub game_name: String,
    pub discord_app_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PresenceState {
    Default,
    UnknownSong(i32),
    Song { id: i32, difficulty: i32 },
}
