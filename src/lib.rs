mod logging;
mod memory;
mod songs;
mod types;

use configparser::ini::Ini;
use discord_rich_presence::{activity, DiscordIpc, DiscordIpcClient};
use logging::{
    debug_logging_enabled, log_file_path, log_hook_install_results, log_memory_probe_status,
    log_memory_snapshot, log_message as log, log_resolved_game_module, log_stopped_playing,
    runtime_base_dir, set_dll_module,
};
use memory::{install_runtime_hooks, read_presence_state_from_memory};
use songs::{get_song_by_id, load_initial_songs, start_songs_refresh, SongsById};
use std::ffi::c_void;
use std::thread;
use std::time::{Duration, Instant};
use types::{PresenceState, RichPresenceConfig, Song};
use windows_sys::Win32::Foundation::{CloseHandle, BOOL, HMODULE, TRUE};
use windows_sys::Win32::System::LibraryLoader::DisableThreadLibraryCalls;
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows_sys::Win32::System::Threading::CreateThread;

struct RuntimeState {
    current_presence_state: Option<PresenceState>,
    last_presence_update: Instant,
    discord_client: Option<DiscordIpcClient>,
    last_discord_connect_attempt: Instant,
    last_song_id: i32,
    latched_song_id: Option<i32>,
    latched_difficulty: Option<i32>,
    was_playing: bool,
    memory_read_available: bool,
}

const SONG_JACKET_BASE_URL: &str = "https://chunithm.beerpsi.cc/assets/jackets/";

// Default user-facing configuration.
const DEFAULT_LOGO_URL: &str = "https://chunithm.org/assets/logo.png";
const DEFAULT_GAME_NAME: &str = "Chunithm";
const DEFAULT_DISCORD_APP_ID: &str = "1482780703128289493";

// Runtime timing.
const DISCORD_RECONNECT_INTERVAL: Duration = Duration::from_secs(10);
const PRESENCE_REFRESH_INTERVAL: Duration = Duration::from_secs(10);
const SEGATOOLS_INI_FILE_NAME: &str = "segatools.ini";

// Config and shared labels.
impl Default for RichPresenceConfig {
    fn default() -> Self {
        Self {
            logo_url: DEFAULT_LOGO_URL.to_string(),
            game_name: DEFAULT_GAME_NAME.to_string(),
            discord_app_id: DEFAULT_DISCORD_APP_ID.to_string(),
            show_rating: true,
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

fn difficulty_state_label(song: Option<&Song>, difficulty: i32) -> String {
    let label = difficulty_label(difficulty);
    match song.and_then(|song| song.chart_level_for_difficulty(difficulty)) {
        Some(level) => format!("{label} {level}"),
        None => label.to_string(),
    }
}

fn rating_label(player_rating: Option<i32>) -> Option<String> {
    player_rating
        .filter(|player_rating| *player_rating > 0)
        .map(|player_rating| {
            let whole = player_rating / 100;
            let fractional = player_rating % 100;
            format!("{whole}.{fractional:02} Rating")
        })
}

fn display_player_rating(
    config: &RichPresenceConfig,
    player_rating: Option<i32>,
) -> Option<i32> {
    config.show_rating.then_some(player_rating).flatten()
}

fn song_select_state_label(player_rating: Option<i32>) -> Option<String> {
    rating_label(player_rating)
}

fn playing_state_label(base_state: String, player_rating: Option<i32>) -> String {
    match rating_label(player_rating) {
        Some(rating) => format!("{base_state} | {rating}"),
        None => base_state,
    }
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
    let ini_path = runtime_base_dir().join(SEGATOOLS_INI_FILE_NAME);
    let mut config = RichPresenceConfig::default();
    let mut ini = Ini::new();

    match ini.load(&ini_path) {
        Ok(_) => {}
        Err(error) => {
            log(format!(
                "failed to read {} ({}), using defaults",
                ini_path.display(),
                error
            ));
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

    if let Some(value) = sanitize_ini_value(ini.get("chunirichpresence", "show_rating")) {
        config.show_rating = value != "0";
    }

    log(format!(
        "Loaded config from {} (game_name='{}', logo_url='{}', discord_app_id='{}', show_rating={})",
        ini_path.display(),
        config.game_name,
        config.logo_url,
        config.discord_app_id,
        config.show_rating
    ));

    config
}

fn default_activity(
    config: &RichPresenceConfig,
    player_rating: Option<i32>,
) -> activity::Activity<'static> {
    let player_rating = display_player_rating(config, player_rating);
    let activity = activity::Activity::new()
        .name(config.game_name.clone())
        .details("In song select")
        .assets(
            activity::Assets::new()
                .large_image(config.logo_url.clone())
                .large_text(config.game_name.clone()),
        );

    match song_select_state_label(player_rating) {
        Some(state) => activity.state(state),
        None => activity,
    }
}

fn song_activity(
    song: &Song,
    difficulty: i32,
    player_rating: Option<i32>,
    config: &RichPresenceConfig,
) -> activity::Activity<'static> {
    let player_rating = display_player_rating(config, player_rating);
    let image_url = format!("{}{id}.webp", SONG_JACKET_BASE_URL, id = song.id);
    let subtitle = format!("Playing {} - {}", song.title, song.artist);
    let state = playing_state_label(difficulty_state_label(Some(song), difficulty), player_rating);

    activity::Activity::new()
        .name(config.game_name.clone())
        .details(subtitle)
        .state(state)
        .assets(
            activity::Assets::new()
                .large_image(image_url.clone())
                .large_text(song.title.clone()),
        )
}

fn unknown_song_activity(
    difficulty: i32,
    player_rating: Option<i32>,
    config: &RichPresenceConfig,
) -> activity::Activity<'static> {
    let player_rating = display_player_rating(config, player_rating);
    let state = playing_state_label(difficulty_state_label(None, difficulty), player_rating);

    activity::Activity::new()
        .name(config.game_name.clone())
        .details("Playing a song")
        .state(state)
        .assets(
            activity::Assets::new()
                .large_image(config.logo_url.clone())
                .large_text(config.game_name.clone()),
        )
}

pub(crate) fn resolve_playing_presence_state(
    songs_by_id: &SongsById,
    current_song_id: Option<i32>,
    current_difficulty: i32,
    current_player_rating: Option<i32>,
    last_song_id: &mut i32,
) -> PresenceState {
    match current_song_id {
        Some(song_id) if song_id != -1 => {
            let song = get_song_by_id(songs_by_id, song_id);

            if song_id != *last_song_id {
                if let Some(song) = song.as_ref() {
                    log(format!("Now playing: {} - {}", song.title, song.artist));
                } else {
                    log(format!("Song ID {} not found in dataset", song_id));
                }
                *last_song_id = song_id;
            }

            if song.is_some() {
                PresenceState::Song {
                    id: song_id,
                    difficulty: current_difficulty,
                    player_rating: current_player_rating,
                }
            } else {
                PresenceState::UnknownSong {
                    difficulty: current_difficulty,
                    player_rating: current_player_rating,
                }
            }
        }
        Some(song_id) => {
            if song_id != *last_song_id {
                log(format!("Invalid Song ID while playing: {}", song_id));
                *last_song_id = song_id;
            }
            PresenceState::UnknownSong {
                difficulty: current_difficulty,
                player_rating: current_player_rating,
            }
        }
        None => PresenceState::UnknownSong {
            difficulty: current_difficulty,
            player_rating: current_player_rating,
        },
    }
}

fn create_discord_client(app_id: &str) -> Option<DiscordIpcClient> {
    if app_id.trim().is_empty() || app_id == "YOUR_DISCORD_APP_ID" {
        log("Discord app ID is not configured".to_string());
        return None;
    }

    let mut client = DiscordIpcClient::new(app_id);
    match client.connect() {
        Ok(_) => {
            log("Connected to Discord RPC".to_string());
            Some(client)
        }
        Err(error) => {
            log(format!("Failed to connect Discord RPC: {}", error));
            None
        }
    }
}

fn connect_discord_and_set_default(
    current_presence_state: &mut Option<PresenceState>,
    config: &RichPresenceConfig,
    last_presence_update: &mut Instant,
) -> Option<DiscordIpcClient> {
    let mut client = create_discord_client(&config.discord_app_id)?;
    if client.set_activity(default_activity(config, None)).is_ok() {
        log("Discord presence updated: default presence (song select)".to_string());
        *current_presence_state = Some(PresenceState::Default {
            player_rating: None,
        });
        *last_presence_update = Instant::now();
    } else {
        log("Failed to update Discord RPC for default presence (song select)".to_string());
        *current_presence_state = None;
    }

    Some(client)
}

fn update_presence(
    client: &mut DiscordIpcClient,
    state: &PresenceState,
    songs_by_id: &SongsById,
    config: &RichPresenceConfig,
) -> bool {
    let (activity_description, activity) = match state {
        PresenceState::Default { player_rating } => (
            "default presence (song select)".to_string(),
            default_activity(config, *player_rating),
        ),
        PresenceState::UnknownSong {
            difficulty,
            player_rating,
        } => (
            format!("unknown song ({})", difficulty_label(*difficulty)),
            unknown_song_activity(*difficulty, *player_rating, config),
        ),
        PresenceState::Song {
            id,
            difficulty,
            player_rating,
        } => match get_song_by_id(songs_by_id, *id) {
            Some(song) => {
                let state_label =
                    playing_state_label(difficulty_state_label(Some(&song), *difficulty), *player_rating);
                (
                    format!(
                        "song {} - {} ({})",
                        song.title, song.artist, state_label
                    ),
                    song_activity(&song, *difficulty, *player_rating, config),
                )
            }
            None => (
                format!(
                    "song id {} missing from dataset ({})",
                    id,
                    difficulty_label(*difficulty)
                ),
                unknown_song_activity(*difficulty, *player_rating, config),
            ),
        },
    };

    match client.set_activity(activity) {
        Ok(_) => {
            log(format!(
                "Discord presence updated: {}",
                activity_description
            ));
            true
        }
        Err(error) => {
            log(format!(
                "Failed to update Discord RPC for {}: {}",
                activity_description, error
            ));
            false
        }
    }
}

fn reconnect_discord_if_needed(runtime: &mut RuntimeState, config: &RichPresenceConfig) {
    if runtime.discord_client.is_some() {
        return;
    }

    if runtime.last_discord_connect_attempt.elapsed() < DISCORD_RECONNECT_INTERVAL {
        return;
    }

    runtime.last_discord_connect_attempt = Instant::now();
    runtime.discord_client = connect_discord_and_set_default(
        &mut runtime.current_presence_state,
        config,
        &mut runtime.last_presence_update,
    );
}

fn apply_presence_state_if_needed(
    runtime: &mut RuntimeState,
    desired_presence_state: PresenceState,
    songs_by_id: &SongsById,
    config: &RichPresenceConfig,
) {
    let Some(client) = runtime.discord_client.as_mut() else {
        return;
    };

    let state_changed = runtime.current_presence_state.as_ref() != Some(&desired_presence_state);
    let refresh_due = runtime.last_presence_update.elapsed() >= PRESENCE_REFRESH_INTERVAL;

    if !state_changed && !refresh_due {
        return;
    }

    if update_presence(client, &desired_presence_state, songs_by_id, config) {
        runtime.current_presence_state = Some(desired_presence_state);
        runtime.last_presence_update = Instant::now();
        return;
    }

    runtime.discord_client = None;
    runtime.current_presence_state = None;
    runtime.last_discord_connect_attempt = Instant::now();
}

// Main runtime loop.
fn main_thread() {
    log("Injected successfully into Chunithm!".to_string());
    log(format!(
        "Runtime base directory: {}",
        runtime_base_dir().display()
    ));
    log(format!("General log file: {}", log_file_path().display()));
    log_resolved_game_module();

    let hook_install_results = install_runtime_hooks();
    log_hook_install_results(&hook_install_results);

    let presence_config = load_presence_config();
    let songs_by_id = load_initial_songs();
    start_songs_refresh(&songs_by_id);

    let mut current_presence_state = None;
    let mut last_presence_update = Instant::now();
    let discord_client = connect_discord_and_set_default(
        &mut current_presence_state,
        &presence_config,
        &mut last_presence_update,
    );
    let mut runtime = RuntimeState {
        current_presence_state,
        last_presence_update,
        discord_client,
        last_discord_connect_attempt: Instant::now(),
        last_song_id: -1,
        latched_song_id: None,
        latched_difficulty: None,
        was_playing: false,
        memory_read_available: true,
    };

    loop {
        reconnect_discord_if_needed(&mut runtime, &presence_config);

        if debug_logging_enabled() {
            log_memory_snapshot();
            log_memory_probe_status();
        }

        let Some((is_playing, desired_presence_state)) = (unsafe {
            read_presence_state_from_memory(
                &songs_by_id,
                runtime.was_playing,
                &mut runtime.last_song_id,
                &mut runtime.latched_song_id,
                &mut runtime.latched_difficulty,
            )
        }) else {
            if runtime.memory_read_available {
                log(
                    "Memory read unavailable, keeping default presence until pointers recover"
                        .to_string(),
                );
                runtime.memory_read_available = false;
            }

            if runtime.discord_client.is_some() && runtime.current_presence_state.is_none() {
                apply_presence_state_if_needed(
                    &mut runtime,
                    PresenceState::Default {
                        player_rating: None,
                    },
                    &songs_by_id,
                    &presence_config,
                );
            }

            thread::sleep(Duration::from_secs(1));
            continue;
        };

        if !runtime.memory_read_available {
            log("Memory read recovered".to_string());
            runtime.memory_read_available = true;
        }

        if runtime.was_playing && !is_playing {
            log_stopped_playing();
        }

        apply_presence_state_if_needed(
            &mut runtime,
            desired_presence_state,
            &songs_by_id,
            &presence_config,
        );
        runtime.was_playing = is_playing;

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

// Magic stub to fix 32-bit MinGW cross-compilation linker errors.
// We compile with panic="abort", so this should never be reached in practice.
#[cfg(all(target_os = "windows", target_env = "gnu", target_pointer_width = "32"))]
#[no_mangle]
pub extern "C" fn _Unwind_Resume() -> ! {
    std::process::abort();
}
