/// Marker for types that are `Sync` but not `Send`
/// 标记类型为`Sync`但不是`Send`
#[allow(dead_code)]
pub(crate) struct SyncNotSend(#[allow(dead_code)] *mut ());

unsafe impl Sync for SyncNotSend {}

cfg_rt! {
    pub(crate) struct NotSendOrSync(#[allow(dead_code)] *mut ());
}
