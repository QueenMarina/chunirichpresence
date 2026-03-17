use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};
use windows_sys::Win32::Foundation::{HMODULE, SYSTEMTIME};
use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;

const DEBUG_ENV_VAR_NAME: &str = "CHUNIRICHPRESENCE_DEBUG";
const LOG_FILE_NAME: &str = "chunirichpresence.log";
const CRASH_LOG_FILE_NAME: &str = "chunirichpresence_crash.log";

static LOG_FILE: OnceLock<Mutex<Option<fs::File>>> = OnceLock::new();
static DLL_MODULE: AtomicIsize = AtomicIsize::new(0);
static GENERAL_LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static CRASH_LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static DEBUG_LOGGING_ENABLED: OnceLock<bool> = OnceLock::new();

pub fn set_dll_module(module: HMODULE) {
    DLL_MODULE.store(module, Ordering::Relaxed);
}

fn parse_debug_env(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

pub fn debug_logging_enabled() -> bool {
    *DEBUG_LOGGING_ENABLED.get_or_init(|| {
        std::env::var(DEBUG_ENV_VAR_NAME)
            .map(|value| parse_debug_env(&value))
            .unwrap_or(false)
    })
}

fn dll_module_handle() -> Option<HMODULE> {
    let module = DLL_MODULE.load(Ordering::Relaxed);
    if module == 0 {
        None
    } else {
        Some(module as HMODULE)
    }
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

unsafe fn module_base_dir(module: HMODULE) -> Option<PathBuf> {
    module_path(module).and_then(|path| path.parent().map(|parent| parent.to_path_buf()))
}

fn dll_base_dir() -> Option<PathBuf> {
    let module = dll_module_handle()?;
    unsafe { module_base_dir(module) }
}

fn process_base_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.to_path_buf()))
}

pub fn runtime_base_dir() -> PathBuf {
    dll_base_dir()
        .or_else(process_base_dir)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn log_file_path() -> PathBuf {
    GENERAL_LOG_PATH
        .get()
        .cloned()
        .unwrap_or_else(|| runtime_base_dir().join(LOG_FILE_NAME))
}

pub fn crash_log_path() -> PathBuf {
    CRASH_LOG_PATH
        .get()
        .cloned()
        .unwrap_or_else(|| runtime_base_dir().join(CRASH_LOG_FILE_NAME))
}

fn timestamp_string() -> String {
    unsafe {
        let mut local_time = std::mem::zeroed::<SYSTEMTIME>();
        GetLocalTime(&mut local_time);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
            local_time.wYear,
            local_time.wMonth,
            local_time.wDay,
            local_time.wHour,
            local_time.wMinute,
            local_time.wSecond,
            local_time.wMilliseconds
        )
    }
}

fn candidate_log_paths(file_name: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let candidate_dirs = [
        dll_base_dir(),
        process_base_dir(),
        std::env::current_dir().ok(),
        Some(std::env::temp_dir()),
    ];

    for dir in candidate_dirs.into_iter().flatten() {
        let path = dir.join(file_name);
        if !paths.iter().any(|existing| existing == &path) {
            paths.push(path);
        }
    }

    if paths.is_empty() {
        paths.push(PathBuf::from(file_name));
    }

    paths
}

fn open_append_file_at(path: &PathBuf) -> Option<fs::File> {
    OpenOptions::new().create(true).append(true).open(path).ok()
}

fn open_append_file_with_fallback(
    file_name: &str,
    selected_path: &OnceLock<PathBuf>,
) -> Option<fs::File> {
    if let Some(path) = selected_path.get() {
        return open_append_file_at(path);
    }

    for path in candidate_log_paths(file_name) {
        if let Some(file) = open_append_file_at(&path) {
            let _ = selected_path.set(path);
            return Some(file);
        }
    }

    None
}

fn open_general_log_file() -> Option<fs::File> {
    open_append_file_with_fallback(LOG_FILE_NAME, &GENERAL_LOG_PATH)
}

fn open_crash_log_file() -> Option<fs::File> {
    open_append_file_with_fallback(CRASH_LOG_FILE_NAME, &CRASH_LOG_PATH)
}

fn append_general_log_line(line: &str) {
    let log_file = LOG_FILE.get_or_init(|| Mutex::new(open_general_log_file()));
    let Ok(mut file_guard) = log_file.lock() else {
        return;
    };

    if file_guard.is_none() {
        *file_guard = open_general_log_file();
    }

    if let Some(file) = file_guard.as_mut() {
        let _ = writeln!(file, "{}", line);
        let _ = file.flush();
        let _ = file.sync_data();
    }
}

fn append_crash_log_line(line: &str) {
    if let Some(mut file) = open_crash_log_file() {
        let _ = writeln!(file, "{}", line);
        let _ = file.flush();
        let _ = file.sync_data();
    }
}

pub fn log_message(message: String) {
    if !debug_logging_enabled() {
        return;
    }

    let line = format!("[{}][ChuniRichPresence] {}", timestamp_string(), message);
    append_general_log_line(&line);
}

pub fn write_crash_log(message: String) {
    if !debug_logging_enabled() {
        return;
    }

    let line = format!("[{}][ChuniRichPresence] {}", timestamp_string(), message);
    append_crash_log_line(&line);
}
