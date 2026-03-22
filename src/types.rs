use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Once, OnceLock};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Song {
    pub id: String,
    pub title: String,
    pub artist: String,
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

#[derive(Copy, Clone)]
pub(crate) enum PatternByte {
    Exact(u8),
    Any,
}

#[derive(Clone, Debug)]
pub(crate) enum HookInstallError {
    GameModuleHandleNotFound,
    HookSignatureNotFound(&'static str),
    RelativeJumpSourceOverflow,
    RelativeJumpOutOfRange,
    HookPatchOverflow,
    VirtualProtectFailed,
    ExecutableBlockAllocationFailed,
}

impl fmt::Display for HookInstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GameModuleHandleNotFound => f.write_str("game module handle not found"),
            Self::HookSignatureNotFound(name) => {
                write!(f, "{name} hook signature not found")
            }
            Self::RelativeJumpSourceOverflow => f.write_str("relative jump source overflowed"),
            Self::RelativeJumpOutOfRange => f.write_str("relative jump out of range"),
            Self::HookPatchOverflow => f.write_str("hook patch overflowed"),
            Self::VirtualProtectFailed => f.write_str("VirtualProtect failed while patching hook"),
            Self::ExecutableBlockAllocationFailed => {
                f.write_str("VirtualAlloc failed for executable block")
            }
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub(crate) enum HookResolutionSource {
    HardcodedAddress,
    PatternScan,
}

impl fmt::Display for HookResolutionSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HardcodedAddress => f.write_str("hardcoded address"),
            Self::PatternScan => f.write_str("pattern scan"),
        }
    }
}

pub(crate) struct HookState {
    pub once: Once,
    pub installed: AtomicBool,
    pub target: AtomicUsize,
    pub trampoline: AtomicUsize,
    pub stub: AtomicUsize,
    pub resolution_source: OnceLock<HookResolutionSource>,
    pub install_error: OnceLock<HookInstallError>,
}

impl HookState {
    pub(crate) const fn new() -> Self {
        Self {
            once: Once::new(),
            installed: AtomicBool::new(false),
            target: AtomicUsize::new(0),
            trampoline: AtomicUsize::new(0),
            stub: AtomicUsize::new(0),
            resolution_source: OnceLock::new(),
            install_error: OnceLock::new(),
        }
    }

    pub(crate) fn store_install(&self, install: InstalledHook) {
        self.installed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.target
            .store(install.target_addr, std::sync::atomic::Ordering::Relaxed);
        self.trampoline.store(
            install.trampoline_addr,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.stub
            .store(install.stub_addr, std::sync::atomic::Ordering::Relaxed);
        let _ = self.resolution_source.set(install.resolution_source);
    }

    pub(crate) fn store_error(&self, error: HookInstallError) {
        let _ = self.install_error.set(error);
    }

    pub(crate) fn target_addr(&self) -> Option<usize> {
        match self.target.load(std::sync::atomic::Ordering::Relaxed) {
            0 => None,
            addr => Some(addr),
        }
    }

    pub(crate) fn resolution_source(&self) -> Option<HookResolutionSource> {
        self.resolution_source.get().copied()
    }
}

pub(crate) struct HookConfig {
    pub name: &'static str,
    pub targets: &'static [HookTargetConfig],
    pub callback: unsafe extern "system" fn(*const PushadRegisters),
}

#[derive(Copy, Clone)]
pub(crate) struct HookTargetConfig {
    pub overwrite_len: usize,
    pub fallback_rva: usize,
    pub pattern: &'static [PatternByte],
}

pub(crate) struct InstalledHook {
    pub target_addr: usize,
    pub trampoline_addr: usize,
    pub stub_addr: usize,
    pub resolution_source: HookResolutionSource,
}

#[repr(C)]
pub(crate) struct PushadRegisters {
    pub edi: u32,
    pub esi: u32,
    pub ebp: u32,
    pub esp: u32,
    pub ebx: u32,
    pub edx: u32,
    pub ecx: u32,
    pub eax: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct GameModuleInfo {
    pub path: Option<PathBuf>,
    pub base_addr: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct HookInstallStatus {
    pub name: &'static str,
    pub target_addr: Option<usize>,
    pub resolution_source: Option<HookResolutionSource>,
    pub error: Option<HookInstallError>,
}

#[derive(Clone, Debug)]
pub(crate) struct IntegerHookProbeStatus {
    pub installed: bool,
    pub target_addr: Option<usize>,
    pub cached_value: Option<i32>,
}

#[derive(Clone, Debug)]
pub(crate) struct PlayStateHookProbeStatus {
    pub enter_installed: bool,
    pub enter_target_addr: Option<usize>,
    pub exit_installed: bool,
    pub exit_target_addr: Option<usize>,
    pub cached_value: Option<bool>,
}

#[derive(Clone, Debug)]
pub(crate) struct MemoryProbeStatus {
    pub module: Option<GameModuleInfo>,
    pub song_id: IntegerHookProbeStatus,
    pub difficulty: IntegerHookProbeStatus,
    pub play_state: PlayStateHookProbeStatus,
}

#[derive(Clone, Debug)]
pub(crate) struct MemorySnapshot {
    pub is_playing: Option<bool>,
    pub song_id: Option<i32>,
    pub difficulty: Option<i32>,
}
