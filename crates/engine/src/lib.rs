pub mod gemini;
pub mod git;
pub mod grouping;
pub mod update;

pub use gemini::call_gemini;
pub use git::{get_branch_diff, get_diff};
pub use grouping::{
    build_commit_prompt, group_files_locally, parse_grouping_response, GroupingResult,
    ProposedCommit,
};
pub use update::{
    check_latest_release, clear_pending_version, current_version, download_and_replace_binary,
    find_matching_asset, is_newer, pending_version, record_pending_version, ReleaseAsset,
    ReleaseInfo,
};
