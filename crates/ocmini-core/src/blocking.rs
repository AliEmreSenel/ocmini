/// Run synchronous database/engine sections without occupying an async worker.
/// The executable uses a multithread runtime with one worker by default; Tokio
/// hands its scheduler to a replacement while this thread blocks. Plain callers
/// and current-thread unit fixtures need no scheduler handoff.
pub fn run<T>(work: impl FnOnce() -> T) -> T {
    if tokio::runtime::Handle::try_current()
        .is_ok_and(|handle| handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
    {
        tokio::task::block_in_place(work)
    } else {
        work()
    }
}
