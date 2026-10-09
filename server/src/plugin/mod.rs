//! Owns filesystem plugin discovery, sandboxed workers, and plugin providers.
mod asset;
mod builtin;
mod catalog;
mod data;
mod definition;
mod descriptor;
mod installation;
mod manifest;
mod oauth_callback;
mod protocol;
mod quota;
mod registry;
mod runtime;
mod selection;
mod state;
mod user_install;
mod wire;
mod worker;

pub use descriptor::{
    PluginDescriptor, PluginModelDescriptor, PluginProviderDescriptor, PluginResourceDescriptor,
    PluginResourceView,
};
pub use registry::{
    ImportResponse, InstallPluginResponse, OAuthBeginResponse, OAuthPollResponse, PluginRegistry,
};
pub use runtime::{PluginRuntime, PluginRuntimePhase, PluginRuntimeState, PluginRuntimeStatus};

/// Windows 下阻止 Deno 子进程弹出控制台窗口(CREATE_NO_WINDOW)。
#[cfg(windows)]
fn detach_console(command: &mut tokio::process::Command) {
    command.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn detach_console(_command: &mut tokio::process::Command) {}
