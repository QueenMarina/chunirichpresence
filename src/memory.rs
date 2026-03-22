use crate::types::{
    GameModuleInfo, HookConfig, HookInstallError, HookInstallStatus, HookResolutionSource,
    HookState, HookTargetConfig, InstalledHook, IntegerHookProbeStatus, MemoryProbeStatus,
    MemorySnapshot, PatternByte, PlayStateHookProbeStatus, PresenceState, PushadRegisters, Song,
};
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, RwLock};
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::Diagnostics::Debug::{FlushInstructionCache, ReadProcessMemory};
use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleA};
use windows_sys::Win32::System::Memory::{
    VirtualAlloc, VirtualProtect, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

// Game-specific addresses and values.
const GAME_MODULE_NAME: &[u8] = b"chusanApp.exe\0";
const PLAY_STATE_VALUE_RVA: usize = 0x018849E0;
const PLAY_STATE_VALUE_PLAYING: i32 = 2;
const PLAY_STATE_VALUE_SONG_SELECT: i32 = 3;

// Sentinel values used for hook caches.
const SONG_ID_UNSET: i32 = i32::MIN;
const DIFFICULTY_UNSET: i32 = i32::MIN;
const PLAYER_RATING_UNSET: i32 = i32::MIN;
const PLAY_STATE_UNSET: i32 = i32::MIN;

// PE header offsets used when scanning the module image.
const PE_LFANEW_OFFSET: usize = 0x3C;
const PE_SIZE_OF_IMAGE_OFFSET: usize = 0x50;

// Cached values captured from runtime hooks.
static SONG_ID_HOOK_STATE: HookState = HookState::new();
static LAST_HOOKED_SONG_ID: AtomicI32 = AtomicI32::new(SONG_ID_UNSET);

const SONG_ID_CURRENT_TARGET: HookTargetConfig = HookTargetConfig {
    overwrite_len: 7,
    fallback_rva: 0x0085BB83,
    pattern: &[
        PatternByte::Exact(0x89),
        PatternByte::Exact(0x41),
        PatternByte::Exact(0x10),
        PatternByte::Exact(0x0F),
        PatternByte::Exact(0xB6),
        PatternByte::Exact(0x46),
        PatternByte::Exact(0x14),
        PatternByte::Exact(0x88),
        PatternByte::Exact(0x41),
        PatternByte::Exact(0x14),
        PatternByte::Exact(0x8B),
        PatternByte::Exact(0x46),
        PatternByte::Exact(0x18),
        PatternByte::Exact(0x89),
        PatternByte::Exact(0x41),
        PatternByte::Exact(0x18),
        PatternByte::Exact(0x8B),
        PatternByte::Exact(0x46),
        PatternByte::Exact(0x1C),
        PatternByte::Exact(0x89),
        PatternByte::Exact(0x41),
        PatternByte::Exact(0x1C),
        PatternByte::Exact(0x8B),
        PatternByte::Exact(0x46),
        PatternByte::Exact(0x20),
    ],
};

const SONG_ID_OLD_TARGET: HookTargetConfig = HookTargetConfig {
    overwrite_len: 7,
    fallback_rva: 0x008C4FB7,
    pattern: &[
        PatternByte::Exact(0x89),
        PatternByte::Exact(0x41),
        PatternByte::Exact(0x10),
        PatternByte::Exact(0x0F),
        PatternByte::Exact(0xB6),
        PatternByte::Exact(0x46),
        PatternByte::Exact(0x14),
        PatternByte::Exact(0x88),
        PatternByte::Exact(0x41),
        PatternByte::Exact(0x14),
        PatternByte::Exact(0x8B),
        PatternByte::Exact(0x46),
        PatternByte::Exact(0x18),
        PatternByte::Exact(0x89),
        PatternByte::Exact(0x41),
        PatternByte::Exact(0x18),
        PatternByte::Exact(0x8B),
        PatternByte::Exact(0x46),
        PatternByte::Exact(0x1C),
        PatternByte::Exact(0x89),
        PatternByte::Exact(0x41),
        PatternByte::Exact(0x1C),
        PatternByte::Exact(0x8B),
        PatternByte::Exact(0x46),
        PatternByte::Exact(0x20),
        PatternByte::Exact(0x89),
        PatternByte::Exact(0x41),
        PatternByte::Exact(0x20),
        PatternByte::Exact(0x8D),
        PatternByte::Exact(0x73),
        PatternByte::Exact(0x08),
        PatternByte::Exact(0x8B),
        PatternByte::Exact(0xCE),
        PatternByte::Exact(0xE8),
        PatternByte::Any,
        PatternByte::Any,
        PatternByte::Any,
        PatternByte::Any,
    ],
};

const SONG_ID_HOOK: HookConfig = HookConfig {
    name: "song ID",
    targets: &[SONG_ID_CURRENT_TARGET, SONG_ID_OLD_TARGET],
    callback: song_id_hook_callback,
};

static DIFFICULTY_HOOK_STATE: HookState = HookState::new();
static LAST_HOOKED_DIFFICULTY: AtomicI32 = AtomicI32::new(DIFFICULTY_UNSET);

const DIFFICULTY_CURRENT_TARGET: HookTargetConfig = HookTargetConfig {
    overwrite_len: 6,
    fallback_rva: 0x0085BB8A,
    pattern: &[
        PatternByte::Exact(0x88),
        PatternByte::Exact(0x41),
        PatternByte::Exact(0x14),
        PatternByte::Exact(0x8B),
        PatternByte::Exact(0x46),
        PatternByte::Exact(0x18),
        PatternByte::Exact(0x89),
        PatternByte::Exact(0x41),
        PatternByte::Exact(0x18),
        PatternByte::Exact(0x8B),
        PatternByte::Exact(0x46),
        PatternByte::Exact(0x1C),
        PatternByte::Exact(0x89),
        PatternByte::Exact(0x41),
        PatternByte::Exact(0x1C),
        PatternByte::Exact(0x8B),
        PatternByte::Exact(0x46),
        PatternByte::Exact(0x20),
    ],
};

const DIFFICULTY_OLD_TARGET: HookTargetConfig = HookTargetConfig {
    overwrite_len: 6,
    fallback_rva: 0x008C4FBE,
    pattern: &[
        PatternByte::Exact(0x88),
        PatternByte::Exact(0x41),
        PatternByte::Exact(0x14),
        PatternByte::Exact(0x8B),
        PatternByte::Exact(0x46),
        PatternByte::Exact(0x18),
        PatternByte::Exact(0x89),
        PatternByte::Exact(0x41),
        PatternByte::Exact(0x18),
        PatternByte::Exact(0x8B),
        PatternByte::Exact(0x46),
        PatternByte::Exact(0x1C),
        PatternByte::Exact(0x89),
        PatternByte::Exact(0x41),
        PatternByte::Exact(0x1C),
        PatternByte::Exact(0x8B),
        PatternByte::Exact(0x46),
        PatternByte::Exact(0x20),
        PatternByte::Exact(0x89),
        PatternByte::Exact(0x41),
        PatternByte::Exact(0x20),
        PatternByte::Exact(0x8D),
        PatternByte::Exact(0x73),
        PatternByte::Exact(0x08),
        PatternByte::Exact(0x8B),
        PatternByte::Exact(0xCE),
        PatternByte::Exact(0xE8),
        PatternByte::Any,
        PatternByte::Any,
        PatternByte::Any,
        PatternByte::Any,
    ],
};

const DIFFICULTY_HOOK: HookConfig = HookConfig {
    name: "difficulty",
    targets: &[DIFFICULTY_CURRENT_TARGET, DIFFICULTY_OLD_TARGET],
    callback: difficulty_hook_callback,
};

static PLAYER_RATING_HOOK_STATE: HookState = HookState::new();
static LAST_HOOKED_PLAYER_RATING: AtomicI32 = AtomicI32::new(PLAYER_RATING_UNSET);

const PLAYER_RATING_CURRENT_TARGET: HookTargetConfig = HookTargetConfig {
    overwrite_len: 6,
    fallback_rva: 0x006FFC9D,
    pattern: &[
        PatternByte::Exact(0x89),
        PatternByte::Exact(0x82),
        PatternByte::Exact(0x90),
        PatternByte::Exact(0x01),
        PatternByte::Exact(0x00),
        PatternByte::Exact(0x00),
        PatternByte::Exact(0x8B),
        PatternByte::Exact(0x81),
        PatternByte::Exact(0xC4),
        PatternByte::Exact(0x2B),
        PatternByte::Exact(0x00),
        PatternByte::Exact(0x00),
        PatternByte::Exact(0x0F),
        PatternByte::Exact(0x95),
        PatternByte::Exact(0xC3),
        PatternByte::Exact(0x89),
        PatternByte::Exact(0x82),
        PatternByte::Exact(0x94),
        PatternByte::Exact(0x01),
        PatternByte::Exact(0x00),
        PatternByte::Exact(0x00),
        PatternByte::Exact(0x8B),
        PatternByte::Exact(0x81),
        PatternByte::Exact(0xC0),
        PatternByte::Exact(0x2B),
        PatternByte::Exact(0x00),
        PatternByte::Exact(0x00),
    ],
};

const PLAYER_RATING_HOOK: HookConfig = HookConfig {
    name: "player rating",
    targets: &[PLAYER_RATING_CURRENT_TARGET],
    callback: player_rating_hook_callback,
};

static PLAY_STATE_ENTER_HOOK_STATE: HookState = HookState::new();
static PLAY_STATE_EXIT_HOOK_STATE: HookState = HookState::new();
static LAST_HOOKED_PLAY_STATE: AtomicI32 = AtomicI32::new(PLAY_STATE_UNSET);

const PLAY_STATE_ENTER_CURRENT_TARGET: HookTargetConfig = HookTargetConfig {
    overwrite_len: 6,
    fallback_rva: 0x00F5BF9E,
    pattern: &[
        PatternByte::Exact(0x89),
        PatternByte::Exact(0x1D),
        PatternByte::Any,
        PatternByte::Any,
        PatternByte::Any,
        PatternByte::Any,
        PatternByte::Exact(0x89),
        PatternByte::Exact(0x70),
        PatternByte::Exact(0x04),
        PatternByte::Exact(0x5B),
    ],
};

const PLAY_STATE_ENTER_OLD_TARGET: HookTargetConfig = HookTargetConfig {
    overwrite_len: 6,
    fallback_rva: 0x00FA83AC,
    pattern: &[
        PatternByte::Exact(0x89),
        PatternByte::Exact(0x1D),
        PatternByte::Any,
        PatternByte::Any,
        PatternByte::Any,
        PatternByte::Any,
        PatternByte::Exact(0x89),
        PatternByte::Exact(0x30),
        PatternByte::Exact(0x89),
        PatternByte::Exact(0x70),
        PatternByte::Exact(0x04),
        PatternByte::Exact(0x5B),
    ],
};

const PLAY_STATE_ENTER_HOOK: HookConfig = HookConfig {
    name: "play-state enter",
    targets: &[PLAY_STATE_ENTER_CURRENT_TARGET, PLAY_STATE_ENTER_OLD_TARGET],
    callback: play_state_enter_hook_callback,
};

const PLAY_STATE_EXIT_CURRENT_TARGET: HookTargetConfig = HookTargetConfig {
    overwrite_len: 6,
    fallback_rva: 0x00F5D8AC,
    pattern: &[
        PatternByte::Exact(0x89),
        PatternByte::Exact(0x1D),
        PatternByte::Any,
        PatternByte::Any,
        PatternByte::Any,
        PatternByte::Any,
        PatternByte::Exact(0x5F),
        PatternByte::Exact(0x5B),
    ],
};

const PLAY_STATE_EXIT_OLD_TARGET: HookTargetConfig = HookTargetConfig {
    overwrite_len: 6,
    fallback_rva: 0x00FA993F,
    pattern: &[
        PatternByte::Exact(0x89),
        PatternByte::Exact(0x1D),
        PatternByte::Any,
        PatternByte::Any,
        PatternByte::Any,
        PatternByte::Any,
        PatternByte::Exact(0x8B),
        PatternByte::Exact(0xC6),
        PatternByte::Exact(0x5F),
        PatternByte::Exact(0x5B),
    ],
};

const PLAY_STATE_EXIT_HOOK: HookConfig = HookConfig {
    name: "play-state exit",
    targets: &[PLAY_STATE_EXIT_CURRENT_TARGET, PLAY_STATE_EXIT_OLD_TARGET],
    callback: play_state_exit_hook_callback,
};

// Public API used by the rest of the DLL.
pub(crate) unsafe fn read_presence_state_from_memory(
    songs_by_id: &Arc<RwLock<HashMap<i32, Song>>>,
    was_playing: bool,
    last_song_id: &mut i32,
    latched_song_id: &mut Option<i32>,
    latched_difficulty: &mut Option<i32>,
) -> Option<(bool, PresenceState)> {
    let current_player_rating = current_hooked_player_rating();
    let is_playing = is_playing_now();
    if !is_playing {
        reset_play_latches(last_song_id, latched_song_id, latched_difficulty);
        return Some((
            false,
            PresenceState::Default {
                player_rating: current_player_rating,
            },
        ));
    }

    let live_song_id = current_hooked_song_id();
    let current_difficulty = current_hooked_difficulty();

    if !was_playing {
        let Some(song_id) = live_song_id.filter(|song_id| *song_id != -1) else {
            reset_play_latches(last_song_id, latched_song_id, latched_difficulty);
            return Some((
                false,
                PresenceState::Default {
                    player_rating: current_player_rating,
                },
            ));
        };

        let Some(difficulty) = current_difficulty else {
            reset_play_latches(last_song_id, latched_song_id, latched_difficulty);
            return Some((
                false,
                PresenceState::Default {
                    player_rating: current_player_rating,
                },
            ));
        };

        *latched_song_id = Some(song_id);
        *latched_difficulty = Some(difficulty);
        crate::logging::log_started_playing(Some(song_id));
    }

    let current_song_id = *latched_song_id;
    let current_difficulty = latched_difficulty.or(current_difficulty).unwrap_or(-1);

    let desired_presence_state = crate::resolve_playing_presence_state(
        songs_by_id,
        current_song_id,
        current_difficulty,
        current_player_rating,
        last_song_id,
    );
    Some((true, desired_presence_state))
}

pub(crate) fn install_runtime_hooks() -> Vec<HookInstallStatus> {
    vec![
        install_runtime_hook(&SONG_ID_HOOK_STATE, &SONG_ID_HOOK),
        install_runtime_hook(&DIFFICULTY_HOOK_STATE, &DIFFICULTY_HOOK),
        install_runtime_hook(&PLAYER_RATING_HOOK_STATE, &PLAYER_RATING_HOOK),
        install_runtime_hook(&PLAY_STATE_ENTER_HOOK_STATE, &PLAY_STATE_ENTER_HOOK),
        install_runtime_hook(&PLAY_STATE_EXIT_HOOK_STATE, &PLAY_STATE_EXIT_HOOK),
    ]
}

pub(crate) fn resolved_game_module() -> Option<GameModuleInfo> {
    unsafe {
        let module_handle = get_module_handle()?;
        Some(GameModuleInfo {
            path: module_path(module_handle as HMODULE),
            base_addr: module_handle as usize,
        })
    }
}

pub(crate) fn memory_probe_status() -> MemoryProbeStatus {
    MemoryProbeStatus {
        module: resolved_game_module(),
        song_id: IntegerHookProbeStatus {
            installed: SONG_ID_HOOK_STATE.installed.load(Ordering::Relaxed),
            target_addr: SONG_ID_HOOK_STATE.target_addr(),
            cached_value: current_hooked_song_id(),
        },
        difficulty: IntegerHookProbeStatus {
            installed: DIFFICULTY_HOOK_STATE.installed.load(Ordering::Relaxed),
            target_addr: DIFFICULTY_HOOK_STATE.target_addr(),
            cached_value: current_hooked_difficulty(),
        },
        player_rating: IntegerHookProbeStatus {
            installed: PLAYER_RATING_HOOK_STATE.installed.load(Ordering::Relaxed),
            target_addr: PLAYER_RATING_HOOK_STATE.target_addr(),
            cached_value: current_hooked_player_rating(),
        },
        play_state: PlayStateHookProbeStatus {
            enter_installed: PLAY_STATE_ENTER_HOOK_STATE
                .installed
                .load(Ordering::Relaxed),
            enter_target_addr: PLAY_STATE_ENTER_HOOK_STATE.target_addr(),
            exit_installed: PLAY_STATE_EXIT_HOOK_STATE.installed.load(Ordering::Relaxed),
            exit_target_addr: PLAY_STATE_EXIT_HOOK_STATE.target_addr(),
            cached_value: current_hooked_play_state(),
        },
    }
}

pub(crate) fn memory_snapshot() -> MemorySnapshot {
    unsafe {
        MemorySnapshot {
            is_playing: Some(is_playing_now()),
            song_id: current_hooked_song_id(),
            difficulty: current_hooked_difficulty(),
            player_rating: current_hooked_player_rating(),
        }
    }
}

// Presence-state reconstruction from cached hook values.
fn reset_play_latches(
    last_song_id: &mut i32,
    latched_song_id: &mut Option<i32>,
    latched_difficulty: &mut Option<i32>,
) {
    *last_song_id = -1;
    *latched_song_id = None;
    *latched_difficulty = None;
}

unsafe fn is_playing_now() -> bool {
    read_live_play_state()
        .or_else(current_hooked_play_state)
        .unwrap_or(false)
}

fn current_cached_i32(cache: &AtomicI32, unset: i32) -> Option<i32> {
    match cache.load(Ordering::Relaxed) {
        value if value == unset => None,
        value => Some(value),
    }
}

fn current_hooked_song_id() -> Option<i32> {
    current_cached_i32(&LAST_HOOKED_SONG_ID, SONG_ID_UNSET)
}

fn current_hooked_difficulty() -> Option<i32> {
    current_cached_i32(&LAST_HOOKED_DIFFICULTY, DIFFICULTY_UNSET)
}

fn current_hooked_player_rating() -> Option<i32> {
    current_cached_i32(&LAST_HOOKED_PLAYER_RATING, PLAYER_RATING_UNSET)
}

fn current_hooked_play_state() -> Option<bool> {
    decode_play_state(LAST_HOOKED_PLAY_STATE.load(Ordering::Relaxed))
}

fn decode_play_state(value: i32) -> Option<bool> {
    match value {
        PLAY_STATE_UNSET => None,
        PLAY_STATE_VALUE_PLAYING => Some(true),
        PLAY_STATE_VALUE_SONG_SELECT => Some(false),
        _ => None,
    }
}

unsafe fn read_live_play_state() -> Option<bool> {
    let module_base = get_module_handle()? as usize;
    let play_state_addr = add_offset(module_base, PLAY_STATE_VALUE_RVA)?;
    let play_state = read_i32(play_state_addr)?;
    decode_play_state(play_state)
}

// Hook callbacks receive the saved register snapshot from the assembly stub.
unsafe extern "system" fn song_id_hook_callback(registers: *const PushadRegisters) {
    let Some(registers) = registers.as_ref() else {
        return;
    };

    let song_id = registers.eax as i32;
    LAST_HOOKED_SONG_ID.store(song_id, Ordering::Relaxed);
}

unsafe extern "system" fn difficulty_hook_callback(registers: *const PushadRegisters) {
    let Some(registers) = registers.as_ref() else {
        return;
    };

    let difficulty = (registers.eax & 0xFF) as i32;
    if (0..=4).contains(&difficulty) {
        LAST_HOOKED_DIFFICULTY.store(difficulty, Ordering::Relaxed);
    }
}

unsafe extern "system" fn player_rating_hook_callback(registers: *const PushadRegisters) {
    let Some(registers) = registers.as_ref() else {
        return;
    };

    let player_rating = registers.eax as i32;
    if player_rating > 0 {
        LAST_HOOKED_PLAYER_RATING.store(player_rating, Ordering::Relaxed);
    } else {
        LAST_HOOKED_PLAYER_RATING.store(PLAYER_RATING_UNSET, Ordering::Relaxed);
    }
}

unsafe extern "system" fn play_state_enter_hook_callback(registers: *const PushadRegisters) {
    capture_play_state_from_ebx(registers);
}

unsafe extern "system" fn play_state_exit_hook_callback(registers: *const PushadRegisters) {
    capture_play_state_from_ebx(registers);
}

unsafe fn capture_play_state_from_ebx(registers: *const PushadRegisters) {
    let Some(registers) = registers.as_ref() else {
        return;
    };

    let play_state = registers.ebx as i32;
    if decode_play_state(play_state).is_some() {
        LAST_HOOKED_PLAY_STATE.store(play_state, Ordering::Relaxed);
    }
}

// Runtime hook installation and detour patching.
fn install_runtime_hook(state: &HookState, config: &HookConfig) -> HookInstallStatus {
    state.once.call_once(|| unsafe {
        match install_hook_once(config) {
            Ok(install) => state.store_install(install),
            Err(error) => state.store_error(error),
        }
    });

    HookInstallStatus {
        name: config.name,
        target_addr: state.target_addr(),
        resolution_source: state.resolution_source(),
        error: state.install_error.get().cloned(),
    }
}

unsafe fn install_hook_once(config: &HookConfig) -> Result<InstalledHook, HookInstallError> {
    let module_handle = get_module_handle().ok_or(HookInstallError::GameModuleHandleNotFound)?;
    let module_base = module_handle as usize;
    let resolved_target = resolve_hook_target(module_base, config)
        .ok_or(HookInstallError::HookSignatureNotFound(config.name))?;
    let target_addr = resolved_target.addr;
    let trampoline_addr = create_trampoline(target_addr, resolved_target.overwrite_len)?;
    let stub_addr = create_hook_stub(trampoline_addr, config.callback)?;
    patch_hook_target(target_addr, resolved_target.overwrite_len, stub_addr)?;

    Ok(InstalledHook {
        target_addr,
        trampoline_addr,
        stub_addr,
        resolution_source: resolved_target.source,
    })
}

struct ResolvedHookTarget {
    addr: usize,
    overwrite_len: usize,
    source: HookResolutionSource,
}

unsafe fn resolve_hook_target(
    module_base: usize,
    config: &HookConfig,
) -> Option<ResolvedHookTarget> {
    for target in config.targets {
        if let Some(addr) =
            verify_hook_fallback_target(module_base, target.fallback_rva, target.pattern)
        {
            return Some(ResolvedHookTarget {
                addr,
                overwrite_len: target.overwrite_len,
                source: HookResolutionSource::HardcodedAddress,
            });
        }
    }

    for target in config.targets {
        if let Some(addr) = find_hook_target(module_base, target.pattern) {
            return Some(ResolvedHookTarget {
                addr,
                overwrite_len: target.overwrite_len,
                source: HookResolutionSource::PatternScan,
            });
        }
    }

    None
}

unsafe fn create_trampoline(
    target_addr: usize,
    overwrite_len: usize,
) -> Result<usize, HookInstallError> {
    let trampoline_size = overwrite_len + 5;
    let trampoline_addr = allocate_executable_block(trampoline_size)?;
    ptr::copy_nonoverlapping(
        target_addr as *const u8,
        trampoline_addr as *mut u8,
        overwrite_len,
    );

    write_relative_jump(trampoline_addr + overwrite_len, target_addr + overwrite_len)?;

    Ok(trampoline_addr)
}

unsafe fn create_hook_stub(
    trampoline_addr: usize,
    callback: unsafe extern "system" fn(*const PushadRegisters),
) -> Result<usize, HookInstallError> {
    // pushad; push esp; mov eax, callback; call eax; popad; jmp trampoline
    let stub_size = 15usize;
    let stub_addr = allocate_executable_block(stub_size)?;
    let mut cursor = stub_addr as *mut u8;

    *cursor = 0x60;
    cursor = cursor.add(1);

    *cursor = 0x54;
    cursor = cursor.add(1);

    *cursor = 0xB8;
    cursor = cursor.add(1);
    ptr::write_unaligned(cursor as *mut u32, callback as *const () as usize as u32);
    cursor = cursor.add(4);

    *cursor = 0xFF;
    *cursor.add(1) = 0xD0;
    cursor = cursor.add(2);

    *cursor = 0x61;
    cursor = cursor.add(1);

    *cursor = 0xE9;
    cursor = cursor.add(1);
    let jump_back_from = cursor as usize - 1;
    ptr::write_unaligned(
        cursor as *mut i32,
        relative_jump_offset(jump_back_from, trampoline_addr)?,
    );

    Ok(stub_addr)
}

unsafe fn patch_hook_target(
    target_addr: usize,
    overwrite_len: usize,
    stub_addr: usize,
) -> Result<(), HookInstallError> {
    let mut old_protect = 0u32;
    if VirtualProtect(
        target_addr as *const c_void,
        overwrite_len,
        PAGE_EXECUTE_READWRITE,
        &mut old_protect,
    ) == 0
    {
        return Err(HookInstallError::VirtualProtectFailed);
    }

    write_relative_jump(target_addr, stub_addr)?;
    for offset in 5..overwrite_len {
        let nop_addr =
            add_offset(target_addr, offset).ok_or(HookInstallError::HookPatchOverflow)?;
        *(nop_addr as *mut u8) = 0x90;
    }

    let _ = FlushInstructionCache(
        GetCurrentProcess(),
        target_addr as *const c_void,
        overwrite_len,
    );

    let mut restore_protect = 0u32;
    let _ = VirtualProtect(
        target_addr as *const c_void,
        overwrite_len,
        old_protect,
        &mut restore_protect,
    );

    Ok(())
}

unsafe fn allocate_executable_block(size: usize) -> Result<usize, HookInstallError> {
    let block = VirtualAlloc(
        ptr::null_mut(),
        size,
        MEM_COMMIT | MEM_RESERVE,
        PAGE_EXECUTE_READWRITE,
    ) as usize;

    if block == 0 {
        Err(HookInstallError::ExecutableBlockAllocationFailed)
    } else {
        Ok(block)
    }
}

unsafe fn write_relative_jump(from_addr: usize, to_addr: usize) -> Result<(), HookInstallError> {
    *(from_addr as *mut u8) = 0xE9;
    let offset = relative_jump_offset(from_addr, to_addr)?;
    ptr::write_unaligned((from_addr + 1) as *mut i32, offset);
    Ok(())
}

fn relative_jump_offset(from_addr: usize, to_addr: usize) -> Result<i32, HookInstallError> {
    let from_next = from_addr
        .checked_add(5)
        .ok_or(HookInstallError::RelativeJumpSourceOverflow)?;
    let distance = to_addr as isize - from_next as isize;
    i32::try_from(distance).map_err(|_| HookInstallError::RelativeJumpOutOfRange)
}

// Module resolution and memory-reading helpers.
unsafe fn get_module_handle() -> Option<isize> {
    let module_handle = GetModuleHandleA(GAME_MODULE_NAME.as_ptr());
    (module_handle != 0).then_some(module_handle)
}

unsafe fn module_path(module: HMODULE) -> Option<PathBuf> {
    let mut buffer = vec![0u16; 260];

    loop {
        let len = GetModuleFileNameW(module, buffer.as_mut_ptr(), buffer.len() as u32) as usize;
        if len == 0 {
            return None;
        }

        if len < buffer.len() - 1 {
            buffer.truncate(len);
            return Some(PathBuf::from(String::from_utf16_lossy(&buffer)));
        }

        if buffer.len() >= 32_768 {
            return None;
        }

        buffer.resize(buffer.len() * 2, 0);
    }
}

unsafe fn read_memory<T: Copy>(addr: usize) -> Option<T> {
    if addr == 0 {
        return None;
    }

    let mut value = std::mem::MaybeUninit::<T>::uninit();
    let mut bytes_read = 0usize;
    let success = ReadProcessMemory(
        GetCurrentProcess(),
        addr as *const c_void,
        value.as_mut_ptr() as *mut c_void,
        std::mem::size_of::<T>(),
        &mut bytes_read,
    );

    if success == 0 || bytes_read != std::mem::size_of::<T>() {
        return None;
    }

    Some(value.assume_init())
}

unsafe fn read_i32(addr: usize) -> Option<i32> {
    read_memory::<i32>(addr)
}

unsafe fn read_u32(addr: usize) -> Option<u32> {
    read_memory::<u32>(addr)
}

fn add_offset(addr: usize, offset: usize) -> Option<usize> {
    addr.checked_add(offset)
}

// Signature matching helpers.
unsafe fn find_hook_target(module_base: usize, pattern: &[PatternByte]) -> Option<usize> {
    find_pattern(module_base, module_size(module_base)?, pattern)
}

unsafe fn verify_hook_fallback_target(
    module_base: usize,
    fallback_rva: usize,
    pattern: &[PatternByte],
) -> Option<usize> {
    let target_addr = add_offset(module_base, fallback_rva)?;
    pattern_matches(target_addr, pattern).then_some(target_addr)
}

unsafe fn module_size(module_base: usize) -> Option<usize> {
    let pe_header_offset = read_u32(add_offset(module_base, PE_LFANEW_OFFSET)?)? as usize;
    let pe_header = add_offset(module_base, pe_header_offset)?;
    read_u32(add_offset(pe_header, PE_SIZE_OF_IMAGE_OFFSET)?)?
        .try_into()
        .ok()
}

unsafe fn find_pattern(
    module_base: usize,
    module_size: usize,
    pattern: &[PatternByte],
) -> Option<usize> {
    if pattern.is_empty() || module_size < pattern.len() {
        return None;
    }

    let module_bytes = std::slice::from_raw_parts(module_base as *const u8, module_size);
    for offset in 0..=(module_size - pattern.len()) {
        if pattern
            .iter()
            .enumerate()
            .all(|(index, expected)| pattern_byte_matches(module_bytes[offset + index], *expected))
        {
            return Some(module_base + offset);
        }
    }

    None
}

unsafe fn pattern_matches(addr: usize, pattern: &[PatternByte]) -> bool {
    pattern
        .iter()
        .enumerate()
        .all(|(index, expected)| pattern_byte_matches(*((addr + index) as *const u8), *expected))
}

fn pattern_byte_matches(actual: u8, expected: PatternByte) -> bool {
    match expected {
        PatternByte::Exact(byte) => actual == byte,
        PatternByte::Any => true,
    }
}
