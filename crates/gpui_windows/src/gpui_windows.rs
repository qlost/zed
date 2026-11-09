#![cfg(target_os = "windows")]

mod clipboard;
mod destination_list;
mod dialog;
// Direct Manipulation is Windows 8.1+; on Win7 the handler is never constructed (dead code there).
#[cfg_attr(target_vendor = "win7", allow(dead_code))]
mod direct_manipulation;
// On Win7 only a handful of DWrite-1.0 helpers are reused from `direct_write`; the rest of the
// module is dead code there.
#[cfg_attr(target_vendor = "win7", allow(dead_code))]
mod direct_write;
#[cfg(target_vendor = "win7")]
mod direct_write_win7;
mod directx_atlas;
mod directx_devices;
mod directx_renderer;
mod dispatcher;
mod display;
mod events;
mod keyboard;
mod platform;
mod system_notifications;
mod system_settings;
mod util;
mod vsync;
mod window;
mod wrapper;

pub(crate) use clipboard::*;
pub(crate) use destination_list::*;
#[cfg(not(target_vendor = "win7"))]
pub(crate) use direct_write::*;
#[cfg(target_vendor = "win7")]
pub(crate) use direct_write_win7::*;
pub(crate) use directx_atlas::*;
pub(crate) use directx_devices::*;
pub(crate) use directx_renderer::*;
pub(crate) use dispatcher::*;
pub(crate) use display::*;
pub(crate) use events::*;
pub(crate) use keyboard::*;
pub(crate) use platform::*;
pub(crate) use system_notifications::*;
pub(crate) use system_settings::*;
pub(crate) use util::*;
pub(crate) use vsync::*;
pub(crate) use window::*;
pub(crate) use wrapper::*;

pub use platform::WindowsPlatform;

pub(crate) use windows::Win32::Foundation::HWND;
