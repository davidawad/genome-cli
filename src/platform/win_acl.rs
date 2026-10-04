//! Windows: owner-only directories through a protected DACL.
//!
//! [`protect`] replaces the directory's DACL with a single inheritable
//! "current user: full control" entry and blocks inheritance from the parent
//! (`PROTECTED_DACL_SECURITY_INFORMATION`), so files created inside inherit
//! it. [`check`] reports any allow entry for another principal (SYSTEM and
//! Administrators, who can take ownership of any file anyway, are tolerated).

use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, ERROR_SUCCESS, GENERIC_ALL, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, EXPLICIT_ACCESS_W,
    NO_MULTIPLE_TRUSTEE, SET_ACCESS, SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_USER, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    AclSizeInformation, EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorControl, GetTokenInformation,
    IsWellKnownSid, TokenUser, WinBuiltinAdministratorsSid, WinLocalSystemSid, ACCESS_ALLOWED_ACE, ACL,
    ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SE_DACL_PROTECTED, SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::perms::Access;

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn win_err(code: u32, what: &str) -> std::io::Error {
    let e = std::io::Error::from_raw_os_error(code as i32);
    std::io::Error::new(e.kind(), format!("{what}: {e}"))
}

/// The current user's SID (a `TOKEN_USER` buffer that owns it).
struct UserSid(Vec<u64>);

impl UserSid {
    fn current() -> std::io::Result<Self> {
        // SAFETY: plain Win32 calls with valid out-pointers; the token handle is closed below.
        unsafe {
            let mut token: HANDLE = null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            let mut len = 0u32;
            GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len);
            let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
            let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len);
            let err = std::io::Error::last_os_error();
            CloseHandle(token);
            if ok == 0 {
                return Err(err);
            }
            Ok(Self(buf))
        }
    }

    fn sid(&self) -> PSID {
        // SAFETY: the buffer was filled by GetTokenInformation(TokenUser) and is 8-byte aligned.
        unsafe { (*self.0.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }
}

pub fn protect(dir: &Path) -> std::io::Result<()> {
    let user = UserSid::current()?;
    let ea = EXPLICIT_ACCESS_W {
        grfAccessPermissions: GENERIC_ALL,
        grfAccessMode: SET_ACCESS,
        grfInheritance: SUB_CONTAINERS_AND_OBJECTS_INHERIT,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: user.sid().cast(),
        },
    };
    let path = wide(dir);
    // SAFETY: `ea` and `path` outlive the calls; the new ACL is freed with LocalFree.
    unsafe {
        let mut acl: *mut ACL = null_mut();
        let rc = SetEntriesInAclW(1, &ea, null(), &mut acl);
        if rc != ERROR_SUCCESS {
            return Err(win_err(rc, "building the ACL"));
        }
        let rc = SetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            acl,
            null(),
        );
        LocalFree(acl.cast());
        if rc != ERROR_SUCCESS {
            return Err(win_err(rc, &format!("restricting {} to the current user", dir.display())));
        }
    }
    Ok(())
}

fn sid_string(sid: PSID) -> String {
    // SAFETY: `sid` points into a live ACE; the returned string is freed with LocalFree.
    unsafe {
        let mut s = null_mut();
        if ConvertSidToStringSidW(sid, &mut s) == 0 {
            return "?".into();
        }
        let len = (0..).take_while(|&i| *s.add(i) != 0).count();
        let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, len));
        LocalFree(s.cast());
        out
    }
}

pub fn check(dir: &Path) -> Access {
    let user = match UserSid::current() {
        Ok(u) => u,
        Err(e) => return Access::Unknown(format!("reading the current user: {e}")),
    };
    let path = wide(dir);
    // SAFETY: out-pointers are valid; `sd` owns `dacl` and is freed with LocalFree.
    unsafe {
        let mut dacl: *mut ACL = null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        let rc = GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut sd,
        );
        if rc != ERROR_SUCCESS {
            return Access::Unknown(win_err(rc, "reading the ACL").to_string());
        }
        let result = inspect(sd, dacl, user.sid());
        LocalFree(sd.cast());
        result
    }
}

/// SAFETY: `sd` and `dacl` come from GetNamedSecurityInfoW and are still alive.
unsafe fn inspect(sd: PSECURITY_DESCRIPTOR, dacl: *mut ACL, user: PSID) -> Access {
    if dacl.is_null() {
        return Access::Open("no DACL: everyone has full access".into());
    }
    let (mut control, mut rev) = (0u16, 0u32);
    GetSecurityDescriptorControl(sd, &mut control, &mut rev);
    let mut info = ACL_SIZE_INFORMATION::default();
    let size = std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32;
    if GetAclInformation(dacl, (&raw mut info).cast(), size, AclSizeInformation) == 0 {
        return Access::Unknown(std::io::Error::last_os_error().to_string());
    }
    let mut others = Vec::new();
    for i in 0..info.AceCount {
        let mut ace = null_mut();
        if GetAce(dacl, i, &mut ace) == 0 {
            continue;
        }
        let ace = ace.cast::<ACCESS_ALLOWED_ACE>();
        if (*ace).Header.AceType != ACCESS_ALLOWED_ACE_TYPE {
            continue;
        }
        let sid: PSID = (&raw mut (*ace).SidStart).cast();
        let trusted = EqualSid(sid, user) != 0
            || IsWellKnownSid(sid, WinLocalSystemSid) != 0
            || IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0;
        if !trusted {
            others.push(sid_string(sid));
        }
    }
    let protected = control & SE_DACL_PROTECTED != 0;
    match (others.is_empty(), protected) {
        (true, true) => Access::Private("ACL: current user only (protected)".into()),
        (true, false) => Access::Private("ACL: current user only (inherits from parent)".into()),
        (false, _) => Access::Open(format!("ACL also grants access to {}", others.join(", "))),
    }
}
