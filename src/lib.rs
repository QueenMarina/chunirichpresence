mod logging;
mod memory;
mod types;

use configparser::ini::Ini;
use discord_rich_presence::{activity, DiscordIpc, DiscordIpcClient};
use std::collections::HashMap;
use std::ffi::c_void;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};
use logging::{debug_logging_enabled, log_file_path, runtime_base_dir, set_dll_module};
use memory::{
    log_memory_probe_status, log_memory_snapshot, log_resolved_game_module,
    read_presence_state_from_memory,
};
use types::{PresenceState, RichPresenceConfig, Song};
use windows_sys::Win32::Foundation::{CloseHandle, BOOL, HMODULE, TRUE};
use windows_sys::Win32::System::LibraryLoader::DisableThreadLibraryCalls;
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows_sys::Win32::System::Threading::CreateThread;

// music.json gives us all songs from the japanese release, we can then use image to get image url to give to discord
// Taken from here https://github.com/zetaraku/arcade-songs-fetch/blob/master/src/chunithm/fetch-songs.ts
const SONGS_URL: &str = "https://chunithm.sega.jp/storage/json/music.json";
const SONG_IMAGE_BASE_URL: &str = "https://new.chunithm-net.com/chuni-mobile/html/mobile/img/";

// User definable values
const DEFAULT_LOGO_URL: &str = "https://chunithm.org/assets/logo.png";
const DEFAULT_GAME_NAME: &str = "Chunithm";
const DEFAULT_DISCORD_APP_ID: &str = "1482780703128289493";

const DISCORD_RECONNECT_INTERVAL: Duration = Duration::from_secs(10);
const SONGS_FETCH_RETRIES: usize = 5;
const SONGS_FETCH_RETRY_DELAY: Duration = Duration::from_millis(500);
const SONGS_CACHE_FILE_NAME: &str = "chunithm_songs_cache.json";
const SEGATOOLS_INI_FILE_NAME: &str = "segatools.ini";

macro_rules! log {
    ($($arg:tt)*) => {{
        crate::logging::log_message(format!($($arg)*));
    }};
}

impl Default for RichPresenceConfig {
    fn default() -> Self {
        Self {
            logo_url: DEFAULT_LOGO_URL.to_string(),
            game_name: DEFAULT_GAME_NAME.to_string(),
            discord_app_id: DEFAULT_DISCORD_APP_ID.to_string(),
        }
    }
}

fn difficulty_label(difficulty: i32) -> &'static str {
    match difficulty {
        0 => "BASIC",
        1 => "ADVANCED",
        2 => "EXPERT",
        3 => "MASTER",
        4 => "ULTIMA",
        _ => "UNKNOWN",
    }
}

fn songs_cache_path() -> PathBuf {
    runtime_base_dir().join(SONGS_CACHE_FILE_NAME)
}

fn segatools_ini_path() -> PathBuf {
    runtime_base_dir().join(SEGATOOLS_INI_FILE_NAME)
}

fn sanitize_ini_value(value: Option<String>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(stripped) = trimmed
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
    {
        let stripped = stripped.trim();
        if stripped.is_empty() {
            None
        } else {
            Some(stripped.to_string())
        }
    } else {
        Some(trimmed.to_string())
    }
}


fn load_presence_config() -> RichPresenceConfig {
    let ini_path = segatools_ini_path();
    let mut config = RichPresenceConfig::default();
    let mut ini = Ini::new();
    match ini.load(&ini_path) {
        Ok(_) => {}
        Err(error) => {
            log!(
                "failed to read {} ({}), using defaults",
                ini_path.display(),
                error
            );
            return config;
        }
    }

    if let Some(value) = sanitize_ini_value(ini.get("chunirichpresence", "logo_url")) {
        config.logo_url = value;
    }

    if let Some(value) = sanitize_ini_value(ini.get("chunirichpresence", "game_name")) {
        config.game_name = value;
    }

    if let Some(value) = sanitize_ini_value(ini.get("chunirichpresence", "discord_app_id")) {
        config.discord_app_id = value;
    }

    log!(
        "Loaded config from {} (game_name='{}', logo_url='{}', discord_app_id='{}')",
        ini_path.display(),
        config.game_name,
        config.logo_url,
        config.discord_app_id
    );

    config
}

fn presence_state_description(
    state: &PresenceState,
    songs_by_id: &Arc<RwLock<HashMap<i32, Song>>>,
) -> String {
    match state {
        PresenceState::Default => "default presence (song select)".to_string(),
        PresenceState::UnknownSong(difficulty) => {
            format!("unknown song ({})", difficulty_label(*difficulty))
        }
        PresenceState::Song { id, difficulty } => match get_song_by_id(songs_by_id, *id) {
            Some(song) => format!(
                "song {} - {} ({})",
                song.title,
                song.artist,
                difficulty_label(*difficulty)
            ),
            None => format!("song id {} missing from dataset ({})", id, difficulty_label(*difficulty)),
        },
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
                log!("Skipping song with non-numeric ID: {}", song.id);
                None
            }
        })
        .collect::<HashMap<_, _>>()
}

fn load_songs_from_cache() -> Result<HashMap<i32, Song>, Box<dyn std::error::Error>> {
    let cache_path = songs_cache_path();
    let cached_bytes = fs::read(&cache_path)?;
    let songs = parse_songs_json(&cached_bytes)?;
    let songs_by_id = songs_to_map(songs);
    log!(
        "Loaded {} songs from local cache {}",
        songs_by_id.len(),
        cache_path.display()
    );
    Ok(songs_by_id)
}

fn fetch_online_songs_once(
    client: &reqwest::blocking::Client,
) -> Result<(Vec<Song>, Vec<u8>), String> {
    let response = client
        .get(SONGS_URL)
        .header(reqwest::header::USER_AGENT, "ChuniRichPresence/0.1")
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .and_then(|response| response.error_for_status())
        .map_err(|error| format!("request failed: {}", error))?;

    let body = response
        .bytes()
        .map_err(|error| format!("failed to read songs response body: {}", error))?;

    let body_vec = body.to_vec();
    let songs = parse_songs_json(&body_vec)
        .map_err(|error| format!("failed to parse songs JSON: {}", error))?;

    Ok((songs, body_vec))
}

fn refresh_songs_from_online(shared_songs: Arc<RwLock<HashMap<i32, Song>>>) {
    log!("Refreshing songs from URL {}", SONGS_URL);

    let client = match reqwest::blocking::Client::builder().http1_only().build() {
        Ok(client) => client,
        Err(error) => {
            log!("Failed to create HTTP client for songs refresh: {}", error);
            return;
        }
    };

    let cache_path = songs_cache_path();
    for attempt in 1..=SONGS_FETCH_RETRIES {
        match fetch_online_songs_once(&client) {
            Ok((loaded_songs, body_bytes)) => {
                let songs_by_id = songs_to_map(loaded_songs);

                if let Err(error) = fs::write(&cache_path, &body_bytes) {
                    log!(
                        "Warning: failed to write songs cache {}: {}",
                        cache_path.display(),
                        error
                    );
                } else {
                    log!("Updated songs cache at {}", cache_path.display());
                }

                match shared_songs.write() {
                    Ok(mut songs_guard) => {
                        *songs_guard = songs_by_id;
                        log!("Songs dataset refreshed from online source");
                    }
                    Err(error) => {
                        log!("Failed to update shared songs dataset: {}", error);
                    }
                }
                return;
            }
            Err(error) => {
                if attempt < SONGS_FETCH_RETRIES {
                    log!(
                        "Online songs refresh attempt {}/{} failed: {}",
                        attempt,
                        SONGS_FETCH_RETRIES,
                        error
                    );
                    thread::sleep(SONGS_FETCH_RETRY_DELAY);
                } else {
                    log!(
                        "Online songs refresh failed after {} attempts: {}. Keeping local dataset.",
                        SONGS_FETCH_RETRIES,
                        error
                    );
                }
            }
        }
    }
}

fn get_song_by_id(shared_songs: &Arc<RwLock<HashMap<i32, Song>>>, song_id: i32) -> Option<Song> {
    shared_songs
        .read()
        .ok()
        .and_then(|songs_map| songs_map.get(&song_id).cloned())
}

fn default_activity(config: &RichPresenceConfig) -> activity::Activity<'static> {
    activity::Activity::new()
        .name(config.game_name.clone())
        .details(format!("Playing {}", config.game_name))
        .state("In song select")
        .assets(
            activity::Assets::new()
                .large_image(config.logo_url.clone())
                .large_text(config.game_name.clone()),
        )
}

fn song_activity(
    song: &Song,
    difficulty: i32,
    config: &RichPresenceConfig,
) -> activity::Activity<'static> {
    let image_url = format!("{}{}", SONG_IMAGE_BASE_URL, song.image);
    let subtitle = format!("Playing {} - {}", song.title, song.artist);

    activity::Activity::new()
        .name(config.game_name.clone())
        .details(subtitle)
        .state(difficulty_label(difficulty))
        .assets(
            activity::Assets::new()
                .large_image(image_url.clone())
                .large_text(song.title.clone()),
        )
}

fn unknown_song_activity(difficulty: i32, config: &RichPresenceConfig) -> activity::Activity<'static> {
    activity::Activity::new()
        .name(config.game_name.clone())
        .details("Playing a song")
        .state(difficulty_label(difficulty))
        .assets(
            activity::Assets::new()
                .large_image(config.logo_url.clone())
                .large_text(config.game_name.clone()),
        )
}

fn create_discord_client(app_id: &str) -> Option<DiscordIpcClient> {
    if app_id.trim().is_empty() || app_id == "YOUR_DISCORD_APP_ID" {
        log!("Discord app ID is not configured");
        return None;
    }

    let mut client = DiscordIpcClient::new(app_id);
    match client.connect() {
        Ok(_) => {
            log!("Connected to Discord RPC");
            Some(client)
        }
        Err(error) => {
            log!(
                "Failed to connect Discord RPC: {}",
                error
            );
            None
        }
    }
}

fn connect_discord_and_set_default(
    current_presence_state: &mut Option<PresenceState>,
    config: &RichPresenceConfig,
) -> Option<DiscordIpcClient> {
    let mut client = create_discord_client(&config.discord_app_id)?;

    if update_presence(
        &mut client,
        &PresenceState::Default,
        &Arc::new(RwLock::new(HashMap::new())),
        config,
    ) {
        *current_presence_state = Some(PresenceState::Default);
    } else {
        *current_presence_state = None;
    }

    Some(client)
}

fn update_presence(
    client: &mut DiscordIpcClient,
    state: &PresenceState,
    songs_by_id: &Arc<RwLock<HashMap<i32, Song>>>,
    config: &RichPresenceConfig,
) -> bool {
    let activity_description = presence_state_description(state, songs_by_id);
    let activity = match state {
        PresenceState::Default => default_activity(config),
        PresenceState::UnknownSong(difficulty) => unknown_song_activity(*difficulty, config),
        PresenceState::Song { id, difficulty } => get_song_by_id(songs_by_id, *id)
            .as_ref()
            .map(|song| song_activity(song, *difficulty, config))
            .unwrap_or_else(|| unknown_song_activity(*difficulty, config)),
    };

    match client.set_activity(activity) {
        Ok(_) => {
            log!("Discord presence updated: {}", activity_description);
            true
        }
        Err(error) => {
            log!(
                "Failed to update Discord RPC for {}: {}",
                activity_description,
                error
            );
            false
        }
    }
}

pub(crate) fn log_started_playing(current_song_id: Option<i32>) {
    match current_song_id {
        Some(song_id) => log!("Started playing, Song ID: {}", song_id),
        None => log!("Started playing, Song ID unavailable"),
    }
}

pub(crate) fn resolve_playing_presence_state(
    songs_by_id: &Arc<RwLock<HashMap<i32, Song>>>,
    current_song_id: Option<i32>,
    current_difficulty: i32,
    last_song_id: &mut i32,
) -> PresenceState {
    match current_song_id {
        Some(song_id) if song_id != -1 => {
            let song = get_song_by_id(songs_by_id, song_id);

            if song_id != *last_song_id {
                if let Some(song) = song.as_ref() {
                    log!("Now playing: {} - {}", song.title, song.artist);
                } else {
                    log!("Song ID {} not found in dataset", song_id);
                }
                *last_song_id = song_id;
            }

            if song.is_some() {
                PresenceState::Song {
                    id: song_id,
                    difficulty: current_difficulty,
                }
            } else {
                PresenceState::UnknownSong(current_difficulty)
            }
        }
        Some(song_id) => {
            if song_id != *last_song_id {
                log!("Invalid Song ID while playing: {}", song_id);
                *last_song_id = song_id;
            }
            PresenceState::UnknownSong(current_difficulty)
        }
        None => PresenceState::UnknownSong(current_difficulty),
    }
}

fn reconnect_discord_if_needed(
    discord_client: &mut Option<DiscordIpcClient>,
    last_discord_connect_attempt: &mut Instant,
    current_presence_state: &mut Option<PresenceState>,
    config: &RichPresenceConfig,
) {
    if discord_client.is_some() {
        return;
    }

    if last_discord_connect_attempt.elapsed() < DISCORD_RECONNECT_INTERVAL {
        return;
    }

    *last_discord_connect_attempt = Instant::now();
    *discord_client = connect_discord_and_set_default(current_presence_state, config);
}

fn apply_presence_state_if_needed(
    discord_client: &mut Option<DiscordIpcClient>,
    current_presence_state: &mut Option<PresenceState>,
    desired_presence_state: PresenceState,
    songs_by_id: &Arc<RwLock<HashMap<i32, Song>>>,
    config: &RichPresenceConfig,
    last_discord_connect_attempt: &mut Instant,
) {
    let Some(client) = discord_client.as_mut() else {
        return;
    };

    if current_presence_state.as_ref() == Some(&desired_presence_state) {
        return;
    }

    if update_presence(client, &desired_presence_state, songs_by_id, config) {
        *current_presence_state = Some(desired_presence_state);
        return;
    }

    *discord_client = None;
    *current_presence_state = None;
    *last_discord_connect_attempt = Instant::now();
}

fn main_thread() {
    log!("Injected successfully into Chunithm!");
    log!("Runtime base directory: {}", runtime_base_dir().display());
    log!("General log file: {}", log_file_path().display());
    log_resolved_game_module();
    let presence_config = load_presence_config();

    let songs_by_id = Arc::new(RwLock::new(match load_songs_from_cache() {
        Ok(songs) => songs,
        Err(error) => {
            log!("Failed to load local songs cache: {}. Starting with empty dataset.", error);
            HashMap::new()
        }
    }));

    let songs_refresh_handle = Arc::clone(&songs_by_id);
    thread::spawn(move || refresh_songs_from_online(songs_refresh_handle));

    let mut current_presence_state: Option<PresenceState> = None;
    let mut discord_client = connect_discord_and_set_default(&mut current_presence_state, &presence_config);
    let mut last_discord_connect_attempt = Instant::now();

    let mut last_song_id = -1;
    let mut latched_difficulty = None;
    let mut was_playing = false;
    let mut memory_read_available = true;

    loop {
        reconnect_discord_if_needed(
            &mut discord_client,
            &mut last_discord_connect_attempt,
            &mut current_presence_state,
            &presence_config,
        );

        if debug_logging_enabled() {
            log_memory_snapshot();
            log_memory_probe_status();
        }

        let Some((is_playing, desired_presence_state)) =
            (unsafe {
                read_presence_state_from_memory(
                    &songs_by_id,
                    was_playing,
                    &mut last_song_id,
                    &mut latched_difficulty,
                )
            })
        else {
            if memory_read_available {
                log!("Memory read unavailable, keeping default presence until pointers recover");
                memory_read_available = false;
            }
            if discord_client.is_some() && current_presence_state.is_none() {
                apply_presence_state_if_needed(
                    &mut discord_client,
                    &mut current_presence_state,
                    PresenceState::Default,
                    &songs_by_id,
                    &presence_config,
                    &mut last_discord_connect_attempt,
                );
            }
            thread::sleep(Duration::from_secs(1));
            continue;
        };

        if !memory_read_available {
            log!("Memory read recovered");
            memory_read_available = true;
        }

        apply_presence_state_if_needed(
            &mut discord_client,
            &mut current_presence_state,
            desired_presence_state,
            &songs_by_id,
            &presence_config,
            &mut last_discord_connect_attempt,
        );

        was_playing = is_playing;
        thread::sleep(Duration::from_secs(1));
    }
}

unsafe extern "system" fn main_thread_entry(_: *mut c_void) -> u32 {
    main_thread();
    0
}

#[no_mangle]
#[allow(non_snake_case, unused_variables)]
pub extern "system" fn DllMain(
    hinst_dll: HMODULE,
    fdw_reason: u32,
    lpv_reserved: *mut c_void,
) -> BOOL {
    match fdw_reason {
        DLL_PROCESS_ATTACH => {
            unsafe {
                set_dll_module(hinst_dll);
                DisableThreadLibraryCalls(hinst_dll);

                let thread_handle = CreateThread(
                    std::ptr::null(),
                    0,
                    Some(main_thread_entry),
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                );

                if thread_handle != 0 {
                    CloseHandle(thread_handle);
                }
            }
            TRUE
        }
        _ => TRUE,
    }
}

// ============================================================================
// Magic stub to fix 32-bit MinGW cross-compilation linker errors
// Since we use panic="abort", this is never actually called.
// I am not familiar enough with rust to know exactly what this does, but it fixed my issues when compiling this as a 32bit dll
#[cfg(all(target_os = "windows", target_env = "gnu", target_pointer_width = "32"))]
#[no_mangle]
pub extern "C" fn _Unwind_Resume() -> ! {
    std::process::abort();
}
