#![allow(dead_code)]

use std::{
    ffi::{OsStr, c_void},
    fs::File,
    io,
    mem::size_of,
    os::windows::{ffi::OsStrExt, io::FromRawHandle},
    path::Path,
    ptr::{null, null_mut},
    time::Instant,
};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_FAILED, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    },
    Security::SECURITY_ATTRIBUTES,
    System::{
        Environment::{FreeEnvironmentStringsW, GetEnvironmentStringsW},
        JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOBOBJECTINFOCLASS, JobObjectBasicAccountingInformation,
            JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
            TerminateJobObject,
        },
        Memory::{GetProcessHeap, HEAP_ZERO_MEMORY, HeapAlloc, HeapFree},
        Pipes::CreatePipe,
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
            DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
            InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION,
            ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
            UpdateProcThreadAttribute, WaitForSingleObject,
        },
    },
};

use crate::{
    lsp_process::{ProcessIo, RetainedSpawnFailure, SpawnedLanguageServer},
    navigation::TrustedServer,
};

type CreateProcessWFn = unsafe extern "system" fn(
    *const u16,
    *mut u16,
    *const SECURITY_ATTRIBUTES,
    *const SECURITY_ATTRIBUTES,
    i32,
    PROCESS_CREATION_FLAGS,
    *const c_void,
    *const u16,
    *const STARTUPINFOW,
    *mut PROCESS_INFORMATION,
) -> i32;
type ResumeThreadFn = unsafe extern "system" fn(HANDLE) -> u32;
type TerminateJobFn = unsafe extern "system" fn(HANDLE, u32) -> i32;
type WaitFn = unsafe extern "system" fn(HANDLE, u32) -> u32;
type QueryJobFn =
    unsafe extern "system" fn(HANDLE, JOBOBJECTINFOCLASS, *mut c_void, u32, *mut u32) -> i32;

#[derive(Clone, Copy)]
struct Win32Api {
    create_process_w: CreateProcessWFn,
    resume_thread: ResumeThreadFn,
    terminate_job: TerminateJobFn,
    wait: WaitFn,
    query_job: QueryJobFn,
}

static WIN32_API: Win32Api = Win32Api {
    create_process_w: CreateProcessW,
    resume_thread: ResumeThread,
    terminate_job: TerminateJobObject,
    wait: WaitForSingleObject,
    query_job: QueryInformationJobObject,
};

struct OwnedHandle(HANDLE);

// SAFETY: a Windows kernel HANDLE is process-global and may be used from any
// thread. OwnedHandle keeps unique close ownership; shared access only exposes
// the stable raw value to Win32 calls, while transfer/close require ownership.
unsafe impl Send for OwnedHandle {}
// SAFETY: see the Send justification above. Concurrent Win32 observation does
// not mutate this RAII owner's close state.
unsafe impl Sync for OwnedHandle {}

impl OwnedHandle {
    fn new(handle: HANDLE, context: &'static str) -> Result<Self> {
        if handle.is_null() {
            return Err(io::Error::last_os_error()).context(context);
        }
        Ok(Self(handle))
    }

    fn raw(&self) -> HANDLE {
        self.0
    }

    fn into_file(mut self) -> File {
        let handle = self.0;
        self.0 = null_mut();
        // SAFETY: ownership of the unique Win32 handle is transferred to File.
        unsafe { File::from_raw_handle(handle) }
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: this RAII owner closes its unique live handle once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

struct PipePair {
    read: OwnedHandle,
    write: OwnedHandle,
}

impl PipePair {
    fn inheritable() -> Result<Self> {
        let mut read = null_mut();
        let mut write = null_mut();
        let attributes = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).expect("structure fits u32"),
            lpSecurityDescriptor: null_mut(),
            bInheritHandle: 1,
        };
        // SAFETY: output pointers and security attributes remain valid for the call.
        if unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) } == 0 {
            return Err(io::Error::last_os_error()).context("cannot create LSP anonymous pipe");
        }
        Ok(Self {
            read: OwnedHandle::new(read, "CreatePipe returned null read handle")?,
            write: OwnedHandle::new(write, "CreatePipe returned null write handle")?,
        })
    }
}

struct AttributeList {
    heap: HANDLE,
    pointer: *mut c_void,
    initialized: bool,
}

impl AttributeList {
    fn for_handles_and_job(handles: &mut [HANDLE; 3], job: &mut HANDLE) -> Result<Self> {
        let mut bytes = 0usize;
        // SAFETY: the documented sizing call accepts a null list.
        unsafe { InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut bytes) };
        if bytes == 0 {
            return Err(io::Error::last_os_error()).context("cannot size process attribute list");
        }
        // SAFETY: GetProcessHeap returns the current process heap.
        let heap = unsafe { GetProcessHeap() };
        if heap.is_null() {
            return Err(io::Error::last_os_error()).context("cannot obtain process heap");
        }
        // SAFETY: allocation size came from the Windows sizing call.
        let pointer = unsafe { HeapAlloc(heap, HEAP_ZERO_MEMORY, bytes) };
        if pointer.is_null() {
            bail!("cannot allocate process attribute list");
        }
        let mut list = Self {
            heap,
            pointer,
            initialized: false,
        };
        // SAFETY: allocated storage is the exact size requested by the API.
        if unsafe { InitializeProcThreadAttributeList(pointer, 2, 0, &mut bytes) } == 0 {
            return Err(io::Error::last_os_error()).context("cannot initialize process attributes");
        }
        list.initialized = true;
        // SAFETY: the three inheritable child handles and array remain live
        // until CreateProcessW returns.
        if unsafe {
            UpdateProcThreadAttribute(
                pointer,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast(),
                size_of::<[HANDLE; 3]>(),
                null_mut(),
                null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error()).context("cannot isolate inherited LSP handles");
        }
        // SAFETY: the private Job handle remains live until CreateProcessW
        // returns; creation-time association prevents an unowned child window.
        if unsafe {
            UpdateProcThreadAttribute(
                pointer,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                (job as *mut HANDLE).cast(),
                size_of::<HANDLE>(),
                null_mut(),
                null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error())
                .context("cannot associate the LSP Job at process creation");
        }
        Ok(list)
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: initialization state tracks whether Delete is required;
        // HeapFree then releases the unique allocation.
        unsafe {
            if self.initialized {
                DeleteProcThreadAttributeList(self.pointer);
            }
            HeapFree(self.heap, 0, self.pointer);
        }
    }
}

struct EnvironmentBlock(*mut u16);

impl EnvironmentBlock {
    fn current() -> Result<Self> {
        // SAFETY: Windows returns a process-owned environment snapshot.
        let pointer = unsafe { GetEnvironmentStringsW() };
        if pointer.is_null() {
            return Err(io::Error::last_os_error()).context("cannot read process environment");
        }
        Ok(Self(pointer))
    }
}

impl Drop for EnvironmentBlock {
    fn drop(&mut self) {
        // SAFETY: pointer came from GetEnvironmentStringsW and is freed once.
        unsafe { FreeEnvironmentStringsW(self.0) };
    }
}

pub(crate) struct OwnedProcessTree {
    process: OwnedHandle,
    job: OwnedHandle,
    process_id: u32,
    terminated: bool,
    api: &'static Win32Api,
}

impl OwnedProcessTree {
    pub(crate) fn id(&self) -> u32 {
        self.process_id
    }

    pub(crate) fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        // SAFETY: process handle is live.
        match unsafe { (self.api.wait)(self.process.raw(), 0) } {
            WAIT_OBJECT_0 => {
                let mut code = 0u32;
                // SAFETY: signaled process handle has a stable exit code.
                if unsafe { GetExitCodeProcess(self.process.raw(), &mut code) } == 0 {
                    return Err(io::Error::last_os_error())
                        .context("cannot obtain language server exit code");
                }
                use std::os::windows::process::ExitStatusExt;
                Ok(Some(std::process::ExitStatus::from_raw(code)))
            }
            WAIT_TIMEOUT => Ok(None),
            WAIT_FAILED => {
                Err(io::Error::last_os_error()).context("cannot query language server process")
            }
            other => bail!("unexpected WaitForSingleObject result {other}"),
        }
    }

    pub(crate) fn wait_for_exit(&mut self, deadline: Instant) -> Result<bool> {
        while Instant::now() < deadline {
            if self.poll_exit()? {
                return Ok(true);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        self.poll_exit()
    }

    pub(crate) fn force_cleanup(&mut self) -> Result<()> {
        if self.terminated {
            return Ok(());
        }
        self.begin_force_cleanup()?;
        let deadline = Instant::now() + std::time::Duration::from_secs(1);
        while Instant::now() < deadline {
            if self.poll_force_cleanup()? {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if self.poll_force_cleanup()? {
            return Ok(());
        }
        bail!("language server process did not terminate after Job cleanup")
    }

    pub(crate) fn poll_exit(&mut self) -> Result<bool> {
        if self.terminated {
            return Ok(true);
        }
        // Direct-child exit alone is insufficient: descendants in the Job can
        // keep stdio pipes and the process tree alive.
        let direct_exited = match unsafe { (self.api.wait)(self.process.raw(), 0) } {
            WAIT_OBJECT_0 => true,
            WAIT_TIMEOUT => false,
            WAIT_FAILED => {
                return Err(io::Error::last_os_error())
                    .context("cannot wait for language server process");
            }
            other => bail!("unexpected WaitForSingleObject result {other}"),
        };
        if direct_exited && self.active_processes()? == 0 {
            self.terminated = true;
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) fn begin_force_cleanup(&mut self) -> Result<()> {
        if self.terminated {
            return Ok(());
        }
        // SAFETY: job handle is live; termination covers all contained descendants.
        if unsafe { (self.api.terminate_job)(self.job.raw(), 1) } == 0 {
            return Err(io::Error::last_os_error())
                .context("cannot terminate language server Job Object");
        }
        Ok(())
    }

    pub(crate) fn escalate_force_cleanup(&mut self) -> Result<()> {
        // TerminateJobObject is already the Windows force escalation. Keep a
        // separate primitive so the manager can drive identical shared phases.
        Ok(())
    }

    pub(crate) fn poll_force_cleanup(&mut self) -> Result<bool> {
        self.poll_exit()
    }

    fn active_processes(&self) -> Result<u32> {
        let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: output points to a correctly sized accounting structure.
        if unsafe {
            (self.api.query_job)(
                self.job.raw(),
                JobObjectBasicAccountingInformation,
                (&mut accounting as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                u32::try_from(size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>())
                    .expect("structure fits u32"),
                null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error())
                .context("cannot confirm language server Job Object is empty");
        }
        Ok(accounting.ActiveProcesses)
    }
}

impl Drop for OwnedProcessTree {
    fn drop(&mut self) {
        if !self.terminated {
            let _ = self.force_cleanup();
        }
    }
}

pub(crate) fn spawn(
    server: &TrustedServer,
    workspace_root: &Path,
    server_root: &Path,
) -> Result<SpawnedLanguageServer> {
    spawn_with_api(server, workspace_root, server_root, &WIN32_API)
}

fn spawn_with_api(
    server: &TrustedServer,
    workspace_root: &Path,
    server_root: &Path,
    api: &'static Win32Api,
) -> Result<SpawnedLanguageServer> {
    server.revalidate_before_spawn(workspace_root)?;

    let stdin = PipePair::inheritable()?;
    let stdout = PipePair::inheritable()?;
    let stderr = PipePair::inheritable()?;
    clear_inheritance(stdin.write.raw())?;
    clear_inheritance(stdout.read.raw())?;
    clear_inheritance(stderr.read.raw())?;

    // Create and configure containment before the suspended child exists, then
    // include that Job in the same creation-time attribute list as stdio.
    // SAFETY: null security/name creates a private Job Object.
    let job = OwnedHandle::new(
        unsafe { CreateJobObjectW(null(), null()) },
        "cannot create LSP Job Object",
    )?;
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: input points to a correctly sized initialized limits structure.
    if unsafe {
        SetInformationJobObject(
            job.raw(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                .expect("structure fits u32"),
        )
    } == 0
    {
        return Err(io::Error::last_os_error()).context("cannot configure LSP Job Object");
    }
    let mut child_handles = [stdin.read.raw(), stdout.write.raw(), stderr.write.raw()];
    let mut child_job = job.raw();
    let attributes = AttributeList::for_handles_and_job(&mut child_handles, &mut child_job)?;

    let application = wide_nul(server.program().as_os_str())?;
    let cwd = wide_nul(server_root.as_os_str())?;
    let mut command_line = quote_command_line(server.program().as_os_str(), server.args())?;
    let environment = EnvironmentBlock::current()?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb =
        u32::try_from(size_of::<STARTUPINFOEXW>()).expect("structure fits u32");
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = stdin.read.raw();
    startup.StartupInfo.hStdOutput = stdout.write.raw();
    startup.StartupInfo.hStdError = stderr.write.raw();
    startup.lpAttributeList = attributes.pointer;
    let mut process = PROCESS_INFORMATION::default();
    let flags = CREATE_UNICODE_ENVIRONMENT
        | CREATE_SUSPENDED
        | CREATE_NO_WINDOW
        | EXTENDED_STARTUPINFO_PRESENT;
    // SAFETY: all pointers refer to live, NUL-terminated or documented blocks;
    // command_line is mutable and child handles are explicitly allowlisted.
    if unsafe {
        (api.create_process_w)(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            null(),
            null(),
            1,
            flags,
            environment.0.cast(),
            cwd.as_ptr(),
            &startup.StartupInfo,
            &mut process,
        )
    } == 0
    {
        // Capture the creation/Job-policy error before any RAII cleanup or
        // formatting can overwrite the thread-local Win32 error.
        let original = io::Error::last_os_error();
        return Err(original).with_context(|| {
            format!(
                "cannot start language server {}",
                server.program().display()
            )
        });
    }
    // CreateProcessW success transfers both handles. Build the Job/process
    // owner immediately; no post-create path may hold a bare suspended child.
    let thread_handle = OwnedHandle(process.hThread);
    let tree = OwnedProcessTree {
        process: OwnedHandle(process.hProcess),
        job,
        process_id: process.dwProcessId,
        terminated: false,
        api,
    };
    drop(attributes);
    drop(environment);
    drop(stdin.read);
    drop(stdout.write);
    drop(stderr.write);

    // SAFETY: primary thread is suspended and uniquely owned here.
    if unsafe { (api.resume_thread)(thread_handle.raw()) } == u32::MAX {
        let original = io::Error::last_os_error();
        let original = format!("cannot resume language server thread: {original}");
        drop(thread_handle);
        return match cleanup_post_create_failure(tree) {
            Ok(()) => Err(anyhow::anyhow!(original)),
            Err((cleanup_error, retained_tree)) => Err(anyhow::Error::new(
                RetainedSpawnFailure::new(original, cleanup_error, retained_tree),
            )),
        };
    }
    drop(thread_handle);

    Ok(SpawnedLanguageServer {
        tree,
        io: ProcessIo {
            stdin: Box::new(stdin.write.into_file()),
            stdout: Box::new(stdout.read.into_file()),
            stderr: Box::new(stderr.read.into_file()),
        },
    })
}

fn cleanup_post_create_failure(
    mut tree: OwnedProcessTree,
) -> std::result::Result<(), (String, OwnedProcessTree)> {
    // This predicate is intentionally stricter than best-effort Drop cleanup:
    // every condition must prove ownership has reached a terminal state.
    if unsafe { (tree.api.terminate_job)(tree.job.raw(), 1) } == 0 {
        let error = io::Error::last_os_error();
        return Err((
            format!("cannot terminate language server Job Object: {error}"),
            tree,
        ));
    }
    match unsafe { (tree.api.wait)(tree.process.raw(), 1_000) } {
        WAIT_OBJECT_0 => {}
        WAIT_TIMEOUT => {
            return Err((
                "language server process did not terminate after Job cleanup".to_owned(),
                tree,
            ));
        }
        other => {
            let error = io::Error::last_os_error();
            return Err((
                format!("cannot reap language server process ({other}): {error}"),
                tree,
            ));
        }
    }
    match tree.active_processes() {
        Ok(0) => {
            tree.terminated = true;
            Ok(())
        }
        Ok(active) => Err((
            format!("language server Job Object still contains {active} active processes"),
            tree,
        )),
        Err(error) => Err((format!("{error:#}"), tree)),
    }
}

fn clear_inheritance(handle: HANDLE) -> Result<()> {
    // SAFETY: the handle is a live parent pipe end.
    if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(io::Error::last_os_error()).context("cannot isolate parent LSP pipe handle");
    }
    Ok(())
}

fn wide_nul(value: &OsStr) -> Result<Vec<u16>> {
    let mut encoded: Vec<u16> = value.encode_wide().collect();
    if encoded.contains(&0) {
        bail!("Windows process string contains NUL");
    }
    encoded.push(0);
    if encoded.len() > 32_767 {
        bail!("Windows process string exceeds 32,767 UTF-16 units");
    }
    Ok(encoded)
}

pub(crate) fn quote_command_line(program: &OsStr, args: &[String]) -> Result<Vec<u16>> {
    let mut command = Vec::new();
    for (index, argument) in std::iter::once(program)
        .chain(args.iter().map(|argument| OsStr::new(argument)))
        .enumerate()
    {
        if index > 0 {
            command.push(u16::from(b' '));
        }
        quote_argument(argument, &mut command)?;
    }
    command.push(0);
    if command.len() > 32_767 {
        bail!("Windows command line exceeds 32,767 UTF-16 units");
    }
    Ok(command)
}

fn quote_argument(argument: &OsStr, output: &mut Vec<u16>) -> Result<()> {
    let units: Vec<u16> = argument.encode_wide().collect();
    if units.contains(&0) {
        bail!("Windows process argument contains NUL");
    }
    output.push(u16::from(b'"'));
    let mut slashes = 0usize;
    for unit in units {
        if unit == u16::from(b'\\') {
            slashes += 1;
            continue;
        }
        if unit == u16::from(b'"') {
            output.extend(std::iter::repeat_n(u16::from(b'\\'), slashes * 2 + 1));
            output.push(unit);
        } else {
            output.extend(std::iter::repeat_n(u16::from(b'\\'), slashes));
            output.push(unit);
        }
        slashes = 0;
    }
    output.extend(std::iter::repeat_n(u16::from(b'\\'), slashes * 2));
    output.push(u16::from(b'"'));
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    };

    use windows_sys::Win32::{
        Foundation::{ERROR_ACCESS_DENIED, ERROR_GEN_FAILURE, SetLastError},
        Storage::FileSystem::SYNCHRONIZE,
        System::Threading::OpenProcess,
    };

    use super::*;
    use crate::navigation::validate_executable;

    static FAULT_TEST_LOCK: Mutex<()> = Mutex::new(());
    static LAST_CREATED_PID: AtomicU32 = AtomicU32::new(0);

    unsafe extern "system" fn recording_create_process(
        application: *const u16,
        command_line: *mut u16,
        process_attributes: *const SECURITY_ATTRIBUTES,
        thread_attributes: *const SECURITY_ATTRIBUTES,
        inherit_handles: i32,
        creation_flags: PROCESS_CREATION_FLAGS,
        environment: *const c_void,
        current_directory: *const u16,
        startup: *const STARTUPINFOW,
        process: *mut PROCESS_INFORMATION,
    ) -> i32 {
        // SAFETY: this adapter forwards the exact production call contract.
        let result = unsafe {
            CreateProcessW(
                application,
                command_line,
                process_attributes,
                thread_attributes,
                inherit_handles,
                creation_flags,
                environment,
                current_directory,
                startup,
                process,
            )
        };
        if result != 0 {
            // SAFETY: successful CreateProcessW initialized PROCESS_INFORMATION.
            LAST_CREATED_PID.store(unsafe { (*process).dwProcessId }, Ordering::SeqCst);
        }
        result
    }

    unsafe extern "system" fn reject_create_process(
        _application: *const u16,
        _command_line: *mut u16,
        _process_attributes: *const SECURITY_ATTRIBUTES,
        _thread_attributes: *const SECURITY_ATTRIBUTES,
        _inherit_handles: i32,
        _creation_flags: PROCESS_CREATION_FLAGS,
        _environment: *const c_void,
        _current_directory: *const u16,
        _startup: *const STARTUPINFOW,
        _process: *mut PROCESS_INFORMATION,
    ) -> i32 {
        // SAFETY: the fault adapter deliberately supplies a stable original error.
        unsafe { SetLastError(ERROR_ACCESS_DENIED) };
        0
    }

    unsafe extern "system" fn fail_resume(_thread: HANDLE) -> u32 {
        // SAFETY: the fault adapter deliberately supplies a stable original error.
        unsafe { SetLastError(ERROR_GEN_FAILURE) };
        u32::MAX
    }

    unsafe extern "system" fn fail_terminate(_job: HANDLE, _exit_code: u32) -> i32 {
        // SAFETY: the fault adapter deliberately supplies a distinct cleanup error.
        unsafe { SetLastError(ERROR_ACCESS_DENIED) };
        0
    }

    unsafe extern "system" fn timeout_wait(_process: HANDLE, _milliseconds: u32) -> u32 {
        WAIT_TIMEOUT
    }

    static CREATE_REJECTION_API: Win32Api = Win32Api {
        create_process_w: reject_create_process,
        resume_thread: ResumeThread,
        terminate_job: TerminateJobObject,
        wait: WaitForSingleObject,
        query_job: QueryInformationJobObject,
    };
    static RESUME_FAILURE_API: Win32Api = Win32Api {
        create_process_w: recording_create_process,
        resume_thread: fail_resume,
        terminate_job: TerminateJobObject,
        wait: WaitForSingleObject,
        query_job: QueryInformationJobObject,
    };
    static TERMINATE_FAILURE_API: Win32Api = Win32Api {
        create_process_w: recording_create_process,
        resume_thread: fail_resume,
        terminate_job: fail_terminate,
        wait: WaitForSingleObject,
        query_job: QueryInformationJobObject,
    };
    static WAIT_TIMEOUT_API: Win32Api = Win32Api {
        create_process_w: recording_create_process,
        resume_thread: fail_resume,
        terminate_job: TerminateJobObject,
        wait: timeout_wait,
        query_job: QueryInformationJobObject,
    };

    fn test_server() -> (tempfile::TempDir, std::path::PathBuf, TrustedServer) {
        let container = tempfile::tempdir().unwrap();
        let workspace = container.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
        let validated = validate_executable(&executable, &workspace).unwrap();
        let server = TrustedServer {
            program: validated.path,
            args: Arc::from([]),
            identity: validated.identity,
        };
        (container, workspace, server)
    }

    fn retained_tree(mut error: anyhow::Error) -> OwnedProcessTree {
        error
            .downcast_mut::<RetainedSpawnFailure>()
            .expect("failure must retain its process-tree owner")
            .take_tree()
            .expect("retained process-tree owner is present")
    }

    fn explicitly_cleanup(mut tree: OwnedProcessTree) {
        tree.api = &WIN32_API;
        tree.force_cleanup().unwrap();
        let pid = tree.id();
        drop(tree);
        assert!(!process_is_running(pid));
    }

    fn process_is_running(pid: u32) -> bool {
        // SAFETY: OpenProcess returns an owned query handle or null.
        let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            return false;
        }
        // SAFETY: handle is live for this zero-time query and closed once.
        let wait = unsafe { WaitForSingleObject(handle, 0) };
        unsafe { CloseHandle(handle) };
        wait == WAIT_TIMEOUT
    }

    #[test]
    fn windows_quoting_doubles_backslashes_before_quotes_and_end_quote() {
        let quoted = quote_command_line(
            OsStr::new(r#"C:\Program Files\server.exe"#),
            &[r#"a\"b"#.to_owned(), r#"tail\"#.to_owned(), String::new()],
        )
        .unwrap();
        let decoded = String::from_utf16(&quoted[..quoted.len() - 1]).unwrap();
        assert_eq!(
            decoded,
            r#""C:\Program Files\server.exe" "a\"b" "tail\\" """#
        );
    }

    #[test]
    fn creation_time_job_rejection_never_returns_an_unowned_child() {
        let _guard = FAULT_TEST_LOCK.lock().unwrap();
        LAST_CREATED_PID.store(0, Ordering::SeqCst);
        let (_container, workspace, server) = test_server();
        let error = spawn_with_api(&server, &workspace, &workspace, &CREATE_REJECTION_API)
            .err()
            .expect("injected CreateProcess rejection must fail");
        assert!(format!("{error:#}").contains("cannot start language server"));
        assert_eq!(LAST_CREATED_PID.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn resume_failure_is_clean_only_after_verified_job_termination() {
        let _guard = FAULT_TEST_LOCK.lock().unwrap();
        LAST_CREATED_PID.store(0, Ordering::SeqCst);
        let (_container, workspace, server) = test_server();
        let error = spawn_with_api(&server, &workspace, &workspace, &RESUME_FAILURE_API)
            .err()
            .expect("injected ResumeThread failure must fail");
        assert!(format!("{error:#}").contains("cannot resume language server thread"));
        let pid = LAST_CREATED_PID.load(Ordering::SeqCst);
        assert!(pid != 0);
        assert!(!process_is_running(pid));
        assert!(error.downcast_ref::<RetainedSpawnFailure>().is_none());
    }

    #[test]
    fn terminate_failure_returns_a_retained_owner_for_explicit_cleanup() {
        let _guard = FAULT_TEST_LOCK.lock().unwrap();
        LAST_CREATED_PID.store(0, Ordering::SeqCst);
        let (_container, workspace, server) = test_server();
        let error = spawn_with_api(&server, &workspace, &workspace, &TERMINATE_FAILURE_API)
            .err()
            .expect("injected Job termination failure must fail");
        let message = format!("{error:#}");
        assert!(message.contains("cannot resume language server thread"));
        assert!(message.contains("cannot terminate language server Job Object"));
        let tree = retained_tree(error);
        assert!(process_is_running(tree.id()));
        explicitly_cleanup(tree);
    }

    #[test]
    fn wait_timeout_returns_a_retained_owner_for_explicit_cleanup() {
        let _guard = FAULT_TEST_LOCK.lock().unwrap();
        LAST_CREATED_PID.store(0, Ordering::SeqCst);
        let (_container, workspace, server) = test_server();
        let error = spawn_with_api(&server, &workspace, &workspace, &WAIT_TIMEOUT_API)
            .err()
            .expect("injected wait timeout must fail");
        assert!(format!("{error:#}").contains("did not terminate after Job cleanup"));
        explicitly_cleanup(retained_tree(error));
    }

    #[test]
    fn post_create_cleanup_preserves_original_and_secondary_errors() {
        let _guard = FAULT_TEST_LOCK.lock().unwrap();
        let (_container, workspace, server) = test_server();
        let error = spawn_with_api(&server, &workspace, &workspace, &TERMINATE_FAILURE_API)
            .err()
            .expect("injected post-create failures must fail");
        let message = format!("{error:#}");
        let original = message
            .find("cannot resume language server thread")
            .unwrap();
        let secondary = message
            .find("secondary cleanup failure: cannot terminate")
            .unwrap();
        assert!(original < secondary);
        explicitly_cleanup(retained_tree(error));
    }
}
