pub mod gemini;
pub mod git;
pub mod grouping;

pub use gemini::call_gemini;
pub use git::{get_branch_diff, get_diff};
pub use grouping::{
    GroupingResult, ProposedCommit, build_commit_prompt, group_files_locally,
    parse_grouping_response,
};
