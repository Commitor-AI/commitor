// These modules are the core scan/commit engine. Nothing in the CLI
// calls them yet — they are wired up when those commands land — so
// dead_code is expected and allowed until then.
#[allow(dead_code)]
pub mod gemini;
#[allow(dead_code)]
pub mod git;
#[allow(dead_code)]
pub mod grouping;

pub mod update;
