/// Apply user-supplied V8 flags. No-op after the QuickJS cutover; callers
/// (CLI `--v8-flags`, embedders) keep compiling against the same name.
pub fn set_v8_flags(_flags: &str) {}

