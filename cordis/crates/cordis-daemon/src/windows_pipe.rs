use std::future::Future;
use std::io;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::task::JoinHandle;
use windows_sys::Win32::Foundation::{
    LocalFree, ERROR_BROKEN_PIPE, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_DATA,
    ERROR_PIPE_NOT_CONNECTED,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

struct LocalAllocation<T>(*mut T);

impl<T> Drop for LocalAllocation<T> {
    fn drop(&mut self) {
        // SAFETY: the pointer came from a successful Win32 conversion allocating with LocalAlloc.
        unsafe { LocalFree(self.0.cast()) };
    }
}

fn current_user_sddl() -> io::Result<Vec<u16>> {
    let mut raw_token = null_mut();
    // SAFETY: OpenProcessToken fills the handle for the current process on success.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: OpenProcessToken returned an owned handle, closed when this function finishes.
    let token = unsafe { OwnedHandle::from_raw_handle(raw_token) };
    let mut length = 0;
    // SAFETY: A null buffer with zero length queries the required size without writing token data.
    if unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut length) }
        == 0
    {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(error);
        }
    }
    if (length as usize) < size_of::<TOKEN_USER>() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid token user length"));
    }
    let mut buffer = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
    // SAFETY: The buffer is aligned for TOKEN_USER and covers the requested size.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buffer.as_mut_ptr().cast(),
            length,
            &mut length,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut sid = null_mut();
    // SAFETY: The successful token query filled TOKEN_USER with a SID backed by this buffer.
    if unsafe { ConvertSidToStringSidW((*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid, &mut sid) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    let sid = LocalAllocation(sid);
    let mut sddl: Vec<u16> = "D:P(A;;GA;;;".encode_utf16().collect();
    let mut index = 0;
    loop {
        // SAFETY: ConvertSidToStringSidW returns a NUL-terminated UTF-16 string.
        let code = unsafe { *sid.0.add(index) };
        if code == 0 {
            break;
        }
        sddl.push(code);
        index += 1;
    }
    sddl.extend([u16::from(b')'), 0]);
    Ok(sddl)
}

fn create_instance(path: &Path, sddl: &[u16], first: bool) -> io::Result<NamedPipeServer> {
    let mut descriptor = null_mut();
    // SAFETY: The SDDL from current_user_sddl is NUL-terminated and the output pointer is writable.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let descriptor = LocalAllocation(descriptor);
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    // SAFETY: The attributes and descriptor remain valid for the duration of the synchronous call.
    unsafe {
        ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                path,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
    }
}

pub(super) fn listen<F, T>(path: PathBuf, serve: F) -> io::Result<JoinHandle<()>>
where
    F: Fn(NamedPipeServer) -> T + Send + 'static,
    T: Future<Output = ()> + Send + 'static,
{
    let prefix = r"\\.\pipe\";
    let name = path.to_string_lossy();
    if name.len() <= prefix.len()
        || !name.get(..prefix.len()).is_some_and(|actual| actual.eq_ignore_ascii_case(prefix))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a local named pipe path",
        ));
    }

    let sddl = current_user_sddl()?;
    let mut next = create_instance(&path, &sddl, true)?;
    Ok(tokio::spawn(async move {
        loop {
            let connected = match next.connect().await {
                Ok(()) => true,
                Err(error)
                    if error.raw_os_error().is_some_and(|code| {
                        matches!(
                            code as u32,
                            ERROR_NO_DATA | ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED
                        )
                    }) =>
                {
                    false
                }
                Err(error) => {
                    eprintln!("connect to Cordis named pipe {}: {error}", path.display());
                    break;
                }
            };
            // Keep the pipe name live across accepted connections.
            let replacement = match create_instance(&path, &sddl, false) {
                Ok(pipe) => pipe,
                Err(error) => {
                    eprintln!("create Cordis named pipe instance {}: {error}", path.display());
                    break;
                }
            };
            let client = std::mem::replace(&mut next, replacement);
            if connected {
                tokio::spawn(serve(client));
            }
        }
    }))
}
