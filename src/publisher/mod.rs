mod git_current_target;
mod git_projection_materializer;

pub use git_current_target::{GitCurrentTarget, GitCurrentTargetAdapter, GitCurrentTargetError};
pub use git_projection_materializer::{
    GitProjectionMaterializationError, GitProjectionMaterializer, ReviewedGitTree,
};
