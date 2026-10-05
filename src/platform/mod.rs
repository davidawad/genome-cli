//! OS-specific behaviour behind portable interfaces: conventional directories,
//! owner-only permissions and executable lookup.

pub mod dirs;
pub mod exe;
pub mod perms;
#[cfg(windows)]
mod win_acl;
