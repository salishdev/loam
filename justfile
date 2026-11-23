# The same commands, in the same order, as .github/workflows/ci.yml.
# Keep the two in step. Live API tests are #[ignore]d and stay skipped.
check:
    cargo fmt --check
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked
