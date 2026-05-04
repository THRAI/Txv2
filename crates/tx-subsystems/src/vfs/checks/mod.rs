pub mod predicates;
pub mod require;
pub mod resolution;
pub mod witness;

pub use require::{
    require_directory, require_entity, require_entity_or_parent_and_name,
    require_entity_unfollowed, require_mount_point, require_parent_and_name,
    require_parent_and_named_child, require_real_path, require_rmdirable_dir_child,
    require_symlink_for_readlink, require_unlinkable_non_dir_child, ResolveCtx, VfsError,
};
pub use resolution::state::RootCtxCaps;
pub use witness::{
    DirectoryAtPath, EntityAtPath, EntityOrParentAndName, MountPointAtPath, ParentAndName,
    ParentAndNamedChild, RealPath, RmdirableDirChild, SymlinkAtPath, UnlinkableNonDirChild,
};
