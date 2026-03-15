use std::ffi::c_void;
use std::thread;
use std::time::Duration;
use windows_sys::Win32::Foundation::{BOOL, HMODULE, TRUE};
use windows_sys::Win32::System::Console::AllocConsole;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleA;
use windows_sys::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

const SONG_ID_BASE_OFFSET: usize = 0x0180EDF4;
const SONG_ID_OFFSET: usize = 0xD60;

const PLAY_STATE_BASE_OFFSET: usize = 0x01839540;
const PLAY_STATE_OFFSETS: &[usize; 5] = &[0x18, 0x1D4, 0x0, 0xD8, 0xC];

unsafe fn get_module_handle() -> Option<isize> {
    let module_handle = GetModuleHandleA(b"chusanApp.exe\0".as_ptr());

    if module_handle == 0 {
        return None;
    }

    Some(module_handle)
}

unsafe fn get_current_song_id() -> Option<i32> {
    let module_handle = get_module_handle().unwrap();

    let base_ptr_addr = (module_handle as usize) + SONG_ID_BASE_OFFSET;

    let ptr_value = *(base_ptr_addr as *const usize);
    if ptr_value == 0 {
        return None;
    }

    let song_id_addr = ptr_value + SONG_ID_OFFSET;
    let song_id = *(song_id_addr as *const i32);

    Some(song_id)
}

unsafe fn get_is_playing() -> Option<bool> {
    let module_handle = get_module_handle()?;
    let base_ptr_addr = (module_handle as usize) + PLAY_STATE_BASE_OFFSET;


    let mut addr = *(base_ptr_addr as *const usize);
    if addr == 0 {
        return None;
    }


    for &offset in PLAY_STATE_OFFSETS.iter().take(PLAY_STATE_OFFSETS.len() - 1) {
        let next_ptr_addr = addr + offset;
        addr = *(next_ptr_addr as *const usize);
        if addr == 0 {
            return None;
        }
    }

    let last_offset = PLAY_STATE_OFFSETS[PLAY_STATE_OFFSETS.len() - 1];
    let play_state_addr = addr + last_offset;
    let play_state = *(play_state_addr as *const i32);

    println!("[ChuniRichPresence] Playing state: {}", play_state);

    Some(play_state == 8)
}


fn main_thread() {
    unsafe {
        AllocConsole();
    }
    println!("[ChuniRichPresence] Injected successfully into Chunithm!");

    let mut last_song_id = -1;

    loop {
        if let Some(current_song_id) = unsafe { get_current_song_id() } {
            if current_song_id != -1 && current_song_id != last_song_id {
                println!(
                    "[ChuniRichPresence] Now playing Song ID: {}",
                    current_song_id
                );
                last_song_id = current_song_id;
            }
        }
        unsafe {
            match get_is_playing() {
                Some(value) => {
                    if value {
                        println!("[ChuniRichPresence] Currently playing!");
                    } else {
                        println!("[ChuniRichPresence] Currently NOT playing!");
                    }
                }
                None => {
                    println!("[ChuniRichPresence] Failed to get playing state");
                }
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
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
            thread::spawn(main_thread);
            TRUE
        }
        _ => TRUE,
    }
}

// ============================================================================
// Magic stub to fix 32-bit MinGW cross-compilation linker errors
// Since we use panic="abort", this is never actually called.
#[cfg(all(target_os = "windows", target_env = "gnu", target_pointer_width = "32"))]
#[no_mangle]
pub extern "C" fn _Unwind_Resume() -> ! {
    std::process::abort();
}
