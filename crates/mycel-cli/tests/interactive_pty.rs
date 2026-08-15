#![cfg(unix)]

use std::{
    fs::{self, File},
    io::{self, Read, Write},
    net::TcpListener,
    os::{
        fd::{FromRawFd, RawFd},
        unix::process::CommandExt,
    },
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use mycel_cli::terminal::{DISABLE_BRACKETED_PASTE, KITTY_KEYBOARD_QUERY, LEAVE_ALTERNATE_SCREEN};
use tempfile::TempDir;

#[test]
fn production_binary_streams_in_a_pty_and_restores_on_ctrl_d() {
    let temp = TempDir::new().expect("temp");
    let mycel_home = temp.path().join("mycel-home");
    let user_home = temp.path().join("user-home");
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(&mycel_home).expect("mycel home");
    fs::create_dir_all(&user_home).expect("user home");
    fs::create_dir_all(&workspace).expect("workspace");

    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
    let address = listener.local_addr().expect("listener address");
    let server_state = Arc::new(AtomicU8::new(0));
    let server_state_worker = Arc::clone(&server_state);
    let server =
        thread::spawn(move || serve_openai_stream(listener, "pty answer", &server_state_worker));
    fs::write(
        mycel_home.join("config.toml"),
        format!(
            r#"
default_model = "local"

[providers.local]
type = "openai"
base_url = "http://{address}/v1"
api_key = "test-key"

[models.local]
provider = "local"
model = "gpt-test"
max_context_size = 8192
max_output_size = 128
"#
        ),
    )
    .expect("config");

    let (master_fd, slave_fd) = open_pty(100, 30).expect("pty");
    let stdin_fd = duplicate_fd(slave_fd).expect("stdin dup");
    let stdout_fd = duplicate_fd(slave_fd).expect("stdout dup");
    let stderr_fd = duplicate_fd(slave_fd).expect("stderr dup");
    close_fd(slave_fd);

    let mut command = Command::new(env!("CARGO_BIN_EXE_mycel"));
    command
        .current_dir(&workspace)
        .env("MYCEL_HOME", &mycel_home)
        // Production discovers retained user skills under HOME before terminal
        // startup. Isolate it so this process test cannot scan developer state.
        .env("HOME", &user_home)
        // SAFETY: every descriptor is uniquely owned by its File/Stdio.
        .stdin(Stdio::from(unsafe { File::from_raw_fd(stdin_fd) }))
        .stdout(Stdio::from(unsafe { File::from_raw_fd(stdout_fd) }))
        .stderr(Stdio::from(unsafe { File::from_raw_fd(stderr_fd) }));
    // SAFETY: pre_exec uses only async-signal-safe libc operations before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().expect("spawn mycel");
    // SAFETY: the parent owns the master descriptor returned by openpty.
    let mut master = unsafe { File::from_raw_fd(master_fd) };
    set_nonblocking(master_fd).expect("nonblocking master");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut output = Vec::new();
    let mut sent_prompt = false;
    let mut sent_eof = false;
    let status = loop {
        let mut chunk = [0u8; 4096];
        match master.read(&mut chunk) {
            Ok(0) => {}
            Ok(count) => output.extend_from_slice(&chunk[..count]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.raw_os_error() == Some(libc::EIO) => {}
            Err(error) => panic!("read pty: {error}"),
        }
        if !sent_prompt
            && output
                .windows(KITTY_KEYBOARD_QUERY.len())
                .any(|window| window == KITTY_KEYBOARD_QUERY)
        {
            master.write_all(b"hello\r").expect("write prompt");
            sent_prompt = true;
        }
        if sent_prompt
            && !sent_eof
            && output
                .windows(b"pty answer".len())
                .any(|w| w == b"pty answer")
        {
            master.write_all(&[0x04]).expect("write ctrl-d");
            sent_eof = true;
        }
        if let Some(status) = child.try_wait().expect("wait child") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "interactive PTY timed out (provider state {}): {}",
                server_state.load(Ordering::SeqCst),
                String::from_utf8_lossy(&output)
            );
        }
        thread::sleep(Duration::from_millis(10));
    };
    // Report an early child exit BEFORE joining the provider thread: if the
    // child died before ever calling the provider, joining would block on a
    // request that never comes and the test would hang instead of fail.
    assert!(
        sent_prompt && sent_eof,
        "child exited early (status={status}, provider state {}, prompt sent {sent_prompt}, \
         eof sent {sent_eof}): {}",
        server_state.load(Ordering::SeqCst),
        String::from_utf8_lossy(&output)
    );
    server.join().expect("provider server");
    assert_eq!(server_state.load(Ordering::SeqCst), 3);

    assert!(
        status.success(),
        "status={status}; {}",
        String::from_utf8_lossy(&output)
    );
    assert!(
        sent_prompt,
        "terminal startup missing: {}",
        String::from_utf8_lossy(&output)
    );
    assert!(
        sent_eof,
        "assistant response missing: {}",
        String::from_utf8_lossy(&output)
    );
    assert!(output
        .windows(DISABLE_BRACKETED_PASTE.len())
        .any(|window| window == DISABLE_BRACKETED_PASTE));
    assert!(output
        .windows(LEAVE_ALTERNATE_SCREEN.len())
        .any(|window| window == LEAVE_ALTERNATE_SCREEN));
    let sessions = fs::read_dir(mycel_home.join("sessions"))
        .expect("sessions")
        .collect::<Result<Vec<_>, _>>()
        .expect("session entries")
        .into_iter()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .collect::<Vec<_>>();
    assert_eq!(sessions.len(), 1);
    assert!(mycel_home.join("sessions/index.jsonl").is_file());
    assert!(sessions[0]
        .path()
        .join("agents/main/records.jsonl")
        .is_file());
}

fn serve_openai_stream(listener: TcpListener, answer: &str, state: &AtomicU8) {
    // Bounded accept: a blocking accept() with no client would hang the join
    // forever (observed on CI). Poll until the main test's own deadline has
    // long since expired, then give up loudly.
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let accept_deadline = Instant::now() + Duration::from_secs(20);
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < accept_deadline,
                    "provider server never received a request (mycel never called the provider)"
                );
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("accept provider request: {error}"),
        }
    };
    stream.set_nonblocking(false).expect("blocking stream");
    state.store(1, Ordering::SeqCst);
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let request = read_http_request(&mut stream);
    assert!(String::from_utf8_lossy(&request).starts_with("POST /v1/chat/completions"));
    state.store(2, Ordering::SeqCst);
    let body = format!(
        "data: {{\"id\":\"chat\",\"choices\":[{{\"delta\":{{\"content\":{}}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n",
        serde_json::to_string(answer).expect("answer JSON")
    );
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .expect("write response");
    stream.flush().expect("flush response");
    state.store(3, Ordering::SeqCst);
}

fn read_http_request(stream: &mut impl Read) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0u8; 2048];
    let header_end = loop {
        let count = stream.read(&mut chunk).expect("read request headers");
        assert_ne!(count, 0, "provider request closed before headers");
        request.extend_from_slice(&chunk[..count]);
        assert!(request.len() <= 1024 * 1024, "provider request too large");
        if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().expect("content length"))
        })
        .unwrap_or(0);
    let required = header_end + content_length;
    while request.len() < required {
        let count = stream.read(&mut chunk).expect("read request body");
        assert_ne!(count, 0, "provider request closed before body");
        request.extend_from_slice(&chunk[..count]);
    }
    request
}

fn open_pty(columns: u16, rows: u16) -> io::Result<(RawFd, RawFd)> {
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_row: rows,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: all pointers reference initialized writable slots for openpty.
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    } == -1
    {
        Err(io::Error::last_os_error())
    } else {
        Ok((master, slave))
    }
}

fn duplicate_fd(fd: RawFd) -> io::Result<RawFd> {
    // SAFETY: dup does not borrow memory and returns a new descriptor.
    let duplicated = unsafe { libc::dup(fd) };
    if duplicated == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(duplicated)
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl operates on the owned master descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn close_fd(fd: RawFd) {
    // SAFETY: the descriptor is no longer used after this call.
    unsafe {
        libc::close(fd);
    }
}
