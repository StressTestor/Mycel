use mycel_cli::{
    terminal::{
        grapheme_width, graphemes, truncate_to_width, visible_width, wrap_text,
        DifferentialRenderer, InputDecoder, InputEvent, KeyCode, KeyKind, MemoryTerminalSink,
        VirtualTerminal,
    },
    tui::{compose_overlay, FocusStack, Overlay},
};

#[test]
fn common_extended_graphemes_remain_atomic_and_have_terminal_widths() {
    let clusters: Vec<&str> = graphemes("e\u{301}👩\u{200d}💻🇺🇸界").collect();
    assert_eq!(clusters, vec!["e\u{301}", "👩\u{200d}💻", "🇺🇸", "界"]);
    assert_eq!(
        clusters
            .iter()
            .map(|cluster| grapheme_width(cluster))
            .collect::<Vec<_>>(),
        vec![1, 2, 2, 2]
    );
    assert_eq!(visible_width("e\u{301}👩\u{200d}💻🇺🇸界"), 7);
}

#[test]
fn wrap_and_truncate_never_split_grapheme_clusters() {
    assert_eq!(
        wrap_text("A界B👩\u{200d}💻C", 4),
        vec!["A界B", "👩\u{200d}💻C"]
    );
    assert_eq!(truncate_to_width("ab界cd", 5, "…"), "ab界…");
    assert_eq!(wrap_text("界", 1), vec!["界"]);
    assert_eq!(truncate_to_width("界x", 1, "…"), "…");
}

#[test]
fn decoder_preserves_split_paste_and_decodes_kitty_press_release() {
    let mut decoder = InputDecoder::default();
    assert!(decoder.feed(b"\x1b[200~hello").is_empty());
    assert!(decoder.feed(b"\nworld\x1b[20").is_empty());
    assert_eq!(
        decoder.feed(b"1~"),
        vec![InputEvent::Paste("hello\nworld".to_owned())]
    );

    let events = decoder.feed(b"\x1b[120;5u\x1b[121;1:3u");
    let InputEvent::Key(control_x) = &events[0] else {
        panic!("expected Kitty key");
    };
    assert_eq!(control_x.code, KeyCode::Char('x'));
    assert!(control_x.modifiers.control);
    assert_eq!(control_x.kind, KeyKind::Press);
    let InputEvent::Key(released_y) = &events[1] else {
        panic!("expected Kitty release");
    };
    assert_eq!(released_y.code, KeyCode::Char('y'));
    assert_eq!(released_y.kind, KeyKind::Release);
}

#[test]
fn decoder_normalizes_escape_shift_tab_and_ctrl_caps_lock() {
    let mut decoder = InputDecoder::default();
    assert!(decoder.feed(b"\x1b").is_empty());
    assert_eq!(
        decoder.flush(),
        vec![InputEvent::Key(mycel_cli::terminal::KeyEvent::press(
            KeyCode::Escape
        ))]
    );

    let events = decoder.feed(b"\x1b[Z\x1b[67;69u");
    let InputEvent::Key(shift_tab) = &events[0] else {
        panic!("expected shift-tab");
    };
    assert_eq!(shift_tab.code, KeyCode::Tab);
    assert!(shift_tab.modifiers.shift);

    let InputEvent::Key(control_c) = &events[1] else {
        panic!("expected Kitty control key");
    };
    assert_eq!(control_c.code, KeyCode::Char('c'));
    assert!(control_c.modifiers.control);
    assert!(control_c.modifiers.caps_lock);

    let events = decoder.feed(b"\x1b\r\x1b[13;2~");
    assert_eq!(events.len(), 2);
    for event in events {
        let InputEvent::Key(shift_enter) = event else {
            panic!("expected shift-enter");
        };
        assert_eq!(shift_enter.code, KeyCode::Enter);
        assert!(shift_enter.modifiers.shift);
    }
}

#[test]
fn overlays_replace_cells_without_splitting_wide_graphemes_and_focus_is_lifo() {
    let base = vec!["012345".to_owned(), "ab界ef".to_owned()];
    let overlay = Overlay {
        x: 2,
        y: 0,
        width: 3,
        height: 2,
        lines: vec!["XYZ".to_owned(), "界Q".to_owned()],
        captures_focus: true,
    };
    assert_eq!(
        compose_overlay(&base, 6, 2, &overlay),
        vec!["01XYZ5", "ab界Qf"]
    );

    let mut focus = FocusStack::default();
    focus.push("editor");
    focus.push("dialog");
    assert_eq!(focus.current(), Some("dialog"));
    assert!(focus.remove("dialog"));
    assert_eq!(focus.current(), Some("editor"));
}

#[test]
fn differential_renderer_writes_only_changed_rows_into_injected_sink() {
    let mut renderer = DifferentialRenderer::default();
    let mut terminal = VirtualTerminal::new(10, 3);
    let mut first = MemoryTerminalSink::default();
    renderer
        .render(&["alpha".to_owned(), "beta".to_owned()], 10, &mut first)
        .expect("initial render");
    terminal.feed(&first.bytes);
    assert_eq!(terminal.lines(), vec!["alpha", "beta", ""]);

    let mut second = MemoryTerminalSink::default();
    renderer
        .render(&["alpha".to_owned(), "gamma".to_owned()], 10, &mut second)
        .expect("differential render");
    let bytes = String::from_utf8(second.bytes.clone()).expect("ANSI is UTF-8");
    assert!(!bytes.contains("\x1b[1;1H"));
    assert!(bytes.contains("\x1b[2;1H"));
    terminal.feed(&second.bytes);
    assert_eq!(terminal.lines(), vec!["alpha", "gamma", ""]);
}
