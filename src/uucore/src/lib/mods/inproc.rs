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
//     "unwind"` in the host);
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

    /// A raw handle used as a stream; never closed here.
    #[derive(Clone, Copy)]
    struct RawFile(isize);

    impl RawFile {
        fn file(self) -> ManuallyDrop<File> {
            // SAFETY: the host keeps the handle open for the whole run; `ManuallyDrop` keeps this
            // wrapper from closing it.
            ManuallyDrop::new(unsafe { File::from_raw_handle(self.0 as RawHandle) })
        }
        const fn raw(self) -> RawHandle {
            self.0 as RawHandle
        }
    }

    impl Read for RawFile {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            stop_if_cancelled();
            let result = (&*self.file()).read(buf);
            stop_if_cancelled();
            result
        }
    }

    impl Write for RawFile {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            stop_if_cancelled();
            let result = (&*self.file()).write(buf);
            stop_if_cancelled();
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

    fn stop_if_cancelled() {
        let cancelled = CANCEL.with(|c| {
            c.borrow()
                .as_ref()
                .is_some_and(|token| token.is_cancelled())
        });
        if cancelled {
            exit(130);
        }
    }

    /// `std::process::exit`, or, inside a host, an unwind carrying [`ToolExit`] to [`run`].
    pub fn exit(code: i32) -> ! {
        if active() {
            std::panic::resume_unwind(Box::new(ToolExit(code)));
        }
        std::process::exit(code)
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
        STDIN.with(|s| s.set(Some(RawFile(stdin as isize))));
        STDOUT.with(|s| *s.borrow_mut() = Some(LineWriter::new(RawFile(stdout as isize))));
        STDERR.with(|s| s.set(Some(RawFile(stderr as isize))));
        ENV.with(|e| *e.borrow_mut() = env);
        CWD.with(|c| *c.borrow_mut() = cwd);
        cancel.attach();
        CANCEL.with(|c| *c.borrow_mut() = Some(Arc::clone(&cancel)));

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        // What the utility left in the line buffer goes out first (errors: the reader may be
        // gone). A cancelled run drops it instead: the reader may never read again and the
        // thread must end now (the buffer, at most 1 KiB, is leaked).
        let out = STDOUT.with(|s| s.borrow_mut().take());
        CANCEL.with(|c| *c.borrow_mut() = None);
        if let Some(mut out) = out {
            if cancel.is_cancelled() {
                std::mem::forget(out);
            } else {
                let _ = out.flush();
            }
        }
        cancel.detach();
        ACTIVE.with(|a| a.set(false));
        NAME.with(|n| n.set(None));
        STDIN.with(|s| s.set(None));
        STDERR.with(|s| s.set(None));
        ENV.with(|e| *e.borrow_mut() = Vec::new());
        CWD.with(|c| *c.borrow_mut() = PathBuf::new());

        match result {
            Ok(code) => code,
            Err(payload) => {
                if let Some(exit) = payload.downcast_ref::<ToolExit>() {
                    exit.0
                } else if is_broken_pipe_payload(payload.as_ref()) {
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

    fn is_broken_pipe_payload(payload: &(dyn std::any::Any + Send)) -> bool {
        payload_text(payload)
            .is_some_and(|text| text.contains("BrokenPipe") || text.contains("Broken pipe"))
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
                if payload_text(info.payload())
                    .is_some_and(|t| t.contains("BrokenPipe") || t.contains("Broken pipe"))
                {
                    return; // `run` turns this into 141, quietly
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

    /// The path to hand to `std::fs`: a relative path is joined to the context's directory (the
    /// process has one directory for all threads; the shell's is per command). Absolute paths,
    /// rooted paths and `C:`-prefixed paths are returned as they are.
    pub fn resolve(path: &(impl AsRef<Path> + ?Sized)) -> Cow<'_, Path> {
        let path = path.as_ref();
        if !active()
            || path.has_root()
            || matches!(path.components().next(), Some(Component::Prefix(_)))
        {
            return Cow::Borrowed(path);
        }
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
    /// `OVERLAPPED` block so that it is also valid on an overlapped pipe handle; the block and
    /// the byte count are leaked (32 bytes, only on the error path) so that a write that
    /// completes later never touches freed memory.
    pub fn closed_pipe(handle: RawHandle) -> bool {
        const ERROR_BROKEN_PIPE: i32 = 109;
        const ERROR_NO_DATA: i32 = 232;
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
        let overlapped: &mut [u64; 4] = Box::leak(Box::new([0; 4]));
        let written: &mut u32 = Box::leak(Box::new(0));
        // SAFETY: a zero-length write sends no data; `overlapped` and `written` are valid,
        // zero-initialised and never freed.
        let ok = unsafe {
            WriteFile(
                handle.cast(),
                std::ptr::null(),
                0,
                written,
                overlapped.as_mut_ptr().cast(),
            )
        };
        ok == 0
            && matches!(
                io::Error::last_os_error().raw_os_error(),
                Some(ERROR_BROKEN_PIPE | ERROR_NO_DATA)
            )
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
            write!(f, "RawFile({:#x})", self.0)
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
                    Err(_) => StdinLock::Ctx(BufReader::new(RawFileReader(RawFile(0)))),
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
                    !raw.is_null() && RawFile(raw as isize).file().is_terminal()
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
    use std::io::Write as _;
    use std::os::windows::io::AsRawHandle as _;
    use std::path::{Path, PathBuf};

    fn temp(name: &str) -> (File, PathBuf) {
        let path =
            std::env::temp_dir().join(format!("uucore-inproc-{}-{name}", std::process::id()));
        (File::create(&path).unwrap(), path)
    }

    fn invocation(util: &'static str, files: [&File; 3], env: &[(&str, &str)]) -> Invocation {
        Invocation {
            util_name: util,
            stdin: files[0].as_raw_handle(),
            stdout: files[1].as_raw_handle(),
            stderr: files[2].as_raw_handle(),
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

    #[test]
    fn two_utilities_run_at_once() {
        let handles: Vec<_> = ["one", "two"]
            .into_iter()
            .map(|n| {
                std::thread::spawn(move || {
                    let (f, p) = temp(n);
                    let util: &'static str = if n == "one" { "cat" } else { "wc" };
                    let inv = invocation(util, [&f, &f, &f], &[]);
                    let code = run(inv, || {
                        for _ in 0..1000 {
                            writeln!(stdout(), "{}", crate::util_name()).unwrap();
                        }
                        set_exit_code(if n == "one" { 3 } else { 4 });
                        crate::error::get_exit_code()
                    });
                    drop(f);
                    let text = std::fs::read_to_string(&p).unwrap();
                    let _ = std::fs::remove_file(p);
                    (code, text, util)
                })
            })
            .collect();
        for h in handles {
            let (code, text, util) = h.join().unwrap();
            assert!(code == 3 || code == 4);
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
            let _ = writeln!(stdout(), "never");
            0
        });
        assert_eq!(code, 130);
        let _ = std::fs::remove_file(p);
    }
}
