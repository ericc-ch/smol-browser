/// Apply user-supplied V8 flags. No-op after the QuickJS cutover; callers
/// (CLI `--v8-flags`, embedders) keep compiling against the same name.
pub fn set_v8_flags(_flags: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_noop() {
        // Must not panic.
        set_v8_flags("");
        set_v8_flags("   ");
        set_v8_flags("\t\n");
        set_v8_flags("--max-old-space-size=4096");
    }
}
