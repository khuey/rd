use crate::{
    file_monitor::{FileMonitor, FileMonitorType},
    session::task::{record_task::RecordTask, Task},
    taskish_uid::TaskUid,
};
use std::ffi::OsStr;

/// A FileMonitor to intercept enumerations of /proc/<pid>/fd so that entries
/// for rr's private fds can be hidden when <pid> is a tracee.
pub struct ProcFdDirMonitor {
    /// None if this does not refer to a tracee's proc fd
    /// DIFF NOTE: in rr this is a "0" instead of None.
    maybe_tuid: Option<TaskUid>,
}

impl FileMonitor for ProcFdDirMonitor {
    fn file_monitor_type(&self) -> FileMonitorType {
        FileMonitorType::ProcFd
    }

    fn filter_getdents(&self, _t: &RecordTask) {
        unimplemented!()
    }
}

impl ProcFdDirMonitor {
    pub fn new(_t: &dyn Task, _pathname: &OsStr) -> ProcFdDirMonitor {
        unimplemented!()
    }
}
