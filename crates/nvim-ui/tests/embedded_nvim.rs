//! Against a real Neovim, because the thing worth knowing is whether the
//! protocol was read right, and a recording of it can only ever agree with
//! whoever wrote the recording.
//!
//! `--clean` throughout: these must not depend on whose machine they run on.
//! Skipped rather than failed where there is no `nvim` to talk to.

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use nvim_ui::{Event, Nvim};

const PATIENCE: Duration = Duration::from_secs(10);

fn have_nvim() -> bool {
    let found = std::process::Command::new("nvim")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !found {
        eprintln!("skipping: nvim is not on PATH");
    }
    found
}

/// Nvim redraws when it is ready, not when it is asked, so every assertion
/// here is "this becomes true", never "this is true now".
fn settle(events: &Receiver<Event>, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while !done() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        match events.recv_timeout(left) {
            Ok(Event::Redraw) | Ok(Event::Notify(..)) => {}
            Ok(Event::Exited) | Err(_) => return done(),
        }
    }
    true
}

fn project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    for (name, text) in files {
        std::fs::write(dir.path().join(name), text).expect("writing a fixture file");
    }
    dir
}

/// The whole read path: a Neovim starts with no terminal of its own, opens a
/// file it was told to, and reports both the text on its screen and where in
/// the file the cursor sits -- which is everything a graph needs to follow
/// along beside it.
#[test]
fn an_opened_file_shows_on_the_grid_and_says_where_the_cursor_is() {
    if !have_nvim() {
        return;
    }
    let dir = project(&[("hello.rs", "fn one() {}\nfn two() {}\nfn three() {}\n")]);
    let (nvim, events) = Nvim::spawn(dir.path(), (40, 6), &["--clean"]).expect("spawning nvim");

    nvim.call("nvim_command", vec!["edit hello.rs".into()]).expect("opening the file");
    nvim.call("nvim_win_set_cursor", vec![0.into(), rmpv::Value::Array(vec![2.into(), 0.into()])])
        .expect("moving the cursor");

    assert!(
        settle(&events, || nvim.row(0).starts_with("fn one() {}")),
        "the file never appeared; row 0 was {:?}",
        nvim.row(0)
    );
    assert!(nvim.row(1).starts_with("fn two() {}"));
    assert!(
        settle(&events, || nvim.viewport().curline == 1),
        "cursor line was {}, wanted the second line",
        nvim.viewport().curline
    );
    assert_eq!(nvim.cursor().1, 1, "the cursor is drawn on the row it is on");
}

/// The write path, and the reason this is an editor rather than a listing:
/// keys typed at it land in the buffer and come back on the next redraw.
#[test]
fn keys_typed_at_nvim_change_what_it_shows() {
    if !have_nvim() {
        return;
    }
    let dir = project(&[("notes.txt", "first\n")]);
    let (nvim, events) = Nvim::spawn(dir.path(), (40, 6), &["--clean"]).expect("spawning nvim");
    nvim.call("nvim_command", vec!["edit notes.txt".into()]).expect("opening the file");
    assert!(settle(&events, || nvim.row(0).starts_with("first")), "the file never appeared");

    nvim.input("ggIwas <Esc>");

    assert!(
        settle(&events, || nvim.row(0).starts_with("was first")),
        "row 0 was {:?}",
        nvim.row(0)
    );
    assert_eq!(nvim.mode(), nvim_ui::Mode::Normal, "<Esc> put it back in normal mode");
}

/// A split is an ordinary thing to do in an editor, and nvim scrolls one
/// window at a time: it names the columns it is shifting, and a reader that
/// took every scroll to mean the whole row would drag the window next door
/// along with it -- silently, and only once the user split the screen.
#[test]
fn scrolling_one_window_of_a_split_leaves_the_other_where_it_was() {
    if !have_nvim() {
        return;
    }
    let rows = |tag: char| (1..=200).map(|i| format!("{tag}{i:03}\n")).collect::<String>();
    let dir = project(&[("left.txt", &rows('L')), ("right.txt", &rows('R'))]);
    let (nvim, events) = Nvim::spawn(dir.path(), (60, 12), &["--clean"]).expect("spawning nvim");

    // Nothing but the two files on screen, so a changed row means changed text.
    nvim.call("nvim_command", vec!["set nonumber laststatus=0 noruler noshowcmd".into()])
        .expect("plain screen");
    nvim.call("nvim_command", vec!["edit right.txt".into()]).expect("opening a file");
    nvim.call("nvim_command", vec!["vsplit left.txt".into()]).expect("splitting");
    assert!(settle(&events, || nvim.row(0).starts_with("L001")), "the split never appeared");

    let untouched = |nvim: &Nvim| -> Vec<String> {
        (0..12).map(|y| nvim.row(y).chars().skip(31).collect()).collect()
    };
    let before = untouched(&nvim);

    // Scroll the left window only.
    nvim.call("nvim_command", vec!["normal! 13G".into()]).expect("scrolling");
    assert!(settle(&events, || nvim.row(0).starts_with("L003")), "the left window never scrolled");

    assert_eq!(before, untouched(&nvim), "the window nobody scrolled moved anyway");
}

/// Redraw events are a convenience, not the channel nvim is driven over. A
/// caller that stops listening for them -- one that polls the screen, or has
/// simply put the pane away -- must still be able to ask nvim things, and
/// must still see a current screen when it looks.
#[test]
fn nvim_still_answers_after_nobody_is_listening_for_redraws() {
    if !have_nvim() {
        return;
    }
    let dir = project(&[("notes.txt", "one\n")]);
    let (nvim, events) = Nvim::spawn(dir.path(), (40, 6), &["--clean"]).expect("spawning nvim");
    nvim.call("nvim_command", vec!["edit notes.txt".into()]).expect("opening the file");
    assert!(settle(&events, || nvim.row(0).starts_with("one")), "the file never appeared");

    drop(events);
    nvim.input("ggIzzz <Esc>");

    // Typing is fire and forget, so this asks until the answer catches up
    // rather than once -- every turn of which is another question answered
    // with nobody listening, which is the point.
    let answered = std::thread::spawn(move || {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let line = nvim
                .call("nvim_get_current_line", vec![])
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned));
            // Both, because they arrive separately: the buffer changes when
            // the keys are consumed, the screen when nvim next repaints.
            let row = nvim.row(0);
            if (line.as_deref() == Some("zzz one") && row.starts_with("zzz one"))
                || Instant::now() > deadline
            {
                return (line, row);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    // A `call` that hung would leave this thread running for ever.
    let deadline = Instant::now() + PATIENCE + PATIENCE;
    while !answered.is_finished() {
        assert!(Instant::now() < deadline, "nvim stopped answering once nothing was listening");
        std::thread::sleep(Duration::from_millis(20));
    }
    let (line, row) = answered.join().expect("the asking thread");
    assert_eq!(line.as_deref(), Some("zzz one"), "nvim answered, but never with the edit");
    assert!(row.starts_with("zzz one"), "the screen went stale; row 0 was {row:?}");
}
