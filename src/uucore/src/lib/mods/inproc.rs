// Skua (sh16 P2): per-invocation context for running a utility's `uumain` inside a host process.
//
// A host (the shell) calls [`run`] on a thread of its own with an [`Invocation`]: the utility's
// own stdin/stdout/stderr (the handles a child process would have been given), its name, the
// environment and working directory a child would have started with, and a cancel token. The
// context lives in thread-local storage; nothing process-wide is touched (no `SetStdHandle`, no
// `set_current_dir`, no `set_var`), so several utilities can run at once on different threads.
//
// Utility code reaches the context through this module instead of `std`:
//   * [`stdin`], [`stdout`], [`stderr`]: the std types when no context is installed, the
//     utility's own streams when one is;
//   * [`var_os`], [`var`], [`current_dir`], [`resolve`]: the request's environment and directory;
//   * [`exit`]: `std::process::exit`, or an unwind to [`run`] inside a host (needs `panic =
//     "unwind"` in the host). Utilities hosted in a process return a status instead wherever
//     they can: cancelling a run never unwinds (its stream calls fail with [`Cancelled`]), and
//     a broken pipe is reported by the status;
//   * `util_name()`, `execution_phrase()`, `get/set_exit_code()` follow the context.
// Without a context (a utility running as its own program) every function is the std one.

#[cfg(windows)]
pub use self::imp::*;
#[cfg(not(windows))]
pub use self::other::*;

#[cfg(not(windows))]
mod other {
    use std::borrow::Cow;
    use std::ffi::{OsStr, OsString};
    use std::path::Path;

    pub use std::io::{
        Stderr, StderrLock, Stdin, StdinLock, Stdout, StdoutLock, stderr, stdin, stdout,
    };

    /// `std::process::exit` (there is no host on this platform).
    pub fn exit(code: i32) -> ! {
        std::process::exit(code)
    }
    /// Never a context on this platform.
    pub const fn active() -> bool {
        false
    }
    /// Never a context on this platform.
    pub const fn name_override() -> Option<&'static str> {
        None
    }
    /// Never a context on this platform.
    pub const fn exit_code() -> Option<i32> {
        None
    }
    /// Never a context on this platform.
    pub fn set_exit_code(_code: i32) -> bool {
        false
    }
    /// `std::env::var_os`.
    pub fn var_os(key: impl AsRef<OsStr>) -> Option<OsString> {
        std::env::var_os(key)
    }
    /// `std::env::var`.
    pub fn var(key: impl AsRef<OsStr>) -> Result<String, std::env::VarError> {
        std::env::var(key)
    }
    /// The path as it is.
    pub fn resolve(path: &(impl AsRef<Path> + ?Sized)) -> Cow<'_, Path> {
        let path = path.as_ref();
        Cow::Borrowed(path)
    }
    /// Never a context on this platform.
    pub fn stdout_is_closed_pipe() -> Option<bool> {
        None
    }
    /// Never a context on this platform: nothing to note.
    pub fn note_pipe_closed() {}
    /// A file opened for reading by a utility (the std one on this platform).
    pub type CtxFile = std::fs::File;
    /// `File::open`.
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<CtxFile> {
        std::fs::File::open(path)
    }
}

#[cfg(windows)]
mod imp {
    use std::borrow::Cow;
    use std::cell::{Cell, RefCell};
    use std::ffi::{OsStr, OsString};
    use std::fs::File;
    use std::io::{self, BufRead, BufReader, IsTerminal as _, LineWriter, Read, Write};
    use std::mem::ManuallyDrop;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
    use std::path::{Component, Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, Once, PoisonError};

    /// Unwind payload for an `exit` taken inside a host.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ToolExit(pub i32);

    /// What a child would have been given, owned by the host until [`run`] returns.
    /// The handles stay open for the whole run (the host keeps their owners alive) and are
    /// never closed by the utility.
    #[derive(Debug)]
    pub struct Invocation {
        /// The utility's name (what `util_name()` returns, what messages start with).
        pub util_name: &'static str,
        /// Raw handle of the utility's stdin.
        pub stdin: RawHandle,
        /// Raw handle of the utility's stdout.
        pub stdout: RawHandle,
        /// Raw handle of the utility's stderr.
        pub stderr: RawHandle,
        /// The environment a child would have started with.
        pub env: Vec<(String, String)>,
        /// The directory a child would have started in.
        pub cwd: PathBuf,
        /// Cancellation: see [`CancelToken`].
        pub cancel: Arc<CancelToken>,
    }

    // SAFETY: the raw handles are plain values; the host guarantees they stay open until `run`
    // returns and that the pointed-to objects may be used from any thread.
    unsafe impl Send for Invocation {}

    /// Ends a running utility from another thread: sets a flag the utility's stream calls
    /// check, and cancels the synchronous I/O the utility's thread may be blocked in.
    #[derive(Debug, Default)]
    pub struct CancelToken {
        flag: AtomicBool,
        /// `OpenThread` handle of the thread running the utility while it runs.
        thread: Mutex<Option<isize>>,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
        fn OpenThread(access: u32, inherit: i32, thread_id: u32) -> isize;
        fn CancelSynchronousIo(thread: isize) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }

    const THREAD_TERMINATE: u32 = 0x0001;

    impl CancelToken {
        /// A fresh token.
        pub fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        /// Whether [`Self::cancel`] has been called.
        pub fn is_cancelled(&self) -> bool {
            self.flag.load(Ordering::Acquire)
        }

        /// Asks the utility to stop: its next stream call ends it with status 130, and a read or
        /// write it is blocked in is aborted. Safe to call repeatedly and after the run ended.
        /// Returns whether the utility's thread was still running (callers retry while it is:
        /// a cancel can land before the utility starts its blocking call).
        pub fn cancel(&self) -> bool {
            self.flag.store(true, Ordering::Release);
            let guard = self.thread.lock().unwrap_or_else(PoisonError::into_inner);
            guard.is_some_and(|thread| {
                // SAFETY: `thread` is an open handle; the lock keeps `run` from closing it.
                unsafe { CancelSynchronousIo(thread) };
                true
            })
        }

        fn attach(&self) {
            // SAFETY: plain calls.
            let handle = unsafe { OpenThread(THREAD_TERMINATE, 0, GetCurrentThreadId()) };
            if handle != 0 {
                *self.thread.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);
            }
        }

        fn detach(&self) {
            let mut guard = self.thread.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(handle) = guard.take() {
                // SAFETY: the handle was opened by `attach` and is closed once, here.
                unsafe { CloseHandle(handle) };
            }
        }
    }

    // -- the thread-local context ---------------------------------------------------------

    /// What a stream call returns once the run has been cancelled (see [`CancelToken`]): the
    /// payload of an `io::Error` of kind `Other`. Never `Interrupted`: `write_all` and
    /// `read_to_end` retry that kind forever.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Cancelled;

    impl std::fmt::Display for Cancelled {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("cancelled")
        }
    }

    impl std::error::Error for Cancelled {}

    /// Whether `error` is the one a cancelled run's stream calls return.
    pub fn is_cancelled_error(error: &io::Error) -> bool {
        error
            .get_ref()
            .is_some_and(|inner| inner.downcast_ref::<Cancelled>().is_some())
    }

    /// A raw handle used as a stream; never closed here.
    #[derive(Clone, Copy)]
    struct RawFile {
        handle: isize,
        /// The context's stdout (a failed write on it that says "reader gone" is remembered).
        out: bool,
    }

    impl RawFile {
        const fn new(handle: isize) -> Self {
            Self { handle, out: false }
        }
        fn file(self) -> ManuallyDrop<File> {
            // SAFETY: the host keeps the handle open for the whole run; `ManuallyDrop` keeps this
            // wrapper from closing it.
            ManuallyDrop::new(unsafe { File::from_raw_handle(self.handle as RawHandle) })
        }
        const fn raw(self) -> RawHandle {
            self.handle as RawHandle
        }
    }

    impl Read for RawFile {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if cancelled() {
                return Err(io::Error::other(Cancelled));
            }
            let result = (&*self.file()).read(buf);
            if cancelled() {
                return Err(io::Error::other(Cancelled));
            }
            result
        }
    }

    impl Write for RawFile {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if cancelled() {
                return Err(io::Error::other(Cancelled));
            }
            let result = (&*self.file()).write(buf);
            if cancelled() {
                return Err(io::Error::other(Cancelled));
            }
            if self.out
                && let Err(error) = &result
                && error.kind() == io::ErrorKind::BrokenPipe
            {
                STDOUT_BROKEN.with(|b| b.set(true));
            }
            result
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    thread_local! {
        static ACTIVE: Cell<bool> = const { Cell::new(false) };
        static NAME: Cell<Option<&'static str>> = const { Cell::new(None) };
        static EXIT_CODE: Cell<i32> = const { Cell::new(0) };
        static STDIN: Cell<Option<RawFile>> = const { Cell::new(None) };
        static STDOUT: RefCell<Option<LineWriter<RawFile>>> = const { RefCell::new(None) };
        static STDERR: Cell<Option<RawFile>> = const { Cell::new(None) };
        static ENV: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
        static CWD: RefCell<PathBuf> = RefCell::new(PathBuf::new());
        static CANCEL: RefCell<Option<Arc<CancelToken>>> = const { RefCell::new(None) };
        /// A write to the context's stdout failed because its reader had gone.
        static STDOUT_BROKEN: Cell<bool> = const { Cell::new(false) };
        /// A diagnostic was not printed because stdout's reader had gone (a child would have
        /// exited 141, quietly, at that point).
        static PIPE_CLOSED_NOTED: Cell<bool> = const { Cell::new(false) };
    }

    /// Whether a host is running a utility on this thread.
    pub fn active() -> bool {
        ACTIVE.with(Cell::get)
    }

    /// The utility name of the context, if this thread runs one.
    pub fn name_override() -> Option<&'static str> {
        NAME.with(Cell::get)
    }

    /// The exit code set so far in the context, if this thread runs one.
    pub fn exit_code() -> Option<i32> {
        active().then(|| EXIT_CODE.with(Cell::get))
    }

    /// Sets the context's exit code; `false` (and nothing done) without a context.
    pub fn set_exit_code(code: i32) -> bool {
        let on = active();
        if on {
            EXIT_CODE.with(|c| c.set(code));
        }
        on
    }

    /// A file a utility reads (see [`open`]): reads fail with [`Cancelled`] once the run has been
    /// cancelled, so a loop over a file stops at its next read even when none is blocked (which
    /// is all `CancelSynchronousIo` can interrupt).
    #[derive(Debug)]
    pub struct CtxFile(File);

    impl CtxFile {
        /// The underlying file, for the metadata and seek calls of fast paths.
        pub fn get_mut(&mut self) -> &mut File {
            &mut self.0
        }
    }

    impl Read for CtxFile {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if cancelled() {
                return Err(io::Error::other(Cancelled));
            }
            self.0.read(buf)
        }
    }

    /// `File::open` of `path` made relative to the context's directory (see [`resolve`]).
    pub fn open(path: impl AsRef<Path>) -> io::Result<CtxFile> {
        File::open(resolve(path.as_ref())).map(CtxFile)
    }

    /// Whether this thread's run has been cancelled.
    fn cancelled() -> bool {
        CANCEL.with(|c| {
            c.borrow()
                .as_ref()
                .is_some_and(|token| token.is_cancelled())
        })
    }

    /// `std::process::exit`, or, inside a host, an unwind carrying [`ToolExit`] to [`run`].
    ///
    /// The unwind needs `panic = "unwind"` in the host. It must never be started from a
    /// destructor or while panicking (a second unwind aborts the process), so utilities return
    /// their status where they can and call this only from straight-line code.
    pub fn exit(code: i32) -> ! {
        if active() {
            std::panic::resume_unwind(Box::new(ToolExit(code)));
        }
        std::process::exit(code)
    }

    /// Remembers that a diagnostic was dropped because stdout's reader has gone: the run then
    /// ends with status 141, as a child that exited at that point would have.
    pub fn note_pipe_closed() {
        PIPE_CLOSED_NOTED.with(|b| b.set(true));
    }

    /// Installs the context, runs `f` (the utility's `uumain`), removes the context, returns the
    /// status. Contains `exit` (an unwind carrying [`ToolExit`]) and panics (101; a broken-pipe
    /// panic is 141). Must run on a thread of the host's choosing that nothing else uses
    /// meanwhile; the thread keeps no state afterwards.
    pub fn run(inv: Invocation, f: impl FnOnce() -> i32) -> i32 {
        install_panic_hook();
        let Invocation {
            util_name,
            stdin,
            stdout,
            stderr,
            env,
            cwd,
            cancel,
        } = inv;
        ACTIVE.with(|a| a.set(true));
        NAME.with(|n| n.set(Some(util_name)));
        EXIT_CODE.with(|c| c.set(0));
        STDIN.with(|s| s.set(Some(RawFile::new(stdin as isize))));
        STDOUT.with(|s| {
            *s.borrow_mut() = Some(LineWriter::new(RawFile {
                handle: stdout as isize,
                out: true,
            }));
        });
        STDERR.with(|s| s.set(Some(RawFile::new(stderr as isize))));
        STDOUT_BROKEN.with(|b| b.set(false));
        PIPE_CLOSED_NOTED.with(|b| b.set(false));
        ENV.with(|e| *e.borrow_mut() = env);
        CWD.with(|c| *c.borrow_mut() = cwd);
        cancel.attach();
        CANCEL.with(|c| *c.borrow_mut() = Some(Arc::clone(&cancel)));

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        // What the utility left in the line buffer goes out first (errors: the reader may be
        // gone). The flush runs with the cancel flag still set, so a cancelled run's flush fails
        // at once instead of blocking on a reader that may never read again.
        let out = STDOUT.with(|s| s.borrow_mut().take());
        if let Some(mut out) = out {
            let _ = out.flush();
        }
        let cancelled = cancel.is_cancelled();
        let broken = STDOUT_BROKEN.with(Cell::get);
        let noted = PIPE_CLOSED_NOTED.with(Cell::get);
        CANCEL.with(|c| *c.borrow_mut() = None);
        cancel.detach();
        ACTIVE.with(|a| a.set(false));
        NAME.with(|n| n.set(None));
        STDIN.with(|s| s.set(None));
        STDERR.with(|s| s.set(None));
        ENV.with(|e| *e.borrow_mut() = Vec::new());
        CWD.with(|c| *c.borrow_mut() = PathBuf::new());
        STDOUT_BROKEN.with(|b| b.set(false));
        PIPE_CLOSED_NOTED.with(|b| b.set(false));

        if cancelled {
            return 130;
        }
        match result {
            _ if noted => 141,
            Ok(code) => code,
            Err(payload) => {
                if let Some(exit) = payload.downcast_ref::<ToolExit>() {
                    exit.0
                } else if broken {
                    // A panic after a write to a closed pipe failed: the SIGPIPE death.
                    141
                } else {
                    101
                }
            }
        }
    }

    fn payload_text(payload: &(dyn std::any::Any + Send)) -> Option<&str> {
        payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
    }

    /// Makes a panic inside a context report on the utility's stderr (as a program of its own
    /// would on its own) instead of the host's; other threads keep the previous hook.
    fn install_panic_hook() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                if !active() {
                    previous(info);
                    return;
                }
                if STDOUT_BROKEN.with(Cell::get) || cancelled() {
                    return; // `run` turns this into 141 (or 130), quietly
                }
                let location = info
                    .location()
                    .map_or_else(String::new, |l| format!(" at {l}"));
                let text = payload_text(info.payload()).unwrap_or("Box<dyn Any>");
                let message = format!(
                    "\nthread 'main' panicked{location}:\n{text}\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n"
                );
                if let Some(mut err) = STDERR.with(Cell::get) {
                    let _ = err.write_all(message.trim_start_matches('\n').as_bytes());
                }
            }));
        });
    }

    // -- environment and directory --------------------------------------------------------

    /// `std::env::var_os`, against the context's environment when there is one (names compare
    /// ASCII-case-insensitively, as Windows' do).
    pub fn var_os(key: impl AsRef<OsStr>) -> Option<OsString> {
        if !active() {
            return std::env::var_os(key);
        }
        let key = key.as_ref().to_string_lossy();
        ENV.with(|env| {
            env.borrow()
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(&key))
                .map(|(_, v)| OsString::from(v))
        })
    }

    /// `std::env::var`, against the context's environment when there is one.
    pub fn var(key: impl AsRef<OsStr>) -> Result<String, std::env::VarError> {
        if !active() {
            return std::env::var(key);
        }
        var_os(key)
            .ok_or(std::env::VarError::NotPresent)?
            .into_string()
            .map_err(std::env::VarError::NotUnicode)
    }

    /// The path to hand to `std::fs`, made independent of the process's own directory (the
    /// process has one for all threads; the shell's is per command): a relative path is joined
    /// to the context's directory, and a rooted path without a drive (`\file`, `/file`) takes
    /// the drive (or the UNC share) of the context's directory, as it would for a child started
    /// there. Paths with a prefix (`C:\x`, `\server\share\x`, `\?\...`) are returned as they
    /// are; `C:x` (relative to a drive's own directory) is left to the caller (the host does not
    /// run such a request in the process).
    pub fn resolve(path: &(impl AsRef<Path> + ?Sized)) -> Cow<'_, Path> {
        let path = path.as_ref();
        if !active() || matches!(path.components().next(), Some(Component::Prefix(_))) {
            return Cow::Borrowed(path);
        }
        // `join` does both: a relative path is appended, a rooted one replaces everything but
        // the prefix of the context's directory.
        CWD.with(|cwd| Cow::Owned(cwd.borrow().join(path)))
    }

    // -- standard streams -----------------------------------------------------------------

    fn closed() -> io::Error {
        io::Error::from(io::ErrorKind::BrokenPipe)
    }

    /// `std::io::stdin()`, or the context's stdin (read straight from its handle, no buffer that
    /// outlives the run).
    pub fn stdin() -> Stdin {
        if active() {
            Stdin::Ctx
        } else {
            Stdin::Std(io::stdin())
        }
    }

    /// `std::io::stdout()`, or the context's stdout (line-buffered like std's, flushed when the
    /// run ends).
    pub fn stdout() -> Stdout {
        if active() {
            Stdout::Ctx
        } else {
            Stdout::Std(io::stdout())
        }
    }

    /// `std::io::stderr()`, or the context's stderr (unbuffered).
    pub fn stderr() -> Stderr {
        if active() {
            Stderr::Ctx
        } else {
            Stderr::Std(io::stderr())
        }
    }

    fn ctx_stdin() -> io::Result<RawFile> {
        STDIN.with(Cell::get).ok_or_else(closed)
    }

    fn ctx_stderr() -> io::Result<RawFile> {
        STDERR.with(Cell::get).ok_or_else(closed)
    }

    fn with_stdout<T>(f: impl FnOnce(&mut LineWriter<RawFile>) -> io::Result<T>) -> io::Result<T> {
        STDOUT.with(|s| match s.borrow_mut().as_mut() {
            Some(w) => f(w),
            None => Err(closed()),
        })
    }

    fn ctx_stdout_raw() -> RawHandle {
        STDOUT.with(|s| {
            s.borrow()
                .as_ref()
                .map_or(std::ptr::null_mut(), |w| w.get_ref().raw())
        })
    }

    /// Whether the context's stdout is a pipe whose reader has gone (`None` without a context).
    pub fn stdout_is_closed_pipe() -> Option<bool> {
        active().then(|| closed_pipe(ctx_stdout_raw()))
    }

    /// Whether a zero-byte write fails because the pipe's reader has gone. The write passes an
    /// `OVERLAPPED` block so that it is also valid on an overlapped pipe handle. The block lives
    /// on the heap and is freed after the call, except when the write is still pending (a write
    /// that completes later must not touch freed memory): then, and only then, it is leaked.
    pub fn closed_pipe(handle: RawHandle) -> bool {
        const ERROR_BROKEN_PIPE: i32 = 109;
        const ERROR_NO_DATA: i32 = 232;
        const ERROR_IO_PENDING: i32 = 997;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn WriteFile(
                file: *mut std::ffi::c_void,
                buffer: *const u8,
                len: u32,
                written: *mut u32,
                overlapped: *mut std::ffi::c_void,
            ) -> i32;
        }
        if handle.is_null() {
            return false;
        }
        // `OVERLAPPED` (32 bytes on x64) followed by the byte count.
        let block: *mut ([u64; 4], u32) = Box::into_raw(Box::new(([0; 4], 0)));
        // SAFETY: a zero-length write sends no data; `block` is valid and zero-initialised for
        // the whole call (and for good if the call stays pending).
        let ok = unsafe {
            WriteFile(
                handle.cast(),
                std::ptr::null(),
                0,
                &raw mut (*block).1,
                (&raw mut (*block).0).cast(),
            )
        };
        let error = io::Error::last_os_error().raw_os_error();
        if ok == 0 && error == Some(ERROR_IO_PENDING) {
            // Left to the kernel, never freed.
            return false;
        }
        // SAFETY: the call has completed, nothing refers to `block` any more.
        drop(unsafe { Box::from_raw(block) });
        ok == 0 && matches!(error, Some(ERROR_BROKEN_PIPE | ERROR_NO_DATA))
    }

    /// See [`stdin`].
    #[derive(Debug)]
    pub enum Stdin {
        /// The process's.
        Std(io::Stdin),
        /// The context's.
        Ctx,
    }

    /// See [`Stdin`]; the context variant owns a buffer that lives as long as the lock.
    #[derive(Debug)]
    pub enum StdinLock {
        /// The process's.
        Std(io::StdinLock<'static>),
        /// The context's.
        Ctx(BufReader<RawFileReader>),
    }

    /// Reader over the context's stdin handle (public only because [`StdinLock`] names it).
    #[derive(Debug)]
    pub struct RawFileReader(RawFile);

    impl std::fmt::Debug for RawFile {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "RawFile({:#x})", self.handle)
        }
    }

    impl Read for RawFileReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }

    impl Stdin {
        /// As `std::io::Stdin::lock`.
        pub fn lock(&self) -> StdinLock {
            match self {
                Self::Std(s) => StdinLock::Std(s.lock()),
                Self::Ctx => match ctx_stdin() {
                    Ok(file) => {
                        StdinLock::Ctx(BufReader::with_capacity(64 * 1024, RawFileReader(file)))
                    }
                    Err(_) => StdinLock::Ctx(BufReader::new(RawFileReader(RawFile::new(0)))),
                },
            }
        }
    }

    impl Read for Stdin {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self {
                Self::Std(s) => s.read(buf),
                Self::Ctx => ctx_stdin()?.read(buf),
            }
        }
    }

    impl Read for StdinLock {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self {
                Self::Std(s) => s.read(buf),
                Self::Ctx(r) => r.read(buf),
            }
        }
    }

    impl BufRead for StdinLock {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            match self {
                Self::Std(s) => s.fill_buf(),
                Self::Ctx(r) => r.fill_buf(),
            }
        }
        fn consume(&mut self, amt: usize) {
            match self {
                Self::Std(s) => s.consume(amt),
                Self::Ctx(r) => r.consume(amt),
            }
        }
    }

    impl AsRawHandle for Stdin {
        fn as_raw_handle(&self) -> RawHandle {
            match self {
                Self::Std(s) => s.as_raw_handle(),
                Self::Ctx => ctx_stdin().map_or(std::ptr::null_mut(), RawFile::raw),
            }
        }
    }

    impl Stdin {
        /// As `IsTerminal::is_terminal` (that trait cannot be implemented outside std).
        pub fn is_terminal(&self) -> bool {
            match self {
                Self::Std(s) => s.is_terminal(),
                Self::Ctx => ctx_stdin().is_ok_and(|f| f.file().is_terminal()),
            }
        }
    }

    /// See [`stdout`].
    #[derive(Debug)]
    pub enum Stdout {
        /// The process's.
        Std(io::Stdout),
        /// The context's.
        Ctx,
    }

    /// See [`Stdout`].
    #[derive(Debug)]
    pub enum StdoutLock {
        /// The process's.
        Std(io::StdoutLock<'static>),
        /// The context's.
        Ctx,
    }

    impl Stdout {
        /// As `std::io::Stdout::lock`.
        pub fn lock(&self) -> StdoutLock {
            match self {
                Self::Std(s) => StdoutLock::Std(s.lock()),
                Self::Ctx => StdoutLock::Ctx,
            }
        }
    }

    impl Write for Stdout {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            match self {
                Self::Std(s) => s.write(buf),
                Self::Ctx => with_stdout(|w| w.write(buf)),
            }
        }
        fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
            match self {
                Self::Std(s) => s.write_all(buf),
                Self::Ctx => with_stdout(|w| w.write_all(buf)),
            }
        }
        fn write_fmt(&mut self, args: std::fmt::Arguments<'_>) -> io::Result<()> {
            match self {
                Self::Std(s) => s.write_fmt(args),
                Self::Ctx => with_stdout(|w| w.write_fmt(args)),
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            match self {
                Self::Std(s) => s.flush(),
                Self::Ctx => with_stdout(Write::flush),
            }
        }
    }

    impl Write for StdoutLock {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            match self {
                Self::Std(s) => s.write(buf),
                Self::Ctx => with_stdout(|w| w.write(buf)),
            }
        }
        fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
            match self {
                Self::Std(s) => s.write_all(buf),
                Self::Ctx => with_stdout(|w| w.write_all(buf)),
            }
        }
        fn write_fmt(&mut self, args: std::fmt::Arguments<'_>) -> io::Result<()> {
            match self {
                Self::Std(s) => s.write_fmt(args),
                Self::Ctx => with_stdout(|w| w.write_fmt(args)),
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            match self {
                Self::Std(s) => s.flush(),
                Self::Ctx => with_stdout(Write::flush),
            }
        }
    }

    impl AsRawHandle for Stdout {
        fn as_raw_handle(&self) -> RawHandle {
            match self {
                Self::Std(s) => s.as_raw_handle(),
                Self::Ctx => ctx_stdout_raw(),
            }
        }
    }

    impl Stdout {
        /// As `IsTerminal::is_terminal` (that trait cannot be implemented outside std).
        pub fn is_terminal(&self) -> bool {
            match self {
                Self::Std(s) => s.is_terminal(),
                Self::Ctx => {
                    let raw = ctx_stdout_raw();
                    !raw.is_null() && RawFile::new(raw as isize).file().is_terminal()
                }
            }
        }
    }

    /// See [`stderr`].
    #[derive(Debug)]
    pub enum Stderr {
        /// The process's.
        Std(io::Stderr),
        /// The context's.
        Ctx,
    }

    /// See [`Stderr`].
    #[derive(Debug)]
    pub enum StderrLock {
        /// The process's.
        Std(io::StderrLock<'static>),
        /// The context's.
        Ctx,
    }

    impl Stderr {
        /// As `std::io::Stderr::lock`.
        pub fn lock(&self) -> StderrLock {
            match self {
                Self::Std(s) => StderrLock::Std(s.lock()),
                Self::Ctx => StderrLock::Ctx,
            }
        }
    }

    impl Write for Stderr {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            match self {
                Self::Std(s) => s.write(buf),
                Self::Ctx => ctx_stderr()?.write(buf),
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Write for StderrLock {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            match self {
                Self::Std(s) => s.write(buf),
                Self::Ctx => ctx_stderr()?.write(buf),
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl AsRawHandle for Stderr {
        fn as_raw_handle(&self) -> RawHandle {
            match self {
                Self::Std(s) => s.as_raw_handle(),
                Self::Ctx => ctx_stderr().map_or(std::ptr::null_mut(), RawFile::raw),
            }
        }
    }

    impl Stderr {
        /// As `IsTerminal::is_terminal` (that trait cannot be implemented outside std).
        pub fn is_terminal(&self) -> bool {
            match self {
                Self::Std(s) => s.is_terminal(),
                Self::Ctx => ctx_stderr().is_ok_and(|f| f.file().is_terminal()),
            }
        }
    }

    /// The current directory of the context (the process's without one).
    pub fn current_dir() -> io::Result<PathBuf> {
        if active() {
            Ok(CWD.with(|c| c.borrow().clone()))
        } else {
            std::env::current_dir()
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::{Read as _, Write as _};
    use std::os::windows::io::{AsRawHandle as _, RawHandle};
    use std::path::{Path, PathBuf};
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    fn temp(name: &str) -> (File, PathBuf) {
        let path =
            std::env::temp_dir().join(format!("uucore-inproc-{}-{name}", std::process::id()));
        (File::create(&path).unwrap(), path)
    }

    fn invocation(util: &'static str, files: [&File; 3], env: &[(&str, &str)]) -> Invocation {
        invocation_raw(
            util,
            [
                files[0].as_raw_handle(),
                files[1].as_raw_handle(),
                files[2].as_raw_handle(),
            ],
            env,
        )
    }

    fn invocation_raw(util: &'static str, raw: [RawHandle; 3], env: &[(&str, &str)]) -> Invocation {
        Invocation {
            util_name: util,
            stdin: raw[0],
            stdout: raw[1],
            stderr: raw[2],
            env: env
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
            cwd: std::env::temp_dir(),
            cancel: CancelToken::new(),
        }
    }

    #[test]
    fn exit_panic_and_name_are_contained() {
        let (f, p) = temp("a");
        let inv = invocation("cat", [&f, &f, &f], &[]);
        assert_eq!(run(inv, || exit(7)), 7);
        assert!(!active());
        let inv = invocation("cat", [&f, &f, &f], &[]);
        assert_eq!(run(inv, || panic!("boom")), 101);
        let inv = invocation("wc", [&f, &f, &f], &[]);
        assert_eq!(run(inv, || i32::from(crate::util_name() != "wc")), 0);
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn streams_env_and_resolve_follow_the_context() {
        let (f, p) = temp("b");
        let inv = invocation("cat", [&f, &f, &f], &[("LANG", "x"), ("Path", "y")]);
        let code = run(inv, || {
            write!(stdout(), "out").unwrap();
            write!(stderr(), "err").unwrap();
            assert_eq!(var("lang").unwrap(), "x");
            assert!(var_os("NOPE").is_none());
            assert_eq!(
                resolve("a.txt").as_ref(),
                std::env::temp_dir().join("a.txt")
            );
            assert_eq!(resolve(r"C:\dir.txt").as_ref(), Path::new(r"C:\dir.txt"));
            0
        });
        assert_eq!(code, 0);
        drop(f);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("err") && text.contains("out"), "{text}");
        let _ = std::fs::remove_file(p);
    }

    /// Two utilities at once, each with its own exit code: both set theirs before either reads,
    /// so a code that leaked from one thread to the other would show.
    #[test]
    fn two_utilities_run_at_once() {
        let barrier = std::sync::Arc::new(Barrier::new(2));
        let handles: Vec<_> = [("one", 3), ("two", 4)]
            .into_iter()
            .map(|(n, expected)| {
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let (f, p) = temp(n);
                    let util: &'static str = if n == "one" { "cat" } else { "wc" };
                    let inv = invocation(util, [&f, &f, &f], &[]);
                    let code = run(inv, || {
                        for _ in 0..1000 {
                            writeln!(stdout(), "{}", crate::util_name()).unwrap();
                        }
                        set_exit_code(expected);
                        barrier.wait();
                        for _ in 0..1000 {
                            assert_eq!(crate::error::get_exit_code(), expected);
                        }
                        crate::error::get_exit_code()
                    });
                    drop(f);
                    let text = std::fs::read_to_string(&p).unwrap();
                    let _ = std::fs::remove_file(p);
                    (code, expected, text, util)
                })
            })
            .collect();
        for h in handles {
            let (code, expected, text, util) = h.join().unwrap();
            assert_eq!(code, expected);
            assert_eq!(text.lines().count(), 1000);
            assert!(text.lines().all(|l| l == util));
        }
    }

    #[test]
    fn cancelled_run_ends_at_its_next_stream_call() {
        let (f, p) = temp("c");
        let inv = invocation("cat", [&f, &f, &f], &[]);
        inv.cancel.cancel();
        let code = run(inv, || {
            let error = writeln!(stdout(), "never").and_then(|()| stdout().flush());
            assert!(error.is_err_and(|e| is_cancelled_error(&e)));
            let mut buf = [0u8; 4];
            let error = stdin().read(&mut buf);
            assert!(error.is_err_and(|e| is_cancelled_error(&e)));
            0
        });
        assert_eq!(code, 130);
        let _ = std::fs::remove_file(p);
    }

    /// A file a utility opens through `open` is read relative to the context's directory, and its
    /// reads end at a cancel even though none is blocked.
    #[test]
    fn opened_files_resolve_and_stop_at_a_cancel() {
        let (mut f, p) = temp("file");
        f.write_all(&vec![b'z'; 100_000]).unwrap();
        drop(f);
        let (log, lp) = temp("filelog");
        let mut inv = invocation("wc", [&log, &log, &log], &[]);
        inv.cwd = p.parent().unwrap().to_owned();
        let token = std::sync::Arc::clone(&inv.cancel);
        let name = p.file_name().unwrap().to_owned();
        let code = run(inv, || {
            let mut file = open(&name).unwrap();
            let mut buf = [0u8; 4096];
            assert_eq!(file.read(&mut buf).unwrap(), 4096);
            token.cancel();
            assert!(file.read(&mut buf).is_err_and(|e| is_cancelled_error(&e)));
            0
        });
        assert_eq!(code, 130);
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_file(lp);
    }

    /// Cancelling while output is still buffered must neither unwind nor abort: the buffered
    /// writer's drop flushes into a cancelled stream, which fails quietly.
    #[test]
    fn cancel_with_buffered_output_does_not_unwind() {
        let (f, p) = temp("d");
        let inv = invocation("cat", [&f, &f, &f], &[]);
        let token = std::sync::Arc::clone(&inv.cancel);
        let code = run(inv, || {
            let mut writer = std::io::BufWriter::with_capacity(32 * 1024, stdout().lock());
            writer.write_all(b"pending").unwrap();
            token.cancel();
            // A read after the cancel fails; the writer is dropped by the error path.
            let mut buf = [0u8; 1];
            assert!(stdin().read(&mut buf).is_err());
            drop(writer);
            assert!(!std::thread::panicking());
            0
        });
        assert_eq!(code, 130);
        let _ = std::fs::remove_file(p);
    }

    /// An invocation moved to a thread of the test.
    struct Raw(Invocation);
    // SAFETY: the handles stay open until the end of the test (reader, writer, log).
    unsafe impl Send for Raw {}

    fn unfinished_within(timeout: Duration, f: impl FnOnce() -> i32 + Send + 'static) -> i32 {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(timeout).expect("the run did not end in time")
    }

    /// A utility blocked in a write to a pipe nobody reads, and one blocked in a read from a pipe
    /// nobody writes, end at a cancel (the host cancels again every 10 ms until they do).
    #[test]
    fn cancel_unblocks_pipe_reads_and_writes() {
        for writing in [true, false] {
            let (reader, writer) = std::io::pipe().unwrap();
            let (log, p) = temp(if writing { "w" } else { "r" });
            let raw = if writing {
                [log.as_raw_handle(), writer.as_raw_handle(), log.as_raw_handle()]
            } else {
                [reader.as_raw_handle(), log.as_raw_handle(), log.as_raw_handle()]
            };
            let inv = invocation_raw("cat", raw, &[]);
            let token = std::sync::Arc::clone(&inv.cancel);
            let started = Instant::now();
            let done = std::sync::Arc::new(AtomicBool::new(false));
            let canceller = {
                let done = std::sync::Arc::clone(&done);
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(100));
                    while !done.load(Ordering::Acquire) && started.elapsed() < Duration::from_secs(5) {
                        token.cancel();
                        std::thread::sleep(Duration::from_millis(10));
                    }
                })
            };
            let inv = Raw(inv);
            let code = unfinished_within(Duration::from_secs(10), move || {
                let inv = inv;
                run(inv.0, || {
                    // Buffered like cat -n's writer, so the drop path is exercised too.
                    let mut out = std::io::BufWriter::with_capacity(32 * 1024, stdout().lock());
                    let chunk = vec![b'x'; 64 * 1024];
                    if writing {
                        loop {
                            if out.write_all(&chunk).is_err() {
                                break;
                            }
                        }
                    } else {
                        let mut buf = [0u8; 16];
                        let _ = stdin().read(&mut buf);
                    }
                    0
                })
            });
            done.store(true, Ordering::Release);
            canceller.join().unwrap();
            assert_eq!(code, 130, "writing: {writing}");
            drop((reader, writer, log));
            let _ = std::fs::remove_file(p);
        }
    }

    /// A panic after a write to a closed pipe failed is the SIGPIPE death (141, quietly); any
    /// other panic is 101.
    #[test]
    fn broken_pipe_panic_is_141_and_other_panics_101() {
        let (reader, writer) = std::io::pipe().unwrap();
        let (log, p) = temp("e");
        drop(reader);
        let inv = invocation_raw(
            "wc",
            [log.as_raw_handle(), writer.as_raw_handle(), log.as_raw_handle()],
            &[],
        );
        let code = run(inv, || {
            let chunk = vec![b'x'; 8 * 1024];
            let mut failed = false;
            for _ in 0..4 {
                failed |= stdout().write_all(&chunk).and_then(|()| stdout().flush()).is_err();
            }
            assert!(failed);
            let result: Result<(), std::io::Error> = Err(std::io::Error::other("later"));
            std::hint::black_box(result).unwrap();
            0
        });
        assert_eq!(code, 141);
        let inv = invocation("wc", [&log, &log, &log], &[]);
        assert_eq!(run(inv, || panic!("not a pipe")), 101);
        drop((writer, log));
        let _ = std::fs::remove_file(p);
    }

    /// A diagnostic printed while stdout's reader is gone is dropped and the run ends 141, with no
    /// unwind (what `print_diagnostic` did with `exit` before).
    #[test]
    fn diagnostic_with_a_closed_stdout_ends_141_without_unwinding() {
        let (reader, writer) = std::io::pipe().unwrap();
        let (log, p) = temp("f");
        drop(reader);
        let inv = invocation_raw(
            "wc",
            [log.as_raw_handle(), writer.as_raw_handle(), log.as_raw_handle()],
            &[],
        );
        let code = run(inv, || {
            crate::error::print_diagnostic("", format_args!("a diagnostic"));
            assert!(!std::thread::panicking());
            set_exit_code(1);
            1
        });
        assert_eq!(code, 141);
        drop((writer, log));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "");
        let _ = std::fs::remove_file(p);
        // With a reader that is there, the diagnostic is printed.
        let (log, p) = temp("g");
        let (reader, writer) = std::io::pipe().unwrap();
        let inv = invocation_raw(
            "wc",
            [log.as_raw_handle(), writer.as_raw_handle(), log.as_raw_handle()],
            &[],
        );
        let code = run(inv, || {
            crate::error::print_diagnostic("", format_args!("shown"));
            0
        });
        assert_eq!(code, 0);
        drop((reader, writer, log));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "wc: shown\n");
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn closed_pipe_probe_is_exact_and_repeatable() {
        let (reader, writer) = std::io::pipe().unwrap();
        for _ in 0..1000 {
            assert!(!closed_pipe(writer.as_raw_handle()));
        }
        drop(reader);
        for _ in 0..1000 {
            assert!(closed_pipe(writer.as_raw_handle()));
        }
    }

    #[test]
    fn rooted_paths_take_the_drive_or_share_of_the_context_directory() {
        let (f, p) = temp("h");
        for (cwd, arg, want) in [
            (r"D:\work\sub", r"\file.txt", r"D:\file.txt"),
            (r"D:\work\sub", "/file.txt", r"D:\file.txt"),
            (r"D:\work\sub", r"rel\x", r"D:\work\sub\rel\x"),
            (r"D:\work\sub", r"C:\abs\x", r"C:\abs\x"),
            (r"D:\work\sub", r"\\srv\share\x", r"\\srv\share\x"),
            (r"\\srv\share\dir", r"\file.txt", r"\\srv\share\file.txt"),
            (r"\\srv\share\dir", r"rel", r"\\srv\share\dir\rel"),
            (r"\\?\D:\work", r"\file.txt", r"\\?\D:\file.txt"),
        ] {
            let mut inv = invocation("cat", [&f, &f, &f], &[]);
            inv.cwd = PathBuf::from(cwd);
            let out = std::sync::Mutex::new(PathBuf::new());
            run(inv, || {
                *out.lock().unwrap() = resolve(arg).into_owned();
                0
            });
            assert_eq!(*out.lock().unwrap(), Path::new(want), "{cwd} + {arg}");
        }
        // Outside a context nothing changes.
        assert_eq!(resolve(r"\file.txt").as_ref(), Path::new(r"\file.txt"));
        drop(f);
        let _ = std::fs::remove_file(p);
    }
}
