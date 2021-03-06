use super::{
    on_create_task_common,
    session_common::kill_all_tasks,
    task::{
        record_task::RecordTask,
        task_common::write_val_mem,
        task_inner::{ResumeRequest, SaveTraceeFdNumber, TaskInner, TicksRequest, WaitRequest},
    },
    SessionSharedPtr,
};
use crate::{
    bindings::{
        audit::{AUDIT_ARCH_I386, AUDIT_ARCH_X86_64},
        signal::siginfo_t,
    },
    commands::record_command::RecordCommand,
    event::{Event, EventType, SignalDeterministic, Switchable, SyscallState},
    kernel_abi::{native_arch, SupportedArch},
    kernel_supplement::SYS_SECCOMP,
    log::{LogDebug, LogError},
    perf_counters::TicksSemantics,
    preload_interface::{
        syscallbuf_hdr,
        SYSCALLBUF_ENABLED_ENV_VAR,
        SYSCALLBUF_LIB_FILENAME,
        SYSCALLBUF_LIB_FILENAME_PADDED,
    },
    remote_ptr::{RemotePtr, Void},
    scheduler::Scheduler,
    scoped_fd::ScopedFd,
    seccomp_filter_rewriter::{SeccompFilterRewriter, SECCOMP_MAGIC_SKIP_ORIGINAL_SYSCALLNO},
    session::{
        session_inner::SessionInner,
        task::{Task, TaskSharedPtr},
        Session,
    },
    sig::Sig,
    taskish_uid::TaskUid,
    thread_group::ThreadGroupSharedPtr,
    trace::{
        trace_stream::TraceStream,
        trace_writer::{CloseStatus, TraceWriter},
    },
    util::{
        choose_cpu,
        find,
        good_random,
        resource_path,
        CPUIDData,
        CPUID_GETEXTENDEDFEATURES,
        CPUID_GETFEATURES,
        CPUID_GETXSAVE,
    },
    wait_status::WaitStatus,
};
use goblin::elf::Elf;
use libc::{pid_t, SIGSYS, S_IFREG};
use nix::{
    fcntl::{open, OFlag},
    sys::stat::{stat, Mode},
    unistd::{access, read, AccessFlags},
};
use std::{
    cell::{Ref, RefCell, RefMut},
    env,
    ffi::{OsStr, OsString},
    fs,
    mem,
    ops::{Deref, DerefMut},
    os::unix::ffi::{OsStrExt, OsStringExt},
    rc::Rc,
};

const CPUID_RDRAND_FLAG: u32 = 1 << 30;
const CPUID_RTM_FLAG: u32 = 1 << 11;
const CPUID_RDSEED_FLAG: u32 = 1 << 18;
const CPUID_XSAVEOPT_FLAG: u32 = 1 << 0;

#[derive(Clone, Eq, PartialEq)]
pub struct DisableCPUIDFeatures {
    /// in: EAX=0x01
    features_ecx: u32,
    features_edx: u32,
    /// in: EAX=0x07 ECX=0
    extended_features_ebx: u32,
    extended_features_ecx: u32,
    extended_features_edx: u32,
    /// in: EAX=0x0D ECX=1
    xsave_features_eax: u32,
}

impl Default for DisableCPUIDFeatures {
    fn default() -> Self {
        Self::new()
    }
}

impl DisableCPUIDFeatures {
    pub fn new() -> Self {
        Self {
            features_ecx: 0,
            features_edx: 0,
            extended_features_ebx: 0,
            extended_features_ecx: 0,
            extended_features_edx: 0,
            xsave_features_eax: 0,
        }
    }

    pub fn from(features: (u32, u32), features_ext: (u32, u32, u32), features_xsave: u32) -> Self {
        Self {
            features_ecx: features.0,
            features_edx: features.1,
            extended_features_ebx: features_ext.0,
            extended_features_ecx: features_ext.1,
            extended_features_edx: features_ext.2,
            xsave_features_eax: features_xsave,
        }
    }

    pub fn any_features_disabled(&self) -> bool {
        self.features_ecx != 0
            || self.features_edx != 0
            || self.extended_features_ebx != 0
            || self.extended_features_ecx != 0
            || self.extended_features_edx != 0
            || self.xsave_features_eax != 0
    }

    pub fn amend_cpuid_data(&self, eax_in: u32, ecx_in: u32, cpuid_data: &mut CPUIDData) {
        match eax_in {
            CPUID_GETFEATURES => {
                cpuid_data.ecx &= !(CPUID_RDRAND_FLAG | self.features_ecx);
                cpuid_data.edx &= !self.features_edx;
            }
            CPUID_GETEXTENDEDFEATURES => {
                if ecx_in == 0 {
                    cpuid_data.ebx &=
                        !(CPUID_RDSEED_FLAG | CPUID_RTM_FLAG | self.extended_features_ebx);
                    cpuid_data.ecx &= !self.extended_features_ecx;
                    cpuid_data.edx &= !self.extended_features_edx;
                }
            }
            CPUID_GETXSAVE => {
                if ecx_in == 1 {
                    // Always disable XSAVEOPT because it's nondeterministic,
                    // possibly depending on context switching behavior. Intel
                    // recommends not using it from user space.
                    cpuid_data.eax &= !(CPUID_XSAVEOPT_FLAG | self.xsave_features_eax);
                }
            }
            _ => (),
        }
    }
}

#[derive(Clone, Debug)]
pub struct TraceUuid {
    pub bytes: [u8; 16],
}

impl TraceUuid {
    pub fn inner_bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn generate_new() -> TraceUuid {
        let mut bytes = [0u8; 16];
        good_random(&mut bytes);
        TraceUuid { bytes }
    }

    pub fn zero() -> TraceUuid {
        let bytes = [0u8; 16];
        TraceUuid { bytes }
    }

    pub fn from_array(bytes: [u8; 16]) -> TraceUuid {
        TraceUuid { bytes }
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub enum SyscallBuffering {
    EnableSycallBuf,
    DisableSyscallBuf,
}

/// DIFF NOTE: Subsumes RecordResult and RecordStatus from rr
#[derive(Clone, Eq, PartialEq)]
pub enum RecordResult {
    /// Some execution was recorded. record_step() can be called again.
    StepContinue,
    /// All tracees are dead. record_step() should not be called again.
    StepExited(WaitStatus),
    /// Spawning the initial tracee failed. The OsString represents the error message.
    StepSpawnFailed(OsString),
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub enum ContinueType {
    DontContinue,
    Continue,
    ContinueSyscall,
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub struct StepState {
    continue_type: ContinueType,
}

pub struct RecordSession {
    session_inner: SessionInner,
    trace_out: RefCell<TraceWriter>,
    scheduler_: RefCell<Scheduler>,
    initial_thread_group: Option<ThreadGroupSharedPtr>,
    seccomp_filter_rewriter_: SeccompFilterRewriter,
    trace_id: Box<TraceUuid>,
    disable_cpuid_features_: DisableCPUIDFeatures,
    /// DIFF NOTE: In rr, a None is indicated by value 0
    ignore_sig: Option<Sig>,
    /// DIFF NOTE: In rr, a None is indicated by value 0
    continue_through_sig: Option<Sig>,
    last_task_switchable: Switchable,
    syscall_buffer_size_: usize,
    syscallbuf_desched_sig_: Sig,
    use_syscall_buffer_: bool,

    use_file_cloning_: bool,
    use_read_cloning_: bool,
    /// When true, try to increase the probability of finding bugs.
    enable_chaos_: bool,
    asan_active_: bool,
    /// When true, wait for all tracees to exit before finishing recording.
    wait_for_all_: bool,

    /// DIFF NOTE: This is simply a normal string in rr.
    /// `None` means the user did not provide any trace dir options and we need
    /// to use the default trace dir.
    output_trace_dir: Option<OsString>,
}

impl Drop for RecordSession {
    fn drop(&mut self) {
        unimplemented!()
    }
}

impl RecordSession {
    /// DIFF NOTE:
    /// - The param list is much simpler than rr RecordSession::RecordSession. Takes the
    ///   whole RecordCommand for simplicity.
    /// - This method also incorporates functionality from rr setup_session_from_flags()
    ///   method
    pub fn new(
        exe_path: &OsStr,
        // We don't use flags.extra_env. We augment flags.extra_env producing `envp`.
        envp: &[(OsString, OsString)],
        flags: &RecordCommand,
        asan_active: bool,
    ) -> SessionSharedPtr {
        let sched = Scheduler::new(flags.max_ticks, flags.always_switch);

        if flags.scarce_fds {
            for _ in 0..950 {
                // DIFF NOTE: rr swallows any errors on open. We don't for now.
                open("/dev/null", OFlag::O_RDONLY, Mode::empty()).unwrap();
            }
        }

        let mut rec_sess = RecordSession {
            session_inner: SessionInner::new(),
            trace_out: RefCell::new(TraceWriter::new(
                &flags.args[0],
                choose_cpu(flags.bind_cpu),
                flags.output_trace_dir.as_deref(),
                TicksSemantics::default(),
            )),
            scheduler_: RefCell::new(sched),
            initial_thread_group: Default::default(),
            seccomp_filter_rewriter_: SeccompFilterRewriter,
            trace_id: flags.trace_id.clone(),
            disable_cpuid_features_: flags.disable_cpuid_features.clone(),
            ignore_sig: flags.ignore_sig,
            continue_through_sig: flags.continue_through_sig,
            last_task_switchable: Switchable::PreventSwitch,
            syscall_buffer_size_: flags.syscall_buffer_size,
            syscallbuf_desched_sig_: flags.syscallbuf_desched_sig,
            use_syscall_buffer_: flags.use_syscall_buffer == SyscallBuffering::EnableSycallBuf,
            use_file_cloning_: flags.use_file_cloning,
            use_read_cloning_: flags.use_read_cloning,
            enable_chaos_: Default::default(),
            asan_active_: asan_active,
            wait_for_all_: flags.wait_for_all,
            output_trace_dir: flags.output_trace_dir.clone(),
        };

        if !SessionInner::has_cpuid_faulting()
            && rec_sess.disable_cpuid_features_.any_features_disabled()
        {
            fatal!("CPUID faulting required to disable CPUID features");
        }

        // CPU affinity has been set.
        rec_sess.trace_out.borrow_mut().setup_cpuid_records(
            SessionInner::has_cpuid_faulting(),
            &flags.disable_cpuid_features,
        );

        let env: Vec<OsString> = envp
            .iter()
            .map(|(k, v)| -> OsString {
                let mut kv = k.clone();
                kv.push("=");
                kv.push(v);
                kv
            })
            .collect();
        let error_fd: ScopedFd = rec_sess.create_spawn_task_error_pipe();
        let socket_fd = rec_sess.tracee_socket_fd();

        let mut rc: SessionSharedPtr = Rc::new(Box::new(rec_sess));
        let weak_self = Rc::downgrade(&rc);
        // We never change the weak_self pointer so its a good idea to use
        // a bit of unsafe here otherwise we would unecessarily need a RefCell.
        let rs = unsafe {
            let s = Rc::get_mut_unchecked(&mut rc);
            s.weak_self = weak_self.clone();
            // Use this to also set things that shouldn't change.
            s.as_record_mut().unwrap()
        };

        rs.scheduler_mut().set_session_weak_ptr(weak_self);

        if flags.chaos {
            rs.scheduler_mut().set_enable_chaos(flags.chaos);
        }

        match flags.num_cores {
            Some(num_cores) => {
                // Set the number of cores reported, possibly overriding the chaos mode
                // setting.
                rs.scheduler_mut().set_num_cores(num_cores);
            }
            // This is necessary for the default case
            None => rs.scheduler_mut().regenerate_affinity_mask(),
        }

        let t = TaskInner::spawn(
            (*rc).as_ref(),
            &error_fd,
            socket_fd,
            SaveTraceeFdNumber::SaveToSession,
            exe_path,
            &flags.args,
            &env,
            None,
        );
        // The initial_thread_group is set only once so its worth it to use
        // unsafe
        unsafe {
            Rc::get_mut_unchecked(&mut rc)
                .as_record_mut()
                .unwrap()
                .initial_thread_group = Some(t.borrow().thread_group_shr_ptr());
        }
        rc.on_create_task(t);
        rc
    }

    /// Create a recording session for the initial command line argv.
    ///
    /// DIFF NOTE: Param list very different from rr.
    /// Takes the whole &RecordCommand for simplicity.
    pub fn create(options: &RecordCommand) -> SessionSharedPtr {
        // The syscallbuf library interposes some critical
        // external symbols like XShmQueryExtension(), so we
        // preload it whether or not syscallbuf is enabled. Indicate here whether
        // syscallbuf is enabled.
        if options.use_syscall_buffer == SyscallBuffering::DisableSyscallBuf {
            env::remove_var(SYSCALLBUF_ENABLED_ENV_VAR);
        } else {
            env::set_var(SYSCALLBUF_ENABLED_ENV_VAR, "1");
            check_perf_event_paranoid();
        }

        let mut env: Vec<(OsString, OsString)> = env::vars_os().collect();
        env.extend_from_slice(&options.extra_env);

        let full_path = lookup_by_path(&options.args[0]);
        let exe_info: ExeInfo = read_exe_info(&full_path);

        // LD_PRELOAD the syscall interception lib
        let maybe_syscall_buffer_lib_path = find_helper_library(SYSCALLBUF_LIB_FILENAME);
        match maybe_syscall_buffer_lib_path {
            Some(syscall_buffer_lib_path) => {
                let mut ld_preload = Vec::<u8>::new();
                match &exe_info.libasan_path {
                    Some(asan_path) => {
                        log!(LogDebug, "Prepending {:?} to LD_PRELOAD", asan_path);
                        // Put an LD_PRELOAD entry for it before our preload library, because
                        // it checks that it's loaded first
                        ld_preload.extend_from_slice(asan_path.as_bytes());
                        ld_preload.push(b':');
                    }
                    None => (),
                }

                ld_preload.extend_from_slice(syscall_buffer_lib_path.as_bytes());
                ld_preload.extend_from_slice(SYSCALLBUF_LIB_FILENAME_PADDED.as_bytes());
                inject_ld_helper_library(&mut env, &OsStr::new("LD_PRELOAD"), ld_preload);
            }
            None => (),
        }

        env.push(("RUNNING_UNDER_RD".into(), "1".into()));
        // Stop Mesa using the GPU
        env.push(("LIBGL_ALWAYS_SOFTWARE".into(), "1".into()));
        // Stop sssd from using shared-memory with its daemon
        env.push(("SSS_NSS_USE_MEMCACHE".into(), "NO".into()));

        // Disable Gecko's "wait for gdb to attach on process crash" behavior, since
        // it is useless when running under rr.
        env.push(("MOZ_GDB_SLEEP".into(), "0".into()));

        // If we have CPUID faulting, don't use these environment hacks. We don't
        // need them and the user might want to use them themselves for other reasons.
        if !SessionInner::has_cpuid_faulting() {
            // OpenSSL uses RDRAND, but we can disable it. These bitmasks are inverted
            // and ANDed with the results of CPUID. The number below is 2^62, which is the
            // bit for RDRAND support.
            env.push(("OPENSSL_ia32cap".into(), "~4611686018427387904:~0".into()));
            // Disable Qt's use of RDRAND/RDSEED/RTM
            env.push(("QT_NO_CPU_FEATURE".into(), "rdrand rdseed rtm".into()));
        }

        RecordSession::new(
            &full_path,
            &env,
            options,
            exe_info.has_asan_symbols || exe_info.libasan_path.is_some(),
        )
    }

    pub fn disable_cpuid_features(&self) -> &DisableCPUIDFeatures {
        &self.disable_cpuid_features_
    }

    pub fn use_syscall_buffer(&self) -> bool {
        self.use_syscall_buffer_
    }

    pub fn syscall_buffer_size(&self) -> usize {
        self.syscall_buffer_size_
    }

    pub fn syscallbuf_desched_sig(&self) -> Sig {
        self.syscallbuf_desched_sig_
    }

    pub fn use_read_cloning(&self) -> bool {
        self.use_read_cloning_
    }

    pub fn use_file_cloning(&self) -> bool {
        self.use_file_cloning_
    }

    pub fn set_ignore_sig(&mut self, maybe_sig: Option<Sig>) {
        self.ignore_sig = maybe_sig;
    }

    pub fn get_ignore_sig(&self) -> Option<Sig> {
        self.ignore_sig
    }

    pub fn set_continue_through_sig(&mut self, maybe_sig: Option<Sig>) {
        self.continue_through_sig = maybe_sig;
    }

    pub fn get_continue_through_sig(&self) -> Option<Sig> {
        self.continue_through_sig
    }

    pub fn set_asan_active(&mut self, active: bool) {
        self.asan_active_ = active;
    }

    pub fn asan_active(&self) -> bool {
        self.asan_active_
    }

    pub fn rd_signal_mask() -> u64 {
        unimplemented!()
    }

    /// Record some tracee execution.
    /// This may block. If blocking is interrupted by a signal, will return
    /// StepContinue.
    /// Typically you'd call this in a loop until it returns something other than
    /// StepContinue.
    /// Note that when this returns, some tasks may be running (not in a ptrace-
    /// stop). In particular, up to one task may be executing user code and any
    /// number of tasks may be blocked in syscalls.
    pub fn record_step(&self) -> RecordResult {
        unimplemented!()
    }

    /// Flush buffers and write a termination record to the trace. Don't call
    /// record_step() after this.
    pub fn terminate_recording(&self) {
        unimplemented!()
    }

    /// Close trace output without flushing syscall buffers or writing
    /// task exit/termination records to the trace.
    pub fn close_trace_writer(_status: CloseStatus) {
        unimplemented!()
    }

    pub fn trace_writer(&self) -> Ref<'_, TraceWriter> {
        self.trace_out.borrow()
    }

    pub fn trace_writer_mut(&self) -> RefMut<'_, TraceWriter> {
        self.trace_out.borrow_mut()
    }

    pub fn scheduler(&self) -> Ref<'_, Scheduler> {
        self.scheduler_.borrow()
    }

    pub fn scheduler_mut(&self) -> RefMut<'_, Scheduler> {
        self.scheduler_.borrow_mut()
    }

    pub fn seccomp_filter_rewriter(&self) -> &SeccompFilterRewriter {
        &self.seccomp_filter_rewriter_
    }

    pub fn set_enable_chaos(&mut self, enable_chaos: bool) {
        self.scheduler_mut().set_enable_chaos(enable_chaos);
        self.enable_chaos_ = enable_chaos;
    }

    pub fn enable_chaos(&self) -> bool {
        self.enable_chaos_
    }

    pub fn set_num_cores(&mut self, num_cores: u32) {
        self.scheduler_mut().set_num_cores(num_cores);
    }

    pub fn set_use_read_cloning(&mut self, enable: bool) {
        self.use_read_cloning_ = enable;
    }

    pub fn set_use_file_cloning(&mut self, enable: bool) {
        self.use_file_cloning_ = enable;
    }

    pub fn set_syscall_buffer_size(&mut self, size: usize) {
        self.syscall_buffer_size_ = size;
    }

    pub fn set_wait_for_all(&mut self, wait_for_all: bool) {
        self.wait_for_all_ = wait_for_all;
    }

    /// This gets called when we detect that a task has been revived from the
    /// dead with a PTRACE_EVENT_EXEC. See ptrace man page under "execve(2) under
    /// ptrace" for the horrid details.
    ///
    /// The task in the thread-group that triggered the successful execve has changed
    /// its tid to |rec_tid|. We mirror that, and emit TraceTaskEvents to make it
    /// look like a new task was spawned and the old task exited.
    pub fn revive_task_for_exec(&self, _rec_tid: pid_t) -> &RecordTask {
        unimplemented!()
    }
}

impl Deref for RecordSession {
    type Target = SessionInner;

    fn deref(&self) -> &Self::Target {
        &self.session_inner
    }
}

impl DerefMut for RecordSession {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.session_inner
    }
}

impl Session for RecordSession {
    fn as_record(&self) -> Option<&RecordSession> {
        Some(self)
    }

    fn as_record_mut(&mut self) -> Option<&mut RecordSession> {
        Some(self)
    }

    /// Forwarded method
    fn kill_all_tasks(&self) {
        kill_all_tasks(self)
    }

    fn on_destroy_task(&self, _t: TaskUid) {
        unimplemented!()
    }

    fn as_session_inner(&self) -> &SessionInner {
        &self.session_inner
    }

    fn as_session_inner_mut(&mut self) -> &mut SessionInner {
        &mut self.session_inner
    }

    fn new_task(
        &self,
        tid: pid_t,
        _rec_tid: Option<pid_t>,
        serial: u32,
        a: SupportedArch,
    ) -> Box<dyn Task> {
        RecordTask::new(self, tid, serial, a)
    }

    fn on_create_task(&self, t: TaskSharedPtr) {
        on_create_task_common(self, t.clone());
        self.scheduler_mut().on_create_task(t);
    }

    fn trace_stream(&self) -> Option<Ref<'_, TraceStream>> {
        let r = self.trace_out.borrow();
        Some(Ref::map(r, |t| t.deref()))
    }

    fn trace_stream_mut(&self) -> Option<RefMut<'_, TraceStream>> {
        let r = self.trace_out.borrow_mut();
        Some(RefMut::map(r, |t| t.deref_mut()))
    }
}

fn check_perf_event_paranoid() {
    let fd = ScopedFd::open_path("/proc/sys/kernel/perf_event_paranoid", OFlag::O_RDONLY);
    if fd.is_open() {
        let mut buf = [0u8; 100];
        match read(fd.as_raw(), &mut buf) {
            Ok(siz) if siz != 0 => {
                let int_str = String::from_utf8_lossy(&buf[0..siz]);
                let maybe_val = int_str.trim().parse::<usize>();
                match maybe_val {
                    Ok(val) if val > 1 => {
                        clean_fatal!("rd needs `/proc/sys/kernel/perf_event_paranoid` <= 1, but it is {}.\n\
                                      Change it to 1, or use 'rd record -n' (slow).\n\
                                      Consider putting 'kernel.perf_event_paranoid = 1' in /etc/sysctl.conf", val);
                    }
                    Err(e) => {
                        clean_fatal!(
                            "Error while parsing file `/proc/sys/kernel/perf_event_paranoid`: {:?}",
                            e
                        );
                    }
                    _ => (),
                }
            }
            // @TODO This should actually be just Ok(0) but Rust doesn't accept it and says
            // patterns are not exhaustive.
            Ok(_) => {
                clean_fatal!(
                    "Read 0 bytes from `/proc/sys/kernel/perf_event_paranoid`.\n\
                             Need to read non-zero number of bytes."
                );
            }
            Err(e) => {
                clean_fatal!(
                    "Error while reading file `/proc/sys/kernel/perf_event_paranoid`: {:?}",
                    e
                );
            }
        }
    } else {
        log!(
            LogError,
            "Could not open `/proc/sys/kernel/perf_event_paranoid`. Continuing anyway."
        );
    }
}

fn find_helper_library<T: AsRef<OsStr>>(basepath: T) -> Option<OsString> {
    for suffix in &["lib64/rd/", "lib64/rr/", "lib/rd/", "lib/rr"] {
        let mut lib_path = OsString::from(resource_path());
        lib_path.push(suffix);
        let mut file_name = OsString::from(lib_path.clone());
        file_name.push(basepath.as_ref());
        if access(file_name.as_bytes(), AccessFlags::F_OK).is_ok() {
            return Some(lib_path);
        }
    }
    // File does not exist. Assume install put it in LD_LIBRARY_PATH.
    None
}

#[derive(Clone, Debug, Default)]
struct ExeInfo {
    libasan_path: Option<OsString>,
    has_asan_symbols: bool,
}

fn read_exe_info<T: AsRef<OsStr>>(full_path: T) -> ExeInfo {
    let maybe_data = fs::read(full_path.as_ref());

    let data = match maybe_data {
        Err(e) => fatal!("Error while reading {:?}: {:?}", full_path.as_ref(), e),
        Ok(data) => data,
    };

    match Elf::parse(&data) {
        Err(e) => fatal!("Error while Elf parsing {:?}: {:?}", full_path.as_ref(), e),
        Ok(elf_file) => match elf_file.dynamic {
            Some(dyns) => {
                let mut maybe_libasan_path = None;
                let mut has_asan_init = false;
                for lib in dyns.get_libraries(&elf_file.dynstrtab) {
                    // @TODO Is contains() OK?
                    if lib.contains("libasan") {
                        maybe_libasan_path = Some(OsString::from(lib));
                        break;
                    }
                }
                for s in elf_file.dynsyms.iter() {
                    match elf_file.dynstrtab.get(s.st_name) {
                        Some(name_res) => match name_res {
                            Ok(name) => {
                                if name == "__asan_init" {
                                    has_asan_init = true;
                                    break;
                                }
                            }
                            Err(_) => (),
                        },
                        None => {}
                    }
                }
                ExeInfo {
                    libasan_path: maybe_libasan_path,
                    has_asan_symbols: has_asan_init,
                }
            }
            None => ExeInfo {
                libasan_path: None,
                has_asan_symbols: false,
            },
        },
    }
}

fn lookup_by_path<T: AsRef<OsStr>>(file: T) -> OsString {
    let file_ostr = file.as_ref();
    if find(file_ostr.as_bytes(), b"/").is_some() {
        return file_ostr.to_owned();
    }
    match env::var_os("PATH") {
        Some(path) => {
            let path_vec = path.into_vec();
            let dirs = path_vec.split(|&c| c == b':');
            for dir in dirs {
                let mut full_path = Vec::<u8>::new();
                full_path.extend_from_slice(dir);
                full_path.push(b'/');
                full_path.extend_from_slice(file_ostr.as_bytes());

                match stat(full_path.as_slice()) {
                    Ok(st) if st.st_mode & S_IFREG == S_IFREG => {
                        if access(full_path.as_slice(), AccessFlags::X_OK).is_ok() {
                            return OsString::from_vec(full_path);
                        } else {
                            continue;
                        }
                    }
                    _ => continue,
                }
            }
            file_ostr.to_owned()
        }
        None => file_ostr.to_owned(),
    }
}

fn inject_ld_helper_library(env: &mut Vec<(OsString, OsString)>, name: &OsStr, val: Vec<u8>) {
    // Our preload lib should come first if possible, because that will speed up
    // the loading of the other libraries; it's also a good idea to put our audit
    // library at the head of the list, since there's only sixteen possible link
    // namespaces on glibc and each audit library uses up one.
    //
    // We supply a placeholder which is then mutated to the correct filename in
    // Monkeypatcher::patch_after_exec.
    let mut found = false;
    for (key, curr_value) in env.iter_mut() {
        if key == name {
            let mut new_value = Vec::new();
            new_value.extend_from_slice(&val);
            new_value.push(b':');
            new_value.extend_from_slice(&val);
            curr_value.clear();
            curr_value.push(OsStr::from_bytes(&new_value));
            found = true;
            break;
        }
    }

    if !found {
        env.push((OsString::from(name), OsString::from_vec(val)))
    }
}

union USiginfo {
    native_api: native_arch::siginfo_t,
    linux_api: siginfo_t,
}

fn handle_seccomp_trap(t: &mut RecordTask, step_state: &mut StepState, seccomp_data: u16) {
    // The architecture may be wrong, but that's ok, because an actual syscall
    // entry did happen, so the registers are already updated according to the
    // architecture of the system call.
    let arch = t.detect_syscall_arch();
    t.canonicalize_regs(arch);

    let mut r = t.regs_ref().clone();
    let syscallno = r.original_syscallno() as i32;
    // Cause kernel processing to skip the syscall
    r.set_original_syscallno(SECCOMP_MAGIC_SKIP_ORIGINAL_SYSCALLNO);
    t.set_regs(&r);

    let mut syscall_entry_already_recorded = false;
    if t.ev().is_syscall_event() {
        // A syscall event was already pushed, probably because we did a
        // PTRACE_SYSCALL to enter the syscall during handle_desched_event. Cancel
        // that event now since the seccomp SIGSYS aborts it completely.
        ed_assert_eq!(t, t.ev().syscall().number, syscallno);
        // Make sure any prepared syscall state is discarded and any temporary
        // effects (e.g. redirecting pointers to scratch) undone.
        rec_abort_prepared_syscall(t);
        if t.ev().event_type() == EventType::EvSyscallInterruption {
            // The event could be a syscall-interruption if it was pushed by
            // `handle_desched_event`. In that case, it has not been recorded yet.
            t.pop_syscall_interruption();
        } else {
            t.pop_syscall();
            syscall_entry_already_recorded = true;
        }
    }

    if t.is_in_untraced_syscall() {
        ed_assert!(t, !t.delay_syscallbuf_reset_for_seccomp_trap);
        // Don't reset the syscallbuf immediately after delivering the trap. We have
        // to wait until this buffered syscall aborts completely before resetting
        // the buffer.
        t.delay_syscallbuf_reset_for_seccomp_trap = true;

        t.push_event(Event::seccomp_trap());

        // desched may be armed but we're not going to execute the syscall, let
        // alone block. If it fires, ignore it.
        let syscallbuf_child = t.syscallbuf_child;
        write_val_mem(
            t,
            RemotePtr::<u8>::cast(syscallbuf_child)
                + offset_of!(syscallbuf_hdr, desched_signal_may_be_relevant),
            &0u8,
            None,
        );
    }

    t.push_syscall_event(syscallno);
    t.ev_mut().syscall_mut().failed_during_preparation = true;
    note_entering_syscall(t);

    if t.is_in_untraced_syscall() && !syscall_entry_already_recorded {
        t.record_current_event();
    }

    // Use NativeArch here because different versions of system headers
    // have inconsistent field naming.
    let mut si: USiginfo = unsafe { mem::zeroed() };
    si.native_api.si_signo = SIGSYS;
    si.native_api.si_errno = seccomp_data as i32;
    si.native_api.si_code = SYS_SECCOMP as i32;
    match r.arch() {
        SupportedArch::X86 => si.native_api._sifields._sigsys._arch = AUDIT_ARCH_I386,
        SupportedArch::X64 => si.native_api._sifields._sigsys._arch = AUDIT_ARCH_X86_64,
    }
    si.native_api._sifields._sigsys._syscall = syscallno;
    // Documentation says that si_call_addr is the address of the syscall
    // instruction, but in tests it's immediately after the syscall
    // instruction.
    si.native_api._sifields._sigsys._call_addr =
        native_arch::ptr::<Void>::from_remote_ptr(t.ip().to_data_ptr::<Void>());
    log!(LogDebug, "Synthesizing {}", unsafe { si.linux_api });
    t.stash_synthetic_sig(
        unsafe { &si.linux_api },
        SignalDeterministic::DeterministicSig,
    );

    // Tests show that the current registers are preserved (on x86, eax/rax
    // retains the syscall number).
    r.set_syscallno(syscallno as isize);
    t.set_regs(&r);
    t.maybe_restore_original_syscall_registers();

    if t.is_in_untraced_syscall() {
        // For buffered syscalls, go ahead and record the exit state immediately.
        t.ev_mut().syscall_mut().state = SyscallState::ExitingSyscall;
        t.record_current_event();
        t.pop_syscall();

        // The tracee is currently in the seccomp ptrace-stop. Advance it to the
        // syscall-exit stop so that when we try to deliver the SIGSYS via
        // PTRACE_SINGLESTEP, that doesn't trigger a SIGTRAP stop.
        t.resume_execution(
            ResumeRequest::ResumeSyscall,
            WaitRequest::ResumeWait,
            TicksRequest::ResumeNoTicks,
            None,
        );
    }

    // Don't continue yet. At the next iteration of record_step, if we
    // recorded the syscall-entry we'll enter syscall_state_changed and
    // that will trigger a continue to the syscall exit. If we recorded the
    // syscall-exit we'll go straight into signal delivery.
    step_state.continue_type = ContinueType::DontContinue;
}

fn note_entering_syscall(_t: &mut RecordTask) {
    unimplemented!()
}

fn rec_abort_prepared_syscall(_t: &mut RecordTask) {
    unimplemented!()
}
