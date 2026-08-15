use std::{collections::VecDeque, io, time::Duration};

use super::TerminalSink;

pub const ENTER_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049h";
pub const LEAVE_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049l";
pub const ENABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004h";
pub const DISABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004l";
pub const KITTY_KEYBOARD_QUERY: &[u8] = b"\x1b[>7u\x1b[?u\x1b[c";
pub const POP_KITTY_KEYBOARD: &[u8] = b"\x1b[<u";
pub const ENABLE_MODIFY_OTHER_KEYS: &[u8] = b"\x1b[>4;2m";
pub const DISABLE_MODIFY_OTHER_KEYS: &[u8] = b"\x1b[>4;0m";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub columns: u16,
    pub rows: u16,
}

impl TerminalSize {
    pub const fn new(columns: u16, rows: u16) -> Self {
        Self { columns, rows }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalSignal {
    Interrupt,
    Terminate,
    Hangup,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendEvent {
    Input(Vec<u8>),
    Resize,
    Signal(TerminalSignal),
    EndOfInput,
    Timeout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardProtocol {
    Negotiating,
    Kitty { flags: u16 },
    ModifyOtherKeys,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    Input(Vec<u8>),
    Resize(TerminalSize),
    KeyboardProtocolChanged(KeyboardProtocol),
    Signal(TerminalSignal),
    EndOfInput,
    Timeout,
}

/// Injectable ownership boundary for process terminal effects.
///
/// Implementations capture and restore their own raw-mode representation and
/// own signal registration. [`TerminalSession`] supplies protocol sequencing,
/// cleanup ordering, and the RAII guarantee independently of the OS backend.
pub trait TerminalBackend {
    type SavedMode;

    fn capture_mode(&mut self) -> io::Result<Self::SavedMode>;
    fn enable_raw_mode(&mut self, saved: &Self::SavedMode) -> io::Result<()>;
    fn restore_mode(&mut self, saved: &Self::SavedMode) -> io::Result<()>;
    fn install_signal_handlers(&mut self) -> io::Result<()>;
    fn uninstall_signal_handlers(&mut self) -> io::Result<()>;
    fn write_output(&mut self, bytes: &[u8]) -> io::Result<()>;
    fn flush_output(&mut self) -> io::Result<()>;
    fn terminal_size(&mut self) -> io::Result<TerminalSize>;
    fn next_event(&mut self, timeout: Option<Duration>) -> io::Result<BackendEvent>;
}

#[derive(Debug)]
pub struct TerminalDriver<B> {
    backend: B,
}

impl<B> TerminalDriver<B> {
    pub const fn new(backend: B) -> Self {
        Self { backend }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    pub fn into_backend(self) -> B {
        self.backend
    }
}

impl<B: TerminalBackend> TerminalDriver<B> {
    pub fn start(&mut self) -> io::Result<TerminalSession<'_, B>> {
        let saved_mode = self.backend.capture_mode()?;
        if let Err(error) = self.backend.enable_raw_mode(&saved_mode) {
            let _ = self.backend.restore_mode(&saved_mode);
            return Err(error);
        }

        let mut session = TerminalSession {
            backend: &mut self.backend,
            saved_mode: Some(saved_mode),
            signals_installed: false,
            alternate_screen: false,
            bracketed_paste: false,
            keyboard_pushed: false,
            modify_other_keys: false,
            protocol: KeyboardProtocol::Negotiating,
            negotiation: KeyboardNegotiator::default(),
            pending: VecDeque::new(),
            active: true,
        };

        if let Err(error) = session.activate() {
            let _ = session.restore();
            return Err(error);
        }
        Ok(session)
    }
}

/// Active terminal ownership guard.
///
/// Dropping the guard always attempts every restoration step. Call [`finish`](Self::finish)
/// when cleanup errors need to be reported. Termination-signal events restore
/// the terminal before they are returned to the caller.
pub struct TerminalSession<'a, B: TerminalBackend> {
    backend: &'a mut B,
    saved_mode: Option<B::SavedMode>,
    signals_installed: bool,
    alternate_screen: bool,
    bracketed_paste: bool,
    keyboard_pushed: bool,
    modify_other_keys: bool,
    protocol: KeyboardProtocol,
    negotiation: KeyboardNegotiator,
    pending: VecDeque<TerminalEvent>,
    active: bool,
}

impl<B: TerminalBackend> TerminalSession<'_, B> {
    fn activate(&mut self) -> io::Result<()> {
        self.backend.install_signal_handlers()?;
        self.signals_installed = true;

        self.alternate_screen = true;
        self.backend.write_output(ENTER_ALTERNATE_SCREEN)?;
        self.bracketed_paste = true;
        self.backend.write_output(ENABLE_BRACKETED_PASTE)?;
        self.keyboard_pushed = true;
        self.backend.write_output(KITTY_KEYBOARD_QUERY)?;
        self.backend.flush_output()
    }

    pub fn keyboard_protocol(&self) -> KeyboardProtocol {
        self.protocol
    }

    pub fn size(&mut self) -> io::Result<TerminalSize> {
        self.backend.terminal_size()
    }

    /// Flush bytes written through the terminal sink without exposing the
    /// backend or weakening the session's restoration ownership.
    pub fn flush_output(&mut self) -> io::Result<()> {
        self.backend.flush_output()
    }

    pub fn read_event(&mut self, timeout: Option<Duration>) -> io::Result<TerminalEvent> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(event);
        }
        if !self.active {
            return Ok(TerminalEvent::EndOfInput);
        }

        loop {
            match self.backend.next_event(timeout)? {
                BackendEvent::Input(bytes) => {
                    let items = self.negotiation.push(&bytes);
                    self.queue_negotiation_items(items)?;
                    if let Some(event) = self.pending.pop_front() {
                        return Ok(event);
                    }
                }
                BackendEvent::Resize => {
                    return self.backend.terminal_size().map(TerminalEvent::Resize);
                }
                BackendEvent::Signal(signal) => {
                    // Restore while the signal's intent is still explicit. The
                    // outer process loop can then map/re-raise it safely.
                    let _ = self.restore();
                    return Ok(TerminalEvent::Signal(signal));
                }
                BackendEvent::EndOfInput => return Ok(TerminalEvent::EndOfInput),
                BackendEvent::Timeout => {
                    if let Some(bytes) = self.negotiation.flush() {
                        return Ok(TerminalEvent::Input(bytes));
                    }
                    return Ok(TerminalEvent::Timeout);
                }
            }
        }
    }

    pub fn finish(mut self) -> io::Result<()> {
        self.restore()
    }

    fn queue_negotiation_items(&mut self, items: Vec<NegotiationItem>) -> io::Result<()> {
        for item in items {
            match item {
                NegotiationItem::Input(bytes) => {
                    self.pending.push_back(TerminalEvent::Input(bytes));
                }
                NegotiationItem::KittyFlags(flags) if flags != 0 => {
                    if self.modify_other_keys {
                        self.backend.write_output(DISABLE_MODIFY_OTHER_KEYS)?;
                        self.backend.flush_output()?;
                        self.modify_other_keys = false;
                    }
                    let protocol = KeyboardProtocol::Kitty { flags };
                    if self.protocol != protocol {
                        self.protocol = protocol;
                        self.pending
                            .push_back(TerminalEvent::KeyboardProtocolChanged(protocol));
                    }
                }
                NegotiationItem::KittyFlags(_) | NegotiationItem::DeviceAttributes => {
                    if matches!(self.protocol, KeyboardProtocol::Kitty { .. }) {
                        continue;
                    }
                    if !self.modify_other_keys {
                        // Mark before writing so a partial write is still
                        // reversed by Drop/error cleanup.
                        self.modify_other_keys = true;
                        self.backend.write_output(ENABLE_MODIFY_OTHER_KEYS)?;
                        self.backend.flush_output()?;
                    }
                    if self.protocol != KeyboardProtocol::ModifyOtherKeys {
                        self.protocol = KeyboardProtocol::ModifyOtherKeys;
                        self.pending
                            .push_back(TerminalEvent::KeyboardProtocolChanged(
                                KeyboardProtocol::ModifyOtherKeys,
                            ));
                    }
                }
            }
        }
        Ok(())
    }

    fn restore(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let mut first_error = None;

        if self.keyboard_pushed {
            record_cleanup(
                &mut first_error,
                self.backend.write_output(POP_KITTY_KEYBOARD),
            );
            self.keyboard_pushed = false;
        }
        if self.modify_other_keys {
            record_cleanup(
                &mut first_error,
                self.backend.write_output(DISABLE_MODIFY_OTHER_KEYS),
            );
            self.modify_other_keys = false;
        }
        if self.bracketed_paste {
            record_cleanup(
                &mut first_error,
                self.backend.write_output(DISABLE_BRACKETED_PASTE),
            );
            self.bracketed_paste = false;
        }
        if self.alternate_screen {
            record_cleanup(
                &mut first_error,
                self.backend.write_output(LEAVE_ALTERNATE_SCREEN),
            );
            self.alternate_screen = false;
        }
        record_cleanup(&mut first_error, self.backend.flush_output());
        if self.signals_installed {
            record_cleanup(&mut first_error, self.backend.uninstall_signal_handlers());
            self.signals_installed = false;
        }
        if let Some(saved_mode) = self.saved_mode.take() {
            record_cleanup(&mut first_error, self.backend.restore_mode(&saved_mode));
        }

        first_error.map_or(Ok(()), Err)
    }
}

impl<B: TerminalBackend> TerminalSink for TerminalSession<'_, B> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.backend.write_output(bytes)
    }
}

impl<B: TerminalBackend> Drop for TerminalSession<'_, B> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn record_cleanup(first_error: &mut Option<io::Error>, result: io::Result<()>) {
    if let Err(error) = result {
        if first_error.is_none() {
            *first_error = Some(error);
        }
    }
}

#[derive(Debug, Default)]
struct KeyboardNegotiator {
    pending: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NegotiationItem {
    Input(Vec<u8>),
    KittyFlags(u16),
    DeviceAttributes,
}

impl KeyboardNegotiator {
    fn push(&mut self, bytes: &[u8]) -> Vec<NegotiationItem> {
        self.pending.extend_from_slice(bytes);
        let mut items = Vec::new();
        let mut input = Vec::new();
        let mut index = 0usize;

        while index < self.pending.len() {
            if self.pending[index] != 0x1b {
                input.push(self.pending[index]);
                index += 1;
                continue;
            }
            if index + 1 >= self.pending.len() {
                break;
            }
            if self.pending[index + 1] != b'[' {
                input.push(self.pending[index]);
                index += 1;
                continue;
            }
            let Some(end) = self.pending[index + 2..]
                .iter()
                .position(|byte| (0x40..=0x7e).contains(byte))
                .map(|offset| index + offset + 2)
            else {
                break;
            };
            let sequence = &self.pending[index..=end];
            if let Some(item) = parse_negotiation_sequence(sequence) {
                if !input.is_empty() {
                    items.push(NegotiationItem::Input(std::mem::take(&mut input)));
                }
                items.push(item);
            } else {
                input.extend_from_slice(sequence);
            }
            index = end + 1;
        }

        if !input.is_empty() {
            items.push(NegotiationItem::Input(input));
        }
        self.pending.drain(..index);
        items
    }

    fn flush(&mut self) -> Option<Vec<u8>> {
        (!self.pending.is_empty()).then(|| std::mem::take(&mut self.pending))
    }
}

fn parse_negotiation_sequence(sequence: &[u8]) -> Option<NegotiationItem> {
    if sequence.len() < 4 || !sequence.starts_with(b"\x1b[?") {
        return None;
    }
    let final_byte = *sequence.last()?;
    let parameters = std::str::from_utf8(&sequence[3..sequence.len() - 1]).ok()?;
    match final_byte {
        b'u' if !parameters.is_empty() && parameters.bytes().all(|byte| byte.is_ascii_digit()) => {
            parameters.parse().ok().map(NegotiationItem::KittyFlags)
        }
        b'c' if parameters
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b';') =>
        {
            Some(NegotiationItem::DeviceAttributes)
        }
        _ => None,
    }
}

#[cfg(unix)]
mod process_backend {
    use std::{
        collections::VecDeque,
        io::{self, Write},
        mem::MaybeUninit,
        os::fd::RawFd,
        sync::atomic::{AtomicI32, Ordering},
        time::Duration,
    };

    use super::{BackendEvent, TerminalBackend, TerminalSignal, TerminalSize};

    const HANDLED_SIGNALS: [libc::c_int; 5] = [
        libc::SIGWINCH,
        libc::SIGINT,
        libc::SIGTERM,
        libc::SIGHUP,
        libc::SIGQUIT,
    ];
    static SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

    extern "C" fn forward_signal(signal: libc::c_int) {
        let fd = SIGNAL_WRITE_FD.load(Ordering::Relaxed);
        if fd >= 0 {
            let byte = signal as u8;
            // SAFETY: write is async-signal-safe, fd is process-global while
            // handlers are installed, and the one-byte pointer is valid.
            unsafe {
                libc::write(fd, (&byte as *const u8).cast(), 1);
            }
        }
    }

    struct SignalRegistration {
        read_fd: RawFd,
        write_fd: RawFd,
        previous: Vec<(libc::c_int, libc::sigaction)>,
    }

    impl SignalRegistration {
        fn install() -> io::Result<Self> {
            let mut fds = [0; 2];
            // SAFETY: fds points to two valid c_int slots.
            if unsafe { libc::pipe(fds.as_mut_ptr()) } == -1 {
                return Err(io::Error::last_os_error());
            }
            if let Err(error) = set_fd_flags(fds[0]).and_then(|()| set_fd_flags(fds[1])) {
                close_fd(fds[0]);
                close_fd(fds[1]);
                return Err(error);
            }
            if SIGNAL_WRITE_FD
                .compare_exchange(-1, fds[1], Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                close_fd(fds[0]);
                close_fd(fds[1]);
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "another terminal signal registration is active",
                ));
            }

            let mut registration = Self {
                read_fd: fds[0],
                write_fd: fds[1],
                previous: Vec::with_capacity(HANDLED_SIGNALS.len()),
            };
            for signal in HANDLED_SIGNALS {
                if let Err(error) = registration.install_one(signal) {
                    registration.restore_handlers();
                    return Err(error);
                }
            }
            Ok(registration)
        }

        fn install_one(&mut self, signal: libc::c_int) -> io::Result<()> {
            // SAFETY: zero is a valid baseline for sigaction before its mask
            // and handler fields are initialized below.
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = forward_signal as *const () as usize;
            action.sa_flags = libc::SA_RESTART;
            // SAFETY: action owns a valid signal mask.
            if unsafe { libc::sigemptyset(&mut action.sa_mask) } == -1 {
                return Err(io::Error::last_os_error());
            }
            let mut previous = MaybeUninit::<libc::sigaction>::uninit();
            // SAFETY: action and previous are valid sigaction pointers.
            if unsafe { libc::sigaction(signal, &action, previous.as_mut_ptr()) } == -1 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful sigaction initialized previous.
            self.previous
                .push((signal, unsafe { previous.assume_init() }));
            Ok(())
        }

        fn restore_handlers(&mut self) {
            for (signal, action) in self.previous.drain(..).rev() {
                // SAFETY: action was returned by sigaction for this signal.
                unsafe {
                    libc::sigaction(signal, &action, std::ptr::null_mut());
                }
            }
            let _ = SIGNAL_WRITE_FD.compare_exchange(
                self.write_fd,
                -1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
        }
    }

    impl Drop for SignalRegistration {
        fn drop(&mut self) {
            self.restore_handlers();
            close_fd(self.read_fd);
            close_fd(self.write_fd);
        }
    }

    pub struct ProcessTerminalBackend {
        registration: Option<SignalRegistration>,
        pending_signals: VecDeque<u8>,
    }

    impl ProcessTerminalBackend {
        pub fn new() -> Self {
            Self {
                registration: None,
                pending_signals: VecDeque::new(),
            }
        }

        fn signal_event(&mut self) -> io::Result<Option<BackendEvent>> {
            if let Some(signal) = self.pending_signals.pop_front() {
                return self.map_signal(signal).map(Some);
            }
            let Some(registration) = self.registration.as_ref() else {
                return Ok(None);
            };
            let mut bytes = [0u8; 64];
            // SAFETY: bytes is writable for its length and read_fd is open.
            let count =
                unsafe { libc::read(registration.read_fd, bytes.as_mut_ptr().cast(), bytes.len()) };
            if count == -1 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Ok(None);
                }
                return Err(error);
            }
            self.pending_signals.extend(&bytes[..count as usize]);
            self.pending_signals
                .pop_front()
                .map(|signal| self.map_signal(signal))
                .transpose()
        }

        fn map_signal(&mut self, signal: u8) -> io::Result<BackendEvent> {
            match libc::c_int::from(signal) {
                libc::SIGWINCH => Ok(BackendEvent::Resize),
                libc::SIGINT => Ok(BackendEvent::Signal(TerminalSignal::Interrupt)),
                libc::SIGTERM => Ok(BackendEvent::Signal(TerminalSignal::Terminate)),
                libc::SIGHUP => Ok(BackendEvent::Signal(TerminalSignal::Hangup)),
                libc::SIGQUIT => Ok(BackendEvent::Signal(TerminalSignal::Quit)),
                signal => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unexpected terminal signal {signal}"),
                )),
            }
        }
    }

    impl Default for ProcessTerminalBackend {
        fn default() -> Self {
            Self::new()
        }
    }

    impl TerminalBackend for ProcessTerminalBackend {
        type SavedMode = libc::termios;

        fn capture_mode(&mut self) -> io::Result<Self::SavedMode> {
            if !is_tty(libc::STDIN_FILENO) || !is_tty(libc::STDOUT_FILENO) {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "interactive terminal requires TTY stdin and stdout",
                ));
            }
            let mut mode = MaybeUninit::<libc::termios>::uninit();
            // SAFETY: mode points to writable termios storage.
            if unsafe { libc::tcgetattr(libc::STDIN_FILENO, mode.as_mut_ptr()) } == -1 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful tcgetattr initialized mode.
            Ok(unsafe { mode.assume_init() })
        }

        fn enable_raw_mode(&mut self, saved: &Self::SavedMode) -> io::Result<()> {
            let mut raw = *saved;
            // SAFETY: raw is a valid termios value.
            unsafe {
                libc::cfmakeraw(&mut raw);
            }
            raw.c_cc[libc::VMIN] = 1;
            raw.c_cc[libc::VTIME] = 0;
            set_mode(&raw)
        }

        fn restore_mode(&mut self, saved: &Self::SavedMode) -> io::Result<()> {
            set_mode(saved)
        }

        fn install_signal_handlers(&mut self) -> io::Result<()> {
            if self.registration.is_some() {
                return Ok(());
            }
            self.registration = Some(SignalRegistration::install()?);
            Ok(())
        }

        fn uninstall_signal_handlers(&mut self) -> io::Result<()> {
            self.registration.take();
            self.pending_signals.clear();
            Ok(())
        }

        fn write_output(&mut self, bytes: &[u8]) -> io::Result<()> {
            io::stdout().lock().write_all(bytes)
        }

        fn flush_output(&mut self) -> io::Result<()> {
            io::stdout().lock().flush()
        }

        fn terminal_size(&mut self) -> io::Result<TerminalSize> {
            let mut size = MaybeUninit::<libc::winsize>::zeroed();
            // SAFETY: size points to writable winsize storage.
            if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, size.as_mut_ptr()) }
                == -1
            {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: successful ioctl initialized size.
            let size = unsafe { size.assume_init() };
            if size.ws_col == 0 || size.ws_row == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "terminal reported zero-sized viewport",
                ));
            }
            Ok(TerminalSize::new(size.ws_col, size.ws_row))
        }

        fn next_event(&mut self, timeout: Option<Duration>) -> io::Result<BackendEvent> {
            if let Some(event) = self.signal_event()? {
                return Ok(event);
            }
            let signal_fd = self.registration.as_ref().map(|value| value.read_fd);
            let mut descriptors = [
                libc::pollfd {
                    fd: libc::STDIN_FILENO,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: signal_fd.unwrap_or(-1),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let timeout_ms = timeout.map_or(-1, duration_to_poll_timeout);
            loop {
                // SAFETY: descriptors points to two initialized pollfd values.
                let result = unsafe {
                    libc::poll(
                        descriptors.as_mut_ptr(),
                        descriptors.len() as libc::nfds_t,
                        timeout_ms,
                    )
                };
                if result == -1 {
                    let error = io::Error::last_os_error();
                    if error.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error);
                }
                if result == 0 {
                    return Ok(BackendEvent::Timeout);
                }
                if descriptors[1].revents & libc::POLLIN != 0 {
                    if let Some(event) = self.signal_event()? {
                        return Ok(event);
                    }
                }
                if descriptors[0].revents & libc::POLLIN != 0 {
                    let mut bytes = [0u8; 8192];
                    // SAFETY: bytes is writable and stdin is a valid fd.
                    let count = unsafe {
                        libc::read(libc::STDIN_FILENO, bytes.as_mut_ptr().cast(), bytes.len())
                    };
                    if count == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    if count == 0 {
                        return Ok(BackendEvent::EndOfInput);
                    }
                    return Ok(BackendEvent::Input(bytes[..count as usize].to_vec()));
                }
                if descriptors[0].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                    return Ok(BackendEvent::EndOfInput);
                }
            }
        }
    }

    fn is_tty(fd: RawFd) -> bool {
        // SAFETY: isatty accepts any file descriptor and has no memory effects.
        unsafe { libc::isatty(fd) == 1 }
    }

    fn set_mode(mode: &libc::termios) -> io::Result<()> {
        // SAFETY: mode is a valid termios pointer for stdin.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, mode) } == -1 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn duration_to_poll_timeout(duration: Duration) -> libc::c_int {
        duration
            .as_millis()
            .min(libc::c_int::MAX as u128)
            .try_into()
            .unwrap_or(libc::c_int::MAX)
    }

    fn set_fd_flags(fd: RawFd) -> io::Result<()> {
        // SAFETY: fcntl queries flags for a live pipe fd.
        let status = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if status == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fcntl sets status flags for a live pipe fd.
        if unsafe { libc::fcntl(fd, libc::F_SETFL, status | libc::O_NONBLOCK) } == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fcntl queries descriptor flags for a live pipe fd.
        let descriptor = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if descriptor == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fcntl sets descriptor flags for a live pipe fd.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, descriptor | libc::FD_CLOEXEC) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn close_fd(fd: RawFd) {
        // SAFETY: fd is owned by the caller and closing is idempotent at the
        // ownership level (this helper is called once per owned descriptor).
        unsafe {
            libc::close(fd);
        }
    }
}

#[cfg(unix)]
pub use process_backend::ProcessTerminalBackend;

#[cfg(not(unix))]
pub struct ProcessTerminalBackend;

#[cfg(not(unix))]
impl ProcessTerminalBackend {
    pub const fn new() -> Self {
        Self
    }
}

#[cfg(not(unix))]
impl Default for ProcessTerminalBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(not(unix))]
impl TerminalBackend for ProcessTerminalBackend {
    type SavedMode = ();

    fn capture_mode(&mut self) -> io::Result<Self::SavedMode> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the production terminal backend currently requires Unix",
        ))
    }

    fn enable_raw_mode(&mut self, _saved: &Self::SavedMode) -> io::Result<()> {
        Ok(())
    }

    fn restore_mode(&mut self, _saved: &Self::SavedMode) -> io::Result<()> {
        Ok(())
    }

    fn install_signal_handlers(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn uninstall_signal_handlers(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn write_output(&mut self, _bytes: &[u8]) -> io::Result<()> {
        Ok(())
    }

    fn flush_output(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn terminal_size(&mut self) -> io::Result<TerminalSize> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the production terminal backend currently requires Unix",
        ))
    }

    fn next_event(&mut self, _timeout: Option<Duration>) -> io::Result<BackendEvent> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the production terminal backend currently requires Unix",
        ))
    }
}
