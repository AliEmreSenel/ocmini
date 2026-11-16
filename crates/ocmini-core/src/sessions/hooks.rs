use crate::hooks::SessionHook;
use crate::hooks::{Hook, HookName, Shape};
pub const SESSION_CREATE_BEFORE: Hook = Hook::new(
    HookName::Session(SessionHook::Create),
    &[("title", Shape::Text), ("metadata", Shape::Object)],
);
pub const SESSION_UPDATE_BEFORE: Hook = Hook::new(
    HookName::Session(SessionHook::Update),
    &[("title", Shape::Text), ("metadata", Shape::Object)],
);
pub const SESSION_FORK_BEFORE: Hook = Hook::new(
    HookName::Session(SessionHook::Fork),
    &[("title", Shape::Text)],
);
pub const SESSION_DELETE_BEFORE: Hook = Hook::new(HookName::Session(SessionHook::Delete), &[]);
pub const HOOKS: &[Hook] = &[
    SESSION_CREATE_BEFORE,
    SESSION_UPDATE_BEFORE,
    SESSION_FORK_BEFORE,
    SESSION_DELETE_BEFORE,
];
