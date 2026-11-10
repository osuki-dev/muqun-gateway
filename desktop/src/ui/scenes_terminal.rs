// Terminal panel scenes (included into `scenes.rs`): what a shell and vim
// would print, generated at the panel's size and replayed into the real
// emulator. Demo data only: no real paths, names or output.

/// A shell after a few commands: `ls --color`, the 16 colours, the 256-colour
/// cube, a truecolor ramp, text attributes and wide characters.
fn shell_fixture(cols: u16, _rows: u16) -> Vec<u8> {
    let mut o = String::from("\x1b]2;~/demo/gateway\x07");
    let prompt = |o: &mut String, cmd: &str| {
        o.push_str("\x1b[1;32mdemo\x1b[0m \x1b[1;34m~/demo/gateway\x1b[0m \x1b[35mmain\x1b[0m \x1b[1m$\x1b[0m ");
        o.push_str(cmd);
        o.push_str("\r\n");
    };
    prompt(&mut o, "ls --color");
    let entries: [(&str, &str); 9] = [
        ("01;34", "assets"),
        ("01;34", "desktop"),
        ("01;34", "src"),
        ("", "Cargo.lock"),
        ("", "Cargo.toml"),
        ("01;32", "install.sh"),
        ("", "README.md"),
        ("01;36", "target"),
        ("30;42", "tmp"),
    ];
    let mut col = 0usize;
    for (sgr, name) in entries {
        let cell = 14usize;
        if col + cell > usize::from(cols) {
            o.push_str("\r\n");
            col = 0;
        }
        if sgr.is_empty() {
            o.push_str(name);
        } else {
            o.push_str(&format!("\x1b[{sgr}m{name}\x1b[0m"));
        }
        o.push_str(&" ".repeat(cell - name.len()));
        col += cell;
    }
    o.push_str("\r\n");
    prompt(&mut o, "colors");
    for base in [40, 100] {
        for i in 0..8 {
            o.push_str(&format!("\x1b[{}m {} \x1b[0m", base + i, (base - 40) / 60 * 8 + i));
        }
        o.push_str("\r\n");
    }
    // The 6×6×6 cube, two rows of 108.
    let width = usize::from(cols).min(108);
    for half in 0..2 {
        for i in 0..width {
            let n = 16 + half * 108 + i * 108 / width;
            o.push_str(&format!("\x1b[48;5;{n}m "));
        }
        o.push_str("\x1b[0m\r\n");
    }
    // The grey ramp, then a truecolor sweep.
    for i in 0..width {
        o.push_str(&format!("\x1b[48;5;{}m ", 232 + i * 24 / width));
    }
    o.push_str("\x1b[0m\r\n");
    for i in 0..width {
        let t = i as f32 / width as f32;
        let r = (255.0 * (1.0 - t)) as u8;
        let g = (255.0 * (1.0 - (2.0 * t - 1.0).abs())) as u8;
        let b = (255.0 * t) as u8;
        o.push_str(&format!("\x1b[48;2;{r};{g};{b}m "));
    }
    o.push_str("\x1b[0m\r\n");
    prompt(&mut o, "cargo test -p muqun-desktop");
    o.push_str("\x1b[1;32m   Compiling\x1b[0m muqun-desktop v0.1.0\r\n");
    o.push_str("\x1b[1;32m    Finished\x1b[0m `test` profile in 2.41s\r\n");
    o.push_str("test term::grid::tests::wide_glyphs ... \x1b[32mok\x1b[0m\r\n");
    o.push_str("test result: \x1b[32mok\x1b[0m. 154 passed; \x1b[1m0\x1b[22m failed\r\n");
    prompt(&mut o, "echo styles");
    o.push_str("\x1b[1mbold\x1b[0m \x1b[3mitalic\x1b[0m \x1b[4munderline\x1b[0m \x1b[9mstrike\x1b[0m \x1b[2mfaint\x1b[0m \x1b[7minverse\x1b[0m\r\n");
    o.push_str("Wide chars · Unicode ✓ → ⚡\r\n");
    prompt(&mut o, "");
    o.truncate(o.len() - 2);
    o.into_bytes()
}

/// vim on the alternate screen, editing a small Rust file.
fn vim_fixture(cols: u16, rows: u16) -> Vec<u8> {
    let mut o = String::from("demo $ vim src/term/grid.rs\r\n\x1b[?1049h\x1b[H\x1b[2J");
    let code: [&str; 16] = [
        "\x1b[35muse\x1b[0m std::sync::Arc;",
        "",
        "\x1b[34m/// One grid row, ready to draw.\x1b[0m",
        "\x1b[32m#[derive(Debug, Clone, Default)]\x1b[0m",
        "\x1b[35mpub struct\x1b[0m \x1b[33mRow\x1b[0m {",
        "    \x1b[35mpub\x1b[0m bgs: \x1b[33mVec\x1b[0m<\x1b[33mBgRun\x1b[0m>,",
        "    \x1b[35mpub\x1b[0m runs: \x1b[33mVec\x1b[0m<\x1b[33mTextRun\x1b[0m>,",
        "    \x1b[35mpub\x1b[0m hash: \x1b[33mu64\x1b[0m,",
        "}",
        "",
        "\x1b[35mfn\x1b[0m \x1b[36mfits_cell\x1b[0m(text: &\x1b[33mstr\x1b[0m) -> \x1b[33mbool\x1b[0m {",
        "    \x1b[35mlet mut\x1b[0m chars = text.\x1b[36mchars\x1b[0m();",
        "    \x1b[35mmatches!\x1b[0m(c, \x1b[31m' '\x1b[0m..=\x1b[31m'~'\x1b[0m) \x1b[34m// ASCII\x1b[0m",
        "}",
        "",
        "\x1b[34m// TODO: ligatures stay off in the grid\x1b[0m",
    ];
    let body = usize::from(rows.saturating_sub(2));
    for (i, line) in (0..body).map(|i| (i, code.get(i))) {
        o.push_str(&format!("\x1b[{};1H", i + 1));
        match line {
            Some(l) => o.push_str(&format!("\x1b[33m{:>3} \x1b[0m{l}", i + 1)),
            None => o.push_str("\x1b[34m~\x1b[0m"),
        }
    }
    // Status line (inverse) and the command line.
    let (left, right) = (" src/term/grid.rs [+]", "12,5      Top ");
    let pad = usize::from(cols).saturating_sub(left.len() + right.len());
    let status = format!("{left}{}{right}", " ".repeat(pad));
    o.push_str(&format!("\x1b[{};1H\x1b[7m{status}\x1b[0m", rows - 1));
    o.push_str(&format!("\x1b[{};1H\x1b[1m-- INSERT --\x1b[0m", rows));
    o.push_str("\x1b[12;9H\x1b[6 q");
    o.into_bytes()
}

fn terminal_scene(s: &mut UiState, fixture: fn(u16, u16) -> Vec<u8>) {
    session_scene(s);
    s.right_open = false;
    s.right.set_open(false);
    s.term.fixture = Some(fixture);
    s.term.open = true;
    s.term.pane.set_open(true);
    s.term.sink.focused = true;
}
