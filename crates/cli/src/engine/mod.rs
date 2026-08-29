// These modules are the core scan/commit engine. `git` and `hunks`
// are called by the analysis/commit paths; gemini/grouping remain
// unwired, so dead_code is expected and allowed until then.
#[allow(dead_code)]
pub mod gemini;
pub mod git;
pub mod history;
pub mod hunks;
#[allow(dead_code)]
pub mod grouping;

pub mod update;
