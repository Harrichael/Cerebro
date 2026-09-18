//! Alone in its own test binary on purpose: it empties PATH, and cargo runs
//! the tests in one file on several threads at once, so a neighbour would
//! quietly skip itself for want of an nvim this test had just hidden.

#[test]
fn a_missing_nvim_is_an_error_not_a_panic() {
    let path = std::env::var("PATH").unwrap_or_default();
    // SAFETY: this test is the only thing in its binary, and it puts PATH back.
    unsafe { std::env::set_var("PATH", "") };
    let tried = nvim_ui::Nvim::spawn(std::path::Path::new("."), (10, 3), &[]);
    unsafe { std::env::set_var("PATH", path) };

    let message = tried.err().expect("no nvim on an empty PATH").to_string();
    assert!(message.contains("nvim"), "unhelpful error: {message}");
}
