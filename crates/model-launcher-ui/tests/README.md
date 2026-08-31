# Window lifecycle integration test

`window_lifecycle_windows.rs` is ignored by default because it creates real native windows and
requires an interactive Windows desktop. Slint 1.16.1's published `i-slint-backend-testing` crate
cannot currently replace it: that crate invokes `include_dir!` for
`tests/screenshots/fonts`, but those resources are absent from the crates.io package, causing its
build to fail before tests run. The production `WindowManager` retains an actual `slint::Weak` and
asserts immediately after every close that the component was destroyed; the ignored test exercises
that assertion over fifty real create/close cycles.
