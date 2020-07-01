use anyhow::{bail, format_err, Error};
use std::ffi::CString;
use std::ptr;
use std::os::raw::{c_uchar, c_char, c_int, c_void};
use std::sync::{Mutex, Condvar};

use proxmox::try_block;
use proxmox_backup::client::BackupRepository;
use proxmox_backup::backup::BackupDir;
use chrono::{DateTime, Utc, TimeZone};

mod capi_types;
use capi_types::*;

mod upload_queue;

mod commands;

mod worker_task;
use worker_task::*;

mod restore;
use restore::*;

mod tools;

pub const PROXMOX_BACKUP_DEFAULT_CHUNK_SIZE: u64 = 1024*1024*4;

/// Free returned error messages
///
/// All calls can return error messages, but they are allocated using
/// the rust standard library. This call moves ownership back to rust
/// and free the allocated memory.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn proxmox_backup_free_error(ptr: * mut c_char) {
    if !ptr.is_null() {
        unsafe { CString::from_raw(ptr); }
    }
}

// Note: UTF8 Strings may contain 0 bytes.
fn convert_error_to_cstring(err: String) -> CString {
    match CString::new(err) {
        Ok(msg) => msg,
        Err(err) => {
            eprintln!("got error containung 0 bytes: {}", err);
            CString::new("failed to convert error message containing 0 bytes").unwrap()
        },
    }
}

macro_rules! raise_error_null {
    ($error:ident, $err:expr) => {{
        let errmsg = convert_error_to_cstring($err.to_string());
        unsafe { *$error =  errmsg.into_raw(); }
        return ptr::null_mut();
    }}
}

macro_rules! raise_error_int {
    ($error:ident, $err:expr) => {{
        let errmsg = convert_error_to_cstring($err.to_string());
        unsafe { *$error =  errmsg.into_raw(); }
        return -1 as c_int;
    }}
}

#[derive(Clone)]
pub(crate) struct BackupSetup {
    pub host: String,
    pub store: String,
    pub user: String,
    pub chunk_size: u64,
    pub backup_id: String,
    pub backup_time: DateTime<Utc>,
    pub password: Option<String>,
    pub keyfile: Option<std::path::PathBuf>,
    pub key_password: Option<String>,
    pub fingerprint: Option<String>,
}

// helper class to implement synchrounous interface
struct GotResultCondition {
    lock: Mutex<bool>,
    cond: Condvar,
}

impl GotResultCondition {

    pub fn new() -> Self {
        Self {
            lock: Mutex::new(false),
            cond: Condvar::new(),
        }
    }

    /// Create CallbackPointers
    ///
    /// wait() returns If the contained callback is called.
    pub fn callback_info(
        &mut self,
        result: *mut c_int,
        error: *mut *mut c_char,
    ) -> CallbackPointers {
        CallbackPointers {
            callback: Self::wakeup_callback,
            callback_data: (self) as *mut _ as *mut c_void,
            error,
            result: result,
        }
    }

    /// Waits until the callback from callback_info is called.
    pub fn wait(&mut self) {
        let mut done = self.lock.lock().unwrap();
        while !*done {
            done = self.cond.wait(done).unwrap();
        }
    }

    #[no_mangle]
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    extern "C" fn wakeup_callback(
        callback_data: *mut c_void,
    ) {
        let callback_data = unsafe { &mut *( callback_data as * mut GotResultCondition) };
        let mut done = callback_data.lock.lock().unwrap();
        *done = true;
        callback_data.cond.notify_one();
    }
}


/// Create a new instance
///
/// Uses `PROXMOX_BACKUP_DEFAULT_CHUNK_SIZE` if `chunk_size` is zero.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn proxmox_backup_new(
    repo: *const c_char,
    backup_id: *const c_char,
    backup_time: u64,
    chunk_size: u64,
    password: *const c_char,
    keyfile: *const c_char,
    key_password: *const c_char,
    fingerprint: *const c_char,
    error: * mut * mut c_char,
) -> *mut ProxmoxBackupHandle {

    let task: Result<_, Error> = try_block!({
        let repo: BackupRepository = tools::utf8_c_string(repo)?
            .ok_or_else(|| format_err!("repo must not be NULL"))?
            .parse()?;

        let backup_id = tools::utf8_c_string(backup_id)?
            .ok_or_else(|| format_err!("backup_id must not be NULL"))?;

        let backup_time = Utc.timestamp(backup_time as i64, 0);

        let password = tools::utf8_c_string(password)?;
        let keyfile = tools::utf8_c_string(keyfile)?.map(std::path::PathBuf::from);
        let key_password = tools::utf8_c_string(key_password)?;
        let fingerprint = tools::utf8_c_string(fingerprint)?;

        let setup = BackupSetup {
            host: repo.host().to_owned(),
            user: repo.user().to_owned(),
            store: repo.store().to_owned(),
            chunk_size: if chunk_size > 0 { chunk_size } else { PROXMOX_BACKUP_DEFAULT_CHUNK_SIZE },
            backup_id,
            password,
            backup_time,
            keyfile,
            key_password,
            fingerprint,
        };

        BackupTask::new(setup)
    });

    match task {
        Ok(task) => {
            let boxed_task = Box::new(task);
            Box::into_raw(boxed_task) as * mut ProxmoxBackupHandle
        }
        Err(err) => raise_error_null!(error, err),
    }
}

/// Open connection to the backup server (sync)
///
/// Returns:
///  0 ... Sucecss (no prevbious backup)
///  1 ... Success (found previous backup)
/// -1 ... Error
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn proxmox_backup_connect(
    handle: *mut ProxmoxBackupHandle,
    error: *mut *mut c_char,
) -> c_int {
    let task = unsafe { &mut *(handle as * mut BackupTask) };

    let mut result: c_int = -1;

    let mut got_result_condition = GotResultCondition::new();

    let callback_info = got_result_condition.callback_info(&mut result, error);

    task.runtime().spawn(async move {
        let result = task.connect().await;
        callback_info.send_result(result);
    });

    got_result_condition.wait();

    return result;
}

/// Open connection to the backup server
///
/// Returns:
///  0 ... Sucecss (no prevbious backup)
///  1 ... Success (found previous backup)
/// -1 ... Error
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn proxmox_backup_connect_async(
    handle: *mut ProxmoxBackupHandle,
    callback: extern "C" fn(*mut c_void),
    callback_data: *mut c_void,
    result: *mut c_int,
    error: *mut *mut c_char,
) {
    let task = unsafe { &mut *(handle as * mut BackupTask) };
    let callback_info = CallbackPointers { callback, callback_data, error, result };

    task.runtime().spawn(async move {
        let result = task.connect().await;
        callback_info.send_result(result);
    });
}


/// Abort a running backup task
///
/// This stops the current backup task. It is still necessary to call
/// proxmox_backup_disconnect() to close the connection and free
/// allocated memory.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn proxmox_backup_abort(
    handle: *mut ProxmoxBackupHandle,
    reason: *const c_char,
) {
    let task = unsafe { &mut *(handle as * mut BackupTask) };

    let reason = unsafe { tools::utf8_c_string_lossy_non_null(reason) };
    task.abort(reason);
}


/// Register a backup image (sync)
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn proxmox_backup_register_image(
    handle: *mut ProxmoxBackupHandle,
    device_name: *const c_char, // expect utf8 here
    size: u64,
    incremental: bool,
    error: * mut * mut c_char,
) -> c_int {
    let task = unsafe { &mut *(handle as * mut BackupTask) };

    let mut result: c_int = -1;

    let mut got_result_condition = GotResultCondition::new();

    let callback_info = got_result_condition.callback_info(&mut result, error);

    let device_name = unsafe { tools::utf8_c_string_lossy_non_null(device_name) };

    task.runtime().spawn(async move {
        let result = task.register_image(device_name, size, incremental).await;
        callback_info.send_result(result);
    });

    got_result_condition.wait();

    return result;
}
/// Register a backup image
///
/// Create a new image archive on the backup server
/// ('<device_name>.img.fidx'). The returned integer is the dev_id
/// parameter for the proxmox_backup_write_data_async() method.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn proxmox_backup_register_image_async(
    handle: *mut ProxmoxBackupHandle,
    device_name: *const c_char, // expect utf8 here
    size: u64,
    incremental: bool,
    callback: extern "C" fn(*mut c_void),
    callback_data: *mut c_void,
    result: *mut c_int,
    error: * mut * mut c_char,
) {
    let task = unsafe { &mut *(handle as * mut BackupTask) };
    let callback_info = CallbackPointers { callback, callback_data, error, result };

    let device_name = unsafe { tools::utf8_c_string_lossy_non_null(device_name) };

    task.runtime().spawn(async move {
        let result = task.register_image(device_name, size, incremental).await;
        callback_info.send_result(result);
    });
}

/// Add a configuration blob to the backup (sync)
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn proxmox_backup_add_config(
    handle: *mut ProxmoxBackupHandle,
    name: *const c_char, // expect utf8 here
    data: *const u8,
    size: u64,
    error: * mut * mut c_char,
) -> c_int {
    let task = unsafe { &mut *(handle as * mut BackupTask) };

    let mut result: c_int = -1;

    let mut got_result_condition = GotResultCondition::new();

    let callback_info = got_result_condition.callback_info(&mut result, error);

    let name = unsafe { tools::utf8_c_string_lossy_non_null(name) };

    let data = DataPointer(data); // fixme

    task.runtime().spawn(async move {
        let result = task.add_config(name, data, size).await;
        callback_info.send_result(result);
    });

    got_result_condition.wait();

    return result;
}

/// Add a configuration blob to the backup
///
/// Create and upload a data blob "<name>.blob".
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn proxmox_backup_add_config_async(
    handle: *mut ProxmoxBackupHandle,
    name: *const c_char, // expect utf8 here
    data: *const u8,
    size: u64,
    callback: extern "C" fn(*mut c_void),
    callback_data: *mut c_void,
    result: *mut c_int,
    error: * mut * mut c_char,
) {
    let task = unsafe { &mut *(handle as * mut BackupTask) };

    let callback_info = CallbackPointers { callback, callback_data, error, result };

    let name = unsafe { tools::utf8_c_string_lossy_non_null(name) };
    let data = DataPointer(data); // fixme

    task.runtime().spawn(async move {
        let result = task.add_config(name, data, size).await;
        callback_info.send_result(result);
    });
}

/// Write data to into a registered image (sync)
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn proxmox_backup_write_data(
    handle: *mut ProxmoxBackupHandle,
    dev_id: u8,
    data: *const u8,
    offset: u64,
    size: u64,
    error: * mut * mut c_char,
) -> c_int {
    let task = unsafe { &mut *(handle as * mut BackupTask) };
    let mut result: c_int = -1;

    let mut got_result_condition = GotResultCondition::new();

    let callback_info = got_result_condition.callback_info(&mut result, error);
    let data = DataPointer(data); // fixme

    task.runtime().spawn(async move {
        let result = task.write_data(dev_id, data, offset, size).await;
        callback_info.send_result(result);
    });

    got_result_condition.wait();

    return result;
}

/// Write data to into a registered image
///
/// Upload a chunk of data for the <dev_id> image.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn proxmox_backup_write_data_async(
    handle: *mut ProxmoxBackupHandle,
    dev_id: u8,
    data: *const u8,
    offset: u64,
    size: u64,
    callback: extern "C" fn(*mut c_void),
    callback_data: *mut c_void,
    result: *mut c_int,
    error: * mut * mut c_char,
) {
    let task = unsafe { &mut *(handle as * mut BackupTask) };
    let callback_info = CallbackPointers { callback, callback_data, error, result };
    let data = DataPointer(data); // fixme

    task.runtime().spawn(async move {
        let result = task.write_data(dev_id, data, offset, size).await;
        callback_info.send_result(result);
    });
}

/// Close a registered image (sync)
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn proxmox_backup_close_image(
    handle: *mut ProxmoxBackupHandle,
    dev_id: u8,
    error: * mut * mut c_char,
) -> c_int {
    let task = unsafe { &mut *(handle as * mut BackupTask) };

    let mut result: c_int = -1;

    let mut got_result_condition = GotResultCondition::new();

    let callback_info = got_result_condition.callback_info(&mut result, error);

    task.runtime().spawn(async move {
        let result = task.close_image(dev_id).await;
        callback_info.send_result(result);
    });

    got_result_condition.wait();

    return result;
}

/// Close a registered image
///
/// Mark the image as closed. Further writes are not possible.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn proxmox_backup_close_image_async(
    handle: *mut ProxmoxBackupHandle,
    dev_id: u8,
    callback: extern "C" fn(*mut c_void),
    callback_data: *mut c_void,
    result: *mut c_int,
    error: * mut * mut c_char,
) {
    let task = unsafe { &mut *(handle as * mut BackupTask) };
    let callback_info = CallbackPointers { callback, callback_data, error, result };

    task.runtime().spawn(async move {
        let result = task.close_image(dev_id).await;
        callback_info.send_result(result);
    });
}

/// Finish the backup (sync)
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn proxmox_backup_finish(
    handle: *mut ProxmoxBackupHandle,
    error: * mut * mut c_char,
) -> c_int {
    let task = unsafe { &mut *(handle as * mut BackupTask) };

    let mut result: c_int = -1;

    let mut got_result_condition = GotResultCondition::new();

    let callback_info = got_result_condition.callback_info(&mut result, error);

    task.runtime().spawn(async move {
        let result = task.finish().await;
        callback_info.send_result(result);
    });

    got_result_condition.wait();

    return result;
}

/// Finish the backup
///
/// Finish the backup by creating and uploading the backup manifest.
/// All registered images have to be closed before calling this.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn proxmox_backup_finish_async(
    handle: *mut ProxmoxBackupHandle,
    callback: extern "C" fn(*mut c_void),
    callback_data: *mut c_void,
    result: *mut c_int,
    error: * mut * mut c_char,
) {
    let task = unsafe { &mut *(handle as * mut BackupTask) };
    let callback_info = CallbackPointers { callback, callback_data, error, result };

    task.runtime().spawn(async move {
        let result = task.finish().await;
        callback_info.send_result(result);
    });
}

/// Disconnect and free allocated memory
///
/// The handle becomes invalid after this call.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn proxmox_backup_disconnect(handle: *mut ProxmoxBackupHandle) {

    let task = handle as * mut BackupTask;

    unsafe { Box::from_raw(task) }; // take ownership, drop(task)
}


/// Simple interface to restore images
///
/// Connect the the backup server.
///
/// Note: This implementation is not async
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn proxmox_restore_connect(
    repo: *const c_char,
    snapshot: *const c_char,
    password: *const c_char,
    keyfile: *const c_char,
    key_password: *const c_char,
    fingerprint: *const c_char,
    error: * mut * mut c_char,
) -> *mut ProxmoxRestoreHandle {

    let result: Result<_, Error> = try_block!({
        let repo: BackupRepository = tools::utf8_c_string(repo)?
            .ok_or_else(|| format_err!("repo must not be NULL"))?
            .parse()?;

        let snapshot: BackupDir = tools::utf8_c_string_lossy(snapshot)
            .ok_or_else(|| format_err!("snapshot must not be NULL"))?
            .parse()?;

        let backup_type = snapshot.group().backup_type();
        let backup_id = snapshot.group().backup_id().to_owned();
        let backup_time = snapshot.backup_time();

        if backup_type != "vm" {
            bail!("wrong backup type ({} != vm)", backup_type);
        }

        let password = tools::utf8_c_string(password)?;
        let keyfile = tools::utf8_c_string(keyfile)?.map(std::path::PathBuf::from);
        let key_password = tools::utf8_c_string(key_password)?;
        let fingerprint = tools::utf8_c_string(fingerprint)?;

        let setup = BackupSetup {
            host: repo.host().to_owned(),
            user: repo.user().to_owned(),
            store: repo.store().to_owned(),
            chunk_size: PROXMOX_BACKUP_DEFAULT_CHUNK_SIZE, // not used by restore
            backup_id,
            password,
            backup_time,
            keyfile,
            key_password,
            fingerprint,
        };

        ProxmoxRestore::new(setup)
    });

    match result {
        Ok(conn) => {
            let boxed_task = Box::new(conn);
            Box::into_raw(boxed_task) as * mut ProxmoxRestoreHandle
        }
        Err(err) => raise_error_null!(error, err),
    }
}

/// Disconnect and free allocated memory
///
/// The handle becomes invalid after this call.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn proxmox_restore_disconnect(handle: *mut ProxmoxRestoreHandle) {

    let conn = handle as * mut ProxmoxRestore;
    unsafe { Box::from_raw(conn) }; //drop(conn)
}

/// Restore an image
///
/// Image data is downloaded and sequentially dumped to the callback.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn proxmox_restore_image(
    handle: *mut ProxmoxRestoreHandle,
    archive_name: *const c_char, // expect full name here, i.e. "name.img.fidx"
    callback: extern "C" fn(*mut c_void, u64, *const c_uchar, u64) -> c_int,
    callback_data: *mut c_void,
    error: * mut * mut c_char,
    verbose: bool,
) -> c_int {

    let conn = unsafe { &mut *(handle as * mut ProxmoxRestore) };

    let result: Result<_, Error> = try_block!({

        let archive_name = tools::utf8_c_string(archive_name)?
            .ok_or_else(|| format_err!("archive_name must not be NULL"))?;

        let write_data_callback = move |offset: u64, data: &[u8]| {
            callback(callback_data, offset, data.as_ptr(), data.len() as u64)
        };

        let write_zero_callback = move |offset: u64, len: u64| {
            callback(callback_data, offset, std::ptr::null(), len)
        };

        conn.restore(archive_name, write_data_callback, write_zero_callback, verbose)?;

        Ok(())
    });

    if let Err(err) = result {
        raise_error_int!(error, err);
    };

    0
}
