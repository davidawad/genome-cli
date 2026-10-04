//! OS-specific behaviour behind portable interfaces: conventional directories,
//! owner-only permissions, executable lookup and the OS credential store.

pub mod dirs;
pub mod exe;
pub mod keystore;
pub mod perms;
#[cfg(windows)]
mod win_acl;
