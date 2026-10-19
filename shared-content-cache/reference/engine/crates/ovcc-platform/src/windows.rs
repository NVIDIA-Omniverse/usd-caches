// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Windows backend.

use std::ffi::{OsStr, c_void};
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::fs::{FileExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::os::windows::io::{AsRawHandle as _, AsRawSocket, FromRawHandle as _};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
    NtCreateFile,
};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_HANDLE_EOF, ERROR_SHARING_VIOLATION, GENERIC_READ,
    HANDLE, INVALID_HANDLE_VALUE, RtlNtStatusToDosError, UNICODE_STRING, WAIT_OBJECT_0,
};
use windows_sys::Win32::Networking::WinSock::{
    SIO_TCP_INFO, SOCKET, TCP_INFO_v0, TransmitFile, WSA_IO_PENDING, WSAEINVAL, WSAGetLastError,
    WSAGetOverlappedResult, WSAIoctl,
};
use windows_sys::Win32::Security::Cryptography::{
    AUTHTYPE_SERVER, CERT_CHAIN_CACHE_ONLY_URL_RETRIEVAL, CERT_CHAIN_CONTEXT,
    CERT_CHAIN_DISABLE_AUTH_ROOT_AUTO_UPDATE, CERT_CHAIN_PARA, CERT_CHAIN_POLICY_PARA,
    CERT_CHAIN_POLICY_SSL, CERT_CHAIN_POLICY_STATUS, CERT_CONTEXT, CERT_STORE_ADD_ALWAYS,
    CERT_STORE_CREATE_NEW_FLAG, CERT_STORE_PROV_MEMORY, CertAddCertificateContextToStore,
    CertCloseStore, CertCreateCertificateContext, CertEnumCertificatesInStore,
    CertFreeCertificateChain, CertFreeCertificateContext, CertGetCertificateChain, CertOpenStore,
    CertOpenSystemStoreW, CertVerifyCertificateChainPolicy, HCERTCHAINENGINE, HCERTSTORE,
    HTTPSPolicyCallbackData, PKCS_7_ASN_ENCODING, USAGE_MATCH_TYPE_AND, X509_ASN_ENCODING,
    szOID_PKIX_KP_SERVER_AUTH,
};
#[cfg(test)]
use windows_sys::Win32::Security::Cryptography::{
    CERT_CHAIN_ENGINE_CONFIG, CertCreateCertificateChainEngine, CertFreeCertificateChainEngine,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, ExtendedFileIdType, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_FLAG_OVERLAPPED, FILE_FLAG_SEQUENTIAL_SCAN, FILE_ID_DESCRIPTOR, FILE_ID_INFO,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileAttributeTagInfo, FileIdInfo, FlushFileBuffers, GetDiskFreeSpaceExW,
    GetFileInformationByHandle, GetFileInformationByHandleEx, GetVolumeInformationByHandleW,
    MOVEFILE_WRITE_THROUGH, MoveFileExW, OpenFileById, ReOpenFile, SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::{CancelIoEx, OVERLAPPED};
use windows_sys::Win32::System::SystemServices::FILE_SUPPORTS_OPEN_BY_FILE_ID;
use windows_sys::Win32::System::Threading::{
    CreateEventW, INFINITE, RegisterWaitForSingleObject, UnregisterWaitEx, WT_EXECUTEONLYONCE,
    WaitForSingleObject,
};
use windows_sys::core::PSTR;

use crate::{FileIdentity, FileStamp, Stat};

/// Reparse tags with this bit redirect name resolution (symlinks, junctions).
/// Other tags (dedup, cloud files) keep the file's own data and are followed.
const NAME_SURROGATE_TAG_BIT: u32 = 0x2000_0000;

fn raw(file: &File) -> HANDLE {
    file.as_raw_handle() as HANDLE
}

fn nul_terminated(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

/// Win32 path APIs outside `std` are still bound by `MAX_PATH` unless handed a
/// verbatim path. `std::path::absolute` normalizes via `GetFullPathNameW`,
/// which is exactly what a `\\?\` path skips.
fn verbatim(path: &Path) -> io::Result<Vec<u16>> {
    let absolute = std::path::absolute(path)?;
    let wide = absolute.as_os_str().encode_wide().collect::<Vec<u16>>();
    let starts_with = |prefix: &str| wide.starts_with(&prefix.encode_utf16().collect::<Vec<_>>());
    let mut out = if starts_with(r"\\?\") || starts_with(r"\\.\") {
        Vec::new()
    } else if starts_with(r"\\") {
        let mut prefix = r"\\?\UNC".encode_utf16().collect::<Vec<_>>();
        prefix.extend_from_slice(&wide[1..]);
        return Ok(prefix.into_iter().chain(std::iter::once(0)).collect());
    } else {
        r"\\?\".encode_utf16().collect()
    };
    out.extend_from_slice(&wide);
    out.push(0);
    Ok(out)
}

fn attributes_only(path: &Path, follow: bool) -> io::Result<File> {
    let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
    if !follow {
        flags |= FILE_FLAG_OPEN_REPARSE_POINT;
    }
    OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(flags)
        .open(path)
}

pub(crate) fn file_identity(file: &File) -> io::Result<FileIdentity> {
    // SAFETY: FILE_ID_INFO is plain data; the call writes at most its size.
    let mut info: FILE_ID_INFO = unsafe { zeroed() };
    let ok = unsafe {
        GetFileInformationByHandleEx(
            raw(file),
            FileIdInfo,
            (&raw mut info).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok != 0 {
        return Ok(FileIdentity {
            device: info.VolumeSerialNumber,
            file: u128::from_le_bytes(info.FileId.Identifier),
        });
    }
    // FAT-family volumes lack FileIdInfo; their 64-bit index is still unique.
    // SAFETY: as above, for BY_HANDLE_FILE_INFORMATION.
    let mut legacy: BY_HANDLE_FILE_INFORMATION = unsafe { zeroed() };
    if unsafe { GetFileInformationByHandle(raw(file), &mut legacy) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileIdentity {
        device: u64::from(legacy.dwVolumeSerialNumber),
        file: (u128::from(legacy.nFileIndexHigh) << 32) | u128::from(legacy.nFileIndexLow),
    })
}

pub(crate) fn stat_nofollow(path: &Path) -> io::Result<Stat> {
    // Metadata and identity come from one handle, so they always describe the
    // same object even if the path is being replaced concurrently.
    let file = attributes_only(path, false)?;
    Ok(Stat {
        metadata: file.metadata()?,
        identity: file_identity(&file)?,
    })
}

pub(crate) fn file_stamp(path: &Path) -> io::Result<FileStamp> {
    let file = attributes_only(path, true)?;
    let metadata = file.metadata()?;
    Ok(FileStamp {
        identity: file_identity(&file)?,
        length: metadata.len(),
        modified: metadata.modified().ok(),
        permissions: metadata.file_attributes(),
    })
}

pub(crate) fn open_directory(path: &Path) -> io::Result<File> {
    // std shares read/write/delete, so holding this handle never blocks a
    // rename or removal of the directory.
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let mut flags = 0;
    if unsafe {
        GetVolumeInformationByHandleW(
            raw(&directory),
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            &mut flags,
            null_mut(),
            0,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if flags & FILE_SUPPORTS_OPEN_BY_FILE_ID == 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cache filesystem does not support opening files by id",
        ));
    }
    Ok(directory)
}

/// Opens `name` inside `directory` for attribute queries only, never
/// following a final reparse point.
fn probe_relative(directory: &File, name: &str) -> io::Result<File> {
    let mut wide = name.encode_utf16().collect::<Vec<u16>>();
    let length = u16::try_from(wide.len() * 2)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name too long"))?;
    let object_name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: wide.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: raw(directory),
        ObjectName: &object_name,
        // Case-sensitive, matching the Linux lookup it replaces.
        Attributes: 0,
        SecurityDescriptor: null(),
        SecurityQualityOfService: null(),
    };
    let mut handle: HANDLE = null_mut();
    // SAFETY: IO_STATUS_BLOCK is plain data written by the call.
    let mut status_block = unsafe { zeroed() };
    // SAFETY: every pointer references a live local for the whole call;
    // `wide` backs `object_name` and outlives it.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            &attributes,
            &mut status_block,
            null(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
            null(),
            0,
        )
    };
    if status < 0 {
        // SAFETY: pure status translation.
        let code = unsafe { RtlNtStatusToDosError(status) };
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    // SAFETY: NtCreateFile succeeded, so `handle` is a fresh owned handle.
    Ok(unsafe { File::from_raw_handle(handle as _) })
}

/// NTFS refuses to rename a directory while any file below it is open *by
/// name*, which would stop the cache from retiring a generation that an
/// in-flight HIT is still reading. A handle opened by file id is not linked
/// through its parent, so it neither blocks the rename nor loses its data when
/// the directory is later deleted.
pub(crate) fn open_regular_at(directory: &File, name: &str) -> io::Result<File> {
    // The attribute probe opening the reparse point itself is the O_NOFOLLOW
    // analogue; it lives only until the by-id reopen below.
    let probe = probe_relative(directory, name)?;
    if probe.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        // SAFETY: FILE_ATTRIBUTE_TAG_INFO is plain data; the call writes its size.
        let mut tag: FILE_ATTRIBUTE_TAG_INFO = unsafe { zeroed() };
        if unsafe {
            GetFileInformationByHandleEx(
                raw(&probe),
                FileAttributeTagInfo,
                (&raw mut tag).cast(),
                size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if tag.ReparseTag & NAME_SURROGATE_TAG_BIT != 0 {
            return Err(io::Error::other(
                "refusing to follow a symlink or junction below a pinned directory",
            ));
        }
        // Data-bearing reparse points (dedup, cloud files) fall through: the
        // by-id open below reads them through their filter.
    }

    // SAFETY: FILE_ID_DESCRIPTOR is plain data; zero is a valid start.
    let mut descriptor: FILE_ID_DESCRIPTOR = unsafe { zeroed() };
    descriptor.dwSize = size_of::<FILE_ID_DESCRIPTOR>() as u32;
    // SAFETY: FILE_ID_INFO is plain data; the call writes at most its size.
    let mut info: FILE_ID_INFO = unsafe { zeroed() };
    if unsafe {
        GetFileInformationByHandleEx(
            raw(&probe),
            FileIdInfo,
            (&raw mut info).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    } != 0
    {
        descriptor.Type = ExtendedFileIdType;
        descriptor.Anonymous.ExtendedFileId = info.FileId;
    } else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cache filesystem does not provide extended file ids",
        ));
    }

    // SAFETY: the directory handle is only a volume hint; the descriptor is
    // fully initialized.
    let handle = unsafe {
        OpenFileById(
            raw(directory),
            &descriptor,
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    drop(probe);
    // SAFETY: OpenFileById succeeded, so `handle` is a fresh owned handle.
    Ok(unsafe { File::from_raw_handle(handle as _) })
}

/// Windows reports a rename blocked by a short-lived foreign open (antivirus,
/// the search indexer, or another thread's attribute probe) as an access or
/// sharing violation. Retries for up to ~255 ms before surfacing it.
fn retry_transient<T>(mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut delay = std::time::Duration::from_millis(1);
    for _ in 0..8 {
        match operation() {
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(code) if code == ERROR_ACCESS_DENIED as i32
                        || code == ERROR_SHARING_VIOLATION as i32
                ) =>
            {
                std::thread::sleep(delay);
                delay *= 2;
            }
            result => return result,
        }
    }
    operation()
}

pub(crate) fn rename(source: &Path, destination: &Path) -> io::Result<()> {
    retry_transient(|| std::fs::rename(source, destination))
}

pub(crate) fn sync_file(path: &Path) -> io::Result<()> {
    OpenOptions::new().write(true).open(path)?.sync_all()
}

pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    // NTFS journals directory entries; flushing a write-capable directory
    // handle forces that journal (and the entry) to stable storage, like
    // fsync(dirfd).
    let directory = OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    // SAFETY: the handle is owned by `directory` for the call.
    if unsafe { FlushFileBuffers(raw(&directory)) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    let source_wide = verbatim(source)?;
    let destination_wide = verbatim(destination)?;
    // Without MOVEFILE_REPLACE_EXISTING the move fails if the destination
    // exists. WRITE_THROUGH returns only once the rename is on disk.
    retry_transient(|| {
        // SAFETY: both buffers are NUL-terminated and outlive the call.
        let ok = unsafe {
            MoveFileExW(
                source_wide.as_ptr(),
                destination_wide.as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    })
}

fn exchange_sibling(left: &Path) -> PathBuf {
    let mut name = left.as_os_str().to_owned();
    name.push(".exchange");
    PathBuf::from(name)
}

fn exchange_paths_with(
    left: &Path,
    right: &Path,
    mut rename: impl FnMut(&Path, &Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    let aside = exchange_sibling(left);
    rename(right, &aside)?;
    if let Err(error) = rename(left, right) {
        // Restore the original occupant; if that also fails, `aside` sits in
        // the caller's swept directory.
        let _ = rename(&aside, right);
        return Err(error);
    }
    match rename(&aside, left) {
        Ok(()) => Ok(left.to_path_buf()),
        Err(_) => Ok(aside),
    }
}

pub(crate) fn exchange_paths(left: &Path, right: &Path) -> io::Result<PathBuf> {
    exchange_paths_with(left, right, rename_noreplace)
}

#[cfg(test)]
mod exchange_tests {
    use super::*;

    #[test]
    fn windows_exchange_final_move_failure_reports_retired_path() {
        let temp = tempfile::tempdir().unwrap();
        let staged = temp.path().join("staged");
        let published = temp.path().join("published");
        std::fs::write(&staged, b"new").unwrap();
        std::fs::write(&published, b"old").unwrap();
        let aside = exchange_sibling(&staged);
        let mut step = 0;

        let retired = exchange_paths_with(&staged, &published, |source, destination| {
            step += 1;
            if step == 3 {
                return Err(io::Error::other("injected final move failure"));
            }
            std::fs::rename(source, destination)
        })
        .unwrap();

        assert_eq!(retired, aside);
        assert_eq!(std::fs::read(&published).unwrap(), b"new");
        assert_eq!(std::fs::read(&retired).unwrap(), b"old");
        assert!(!staged.exists());
    }
}

pub(crate) fn available_bytes(path: &Path) -> io::Result<u64> {
    let wide = nul_terminated(std::path::absolute(path)?.as_os_str());
    let mut available = 0u64;
    // SAFETY: `wide` is NUL-terminated; the optional totals are null.
    if unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, null_mut(), null_mut()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(available)
}

pub(crate) fn system_resolver_config() -> io::Result<String> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Windows DNS policy cannot be represented as resolv.conf",
    ))
}

/// Whether Windows itself trusts the root `context` for TLS server
/// authentication. Delegating to the chain engine applies the full Windows
/// policy (EKU extension and administrator property, including
/// `anyExtendedKeyUsage`, the Disallowed store, and Untrusted CTLs) instead of
/// re-implementing parts of it. Revocation is not requested, so no network
/// fetch occurs.
///
/// # Safety
/// `context` must be a live certificate context.
unsafe fn windows_trusts_for_server_auth(context: *const CERT_CONTEXT) -> bool {
    let mut usage = [szOID_PKIX_KP_SERVER_AUTH as PSTR];
    // SAFETY: CERT_CHAIN_PARA is plain data; zero is its documented default.
    let mut parameters: CERT_CHAIN_PARA = unsafe { zeroed() };
    parameters.cbSize = size_of::<CERT_CHAIN_PARA>() as u32;
    parameters.RequestedUsage.dwType = USAGE_MATCH_TYPE_AND;
    parameters.RequestedUsage.Usage.cUsageIdentifier = 1;
    parameters.RequestedUsage.Usage.rgpszUsageIdentifier = usage.as_mut_ptr();
    let mut chain: *mut CERT_CHAIN_CONTEXT = null_mut();
    // SAFETY: every pointer references a live local; a null engine selects
    // the current user's default engine.
    if unsafe {
        CertGetCertificateChain(
            null_mut(),
            context,
            null(),
            null_mut(),
            &parameters,
            CERT_CHAIN_DISABLE_AUTH_ROOT_AUTO_UPDATE | CERT_CHAIN_CACHE_ONLY_URL_RETRIEVAL,
            null(),
            &mut chain,
        )
    } == 0
        || chain.is_null()
    {
        return false;
    }
    // SAFETY: the call returned a chain context we own until freed.
    let status = unsafe { (*chain).TrustStatus.dwErrorStatus };
    // SAFETY: frees the context returned above exactly once.
    unsafe { CertFreeCertificateChain(chain) };
    status == 0
}

/// DER certificates of one system store, keeping those `keep` accepts.
pub(crate) fn system_store_certificates(
    name: &str,
    keep: impl Fn(*const CERT_CONTEXT) -> bool,
) -> io::Result<Vec<Vec<u8>>> {
    let name = nul_terminated(OsStr::new(name));
    // SAFETY: `name` is NUL-terminated; a zero provider selects the default.
    let store = unsafe { CertOpenSystemStoreW(0, name.as_ptr()) };
    if store.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut certificates = Vec::new();
    let mut context: *const CERT_CONTEXT = null();
    loop {
        // SAFETY: enumeration frees the previous context and returns the next
        // one owned by the still-open store.
        context = unsafe { CertEnumCertificatesInStore(store, context) };
        if context.is_null() {
            break;
        }
        if !keep(context) {
            continue;
        }
        // SAFETY: a live context points at `cbCertEncoded` DER bytes.
        let der = unsafe {
            std::slice::from_raw_parts((*context).pbCertEncoded, (*context).cbCertEncoded as usize)
        };
        certificates.push(der.to_vec());
    }
    // SAFETY: enumeration ended (context is null), so no context is held.
    unsafe { CertCloseStore(store, 0) };
    Ok(certificates)
}

pub(crate) fn system_root_certificates() -> io::Result<Vec<Vec<u8>>> {
    system_store_certificates("ROOT", |context| {
        // SAFETY: the enumeration passes a live context.
        unsafe { windows_trusts_for_server_auth(context) }
    })
}

struct CertificateContext(*mut CERT_CONTEXT);

impl CertificateContext {
    fn from_der(der: &[u8]) -> io::Result<Self> {
        let length = u32::try_from(der.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "certificate is too large"))?;
        let context = unsafe {
            CertCreateCertificateContext(
                X509_ASN_ENCODING | PKCS_7_ASN_ENCODING,
                der.as_ptr(),
                length,
            )
        };
        if context.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(context))
    }
}

impl Drop for CertificateContext {
    fn drop(&mut self) {
        unsafe { CertFreeCertificateContext(self.0) };
    }
}

struct CertificateStore(HCERTSTORE);

impl CertificateStore {
    fn memory() -> io::Result<Self> {
        let store = unsafe {
            CertOpenStore(
                CERT_STORE_PROV_MEMORY,
                0,
                0,
                CERT_STORE_CREATE_NEW_FLAG,
                null(),
            )
        };
        if store.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(store))
    }

    fn add(&self, certificate: &CertificateContext) -> io::Result<()> {
        if unsafe {
            CertAddCertificateContextToStore(
                self.0,
                certificate.0,
                CERT_STORE_ADD_ALWAYS,
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for CertificateStore {
    fn drop(&mut self) {
        unsafe { CertCloseStore(self.0, 0) };
    }
}

struct CertificateChain(*mut CERT_CHAIN_CONTEXT);

impl Drop for CertificateChain {
    fn drop(&mut self) {
        unsafe { CertFreeCertificateChain(self.0) };
    }
}

pub(crate) fn verify_system_certificate_chain(
    end_entity: &[u8],
    intermediates: &[&[u8]],
    server_name: &str,
) -> io::Result<()> {
    verify_certificate_chain(null_mut(), end_entity, intermediates, server_name)
}

fn verify_certificate_chain(
    engine: HCERTCHAINENGINE,
    end_entity: &[u8],
    intermediates: &[&[u8]],
    server_name: &str,
) -> io::Result<()> {
    let leaf = CertificateContext::from_der(end_entity)?;
    let store = CertificateStore::memory()?;
    for der in intermediates {
        store.add(&CertificateContext::from_der(der)?)?;
    }

    let mut usage = [szOID_PKIX_KP_SERVER_AUTH as PSTR];
    let mut parameters: CERT_CHAIN_PARA = unsafe { zeroed() };
    parameters.cbSize = size_of::<CERT_CHAIN_PARA>() as u32;
    parameters.RequestedUsage.dwType = USAGE_MATCH_TYPE_AND;
    parameters.RequestedUsage.Usage.cUsageIdentifier = 1;
    parameters.RequestedUsage.Usage.rgpszUsageIdentifier = usage.as_mut_ptr();
    let mut chain = null_mut();
    if unsafe {
        CertGetCertificateChain(
            engine,
            leaf.0,
            null(),
            store.0,
            &parameters,
            CERT_CHAIN_DISABLE_AUTH_ROOT_AUTO_UPDATE | CERT_CHAIN_CACHE_ONLY_URL_RETRIEVAL,
            null(),
            &mut chain,
        )
    } == 0
        || chain.is_null()
    {
        return Err(io::Error::last_os_error());
    }
    let chain = CertificateChain(chain);
    let trust_error = unsafe { (*chain.0).TrustStatus.dwErrorStatus };

    let mut wide_name = OsStr::new(server_name)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut ssl: HTTPSPolicyCallbackData = unsafe { zeroed() };
    ssl.Anonymous.cbStruct = size_of::<HTTPSPolicyCallbackData>() as u32;
    ssl.dwAuthType = AUTHTYPE_SERVER;
    ssl.pwszServerName = wide_name.as_mut_ptr();
    let mut policy: CERT_CHAIN_POLICY_PARA = unsafe { zeroed() };
    policy.cbSize = size_of::<CERT_CHAIN_POLICY_PARA>() as u32;
    policy.pvExtraPolicyPara = (&raw mut ssl).cast();
    let mut status: CERT_CHAIN_POLICY_STATUS = unsafe { zeroed() };
    status.cbSize = size_of::<CERT_CHAIN_POLICY_STATUS>() as u32;
    if unsafe {
        CertVerifyCertificateChainPolicy(CERT_CHAIN_POLICY_SSL, chain.0, &policy, &mut status)
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if trust_error != 0 || status.dwError != 0 {
        return Err(io::Error::other(format!(
            "Windows rejected the origin certificate chain (trust {trust_error:#010x}, policy {:#010x})",
            status.dwError
        )));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn verify_certificate_chain_with_root(
    end_entity: &[u8],
    intermediates: &[&[u8]],
    server_name: &str,
    root: &[u8],
) -> io::Result<()> {
    let roots = CertificateStore::memory()?;
    roots.add(&CertificateContext::from_der(root)?)?;
    let mut config: CERT_CHAIN_ENGINE_CONFIG = unsafe { zeroed() };
    config.cbSize = size_of::<CERT_CHAIN_ENGINE_CONFIG>() as u32;
    config.hExclusiveRoot = roots.0;
    let mut engine = null_mut();
    if unsafe { CertCreateCertificateChainEngine(&config, &mut engine) } == 0 || engine.is_null() {
        return Err(io::Error::last_os_error());
    }
    struct Engine(HCERTCHAINENGINE);
    impl Drop for Engine {
        fn drop(&mut self) {
            unsafe { CertFreeCertificateChainEngine(self.0) };
        }
    }
    let engine = Engine(engine);
    verify_certificate_chain(engine.0, end_entity, intermediates, server_name)
}

pub(crate) fn handle_key(file: &File) -> u64 {
    file.as_raw_handle() as usize as u64
}

pub(crate) fn read_at(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    file.seek_read(buffer, offset)
}

/// One in-flight overlapped `TransmitFile`. Boxed so the kernel-owned
/// `OVERLAPPED` and the wait callback's context never move.
struct Transmission {
    overlapped: OVERLAPPED,
    event: HANDLE,
    wait: HANDLE,
    completed: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    finished: bool,
    socket: SOCKET,
}

unsafe extern "system" fn transmission_signaled(context: *mut c_void, _timed_out: bool) {
    // SAFETY: the context is the boxed Transmission, which outlives every
    // callback because it is freed only after UnregisterWaitEx(.., INVALID)
    // has waited for callbacks to drain.
    let transmission = unsafe { &*context.cast::<Transmission>() };
    if let Some(sender) = transmission
        .completed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
    {
        let _ = sender.send(());
    }
}

/// A failed `TransmitFile` together with the bytes the kernel had already
/// sent, so callers account partial progress the way a partial `sendfile(2)`
/// return does on Linux.
#[derive(Debug)]
pub struct TransmitError {
    pub sent: u32,
    pub error: io::Error,
}

impl From<TransmitError> for io::Error {
    fn from(failure: TransmitError) -> Self {
        failure.error
    }
}

impl std::fmt::Display for TransmitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} after {} bytes", self.error, self.sent)
    }
}

impl std::error::Error for TransmitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

impl Transmission {
    fn result(&self) -> Result<u32, TransmitError> {
        let mut transferred = 0u32;
        let mut flags = 0u32;
        // SAFETY: the operation has completed, so reading its OVERLAPPED is
        // race-free; fWait = FALSE never blocks.
        if unsafe {
            WSAGetOverlappedResult(
                self.socket,
                &self.overlapped,
                &mut transferred,
                0,
                &mut flags,
            )
        } == 0
        {
            // SAFETY: reads this thread's last Winsock error.
            let error = unsafe { WSAGetLastError() };
            // InternalHigh carries the bytes transferred even on failure.
            let sent = transferred.max(self.overlapped.InternalHigh as u32);
            if error == ERROR_HANDLE_EOF as i32 {
                // A window past the end of a shrinking file: a short send,
                // exactly as sendfile(2) reports it.
                return Ok(sent);
            }
            return Err(TransmitError {
                sent,
                error: io::Error::from_raw_os_error(error),
            });
        }
        Ok(transferred)
    }

    fn unregister_wait(&mut self) {
        if !self.wait.is_null() {
            // SAFETY: INVALID_HANDLE_VALUE blocks until any running callback
            // returns, after which the context may be freed.
            unsafe { UnregisterWaitEx(self.wait, INVALID_HANDLE_VALUE) };
            self.wait = null_mut();
        }
    }

    fn cancel(&mut self) -> Result<u32, TransmitError> {
        if !self.finished {
            unsafe {
                CancelIoEx(self.socket as HANDLE, &self.overlapped);
                WaitForSingleObject(self.event, INFINITE);
            }
            self.finished = true;
        }
        self.unregister_wait();
        self.result()
    }
}

impl Drop for Transmission {
    fn drop(&mut self) {
        if !self.finished {
            // The future was dropped (timeout or cancelled request) while the
            // kernel still owns the OVERLAPPED. Cancel it and wait for the
            // cancellation to complete before the memory is released.
            // SAFETY: the socket handle stays open (it is borrowed by the
            // dropped future's caller) and the OVERLAPPED is still live.
            unsafe {
                CancelIoEx(self.socket as HANDLE, &self.overlapped);
                WaitForSingleObject(self.event, INFINITE);
            }
        }
        self.unregister_wait();
        // SAFETY: the event was created by us and is no longer referenced.
        unsafe { CloseHandle(self.event) };
    }
}

// SAFETY: every field is either plain data or a kernel handle, and Windows
// kernel handles and overlapped operations are not bound to the issuing
// thread; the callback only touches the Mutex-guarded sender.
unsafe impl Send for Transmission {}
unsafe impl Sync for Transmission {}

fn failed(error: io::Error) -> TransmitError {
    TransmitError { sent: 0, error }
}

fn tcp_bytes_out(socket: SOCKET) -> io::Result<u64> {
    let version = 0u32;
    let mut info = TCP_INFO_v0::default();
    let mut returned = 0u32;
    let result = unsafe {
        WSAIoctl(
            socket,
            SIO_TCP_INFO,
            (&raw const version).cast(),
            size_of::<u32>() as u32,
            (&raw mut info).cast(),
            size_of::<TCP_INFO_v0>() as u32,
            &mut returned,
            null_mut(),
            None,
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    Ok(info.BytesOut)
}

enum Started {
    Done(Result<u32, TransmitError>),
    Pending(Box<Transmission>, tokio::sync::oneshot::Receiver<()>),
}

/// Issues the overlapped `TransmitFile` and, if it is still in flight,
/// registers a thread-pool wait that signals the returned receiver.
fn start_transmission(socket: SOCKET, file: &File, offset: u64, count: u32) -> Started {
    // SAFETY: a manual-reset, initially non-signaled, unnamed event.
    let event = unsafe { CreateEventW(null(), 1, 0, null()) };
    if event.is_null() {
        return Started::Done(Err(failed(io::Error::last_os_error())));
    }
    let (sender, receiver) = tokio::sync::oneshot::channel();
    // SAFETY: OVERLAPPED is plain data; zero is its documented initial state.
    let mut overlapped: OVERLAPPED = unsafe { zeroed() };
    overlapped.Anonymous.Anonymous.Offset = offset as u32;
    overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
    // The low bit keeps the completion off any I/O completion port the socket
    // may be associated with; the event is still signaled.
    overlapped.hEvent = (event as usize | 1) as HANDLE;
    let mut transmission = Box::new(Transmission {
        overlapped,
        event,
        wait: null_mut(),
        completed: std::sync::Mutex::new(Some(sender)),
        finished: false,
        socket,
    });

    // SAFETY: the socket and file handles are borrowed by the caller for the
    // whole operation, and the boxed OVERLAPPED outlives it (Drop cancels and
    // waits for it).
    let ok = unsafe {
        TransmitFile(
            transmission.socket,
            raw(file),
            count,
            0,
            &mut transmission.overlapped,
            null(),
            0,
        )
    };
    if ok == 0 {
        // SAFETY: reads this thread's last Winsock error.
        let error = unsafe { WSAGetLastError() };
        if error != WSA_IO_PENDING {
            // Nothing was queued, so the kernel does not own the OVERLAPPED.
            transmission.finished = true;
            return Started::Done(Err(failed(io::Error::from_raw_os_error(error))));
        }
    }

    // Completed synchronously, or already done by the time we look.
    // SAFETY: a zero-timeout poll of our own event.
    if unsafe { WaitForSingleObject(event, 0) } == WAIT_OBJECT_0 {
        transmission.finished = true;
        return Started::Done(transmission.result());
    }

    let context = (&raw const *transmission).cast::<c_void>();
    let mut wait: HANDLE = null_mut();
    // SAFETY: the callback context is the boxed Transmission, kept alive until
    // the wait is unregistered with INVALID_HANDLE_VALUE.
    if unsafe {
        RegisterWaitForSingleObject(
            &mut wait,
            event,
            Some(transmission_signaled),
            context,
            INFINITE,
            WT_EXECUTEONLYONCE,
        )
    } == 0
    {
        // Dropping the box cancels the still-pending transfer before freeing it.
        return Started::Done(Err(failed(io::Error::last_os_error())));
    }
    transmission.wait = wait;
    Started::Pending(transmission, receiver)
}

/// A file prepared for [`transmit_file`]: an overlapped reopen of the
/// caller's handle.
///
/// On a handle opened for synchronous I/O (every `std::fs::File`),
/// `TransmitFile` validates and advances against the handle's shared file
/// pointer, so a second send from one handle fails with `WSAEINVAL`, and
/// concurrent sends sharing a handle would race. On an overlapped handle the
/// `OVERLAPPED` offset alone positions each send. Create one per body and
/// reuse it across its chunks; it works on files already deleted by eviction.
///
/// If the original handle's share mode forbids a reopen, the source falls back
/// to a duplicate of it and positions the shared file pointer before each
/// send; that is correct only while no other sender uses the same file
/// object concurrently. Files the cache opens itself always reopen.
#[derive(Debug)]
pub struct TransmitSource {
    file: File,
    positioned: bool,
}

impl TransmitSource {
    pub fn new(file: &File) -> io::Result<Self> {
        // SAFETY: the original handle is borrowed for the call; ReOpenFile
        // returns a new independent handle or INVALID_HANDLE_VALUE.
        let handle = unsafe {
            ReOpenFile(
                raw(file),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_FLAG_OVERLAPPED | FILE_FLAG_SEQUENTIAL_SCAN,
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            // SAFETY: a fresh handle owned by nothing else.
            return Ok(Self {
                file: unsafe { File::from_raw_handle(handle as _) },
                positioned: false,
            });
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_SHARING_VIOLATION as i32) {
            return Err(error);
        }
        Ok(Self {
            file: file.try_clone()?,
            positioned: true,
        })
    }
}

/// Sends `count` bytes of `source` starting at `offset` to `socket` with an
/// overlapped `TransmitFile`: the kernel reads the file through the system
/// cache and sends it without a user-mode copy. Returns the bytes sent.
///
/// Cancel-safe: dropping the future cancels the transfer and waits for the
/// kernel to release it. `count == 0` returns immediately (for `TransmitFile`
/// zero means "the whole file").
///
/// [!] Client SKUs of Windows cap concurrent `TransmitFile` calls at two and
/// queue the rest; Windows Server has no such cap.
pub async fn transmit_file(
    socket: &impl AsRawSocket,
    source: &TransmitSource,
    offset: u64,
    count: u32,
) -> Result<u32, TransmitError> {
    let file = &source.file;
    if count == 0 {
        return Ok(0);
    }
    // TransmitFile rejects a window past end-of-file with WSAEINVAL, whereas
    // sendfile(2) sends what exists. Clamp to the current length so a growing
    // or truncated file reads as a short (or zero) count.
    let length = file.metadata().map_err(failed)?.len();
    if offset >= length {
        return Ok(0);
    }
    let count = count.min(u32::try_from(length - offset).unwrap_or(u32::MAX));
    if source.positioned {
        use std::io::Seek as _;
        (&source.file)
            .seek(std::io::SeekFrom::Start(offset))
            .map_err(failed)?;
    }
    let (mut transmission, receiver) =
        match start_transmission(socket.as_raw_socket() as SOCKET, file, offset, count) {
            Started::Done(result) => return shrunk_to_short_count(result, file, offset, count),
            Started::Pending(transmission, receiver) => (transmission, receiver),
        };
    // The sender lives in the Transmission, so it can only vanish unsent if
    // the callback's lock was poisoned; Drop then cancels the transfer.
    receiver
        .await
        .map_err(|_| failed(io::Error::other("TransmitFile completion signal lost")))?;
    transmission.finished = true;
    transmission.unregister_wait();
    shrunk_to_short_count(transmission.result(), file, offset, count)
}

/// Sends a file window while resetting `inactivity_timeout` whenever the TCP
/// byte counter advances. A timeout cancels the overlapped operation and
/// reports the bytes already sent.
pub async fn transmit_file_with_inactivity_timeout(
    socket: &impl AsRawSocket,
    source: &TransmitSource,
    offset: u64,
    count: u32,
    inactivity_timeout: std::time::Duration,
) -> Result<u32, TransmitError> {
    let raw_socket = socket.as_raw_socket() as SOCKET;
    let baseline = tcp_bytes_out(raw_socket).ok();
    let file = &source.file;
    if count == 0 {
        return Ok(0);
    }
    let length = file.metadata().map_err(failed)?.len();
    if offset >= length {
        return Ok(0);
    }
    let count = count.min(u32::try_from(length - offset).unwrap_or(u32::MAX));
    if source.positioned {
        use std::io::Seek as _;
        (&source.file)
            .seek(std::io::SeekFrom::Start(offset))
            .map_err(failed)?;
    }
    let (mut transmission, mut receiver) = match start_transmission(raw_socket, file, offset, count)
    {
        Started::Done(result) => return shrunk_to_short_count(result, file, offset, count),
        Started::Pending(transmission, receiver) => (transmission, receiver),
    };

    let Some(baseline) = baseline else {
        return match tokio::time::timeout(inactivity_timeout, &mut receiver).await {
            Ok(signal) => {
                signal
                    .map_err(|_| failed(io::Error::other("TransmitFile completion signal lost")))?;
                transmission.finished = true;
                transmission.unregister_wait();
                shrunk_to_short_count(transmission.result(), file, offset, count)
            }
            Err(_) => match transmission.cancel() {
                Ok(sent) => shrunk_to_short_count(Ok(sent), file, offset, count),
                Err(failure) => Err(TransmitError {
                    sent: failure.sent,
                    error: io::Error::new(
                        io::ErrorKind::TimedOut,
                        "downstream TransmitFile timeout without TCP progress counters",
                    ),
                }),
            },
        };
    };

    let poll_interval = inactivity_timeout
        .min(std::time::Duration::from_secs(1))
        .max(std::time::Duration::from_millis(1));
    let mut last_bytes = baseline;
    let mut last_progress = tokio::time::Instant::now();
    loop {
        let remaining = inactivity_timeout.saturating_sub(last_progress.elapsed());
        if let Ok(signal) = tokio::time::timeout(remaining.min(poll_interval), &mut receiver).await
        {
            signal.map_err(|_| failed(io::Error::other("TransmitFile completion signal lost")))?;
            transmission.finished = true;
            transmission.unregister_wait();
            return shrunk_to_short_count(transmission.result(), file, offset, count);
        }

        if let Ok(bytes) = tcp_bytes_out(raw_socket)
            && bytes > last_bytes
        {
            last_bytes = bytes;
            last_progress = tokio::time::Instant::now();
            continue;
        }
        if last_progress.elapsed() < inactivity_timeout {
            continue;
        }

        return match transmission.cancel() {
            Ok(sent) => shrunk_to_short_count(Ok(sent), file, offset, count),
            Err(failure) => Err(TransmitError {
                sent: failure.sent,
                error: io::Error::new(
                    io::ErrorKind::TimedOut,
                    "downstream TransmitFile inactivity timeout",
                ),
            }),
        };
    }
}

/// A file truncated while its window was in flight fails with WSAEINVAL;
/// report the bytes already sent as a short count instead, as sendfile(2) does.
fn shrunk_to_short_count(
    result: Result<u32, TransmitError>,
    file: &File,
    offset: u64,
    count: u32,
) -> Result<u32, TransmitError> {
    match result {
        Err(failure)
            if failure.error.raw_os_error() == Some(WSAEINVAL)
                && file
                    .metadata()
                    .is_ok_and(|metadata| metadata.len() < offset + u64::from(count)) =>
        {
            Ok(failure.sent)
        }
        result => result,
    }
}
