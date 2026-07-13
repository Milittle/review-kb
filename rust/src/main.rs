//! `review-kb` binary entry point.

fn main() {
    // All argument parsing, dispatch, and envelope/exit handling lives in the
    // library so it is unit-testable; the binary just hands control over.
    review_kb::cli::run();
}
