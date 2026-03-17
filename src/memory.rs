use crate::logging::log_message;
use crate::types::{PresenceState, Song};
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::Once;
use std::sync::{Arc, RwLock};
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::Diagnostics::Debug::{FlushInstructionCache, ReadProcessMemory};
use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleA};
use windows_sys::Win32::System::Memory::{
    VirtualAlloc, VirtualProtect, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

const GAME_MODULE_CANDIDATES: &[&[u8]] = &[b"chusanApp.exe\0", b"chuniApp.exe\0"];
const SONG_ID_HOOK_OVERWRITE_LEN: usize = 6;
const SONG_ID_HOOK_FALLBACK_RVA: usize = 0x00965C35;
const DIFFICULTY_HOOK_OVERWRITE_LEN: usize = 6;
const DIFFICULTY_HOOK_FALLBACK_RVA: usize = 0x0085BB8A;
const PLAY_STATE_ENTER_HOOK_OVERWRITE_LEN: usize = 6;
const PLAY_STATE_ENTER_HOOK_FALLBACK_RVA: usize = 0x00F5C4CE;
const PLAY_STATE_EXIT_HOOK_OVERWRITE_LEN: usize = 6;
const PLAY_STATE_EXIT_HOOK_FALLBACK_RVA: usize = 0x00F5DDDC;
const PLAY_STATE_VALUE_RVA: usize = 0x018849E0;
const SONG_ID_UNSET: i32 = i32::MIN;
const DIFFICULTY_UNSET: i32 = i32::MIN;
const PLAY_STATE_UNSET: i32 = i32::MIN;
const PLAY_STATE_VALUE_PLAYING: i32 = 2;
const PLAY_STATE_VALUE_SONG_SELECT: i32 = 3;
const PE_LFANEW_OFFSET: usize = 0x3C;
const PE_SIZE_OF_IMAGE_OFFSET: usize = 0x50;

static SONG_ID_HOOK_ONCE: Once = Once::new();
static SONG_ID_HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);
static SONG_ID_HOOK_TARGET: AtomicUsize = AtomicUsize::new(0);
static SONG_ID_HOOK_TRAMPOLINE: AtomicUsize = AtomicUsize::new(0);
static SONG_ID_HOOK_STUB: AtomicUsize = AtomicUsize::new(0);
static LAST_HOOKED_SONG_ID: AtomicI32 = AtomicI32::new(SONG_ID_UNSET);
static DIFFICULTY_HOOK_ONCE: Once = Once::new();
static DIFFICULTY_HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);
static DIFFICULTY_HOOK_TARGET: AtomicUsize = AtomicUsize::new(0);
static DIFFICULTY_HOOK_TRAMPOLINE: AtomicUsize = AtomicUsize::new(0);
static DIFFICULTY_HOOK_STUB: AtomicUsize = AtomicUsize::new(0);
static LAST_HOOKED_DIFFICULTY: AtomicI32 = AtomicI32::new(DIFFICULTY_UNSET);
static PLAY_STATE_ENTER_HOOK_ONCE: Once = Once::new();
static PLAY_STATE_ENTER_HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);
static PLAY_STATE_ENTER_HOOK_TARGET: AtomicUsize = AtomicUsize::new(0);
static PLAY_STATE_ENTER_HOOK_TRAMPOLINE: AtomicUsize = AtomicUsize::new(0);
static PLAY_STATE_ENTER_HOOK_STUB: AtomicUsize = AtomicUsize::new(0);
static PLAY_STATE_EXIT_HOOK_ONCE: Once = Once::new();
static PLAY_STATE_EXIT_HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);
static PLAY_STATE_EXIT_HOOK_TARGET: AtomicUsize = AtomicUsize::new(0);
static PLAY_STATE_EXIT_HOOK_TRAMPOLINE: AtomicUsize = AtomicUsize::new(0);
static PLAY_STATE_EXIT_HOOK_STUB: AtomicUsize = AtomicUsize::new(0);
static LAST_HOOKED_PLAY_STATE: AtomicI32 = AtomicI32::new(PLAY_STATE_UNSET);

const SONG_ID_HOOK_PATTERN: &[Option<u8>] = &[
    Some(0x83),
    Some(0xC4),
    Some(0x1C),
    Some(0x89),
    Some(0x46),
    Some(0x60),
    Some(0xE8),
    None,
    None,
    None,
    None,
    Some(0x6A),
    Some(0x01),
    Some(0xE9),
    None,
    None,
    None,
    None,
    Some(0x8D),
    Some(0x46),
    Some(0x58),
];

const DIFFICULTY_HOOK_PATTERN: &[Option<u8>] = &[
    Some(0x88),
    Some(0x41),
    Some(0x14),
    Some(0x8B),
    Some(0x46),
    Some(0x18),
    Some(0x89),
    Some(0x41),
    Some(0x18),
    Some(0x8B),
    Some(0x46),
    Some(0x1C),
    Some(0x89),
    Some(0x41),
    Some(0x1C),
    Some(0x8B),
    Some(0x46),
    Some(0x20),
    Some(0x8D),
    Some(0x73),
    Some(0x08),
    Some(0x89),
    Some(0x41),
    Some(0x20),
];

const PLAY_STATE_ENTER_HOOK_PATTERN: &[Option<u8>] = &[
    Some(0x89),
    Some(0x1D),
    None,
    None,
    None,
    None,
    Some(0x89),
    Some(0x70),
    Some(0x04),
    Some(0x5B),
];

const PLAY_STATE_EXIT_HOOK_PATTERN: &[Option<u8>] = &[
    Some(0x89),
    Some(0x1D),
    None,
    None,
    None,
    None,
    Some(0x5F),
    Some(0x5B),
];

#[repr(C)]
struct PushadRegisters {
    edi: u32,
    esi: u32,
    ebp: u32,
    esp: u32,
    ebx: u32,
    edx: u32,
    ecx: u32,
    eax: u32,
}

pub unsafe fn read_presence_state_from_memory(
    songs_by_id: &Arc<RwLock<HashMap<i32, Song>>>,
    was_playing: bool,
    last_song_id: &mut i32,
    latched_song_id: &mut Option<i32>,
    latched_difficulty: &mut Option<i32>,
) -> Option<(bool, PresenceState)> {
    let is_playing = get_is_playing()?;
    if !is_playing {
        if was_playing {
            log_message("Stopped playing, back to song select".to_string());
        }
        *last_song_id = -1;
        *latched_song_id = None;
        *latched_difficulty = None;
        return Some((false, PresenceState::Default));
    }

    let live_song_id = get_current_song_id();
    let current_difficulty = get_current_difficulty();

    if !was_playing {
        let Some(song_id) = live_song_id.filter(|song_id| *song_id != -1) else {
            *last_song_id = -1;
            *latched_song_id = None;
            *latched_difficulty = None;
            return Some((false, PresenceState::Default));
        };

        let Some(difficulty) = current_difficulty else {
            *last_song_id = -1;
            *latched_song_id = None;
            *latched_difficulty = None;
            return Some((false, PresenceState::Default));
        };

        *latched_song_id = Some(song_id);
        *latched_difficulty = Some(difficulty);
        crate::log_started_playing(Some(song_id));
    }

    let current_song_id = *latched_song_id;
    let current_difficulty = latched_difficulty
        .or(current_difficulty)
        .unwrap_or(-1);

    let desired_presence_state = crate::resolve_playing_presence_state(
        songs_by_id,
        current_song_id,
        current_difficulty,
        last_song_id,
    );
    Some((true, desired_presence_state))
}

pub fn install_runtime_hooks() {
    SONG_ID_HOOK_ONCE.call_once(|| unsafe {
        match install_song_id_hook_once() {
            Ok(target_addr) => {
                SONG_ID_HOOK_INSTALLED.store(true, Ordering::Relaxed);
                SONG_ID_HOOK_TARGET.store(target_addr, Ordering::Relaxed);
                log_message(format!(
                    "Installed song ID hook at {}",
                    format_addr(target_addr)
                ));
            }
            Err(error) => {
                log_message(format!(
                    "Failed to install song ID hook: {}",
                    error
                ));
            }
        }
    });

    DIFFICULTY_HOOK_ONCE.call_once(|| unsafe {
        match install_difficulty_hook_once() {
            Ok(target_addr) => {
                DIFFICULTY_HOOK_INSTALLED.store(true, Ordering::Relaxed);
                DIFFICULTY_HOOK_TARGET.store(target_addr, Ordering::Relaxed);
                log_message(format!(
                    "Installed difficulty hook at {}",
                    format_addr(target_addr)
                ));
            }
            Err(error) => {
                log_message(format!(
                    "Failed to install difficulty hook: {}",
                    error
                ));
            }
        }
    });

    PLAY_STATE_ENTER_HOOK_ONCE.call_once(|| unsafe {
        match install_play_state_enter_hook_once() {
            Ok(target_addr) => {
                PLAY_STATE_ENTER_HOOK_INSTALLED.store(true, Ordering::Relaxed);
                PLAY_STATE_ENTER_HOOK_TARGET.store(target_addr, Ordering::Relaxed);
                log_message(format!(
                    "Installed play-state enter hook at {}",
                    format_addr(target_addr)
                ));
            }
            Err(error) => {
                log_message(format!(
                    "Failed to install play-state enter hook: {}",
                    error
                ));
            }
        }
    });

    PLAY_STATE_EXIT_HOOK_ONCE.call_once(|| unsafe {
        match install_play_state_exit_hook_once() {
            Ok(target_addr) => {
                PLAY_STATE_EXIT_HOOK_INSTALLED.store(true, Ordering::Relaxed);
                PLAY_STATE_EXIT_HOOK_TARGET.store(target_addr, Ordering::Relaxed);
                log_message(format!(
                    "Installed play-state exit hook at {}",
                    format_addr(target_addr)
                ));
            }
            Err(error) => {
                log_message(format!(
                    "Failed to install play-state exit hook: {}",
                    error
                ));
            }
        }
    });
}

pub fn log_memory_probe_status() {
    unsafe {
        let Some(module_handle) = get_module_handle() else {
            log_message("Memory probe: failed to resolve game module handle".to_string());
            return;
        };

        let module_base = module_handle as usize;
        if let Some(path) = module_path(module_handle as HMODULE) {
            log_message(format!("Memory probe: game module path {}", path.display()));
        }
        log_message(format!("Memory probe: game module base {}", format_addr(module_base)));
        log_message(format!(
            "Memory probe: song-id hook installed={} target={} cached={:?}",
            SONG_ID_HOOK_INSTALLED.load(Ordering::Relaxed),
            format_optional_addr(SONG_ID_HOOK_TARGET.load(Ordering::Relaxed)),
            current_hooked_song_id()
        ));
        log_message(format!(
            "Memory probe: difficulty hook installed={} target={} cached={:?}",
            DIFFICULTY_HOOK_INSTALLED.load(Ordering::Relaxed),
            format_optional_addr(DIFFICULTY_HOOK_TARGET.load(Ordering::Relaxed)),
            current_hooked_difficulty()
        ));
        log_message(format!(
            "Memory probe: play-state hooks enter_installed={} enter_target={} exit_installed={} exit_target={} cached={:?}",
            PLAY_STATE_ENTER_HOOK_INSTALLED.load(Ordering::Relaxed),
            format_optional_addr(PLAY_STATE_ENTER_HOOK_TARGET.load(Ordering::Relaxed)),
            PLAY_STATE_EXIT_HOOK_INSTALLED.load(Ordering::Relaxed),
            format_optional_addr(PLAY_STATE_EXIT_HOOK_TARGET.load(Ordering::Relaxed)),
            current_hooked_play_state()
        ));
    }
}

pub fn log_memory_snapshot() {
    unsafe {
        log_message(format!(
            "Memory snapshot: is_playing={:?}, song_id={:?}, difficulty={:?}",
            get_is_playing(),
            get_current_song_id(),
            get_current_difficulty()
        ));
    }
}

pub fn log_resolved_game_module() {
    unsafe {
        let Some(module_handle) = get_module_handle() else {
            log_message("Failed to resolve game module handle".to_string());
            return;
        };

        if let Some(path) = module_path(module_handle as HMODULE) {
            log_message(format!("Resolved game module: {}", path.display()));
        }
        log_message(format!(
            "Resolved game module base: {}",
            format_addr(module_handle as usize)
        ));
    }
}

unsafe fn get_current_song_id() -> Option<i32> {
    current_hooked_song_id()
}

unsafe fn get_is_playing() -> Option<bool> {
    Some(
        read_live_play_state()
            .or_else(current_hooked_play_state)
            .unwrap_or(false),
    )
}

unsafe fn get_current_difficulty() -> Option<i32> {
    current_hooked_difficulty()
}

unsafe fn get_module_handle() -> Option<isize> {
    for module_name in GAME_MODULE_CANDIDATES {
        let module_handle = GetModuleHandleA(module_name.as_ptr());
        if module_handle != 0 {
            return Some(module_handle);
        }
    }

    let module_handle = GetModuleHandleA(std::ptr::null());
    if module_handle == 0 {
        return None;
    }

    Some(module_handle)
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

fn add_offset(addr: usize, offset: usize) -> Option<usize> {
    addr.checked_add(offset)
}

fn format_addr(addr: usize) -> String {
    format!("0x{addr:08X}")
}

fn format_optional_addr(addr: usize) -> String {
    if addr == 0 {
        "none".to_string()
    } else {
        format_addr(addr)
    }
}

fn current_hooked_song_id() -> Option<i32> {
    match LAST_HOOKED_SONG_ID.load(Ordering::Relaxed) {
        SONG_ID_UNSET => None,
        value => Some(value),
    }
}

fn current_hooked_difficulty() -> Option<i32> {
    match LAST_HOOKED_DIFFICULTY.load(Ordering::Relaxed) {
        DIFFICULTY_UNSET => None,
        value => Some(value),
    }
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

unsafe fn install_song_id_hook_once() -> Result<usize, String> {
    let module_handle = get_module_handle().ok_or_else(|| "game module handle not found".to_string())?;
    let module_base = module_handle as usize;
    let target_addr = verify_song_id_hook_fallback_target(module_base)
        .or_else(|| find_song_id_hook_target(module_base))
        .ok_or_else(|| "song ID hook signature not found".to_string())?;

    let trampoline_addr = create_song_id_trampoline(target_addr)?;
    let stub_addr = create_song_id_hook_stub(trampoline_addr)?;
    patch_song_id_hook_target(target_addr, stub_addr)?;

    SONG_ID_HOOK_TRAMPOLINE.store(trampoline_addr, Ordering::Relaxed);
    SONG_ID_HOOK_STUB.store(stub_addr, Ordering::Relaxed);

    Ok(target_addr)
}

unsafe fn install_difficulty_hook_once() -> Result<usize, String> {
    let module_handle = get_module_handle().ok_or_else(|| "game module handle not found".to_string())?;
    let module_base = module_handle as usize;
    let target_addr = verify_difficulty_hook_fallback_target(module_base)
        .or_else(|| find_difficulty_hook_target(module_base))
        .ok_or_else(|| "difficulty hook signature not found".to_string())?;

    let trampoline_addr = create_trampoline(target_addr, DIFFICULTY_HOOK_OVERWRITE_LEN)?;
    let stub_addr = create_hook_stub(trampoline_addr, difficulty_hook_callback)?;
    patch_hook_target(target_addr, DIFFICULTY_HOOK_OVERWRITE_LEN, stub_addr)?;

    DIFFICULTY_HOOK_TRAMPOLINE.store(trampoline_addr, Ordering::Relaxed);
    DIFFICULTY_HOOK_STUB.store(stub_addr, Ordering::Relaxed);

    Ok(target_addr)
}

unsafe fn install_play_state_enter_hook_once() -> Result<usize, String> {
    let module_handle = get_module_handle().ok_or_else(|| "game module handle not found".to_string())?;
    let module_base = module_handle as usize;
    let target_addr = verify_play_state_enter_hook_fallback_target(module_base)
        .or_else(|| find_play_state_enter_hook_target(module_base))
        .ok_or_else(|| "play-state enter hook signature not found".to_string())?;

    let trampoline_addr = create_trampoline(target_addr, PLAY_STATE_ENTER_HOOK_OVERWRITE_LEN)?;
    let stub_addr = create_hook_stub(trampoline_addr, play_state_enter_hook_callback)?;
    patch_hook_target(target_addr, PLAY_STATE_ENTER_HOOK_OVERWRITE_LEN, stub_addr)?;

    PLAY_STATE_ENTER_HOOK_TRAMPOLINE.store(trampoline_addr, Ordering::Relaxed);
    PLAY_STATE_ENTER_HOOK_STUB.store(stub_addr, Ordering::Relaxed);

    Ok(target_addr)
}

unsafe fn install_play_state_exit_hook_once() -> Result<usize, String> {
    let module_handle = get_module_handle().ok_or_else(|| "game module handle not found".to_string())?;
    let module_base = module_handle as usize;
    let target_addr = verify_play_state_exit_hook_fallback_target(module_base)
        .or_else(|| find_play_state_exit_hook_target(module_base))
        .ok_or_else(|| "play-state exit hook signature not found".to_string())?;

    let trampoline_addr = create_trampoline(target_addr, PLAY_STATE_EXIT_HOOK_OVERWRITE_LEN)?;
    let stub_addr = create_hook_stub(trampoline_addr, play_state_exit_hook_callback)?;
    patch_hook_target(target_addr, PLAY_STATE_EXIT_HOOK_OVERWRITE_LEN, stub_addr)?;

    PLAY_STATE_EXIT_HOOK_TRAMPOLINE.store(trampoline_addr, Ordering::Relaxed);
    PLAY_STATE_EXIT_HOOK_STUB.store(stub_addr, Ordering::Relaxed);

    Ok(target_addr)
}

unsafe fn find_song_id_hook_target(module_base: usize) -> Option<usize> {
    let pe_header_offset = read_u32(add_offset(module_base, PE_LFANEW_OFFSET)?)? as usize;
    let pe_header = add_offset(module_base, pe_header_offset)?;
    let size_of_image = read_u32(add_offset(pe_header, PE_SIZE_OF_IMAGE_OFFSET)?)? as usize;
    find_pattern(module_base, size_of_image, SONG_ID_HOOK_PATTERN)
}

unsafe fn verify_song_id_hook_fallback_target(module_base: usize) -> Option<usize> {
    let target_addr = add_offset(module_base, SONG_ID_HOOK_FALLBACK_RVA)?;
    if pattern_matches(target_addr, SONG_ID_HOOK_PATTERN) {
        Some(target_addr)
    } else {
        None
    }
}

unsafe fn find_difficulty_hook_target(module_base: usize) -> Option<usize> {
    let pe_header_offset = read_u32(add_offset(module_base, PE_LFANEW_OFFSET)?)? as usize;
    let pe_header = add_offset(module_base, pe_header_offset)?;
    let size_of_image = read_u32(add_offset(pe_header, PE_SIZE_OF_IMAGE_OFFSET)?)? as usize;
    find_pattern(module_base, size_of_image, DIFFICULTY_HOOK_PATTERN)
}

unsafe fn verify_difficulty_hook_fallback_target(module_base: usize) -> Option<usize> {
    let target_addr = add_offset(module_base, DIFFICULTY_HOOK_FALLBACK_RVA)?;
    if pattern_matches(target_addr, DIFFICULTY_HOOK_PATTERN) {
        Some(target_addr)
    } else {
        None
    }
}

unsafe fn find_play_state_enter_hook_target(module_base: usize) -> Option<usize> {
    let pe_header_offset = read_u32(add_offset(module_base, PE_LFANEW_OFFSET)?)? as usize;
    let pe_header = add_offset(module_base, pe_header_offset)?;
    let size_of_image = read_u32(add_offset(pe_header, PE_SIZE_OF_IMAGE_OFFSET)?)? as usize;
    find_pattern(module_base, size_of_image, PLAY_STATE_ENTER_HOOK_PATTERN)
}

unsafe fn verify_play_state_enter_hook_fallback_target(module_base: usize) -> Option<usize> {
    let target_addr = add_offset(module_base, PLAY_STATE_ENTER_HOOK_FALLBACK_RVA)?;
    if pattern_matches(target_addr, PLAY_STATE_ENTER_HOOK_PATTERN) {
        Some(target_addr)
    } else {
        None
    }
}

unsafe fn find_play_state_exit_hook_target(module_base: usize) -> Option<usize> {
    let pe_header_offset = read_u32(add_offset(module_base, PE_LFANEW_OFFSET)?)? as usize;
    let pe_header = add_offset(module_base, pe_header_offset)?;
    let size_of_image = read_u32(add_offset(pe_header, PE_SIZE_OF_IMAGE_OFFSET)?)? as usize;
    find_pattern(module_base, size_of_image, PLAY_STATE_EXIT_HOOK_PATTERN)
}

unsafe fn verify_play_state_exit_hook_fallback_target(module_base: usize) -> Option<usize> {
    let target_addr = add_offset(module_base, PLAY_STATE_EXIT_HOOK_FALLBACK_RVA)?;
    if pattern_matches(target_addr, PLAY_STATE_EXIT_HOOK_PATTERN) {
        Some(target_addr)
    } else {
        None
    }
}

unsafe fn create_song_id_trampoline(target_addr: usize) -> Result<usize, String> {
    create_trampoline(target_addr, SONG_ID_HOOK_OVERWRITE_LEN)
}

unsafe fn create_trampoline(target_addr: usize, overwrite_len: usize) -> Result<usize, String> {
    let trampoline_size = overwrite_len + 5;
    let trampoline_addr = allocate_executable_block(trampoline_size)?;
    ptr::copy_nonoverlapping(
        target_addr as *const u8,
        trampoline_addr as *mut u8,
        overwrite_len,
    );

    write_relative_jump(
        trampoline_addr + overwrite_len,
        target_addr + overwrite_len,
    )?;

    Ok(trampoline_addr)
}

unsafe fn create_song_id_hook_stub(trampoline_addr: usize) -> Result<usize, String> {
    create_hook_stub(trampoline_addr, song_id_hook_callback)
}

unsafe fn create_hook_stub(
    trampoline_addr: usize,
    callback: unsafe extern "system" fn(*const PushadRegisters),
) -> Result<usize, String> {
    let stub_size = 15usize;
    let stub_addr = allocate_executable_block(stub_size)?;
    let mut cursor = stub_addr as *mut u8;

    *cursor = 0x60;
    cursor = cursor.add(1);

    *cursor = 0x54;
    cursor = cursor.add(1);

    *cursor = 0xB8;
    cursor = cursor.add(1);
    ptr::write_unaligned(
        cursor as *mut u32,
        callback as *const () as usize as u32,
    );
    cursor = cursor.add(4);

    *cursor = 0xFF;
    *cursor.add(1) = 0xD0;
    cursor = cursor.add(2);

    *cursor = 0x61;
    cursor = cursor.add(1);

    *cursor = 0xE9;
    cursor = cursor.add(1);
    let jump_back_from = cursor as usize - 1;
    ptr::write_unaligned(cursor as *mut i32, relative_jump_offset(jump_back_from, trampoline_addr)?);

    Ok(stub_addr)
}

unsafe fn patch_song_id_hook_target(target_addr: usize, stub_addr: usize) -> Result<(), String> {
    patch_hook_target(target_addr, SONG_ID_HOOK_OVERWRITE_LEN, stub_addr)
}

unsafe fn patch_hook_target(target_addr: usize, overwrite_len: usize, stub_addr: usize) -> Result<(), String> {
    let mut old_protect = 0u32;
    if VirtualProtect(
        target_addr as *const c_void,
        overwrite_len,
        PAGE_EXECUTE_READWRITE,
        &mut old_protect,
    ) == 0
    {
        return Err("VirtualProtect failed while patching hook".to_string());
    }

    write_relative_jump(target_addr, stub_addr)?;
    for offset in 5..overwrite_len {
        let nop_addr = add_offset(target_addr, offset).ok_or_else(|| "hook patch overflowed".to_string())?;
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

unsafe fn allocate_executable_block(size: usize) -> Result<usize, String> {
    let block = VirtualAlloc(
        ptr::null_mut(),
        size,
        MEM_COMMIT | MEM_RESERVE,
        PAGE_EXECUTE_READWRITE,
    ) as usize;

    if block == 0 {
        Err("VirtualAlloc failed for song ID hook".to_string())
    } else {
        Ok(block)
    }
}

unsafe fn write_relative_jump(from_addr: usize, to_addr: usize) -> Result<(), String> {
    *(from_addr as *mut u8) = 0xE9;
    let offset = relative_jump_offset(from_addr, to_addr)?;
    ptr::write_unaligned((from_addr + 1) as *mut i32, offset);
    Ok(())
}

fn relative_jump_offset(from_addr: usize, to_addr: usize) -> Result<i32, String> {
    let from_next = from_addr
        .checked_add(5)
        .ok_or_else(|| "relative jump source overflowed".to_string())?;
    let distance = to_addr as isize - from_next as isize;
    i32::try_from(distance).map_err(|_| "relative jump out of range".to_string())
}

unsafe fn find_pattern(module_base: usize, module_size: usize, pattern: &[Option<u8>]) -> Option<usize> {
    if pattern.is_empty() || module_size < pattern.len() {
        return None;
    }

    let module_bytes = std::slice::from_raw_parts(module_base as *const u8, module_size);
    for offset in 0..=(module_size - pattern.len()) {
        if pattern.iter().enumerate().all(|(index, expected)| {
            expected.map_or(true, |byte| module_bytes[offset + index] == byte)
        }) {
            return Some(module_base + offset);
        }
    }

    None
}

unsafe fn pattern_matches(addr: usize, pattern: &[Option<u8>]) -> bool {
    pattern.iter().enumerate().all(|(index, expected)| {
        expected.map_or(true, |byte| *((addr + index) as *const u8) == byte)
    })
}

unsafe fn read_u32(addr: usize) -> Option<u32> {
    read_memory::<u32>(addr)
}
