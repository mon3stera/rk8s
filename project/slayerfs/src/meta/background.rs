use crate::chuck::SliceDesc;
use async_channel::Sender;
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub(crate) struct DeleteTask {
    pub(crate) handle: JoinHandle<()>,
}

pub(crate) struct DeleteBackground {
    pub(crate) cancel: CancellationToken,
    pub(crate) sender: Option<Sender<Vec<SliceDesc>>>,
    pub(crate) tasks: Vec<DeleteTask>,
}

pub(crate) struct BackgroundTasks {
    pub(crate) delete: Mutex<DeleteBackground>,
}

impl BackgroundTasks {
    pub fn new() -> Self {
        Self {
            delete: Mutex::new(DeleteBackground {
                cancel: CancellationToken::new(),
                sender: None,
                tasks: Vec::new(),
            }),
        }
    }
}
