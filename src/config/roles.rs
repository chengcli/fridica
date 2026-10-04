//! The worker roles a delegation may name besides `general` (#131). The
//! delegation scope and the parent's decision schema both take this list, so
//! the parent is offered exactly the roles a delegation accepts.
//!
//! Until the role catalog (#126 section 2) loads `assets/roles/*.md`, these are
//! the roles the contract has always named.
use std::sync::LazyLock;

static ROLES: LazyLock<Vec<String>> = LazyLock::new(|| {
    ["implementer", "reviewer", "tester"]
        .into_iter()
        .map(String::from)
        .collect()
});

/// Worker roles other than `general`, in the order the schema offers them.
pub fn worker_roles() -> &'static [String] {
    &ROLES
}
