use crate::hooks::{CommandHook, CompactionHook, PromptHook, TurnHook};
use crate::hooks::{Hook, HookName, InvocationKind, Shape};
pub(crate) const TURN_START_BEFORE: Hook = Hook::new(
    HookName::Turn(TurnHook::StartBefore),
    &[("context", Shape::Text)],
)
.turn("primary");
pub(crate) const TURN_END_BEFORE: Hook = Hook::new(HookName::Turn(TurnHook::EndBefore), &[])
    .turn("primary")
    .gate()
    .only_turn("primary");
pub(crate) const TURN_END_AFTER: Hook = Hook::new(HookName::Turn(TurnHook::EndAfter), &[])
    .turn("primary")
    .kind(InvocationKind::Finalizer);
pub(crate) const COMPACTION_BEFORE: Hook = Hook::new(
    HookName::Compaction(CompactionHook::Before),
    &[
        ("context", Shape::TextArray),
        ("prompt", Shape::NullableText),
    ],
)
.request("compaction");
pub(crate) const COMPACTION_AFTER: Hook = Hook::new(
    HookName::Compaction(CompactionHook::After),
    &[("summary", Shape::Text)],
)
.request("compaction");
pub(crate) const PROMPT_ACCEPT_BEFORE: Hook = Hook::new(
    HookName::Prompt(PromptHook::AcceptBefore),
    &[("parts", Shape::Array)],
)
.kind(InvocationKind::Admission);
pub(crate) const COMMAND_EXECUTE_BEFORE: Hook = Hook::new(
    HookName::Command(CommandHook::ExecuteBefore),
    &[("arguments", Shape::Text)],
)
.kind(InvocationKind::Admission);
pub(crate) const COMMAND_EXPAND_AFTER: Hook = Hook::new(
    HookName::Command(CommandHook::ExpandAfter),
    &[("parts", Shape::Array)],
)
.kind(InvocationKind::Admission);
pub(crate) const HOOKS: &[Hook] = &[
    TURN_START_BEFORE,
    TURN_END_BEFORE,
    TURN_END_AFTER,
    COMPACTION_BEFORE,
    COMPACTION_AFTER,
    PROMPT_ACCEPT_BEFORE,
    COMMAND_EXECUTE_BEFORE,
    COMMAND_EXPAND_AFTER,
];
