# The same commands, in the same order, as .github/workflows/ci.yml.
# Keep the two in step, along with the fmt and clippy steps in release.yml's
# check job. Live API tests are #[ignore]d and stay skipped.
check:
    cargo fmt --check
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked
