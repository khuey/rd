use crate::{
    arch::Architecture,
    event::Switchable,
    file_monitor::virtual_perf_counter_monitor::VirtualPerfCounterMonitor,
    kernel_abi::SupportedArch,
    registers::Registers,
    remote_ptr::{RemotePtr, Void},
    session::task::{record_task::RecordTask, Task},
};
use mmapped_file_monitor::MmappedFileMonitor;
use std::{
    cell::RefCell,
    fs::File,
    io::{BufRead, BufReader},
    mem::size_of,
    rc::{Rc, Weak},
};

pub mod base_file_monitor;
pub mod magic_save_data_monitor;
pub mod mmapped_file_monitor;
pub mod preserve_file_monitor;
pub mod proc_fd_dir_monitor;
pub mod proc_mem_monitor;
pub mod stdio_monitor;
pub mod virtual_perf_counter_monitor;

pub type FileMonitorSharedPtr = Rc<RefCell<Box<dyn FileMonitor>>>;
pub type FileMonitorSharedWeakPtr = Weak<RefCell<Box<dyn FileMonitor>>>;

/// This should NOT impl the FileMonitor trait
pub struct FileMonitorInner;

#[derive(Copy, Clone, Eq, PartialEq)]
pub enum FileMonitorType {
    Base,
    MagicSaveData,
    Mmapped,
    Preserve,
    ProcFd,
    ProcMem,
    Stdio,
    VirtualPerfCounter,
}

/// Notification that task `t` wrote to the file descriptor.
/// Due to races, if will_write did not return PREVENT_SWITCH, it's possible
/// that the data in the buffers is not what was actually written.
#[derive(Copy, Clone)]
pub struct Range {
    pub data: RemotePtr<Void>,
    pub length: usize,
}

impl Range {
    pub fn new(data: RemotePtr<Void>, length: usize) -> Range {
        Range { data, length }
    }
}

/// Encapsulates the offset at which to read or write. Computing this may be
/// an expensive operation if the offset is implicit (i.e. is taken from the
/// file descriptor), so we only do it if we actually need to look at the
/// offset.
pub struct LazyOffset<'b, 'a: 'b> {
    t: &'a mut dyn Task,
    regs: &'b Registers,
    /// DIFF NOTE: @TODO in rr this is an i64
    /// Keeping it as an i32 to be consistent with elsewhere.
    syscallno: i32,
}

impl<'b, 'a: 'b> LazyOffset<'b, 'a> {
    pub fn task_mut(&mut self) -> &mut dyn Task {
        self.t
    }

    pub fn new(t: &'a mut dyn Task, regs: &'b Registers, syscallno: i32) -> LazyOffset<'b, 'a> {
        LazyOffset { t, regs, syscallno }
    }
    /// DIFF NOTE: In rr this returns an i64. We return a Option<u64>.
    /// Need to be careful with the logic here
    pub fn retrieve(&mut self, needed_for_replay: bool) -> Option<u64> {
        let is_replay = self.t.session().is_replaying();
        let is_implicit_offset = is_implict_offset_syscall(self.t.arch(), self.syscallno);
        ed_assert!(self.t, needed_for_replay || !is_replay);
        // There is no way we can figure out this information now, so retrieve it
        // from the trace (we record it below under the same circumstance).
        if is_replay && is_implicit_offset {
            return self
                .t
                .as_replay_task()
                .unwrap()
                .current_trace_frame()
                .event()
                .syscall()
                .write_offset;
        }
        // DIFF NOTE: This is an i64 in rr
        let maybe_offset = retrieve_offset(self.t, self.syscallno, self.regs);
        if needed_for_replay && is_implicit_offset {
            self.t
                .as_record_task_mut()
                .unwrap()
                .ev_mut()
                .syscall_mut()
                .write_offset = maybe_offset;
        }

        maybe_offset
    }
}

fn is_implicit_offset_syscall_arch<Arch: Architecture>(syscallno: i32) -> bool {
    syscallno == Arch::WRITEV || syscallno == Arch::WRITE
}

fn is_implict_offset_syscall(arch: SupportedArch, syscallno: i32) -> bool {
    rd_arch_function_selfless!(is_implicit_offset_syscall_arch, arch, syscallno)
}

fn retrieve_offset_arch<Arch: Architecture>(
    t: &mut dyn Task,
    syscallno: i32,
    regs: &Registers,
) -> Option<u64> {
    // DIFF NOTE: @TODO This is tricky. off_t is signed. Different from how rr does this.
    // But a negative offset for these system calls does not make sense...
    if syscallno == Arch::PWRITE64
        || syscallno == Arch::PWRITEV
        || syscallno == Arch::PREAD64
        || syscallno == Arch::PREADV
    {
        let offset = if size_of::<Arch::unsigned_word>() == 4 {
            regs.arg4() as i64 | ((regs.arg5_signed() as i64) << 32)
        } else {
            regs.arg4_signed() as i64
        };

        if offset < 0 {
            None
        } else {
            Some(offset as u64)
        }
    } else if syscallno == Arch::WRITEV || syscallno == Arch::WRITE {
        ed_assert!(
            t,
            t.session().is_recording(),
            "Can only read a file descriptor's offset while recording"
        );
        let fd: i32 = regs.arg1_signed() as i32;
        // Get the offset from /proc/*/fdinfo/*
        let fdinfo_path = format!("/proc/{}/fdinfo/{}", t.tid, fd);
        let result = File::open(&fdinfo_path);
        let mut f = match result {
            Err(e) => {
                fatal!("Failed to open `{}': {:?}", fdinfo_path, e);
            }
            Ok(file) => BufReader::new(file),
        };

        let mut buf = String::new();
        let mut maybe_offset: Option<u64> = None;
        // @TODO do we need to use read_until() which will give a Vec<u8> instead?
        // But buf being a String should be OK for now. The characters in fdinfo should be ASCII
        // anyways.
        while let Ok(nread) = f.read_line(&mut buf) {
            if nread == 0 {
                break;
            }

            let s = buf.trim();
            let maybe_loc = s.find("pos:\t");
            if maybe_loc.is_none() {
                continue;
            }
            // 5 is length of str "pos:\t"
            let loc = maybe_loc.unwrap() + 5;
            // @TODO This is tricky. Are we sure that a negative offset won't appear in
            // /proc/{}/fdinfo/{} ?
            let maybe_res = s[loc..].parse::<u64>();
            match maybe_res {
                Ok(res) => maybe_offset = Some(res),
                Err(e) => fatal!(
                    "Unable to parse file offset from `{}': {:?}",
                    fdinfo_path,
                    e
                ),
            }
        }

        if maybe_offset.is_none() {
            fatal!("Failed to read position");
        }

        let offset = maybe_offset.unwrap();
        // The pos we just read, was after the write completed. Luckily, we do
        // know how many bytes were written.
        // DIFF NOTE: This is slightly different from the rr approach.
        if offset < regs.syscall_result() as u64 {
            None
        } else {
            Some(offset - regs.syscall_result() as u64)
        }
    } else {
        ed_assert!(t, false, "Cannot retrieve offset for this system call");
        None
    }
}

fn retrieve_offset(t: &mut dyn Task, syscallno: i32, regs: &Registers) -> Option<u64> {
    let arch = t.arch();
    rd_arch_function_selfless!(retrieve_offset_arch, arch, t, syscallno, regs)
}

/// We DONT need a DerefMut<Target=FileMonitorInner> at the moment because
/// The FileMonitorInner struct does not have any members at the moment.
pub trait FileMonitor {
    /// You have to provide a type if you implement this trait
    fn file_monitor_type(&self) -> FileMonitorType;

    fn as_mmapped_file_monitor_mut(&mut self) -> Option<&mut MmappedFileMonitor> {
        None
    }

    fn as_mmapped_file_monitor(&self) -> Option<&MmappedFileMonitor> {
        None
    }

    fn as_virtual_perf_counter_monitor(&self) -> Option<&VirtualPerfCounterMonitor> {
        None
    }

    fn as_virtual_perf_counter_monitor_mut(&mut self) -> Option<&mut VirtualPerfCounterMonitor> {
        None
    }

    /// Overriding this to return true will cause close() (and related fd-smashing
    /// operations such as dup2) to return EBADF, and hide it from the tracee's
    /// /proc/pid/fd/
    fn is_rd_fd(&self) -> bool {
        false
    }

    /// Notification that task `t` is about to write `data` bytes of length
    /// `length` to the file.
    /// In general writes can block, and concurrent blocking writes to the same
    /// file may race so that the kernel performs writes out of order
    /// with respect to will_write notifications.
    /// If it is known that the write cannot block (or that blocking all of rr
    /// on it is OK), this notification can return PREVENT_SWITCH to make the
    /// write a blocking write. This ensures that writes are performed in the order
    /// of will_write notifications.
    fn will_write(&self, _t: &dyn Task) -> Switchable {
        Switchable::AllowSwitch
    }

    /// DIFF NOTE: We don't have a task param like in rr as the task is included
    /// in `l`, the LazyOffset
    fn did_write<'b, 'a: 'b>(&mut self, _rv: &[Range], _l: &mut LazyOffset<'b, 'a>) {}

    /// Return true if the ioctl should be fully emulated. If so the result
    /// is stored in the last parameter.
    /// Only called during recording.
    fn emulate_ioctl(&mut self, _t: &RecordTask, _r: &mut u64) -> bool {
        false
    }

    /// Return true if the fcntl should should be fully emulated. If so the
    /// result is stored in the last parameter.
    /// Only called during recording.
    fn emulate_fcntl(&self, _t: &RecordTask, _r: &mut u64) -> bool {
        false
    }

    /// Return true if the read should should be fully emulated. If so the
    /// result is stored in the last parameter. The emulation should write to the
    /// task's memory ranges.
    /// Only called during recording.
    fn emulate_read(
        &self,
        _t: &RecordTask,
        _vr: &Vec<Range>,
        _o: &LazyOffset,
        _l: &mut u64,
    ) -> bool {
        false
    }

    /// Allows the FileMonitor to rewrite the output of a getdents/getdents64 call
    /// if desired.
    fn filter_getdents(&self, _t: &RecordTask) {}
}
