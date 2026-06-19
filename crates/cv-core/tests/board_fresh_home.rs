//! Regression test: `board::active_claims` on a fresh home (no `board/` dir yet) is empty, not an
//! error.
//!
//! History: `active_claims_in_dir` acquired the per-channel lock by `create_new`-ing
//! `<dir>/<channel>.lock` *without first creating `dir`* — the open failed with `NotFound`, which
//! the acquire loop treated as fatal. So `GET /api/claims/<channel>` 500'd, and `cv board claims`
//! / the MCP `board_claims` tool failed, on every fresh install until something posted to *any*
//! channel. The other readers (`read_from_dir`, `who_in_dir`, `channels_in_dir`) always mapped a
//! missing file/dir to "empty"; `active_claims_in_dir` now does too (a missing dir short-circuits
//! before the lock).

use std::fs;

#[test]
fn active_claims_on_fresh_home_is_empty_not_an_error() {
    let dir = std::env::temp_dir().join(format!(
        "cv-board-fresh-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    // The board dir does NOT exist — the state of every fresh $CLUSTERVISION_HOME.
    let board = dir.join("board");
    assert!(!board.exists());

    let res = cv_core::board::active_claims_in_dir(&board, "fleet");
    fs::remove_dir_all(&dir).ok();

    let claims = res.expect("a channel nobody ever claimed on has no claims — not an error");
    assert!(claims.is_empty());
}
