use std::ffi::c_void;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;

use sha2::{Digest, Sha256};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::Foundation::{HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

use super::{Address, unavailable};

pub(super) type ServerStream = NamedPipeServer;

struct Local(*mut c_void);

impl Drop for Local {
    fn drop(&mut self) {
        // SAFETY: this allocation came from the security conversion APIs' LocalAlloc.
        unsafe { LocalFree(self.0) };
    }
}

fn user(process: HANDLE) -> io::Result<String> {
    // SAFETY: handles are live and all output pointers reference initialized writable storage.
    unsafe {
        let mut token = ptr::null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
            return Err(unavailable());
        }
        let token = OwnedHandle::from_raw_handle(token);
        let mut bytes = 0;
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            ptr::null_mut(),
            0,
            &mut bytes,
        );
        if bytes < size_of::<TOKEN_USER>() as u32 || bytes > 64 * 1024 {
            return Err(unavailable());
        }
        let mut storage = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
        if GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            storage.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        ) == 0
        {
            return Err(unavailable());
        }
        let token_user = &*storage.as_ptr().cast::<TOKEN_USER>();
        let mut text = ptr::null_mut();
        if ConvertSidToStringSidW(token_user.User.Sid, &mut text) == 0 {
            return Err(unavailable());
        }
        let allocation = Local(text.cast());
        let mut length = 0;
        while *text.add(length) != 0 {
            length += 1;
        }
        let result =
            String::from_utf16(std::slice::from_raw_parts(text, length)).map_err(|_| unavailable());
        drop(allocation);
        result
    }
}

fn current_user() -> io::Result<String> {
    // SAFETY: GetCurrentProcess returns the current process's pseudo-handle.
    user(unsafe { GetCurrentProcess() })
}

fn name(address: &Address, user: &str) -> String {
    let key = Sha256::digest(format!("{}\0{user}", address.key));
    format!(r"\\.\pipe\agentscm-hook-{key:x}")
}

pub(super) async fn connect(address: &Address) -> io::Result<NamedPipeClient> {
    let owner = current_user()?;
    let stream = ClientOptions::new().open(name(address, &owner))?;
    // SAFETY: the pipe handle is live; OpenProcess returns a new owned query-only handle.
    unsafe {
        let mut process_id = 0;
        if GetNamedPipeServerProcessId(stream.as_raw_handle(), &mut process_id) == 0 {
            return Err(unavailable());
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id);
        if process.is_null() {
            return Err(unavailable());
        }
        let process = OwnedHandle::from_raw_handle(process);
        if user(process.as_raw_handle())? != owner {
            return Err(unavailable());
        }
    }
    Ok(stream)
}

fn pipe(name: &str, owner: &str, first: bool) -> io::Result<NamedPipeServer> {
    let descriptor: Vec<u16> = format!("D:P(A;;GA;;;{owner})\0").encode_utf16().collect();
    // SAFETY: the NUL-terminated descriptor and attributes live through pipe creation.
    // Only this user's SID gets access; remote clients and pre-existing first instances are refused.
    unsafe {
        let mut security = ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor.as_ptr(),
            SDDL_REVISION_1,
            &mut security,
            ptr::null_mut(),
        ) == 0
        {
            return Err(unavailable());
        }
        let allocation = Local(security);
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: security,
            bInheritHandle: 0,
        };
        let result = ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                name,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            );
        drop(allocation);
        result
    }
}

pub(super) struct Listener {
    pipe: NamedPipeServer,
    name: String,
    owner: String,
}

impl Listener {
    pub(super) fn bind(address: &Address) -> io::Result<Option<Self>> {
        let owner = current_user()?;
        let name = name(address, &owner);
        match pipe(&name, &owner, true) {
            Ok(pipe) => Ok(Some(Self { pipe, name, owner })),
            Err(error) if error.raw_os_error() == Some(5) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(super) async fn accept(&mut self) -> io::Result<ServerStream> {
        self.pipe.connect().await?;
        let next = pipe(&self.name, &self.owner, false)?;
        Ok(std::mem::replace(&mut self.pipe, next))
    }
}
