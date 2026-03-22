use crate::logging::{log_message as log, runtime_base_dir};
use crate::types::Song;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Duration;

pub(crate) type SongsById = Arc<RwLock<HashMap<i32, Song>>>;

const SONGS_URL: &str =
    "https://raw.githubusercontent.com/beer-psi/chuni-penguin/refs/heads/trunk/chuni_penguin/database/seeds/songs.json";
const SONGS_FETCH_RETRIES: usize = 5;
const SONGS_FETCH_RETRY_DELAY: Duration = Duration::from_millis(500);
const SONGS_CACHE_FILE_NAME: &str = "chunithm_songs_cache.json";

#[derive(Debug)]
enum SongsCacheLoadError {
    Read(std::io::Error),
    Parse(serde_json::Error),
}

impl fmt::Display for SongsCacheLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(error) => write!(f, "failed to read songs cache: {error}"),
            Self::Parse(error) => write!(f, "failed to parse songs cache JSON: {error}"),
        }
    }
}

#[derive(Debug)]
enum SongsRefreshError {
    Request(reqwest::Error),
    ReadBody(reqwest::Error),
    Parse(serde_json::Error),
}

impl fmt::Display for SongsRefreshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request(error) => write!(f, "request failed: {error}"),
            Self::ReadBody(error) => write!(f, "failed to read songs response body: {error}"),
            Self::Parse(error) => write!(f, "failed to parse songs JSON: {error}"),
        }
    }
}

fn parse_songs_json(bytes: &[u8]) -> Result<Vec<Song>, serde_json::Error> {
    let json_bytes = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &bytes[3..]
    } else {
        bytes
    };

    serde_json::from_slice::<Vec<Song>>(json_bytes)
}

fn songs_to_map(songs: Vec<Song>) -> HashMap<i32, Song> {
    songs
        .into_iter()
        .filter_map(|song| match song.id.parse::<i32>() {
            Ok(song_id) => Some((song_id, song)),
            Err(_) => {
                log(format!("Skipping song with non-numeric ID: {}", song.id));
                None
            }
        })
        .collect::<HashMap<_, _>>()
}

fn load_songs_from_cache() -> Result<HashMap<i32, Song>, SongsCacheLoadError> {
    let cache_path = runtime_base_dir().join(SONGS_CACHE_FILE_NAME);
    let cached_bytes = fs::read(&cache_path).map_err(SongsCacheLoadError::Read)?;
    let songs = parse_songs_json(&cached_bytes).map_err(SongsCacheLoadError::Parse)?;
    let songs_by_id = songs_to_map(songs);
    log(format!(
        "Loaded {} songs from local cache {}",
        songs_by_id.len(),
        cache_path.display()
    ));
    Ok(songs_by_id)
}

fn fetch_online_songs_once(
    client: &reqwest::blocking::Client,
) -> Result<(Vec<Song>, Vec<u8>), SongsRefreshError> {
    let response = client
        .get(SONGS_URL)
        .header(reqwest::header::USER_AGENT, "ChuniRichPresence/0.1")
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .and_then(|response| response.error_for_status())
        .map_err(SongsRefreshError::Request)?;

    let body = response.bytes().map_err(SongsRefreshError::ReadBody)?;

    let body_vec = body.to_vec();
    let songs = parse_songs_json(&body_vec).map_err(SongsRefreshError::Parse)?;

    Ok((songs, body_vec))
}

fn refresh_songs_from_online(shared_songs: SongsById) {
    log(format!("Refreshing songs from URL {}", SONGS_URL));

    let client = match reqwest::blocking::Client::builder().http1_only().build() {
        Ok(client) => client,
        Err(error) => {
            log(format!(
                "Failed to create HTTP client for songs refresh: {}",
                error
            ));
            return;
        }
    };

    let cache_path = runtime_base_dir().join(SONGS_CACHE_FILE_NAME);
    for attempt in 1..=SONGS_FETCH_RETRIES {
        match fetch_online_songs_once(&client) {
            Ok((loaded_songs, body_bytes)) => {
                let songs_by_id = songs_to_map(loaded_songs);

                if let Err(error) = fs::write(&cache_path, &body_bytes) {
                    log(format!(
                        "Warning: failed to write songs cache {}: {}",
                        cache_path.display(),
                        error
                    ));
                } else {
                    log(format!("Updated songs cache at {}", cache_path.display()));
                }

                match shared_songs.write() {
                    Ok(mut songs_guard) => {
                        *songs_guard = songs_by_id;
                        log("Songs dataset refreshed from online source".to_string());
                    }
                    Err(error) => {
                        log(format!("Failed to update shared songs dataset: {}", error));
                    }
                }
                return;
            }
            Err(error) => {
                if attempt < SONGS_FETCH_RETRIES {
                    log(format!(
                        "Online songs refresh attempt {}/{} failed: {}",
                        attempt, SONGS_FETCH_RETRIES, error
                    ));
                    thread::sleep(SONGS_FETCH_RETRY_DELAY);
                } else {
                    log(format!(
                        "Online songs refresh failed after {} attempts: {}. Keeping local dataset.",
                        SONGS_FETCH_RETRIES, error
                    ));
                }
            }
        }
    }
}

pub(crate) fn load_initial_songs() -> SongsById {
    Arc::new(RwLock::new(match load_songs_from_cache() {
        Ok(songs) => songs,
        Err(error) => {
            log(format!(
                "Failed to load local songs cache: {}. Starting with empty dataset.",
                error
            ));
            HashMap::new()
        }
    }))
}

pub(crate) fn start_songs_refresh(shared_songs: &SongsById) {
    let songs_refresh_handle = Arc::clone(shared_songs);
    thread::spawn(move || refresh_songs_from_online(songs_refresh_handle));
}

pub(crate) fn get_song_by_id(shared_songs: &SongsById, song_id: i32) -> Option<Song> {
    shared_songs
        .read()
        .ok()
        .and_then(|songs_map| songs_map.get(&song_id).cloned())
}
