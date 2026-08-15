use std::{
    collections::VecDeque,
    io,
    panic::{catch_unwind, AssertUnwindSafe},
    time::Duration,
};

use mycel_cli::terminal::{
    BackendEvent, KeyboardProtocol, TerminalBackend, TerminalDriver, TerminalEvent, TerminalSignal,
    TerminalSize, DISABLE_BRACKETED_PASTE, DISABLE_MODIFY_OTHER_KEYS, ENABLE_BRACKETED_PASTE,
    ENABLE_MODIFY_OTHER_KEYS, ENTER_ALTERNATE_SCREEN, KITTY_KEYBOARD_QUERY, LEAVE_ALTERNATE_SCREEN,
    POP_KITTY_KEYBOARD,
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Operation {
    CaptureMode,
    EnableRaw,
    RestoreMode,
    InstallSignals,
    UninstallSignals,
    Write(Vec<u8>),
    Flush,
    Size,
    Read,
}

#[derive(Debug)]
struct FakeBackend {
    operations: Vec<Operation>,
    events: VecDeque<io::Result<BackendEvent>>,
    size: TerminalSize,
    write_count: usize,
    fail_write_at: Option<usize>,
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self {
            operations: Vec::new(),
            events: VecDeque::new(),
            size: TerminalSize::new(120, 40),
            write_count: 0,
            fail_write_at: None,
        }
    }
}

impl FakeBackend {
    fn with_events(events: impl IntoIterator<Item = BackendEvent>) -> Self {
        Self {
            events: events.into_iter().map(Ok).collect(),
            ..Self::default()
        }
    }

    fn cleanup_tail() -> Vec<Operation> {
        vec![
            Operation::Write(POP_KITTY_KEYBOARD.to_vec()),
            Operation::Write(DISABLE_BRACKETED_PASTE.to_vec()),
            Operation::Write(LEAVE_ALTERNATE_SCREEN.to_vec()),
            Operation::Flush,
            Operation::UninstallSignals,
            Operation::RestoreMode,
        ]
    }
}

impl TerminalBackend for FakeBackend {
    type SavedMode = u8;

    fn capture_mode(&mut self) -> io::Result<Self::SavedMode> {
        self.operations.push(Operation::CaptureMode);
        Ok(42)
    }

    fn enable_raw_mode(&mut self, saved: &Self::SavedMode) -> io::Result<()> {
        assert_eq!(*saved, 42);
        self.operations.push(Operation::EnableRaw);
        Ok(())
    }

    fn restore_mode(&mut self, saved: &Self::SavedMode) -> io::Result<()> {
        assert_eq!(*saved, 42);
        self.operations.push(Operation::RestoreMode);
        Ok(())
    }

    fn install_signal_handlers(&mut self) -> io::Result<()> {
        self.operations.push(Operation::InstallSignals);
        Ok(())
    }

    fn uninstall_signal_handlers(&mut self) -> io::Result<()> {
        self.operations.push(Operation::UninstallSignals);
        Ok(())
    }

    fn write_output(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write_count += 1;
        self.operations.push(Operation::Write(bytes.to_vec()));
        if self.fail_write_at == Some(self.write_count) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "injected write failure",
            ));
        }
        Ok(())
    }

    fn flush_output(&mut self) -> io::Result<()> {
        self.operations.push(Operation::Flush);
        Ok(())
    }

    fn terminal_size(&mut self) -> io::Result<TerminalSize> {
        self.operations.push(Operation::Size);
        Ok(self.size)
    }

    fn next_event(&mut self, _timeout: Option<Duration>) -> io::Result<BackendEvent> {
        self.operations.push(Operation::Read);
        self.events.pop_front().unwrap_or(Ok(BackendEvent::Timeout))
    }
}

#[test]
fn normal_finish_enables_and_restores_every_terminal_capability_in_reverse_order() {
    let mut driver = TerminalDriver::new(FakeBackend::default());
    {
        let mut session = driver.start().expect("terminal starts");
        assert_eq!(session.keyboard_protocol(), KeyboardProtocol::Negotiating);
        assert_eq!(session.size().expect("size"), TerminalSize::new(120, 40));
        session.finish().expect("terminal restores");
    }

    assert_eq!(
        driver.backend().operations,
        vec![
            Operation::CaptureMode,
            Operation::EnableRaw,
            Operation::InstallSignals,
            Operation::Write(ENTER_ALTERNATE_SCREEN.to_vec()),
            Operation::Write(ENABLE_BRACKETED_PASTE.to_vec()),
            Operation::Write(KITTY_KEYBOARD_QUERY.to_vec()),
            Operation::Flush,
            Operation::Size,
            Operation::Write(POP_KITTY_KEYBOARD.to_vec()),
            Operation::Write(DISABLE_BRACKETED_PASTE.to_vec()),
            Operation::Write(LEAVE_ALTERNATE_SCREEN.to_vec()),
            Operation::Flush,
            Operation::UninstallSignals,
            Operation::RestoreMode,
        ]
    );
}

#[test]
fn protocol_negotiation_is_fragment_safe_and_upgrades_late_kitty_support() {
    let backend = FakeBackend::with_events([
        BackendEvent::Input(b"\x1b[?".to_vec()),
        BackendEvent::Input(b"1;2cabc".to_vec()),
        BackendEvent::Input(b"\x1b[?7u".to_vec()),
    ]);
    let mut driver = TerminalDriver::new(backend);
    {
        let mut session = driver.start().expect("terminal starts");
        assert_eq!(
            session.read_event(None).expect("fallback"),
            TerminalEvent::KeyboardProtocolChanged(KeyboardProtocol::ModifyOtherKeys)
        );
        assert_eq!(
            session.read_event(None).expect("preserved input"),
            TerminalEvent::Input(b"abc".to_vec())
        );
        assert_eq!(
            session.read_event(None).expect("late Kitty"),
            TerminalEvent::KeyboardProtocolChanged(KeyboardProtocol::Kitty { flags: 7 })
        );
    }

    let writes = driver
        .backend()
        .operations
        .iter()
        .filter_map(|operation| match operation {
            Operation::Write(bytes) => Some(bytes.as_slice()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(writes.contains(&ENABLE_MODIFY_OTHER_KEYS));
    assert!(writes.contains(&DISABLE_MODIFY_OTHER_KEYS));
    assert_eq!(
        writes
            .iter()
            .filter(|bytes| **bytes == DISABLE_MODIFY_OTHER_KEYS)
            .count(),
        1
    );
}

#[test]
fn resize_and_termination_signals_are_plumbed_and_signal_restores_before_return() {
    let backend = FakeBackend::with_events([
        BackendEvent::Resize,
        BackendEvent::Signal(TerminalSignal::Terminate),
    ]);
    let mut driver = TerminalDriver::new(backend);
    {
        let mut session = driver.start().expect("terminal starts");
        assert_eq!(
            session.read_event(None).expect("resize"),
            TerminalEvent::Resize(TerminalSize::new(120, 40))
        );
        assert_eq!(
            session.read_event(None).expect("signal"),
            TerminalEvent::Signal(TerminalSignal::Terminate)
        );
        assert_eq!(
            session.read_event(None).expect("closed after signal"),
            TerminalEvent::EndOfInput
        );
    }
    assert!(driver
        .backend()
        .operations
        .ends_with(&FakeBackend::cleanup_tail()));
}

#[test]
fn partial_start_failure_rolls_back_raw_mode_and_all_started_features() {
    let mut backend = FakeBackend {
        fail_write_at: Some(2),
        ..FakeBackend::default()
    };
    let mut driver = TerminalDriver::new(backend);
    let error = match driver.start() {
        Ok(_) => panic!("start should fail"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    backend = driver.into_backend();
    assert!(backend
        .operations
        .contains(&Operation::Write(DISABLE_BRACKETED_PASTE.to_vec())));
    assert!(backend
        .operations
        .contains(&Operation::Write(LEAVE_ALTERNATE_SCREEN.to_vec())));
    assert!(backend.operations.ends_with(&[
        Operation::Flush,
        Operation::UninstallSignals,
        Operation::RestoreMode,
    ]));
}

#[test]
fn cleanup_reports_first_failure_but_still_restores_signals_and_raw_mode() {
    let backend = FakeBackend {
        // The first cleanup write follows the three startup writes.
        fail_write_at: Some(4),
        ..FakeBackend::default()
    };
    let mut driver = TerminalDriver::new(backend);
    let error = driver
        .start()
        .expect("terminal starts")
        .finish()
        .expect_err("cleanup reports the injected failure");
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    assert!(driver.backend().operations.ends_with(&[
        Operation::Flush,
        Operation::UninstallSignals,
        Operation::RestoreMode,
    ]));
    assert!(driver
        .backend()
        .operations
        .contains(&Operation::Write(DISABLE_BRACKETED_PASTE.to_vec())));
    assert!(driver
        .backend()
        .operations
        .contains(&Operation::Write(LEAVE_ALTERNATE_SCREEN.to_vec())));
}

#[test]
fn read_error_plain_drop_and_panic_unwind_all_restore_the_terminal() {
    let scenarios = ["error", "drop", "panic"];
    for scenario in scenarios {
        let mut backend = FakeBackend::default();
        if scenario == "error" {
            backend.events.push_back(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "injected read failure",
            )));
        }
        let mut driver = TerminalDriver::new(backend);
        if scenario == "panic" {
            let unwind = catch_unwind(AssertUnwindSafe(|| {
                let _session = driver.start().expect("terminal starts");
                panic!("injected panic");
            }));
            assert!(unwind.is_err());
        } else {
            let mut session = driver.start().expect("terminal starts");
            if scenario == "error" {
                assert_eq!(
                    session.read_event(None).expect_err("read fails").kind(),
                    io::ErrorKind::UnexpectedEof
                );
            }
            drop(session);
        }
        assert!(
            driver
                .backend()
                .operations
                .ends_with(&FakeBackend::cleanup_tail()),
            "{scenario} did not restore the terminal"
        );
    }
}

#[test]
fn timeout_releases_ambiguous_escape_prefix_without_losing_input() {
    let backend = FakeBackend::with_events([
        BackendEvent::Input(b"\x1b".to_vec()),
        BackendEvent::Timeout,
        BackendEvent::EndOfInput,
    ]);
    let mut driver = TerminalDriver::new(backend);
    let mut session = driver.start().expect("terminal starts");
    assert_eq!(
        session
            .read_event(Some(Duration::from_millis(10)))
            .expect("escape"),
        TerminalEvent::Input(b"\x1b".to_vec())
    );
    assert_eq!(
        session.read_event(None).expect("end"),
        TerminalEvent::EndOfInput
    );
}
