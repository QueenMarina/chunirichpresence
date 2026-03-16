use crate::logging::log_message;
use crate::types::{PresenceState, Song};
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleA};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

const GAME_MODULE_CANDIDATES: &[&[u8]] = &[b"chusanApp.exe\0", b"chuniApp.exe\0"];

#[cfg(target_env = "gnu")]
const SONG_ID_BASE_OFFSET: usize = 0x0180EDF4;
#[cfg(target_env = "gnu")]
const SONG_ID_OFFSETS: &[usize] = &[0xD60];

#[cfg(target_env = "msvc")]
const SONG_ID_BASE_OFFSET: usize = 0x01851FEC;
#[cfg(target_env = "msvc")]
const SONG_ID_OFFSETS: &[usize] = &[0x4, 0xE9C, 0x34, 0x148];

#[cfg(target_env = "gnu")]
const DIFFICULTY_BASE_OFFSET: usize = 0x0180EF28;
#[cfg(target_env = "gnu")]
const DIFFICULTY_OFFSETS: &[usize] = &[0x44, 0x15C];

#[cfg(target_env = "msvc")]
const DIFFICULTY_BASE_OFFSET: usize = 0x01839540;
#[cfg(target_env = "msvc")]
const DIFFICULTY_OFFSETS: &[usize] = &[0x3C, 0x4, 0x1AC, 0x2B0];

#[cfg(target_env = "gnu")]
const PLAY_STATE_BASE_OFFSET: usize = 0x01839540;
#[cfg(target_env = "gnu")]
const PLAY_STATE_OFFSETS: &[usize] = &[0x18, 0x1D4, 0x0, 0xD8, 0xC];

#[cfg(target_env = "msvc")]
const PLAY_STATE_BASE_OFFSET: usize = 0x0000CDC0;
#[cfg(target_env = "msvc")]
const PLAY_STATE_OFFSETS: &[usize] = &[0xC44];

pub unsafe fn read_presence_state_from_memory(
    songs_by_id: &Arc<RwLock<HashMap<i32, Song>>>,
    was_playing: bool,
    last_song_id: &mut i32,
    latched_difficulty: &mut Option<i32>,
) -> Option<(bool, PresenceState)> {
    let is_playing = get_is_playing()?;
    if !is_playing {
        if was_playing {
            log_message("Stopped playing, back to song select".to_string());
        }
        *last_song_id = -1;
        *latched_difficulty = None;
        return Some((false, PresenceState::Default));
    }

    let current_song_id = get_current_song_id();

    if !was_playing {
        *latched_difficulty = Some(get_current_difficulty().unwrap_or(-1));
        crate::log_started_playing(current_song_id);
    }

    let current_difficulty = latched_difficulty.unwrap_or(-1);

    let desired_presence_state = crate::resolve_playing_presence_state(
        songs_by_id,
        current_song_id,
        current_difficulty,
        last_song_id,
    );
    Some((true, desired_presence_state))
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

        if let Some(play_state_base_addr) = add_offset(module_base, PLAY_STATE_BASE_OFFSET) {
            if let Some((pointer_offsets, value_offset)) =
                split_pointer_chain_offsets(PLAY_STATE_OFFSETS)
            {
                probe_pointer_chain("play-state", play_state_base_addr, pointer_offsets, value_offset);
            } else {
                log_message("Memory probe: play-state offsets are empty".to_string());
            }
        } else {
            log_message("Memory probe: play-state base offset overflowed".to_string());
        }

        if let Some(song_base_addr) = add_offset(module_base, SONG_ID_BASE_OFFSET) {
            if let Some((pointer_offsets, value_offset)) = split_pointer_chain_offsets(SONG_ID_OFFSETS) {
                probe_pointer_chain("song-id", song_base_addr, pointer_offsets, value_offset);
            } else {
                log_message("Memory probe: song-id offsets are empty".to_string());
            }
        } else {
            log_message("Memory probe: song-id base offset overflowed".to_string());
        }

        if let Some(difficulty_base_addr) = add_offset(module_base, DIFFICULTY_BASE_OFFSET) {
            if let Some((pointer_offsets, value_offset)) =
                split_pointer_chain_offsets(DIFFICULTY_OFFSETS)
            {
                probe_pointer_chain("difficulty", difficulty_base_addr, pointer_offsets, value_offset);
            } else {
                log_message("Memory probe: difficulty offsets are empty".to_string());
            }
        } else {
            log_message("Memory probe: difficulty base offset overflowed".to_string());
        }
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
    let module_handle = get_module_handle()?;
    let base_ptr_addr = add_offset(module_handle as usize, SONG_ID_BASE_OFFSET)?;
    read_i32_from_pointer_chain(base_ptr_addr, SONG_ID_OFFSETS)
}

unsafe fn get_is_playing() -> Option<bool> {
    let module_handle = get_module_handle()?;
    let base_ptr_addr = add_offset(module_handle as usize, PLAY_STATE_BASE_OFFSET)?;
    let play_state = read_i32_from_pointer_chain(base_ptr_addr, PLAY_STATE_OFFSETS)?;
    Some(play_state > 7)
}

unsafe fn get_current_difficulty() -> Option<i32> {
    let module_handle = get_module_handle()?;
    let base_ptr_addr = add_offset(module_handle as usize, DIFFICULTY_BASE_OFFSET)?;
    read_i32_from_pointer_chain(base_ptr_addr, DIFFICULTY_OFFSETS)
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

unsafe fn read_usize(addr: usize) -> Option<usize> {
    read_memory::<usize>(addr)
}

unsafe fn read_i32(addr: usize) -> Option<i32> {
    read_memory::<i32>(addr)
}

fn add_offset(addr: usize, offset: usize) -> Option<usize> {
    addr.checked_add(offset)
}

fn split_pointer_chain_offsets(offsets: &[usize]) -> Option<(&[usize], usize)> {
    let (&value_offset, pointer_offsets) = offsets.split_last()?;
    Some((pointer_offsets, value_offset))
}

fn format_addr(addr: usize) -> String {
    format!("0x{addr:08X}")
}

fn printable_ascii_hint(value: usize) -> Option<String> {
    let bytes = value.to_le_bytes();
    let ascii_bytes = bytes
        .iter()
        .copied()
        .take_while(|byte| *byte != 0)
        .collect::<Vec<_>>();

    if ascii_bytes.len() < 3 || ascii_bytes.iter().any(|byte| !(32..=126).contains(byte)) {
        return None;
    }

    Some(String::from_utf8_lossy(&ascii_bytes).into_owned())
}

unsafe fn log_pointer_read(label: &str, addr: usize) -> Option<usize> {
    match read_usize(addr) {
        Some(0) => {
            log_message(format!("Memory probe: {} at {} is null", label, format_addr(addr)));
            None
        }
        Some(value) => {
            if let Some(ascii_hint) = printable_ascii_hint(value) {
                log_message(format!(
                    "Memory probe: {} at {} -> {} (ASCII '{}')",
                    label,
                    format_addr(addr),
                    format_addr(value),
                    ascii_hint
                ));
            } else {
                log_message(format!(
                    "Memory probe: {} at {} -> {}",
                    label,
                    format_addr(addr),
                    format_addr(value)
                ));
            }
            Some(value)
        }
        None => {
            log_message(format!(
                "Memory probe: failed to read {} at {}",
                label,
                format_addr(addr)
            ));
            None
        }
    }
}

unsafe fn probe_pointer_chain(label: &str, base_addr: usize, pointer_offsets: &[usize], value_offset: usize) {
    let Some(mut addr) = log_pointer_read(&format!("{} base pointer", label), base_addr) else {
        return;
    };

    for (index, offset) in pointer_offsets.iter().enumerate() {
        let Some(next_ptr_addr) = add_offset(addr, *offset) else {
            log_message(format!(
                "Memory probe: {} pointer offset {} overflowed from {}",
                label,
                index,
                format_addr(addr)
            ));
            return;
        };

        let stage_label = format!("{} pointer {}", label, index + 1);
        let Some(next_addr) = log_pointer_read(&stage_label, next_ptr_addr) else {
            return;
        };
        addr = next_addr;
    }

    let Some(value_addr) = add_offset(addr, value_offset) else {
        log_message(format!(
            "Memory probe: {} value offset overflowed from {}",
            label,
            format_addr(addr)
        ));
        return;
    };

    match read_i32(value_addr) {
        Some(value) => log_message(format!(
            "Memory probe: {} value at {} -> {}",
            label,
            format_addr(value_addr),
            value
        )),
        None => log_message(format!(
            "Memory probe: failed to read {} value at {}",
            label,
            format_addr(value_addr)
        )),
    }
}

unsafe fn read_i32_from_pointer_chain(base_ptr_addr: usize, offsets: &[usize]) -> Option<i32> {
    let (pointer_offsets, value_offset) = split_pointer_chain_offsets(offsets)?;

    let mut addr = read_usize(base_ptr_addr)?;
    if addr == 0 {
        return None;
    }

    for &offset in pointer_offsets {
        let next_ptr_addr = add_offset(addr, offset)?;
        addr = read_usize(next_ptr_addr)?;
        if addr == 0 {
            return None;
        }
    }

    let value_addr = add_offset(addr, value_offset)?;
    read_i32(value_addr)
}
