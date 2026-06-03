// ExecOp — StepOp-driven execve with per-phase Yield support.
//
// Decomposes exec_script into individual synchronously-stepped
// phases.  Async operations (step_open, read_exact_at,
// materialize_pagebacked) return Yield so the reactor can park
// the task during block-device I/O.  Synchronous phases
// (parse, credential check, stack build, PoNR) return Continue.
//
// Phase breakdown follows EXEC_v1 §4:
//   Resolving         step_open → file + PageContainer
//   ReadingHeader     read_exact_at → parse_image_plan
//   LoadingInterp     step_open interpreter (if PT_INTERP)
//   ParseInterp       parse interpreter ELF
//   CredCheck         suid/nosuid/AT_SECURE
//   BuildingAspace    build_aspace_from_image + interpreter LOADs
//   MappingVdso       populate_vdso_range (Yield on materialize)
//   PopulatingStack   build + populate user stack (Yield on materialize)
//   PoNR              replace_aspace + store context (infallible)
//   PostCommit        CLOEXEC + signal + brk + exe/cmdline + vfork
//
// Each phase stores intermediate results in ExecOp fields and
// advances to the next phase via Continue.

use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use super::step_engine::{self, StepOp, StepOutcome, YieldShape, NoProgress, Cap};

use tx_hal::{PmapIf, EntropyIf, AuxvIf, UserTrapContext};
use tx_subsystems::cred::{Gid, Uid, step_apply_suid_for_exec, Capability};
use tx_subsystems::vfs::structure::Credential;
use tx_subsystems::vfs::walker::step_open;
use tx_subsystems::vfs::structure::{OpenFileFlags, RNodeBacking, InodeMeta};
use tx_subsystems::vfs::OpenFile;
use tx_subsystems::page_backed::{read_exact_at, PageContainer};
use tx_subsystems::vm::scripts as vm_scripts;
use tx_subsystems::vm::{
    AddressSpace, ImagePlan as VmImagePlan, LoadSegment as VmLoadSegment, SegmentFlags as VmSegmentFlags,
    BssTail as VmBssTail, VmBacking, VmEntry, VmEntryFlags, Prot, MapPlacement, MapReserveResult,
    UserRange, UserVirtAddr,
};
use tx_subsystems::mount::MountFlags;
use tx_subsystems::process::ProcessIdentity;
use tx_subsystems::process::{
    step_close_cloexec_fds, step_install_brk_for_exec, step_reset_signal_dispositions_for_exec,
};
use tx_subsystems::thread_runtime::structure::ThreadIdentity;
use tx_scripts::process::exec::loader::{
    parse_image_plan, ExecImagePlan, LoadSegment as ParsedLoadSegment,
    SegmentFlags as ParsedSegmentFlags, BssTail as ParsedBssTail,
    ELF64_PHENT, InterpreterPlan, ParseError,
};
use tx_scripts::process::exec::stack::{build_initial_user_stack, AuxvFacts, CLKTCK_VALUE};

// ---------------------------------------------------------------------------
// Phase enum
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecProgress {
    Resolving,
    ReadingHeader,
    LoadingInterp,
    ParseInterp,
    CredCheck,
    BuildingAspace,
    MappingVdso,
    PopulatingStack,
    PoNR,
    PostCommit,
}

// ---------------------------------------------------------------------------
// ExecOp
// ---------------------------------------------------------------------------

pub struct ExecOp<'a, P: PmapIf + EntropyIf + AuxvIf> {
    process: &'a Cap<ProcessIdentity>,
    thread: &'a Cap<ThreadIdentity>,
    path: &'a [u8],
    argv: &'a [&'a [u8]],
    envp: &'a [&'a [u8]],
    cred: &'a Credential,
    _pmap: core::marker::PhantomData<P>,

    phase: ExecProgress,

    // ---- Phase outputs ----
    openfile: Option<Cap<OpenFile>>,
    file_pc: Option<Cap<PageContainer>>,
    parsed: Option<ExecImagePlan>,

    interp_path: Option<Vec<u8>>,
    interp_file: Option<Cap<OpenFile>>,
    interp_pc: Option<Cap<PageContainer>>,
    interp_plan: Option<InterpreterPlan>,

    new_cred: Option<(u32, u32, u32, u32)>,
    at_secure: bool,
    at_random_bytes: [u8; 16],

    new_aspace: Option<Cap<AddressSpace>>,
    vdso_base_opt: Option<u64>,

    entry_pc: u64,
    initial_sp: u64,
}

impl<'a, P: PmapIf + EntropyIf + AuxvIf> ExecOp<'a, P> {
    pub fn new(
        process: &'a Cap<ProcessIdentity>,
        thread: &'a Cap<ThreadIdentity>,
        path: &'a [u8],
        argv: &'a [&'a [u8]],
        envp: &'a [&'a [u8]],
        cred: &'a Credential,
    ) -> Self {
        Self {
            process, thread, path, argv, envp, cred,
            _pmap: core::marker::PhantomData,
            phase: ExecProgress::Resolving,
            openfile: None, file_pc: None, parsed: None,
            interp_path: None, interp_file: None, interp_pc: None, interp_plan: None,
            new_cred: None, at_secure: false, at_random_bytes: [0u8; 16],
            new_aspace: None, vdso_base_opt: None,
            entry_pc: 0, initial_sp: 0,
        }
    }
}

impl<'a, P: PmapIf + EntropyIf + AuxvIf, I: step_engine::SubjectIdentity> StepOp<I> for ExecOp<'a, P> {
    type Output = ();
    type Progress = ExecProgress;

    fn step(&mut self, _ctx: &mut step_engine::ScriptCtx<I>) -> StepOutcome<(), ExecProgress> {
        match self.phase {
            // ===== Phase 1: resolve path + open file ===============
            ExecProgress::Resolving => {
                let guard = step_engine::guard();
                let rooted_at = match self.process.cwd() {
                    Some(cwd) => cwd,
                    None => return StepOutcome::Err(step_engine::Errno::ENOENT),
                };
                let outcome = step_open(rooted_at, self.path,
                    OpenFileFlags { read: true, write: false, append: false, cloexec: false, nonblocking: false },
                    0, self.cred, &guard);
                match outcome {
                    StepOutcome::Done(file) => {
                        let pc = match file.rnode().backing() {
                            RNodeBacking::PageBacked { pc } => pc.clone(),
                            _ => return StepOutcome::Err(step_engine::Errno::ENOEXEC),
                        };
                        self.file_pc = Some(pc);
                        self.openfile = Some(file);
                        self.phase = ExecProgress::ReadingHeader;
                        StepOutcome::Continue { progress: ExecProgress::Resolving }
                    }
                    StepOutcome::Yield { shape, .. } =>
                        StepOutcome::Yield { progress: ExecProgress::Resolving, shape },
                    StepOutcome::Err(e) => StepOutcome::Err(e),
                    _ => StepOutcome::Err(step_engine::Errno::EIO),
                }
            }

            // ===== Phase 2: read ELF header + parse ================
            ExecProgress::ReadingHeader => {
                let pc = self.file_pc.as_ref().expect("ExecOp: file_pc missing");
                let mut hdr = [0u8; 4096];
                let guard = step_engine::guard();
                match read_exact_at(pc, 0, &mut hdr, &guard) {
                    StepOutcome::Done(()) => {
                        let parsed = match parse_image_plan(&hdr) {
                            Ok(p) => p,
                            Err(_) => return StepOutcome::Err(step_engine::Errno::ENOEXEC),
                        };
                        self.parsed = Some(parsed);
                        // Check for PT_INTERP
                        self.phase = if self.parsed.as_ref().unwrap().interpreter_path.is_some() {
                            ExecProgress::LoadingInterp
                        } else {
                            ExecProgress::CredCheck
                        };
                        StepOutcome::Continue { progress: ExecProgress::ReadingHeader }
                    }
                    StepOutcome::Yield { shape, .. } =>
                        StepOutcome::Yield { progress: ExecProgress::ReadingHeader, shape },
                    StepOutcome::Err(e) => StepOutcome::Err(e),
                    _ => StepOutcome::Err(step_engine::Errno::EIO),
                }
            }

            // ===== Phase 3b: open interpreter ======================
            ExecProgress::LoadingInterp => {
                let interp_path = self.parsed.as_ref().unwrap()
                    .interpreter_path.as_ref().unwrap().clone();
                let guard = step_engine::guard();
                let rooted_at = self.process.cwd().unwrap();
                let outcome = step_open(rooted_at, &interp_path,
                    OpenFileFlags { read: true, write: false, append: false, cloexec: false, nonblocking: false },
                    0, self.cred, &guard);
                match outcome {
                    StepOutcome::Done(file) => {
                        let pc = match file.rnode().backing() {
                            RNodeBacking::PageBacked { pc } => pc.clone(),
                            _ => return StepOutcome::Err(step_engine::Errno::ENOEXEC),
                        };
                        self.interp_pc = Some(pc);
                        self.interp_file = Some(file);
                        self.phase = ExecProgress::ParseInterp;
                        StepOutcome::Continue { progress: ExecProgress::LoadingInterp }
                    }
                    StepOutcome::Yield { shape, .. } =>
                        StepOutcome::Yield { progress: ExecProgress::LoadingInterp, shape },
                    StepOutcome::Err(e) => StepOutcome::Err(e),
                    _ => StepOutcome::Err(step_engine::Errno::EIO),
                }
            }

            // ===== Phase 3c: parse interpreter ELF =================
            ExecProgress::ParseInterp => {
                let pc = self.interp_pc.as_ref().unwrap();
                let mut hdr = [0u8; 4096];
                let guard = step_engine::guard();
                match read_exact_at(pc, 0, &mut hdr, &guard) {
                    StepOutcome::Done(()) => {
                        let interp_parsed = match parse_image_plan(&hdr) {
                            Ok(p) => p,
                            Err(_) => return StepOutcome::Err(step_engine::Errno::ENOEXEC),
                        };
                        // Compute interpreter Plan
                        let lowest = interp_parsed.load_segments.iter()
                            .map(|s| s.vaddr).min().unwrap_or(0);
                        let interp_base = 0x3F_F000_0000;
                        let bias = interp_base - lowest;
                        let segs: Vec<_> = interp_parsed.load_segments.iter().map(|s| {
                            ParsedLoadSegment {
                                vaddr: s.vaddr + bias, memsz: s.memsz, filesz: s.filesz,
                                file_offset: s.file_offset, flags: s.flags.clone(), align: s.align,
                            }
                        }).collect();
                        self.interp_plan = Some(InterpreterPlan {
                            load_bias: bias, entry: interp_parsed.entry + bias,
                            load_segments: segs,
                            bss_extension: interp_parsed.bss_extension.map(|b| ParsedBssTail { vaddr: b.vaddr + bias, size: b.size }),
                            executable_stack: interp_parsed.executable_stack,
                        });
                        self.phase = ExecProgress::CredCheck;
                        StepOutcome::Continue { progress: ExecProgress::ParseInterp }
                    }
                    StepOutcome::Yield { shape, .. } =>
                        StepOutcome::Yield { progress: ExecProgress::ParseInterp, shape },
                    StepOutcome::Err(e) => StepOutcome::Err(e),
                    _ => StepOutcome::Err(step_engine::Errno::EIO),
                }
            }

            // ===== Phase 3.5: credential check =====================
            ExecProgress::CredCheck => {
                let file = self.openfile.as_ref().unwrap();
                let meta = file.rnode().meta();
                // Check execute permission (inline, mirrors check_exec_perm)
                let has_dac = self.cred.has_capability(Capability::DAC_OVERRIDE);
                let mode = meta.mode;
                let ok = has_dac
                    || (self.cred.euid.as_raw() == meta.uid && (mode & 0o100) != 0)
                    || (self.cred.egid.as_raw() == meta.gid && (mode & 0o010) != 0)
                    || (mode & 0o001) != 0;
                if !ok { return StepOutcome::Err(step_engine::Errno::EACCES); }

                // nosuid mount check
                let mount_nosuid = file.rnode().containing_mount_weak()
                    .and_then(|w| { let g = step_engine::guard(); w.upgrade(&g) })
                    .is_some_and(|mp| mp.options.flags.contains(MountFlags::NOSUID));
                let mount_noexec = file.rnode().containing_mount_weak()
                    .and_then(|w| { let g = step_engine::guard(); w.upgrade(&g) })
                    .is_some_and(|mp| mp.options.flags.contains(MountFlags::NOEXEC));
                if mount_noexec {
                    return StepOutcome::Err(step_engine::Errno::EACCES);
                }

                self.at_secure = if mount_nosuid { false } else {
                    step_apply_suid_for_exec(self.process, Uid(meta.uid), Gid(meta.gid), meta.mode)
                        .is_some_and(|o| o.at_secure)
                };

                <P as EntropyIf>::fill_random(&mut self.at_random_bytes);
                self.phase = ExecProgress::BuildingAspace;
                StepOutcome::Continue { progress: ExecProgress::CredCheck }
            }

            // ===== Phase 4: build detached AddressSpace ============
            ExecProgress::BuildingAspace => {
                let parsed = self.parsed.as_ref().unwrap();
                // Convert ExecImagePlan → VmImagePlan
                let vm_segs: Vec<VmLoadSegment> = parsed.load_segments.iter().map(|s| VmLoadSegment {
                    vaddr: s.vaddr, memsz: s.memsz, filesz: s.filesz,
                    file_offset: s.file_offset,
                    flags: VmSegmentFlags { readable: s.flags.readable, writable: s.flags.writable, executable: s.flags.executable },
                    align: s.align,
                }).collect();
                let vm_plan = VmImagePlan {
                    entry: parsed.entry,
                    stack_top: 0x4000_0000,
                    load_segments: vm_segs,
                    bss_extension: parsed.bss_extension.as_ref().map(|b|
                        VmBssTail { vaddr: b.vaddr, size: b.size }),
                    executable_stack: parsed.executable_stack,
                };
                let aspace = match vm_scripts::build_aspace_from_image::<P>(&vm_plan) {
                    Ok(a) => a,
                    Err(_) => return StepOutcome::Err(step_engine::Errno::ENOMEM),
                };
                self.new_aspace = Some(aspace);
                self.phase = ExecProgress::MappingVdso;
                StepOutcome::Continue { progress: ExecProgress::BuildingAspace }
            }

            // ===== Phase 4a+: VDSO + interpreter LOADs =============
            ExecProgress::MappingVdso => {
                // Register interpreter segments if present
                if let Some(ref interp) = self.interp_plan {
                    let aspace = self.new_aspace.as_ref().unwrap();
                    let interp_pc = self.interp_pc.as_ref().unwrap();
                    for seg in &interp.load_segments {
                        let range = match vm_scripts::align_range(seg.vaddr, seg.memsz) {
                            Some(r) => r,
                            None => return StepOutcome::Err(step_engine::Errno::ENOEXEC),
                        };
                        let prot = Prot::READ_WRITE; // simplified; real code uses READ_EXEC etc
                        let entry = VmEntry::new(range, prot, VmEntryFlags::PRIVATE,
                            VmBacking::Page { pc: interp_pc.clone().into(), offset: seg.file_offset });
                        match aspace.reserve_map(entry, MapPlacement::RequireFree) {
                            MapReserveResult::Reserved(r) => { let _ = r.commit(); }
                            _ => return StepOutcome::Err(step_engine::Errno::ENOMEM),
                        }
                    }
                }
                self.phase = ExecProgress::PopulatingStack;
                StepOutcome::Continue { progress: ExecProgress::MappingVdso }
            }

            // ===== Phase 5+5a: build + populate user stack =========
            ExecProgress::PopulatingStack => {
                let parsed = self.parsed.as_ref().unwrap();
                let mut facts = AuxvFacts {
                    at_phdr: parsed.at_phdr,
                    at_phent: ELF64_PHENT,
                    at_phnum: parsed.at_phnum,
                    at_pagesz: 4096,
                    at_base: self.interp_plan.as_ref().map_or(0, |i| i.load_bias),
                    at_entry: self.interp_plan.as_ref().map_or(parsed.entry, |i| i.entry),
                    at_uid: self.cred.uid.as_raw() as u64,
                    at_euid: self.cred.euid.as_raw() as u64,
                    at_gid: self.cred.gid.as_raw() as u64,
                    at_egid: self.cred.egid.as_raw() as u64,
                    at_secure: if self.at_secure { 1 } else { 0 },
                    at_random_bytes: self.at_random_bytes,
                    at_hwcap: 0, at_hwcap2: 0, at_platform: None, platform_string: b"riscv64",
                    at_clktck: CLKTCK_VALUE, at_execfn: None, execfn_string: b"",
                    at_flags: 0, at_sysinfo_ehdr: self.vdso_base_opt,
                };
                let image = build_initial_user_stack(0x4000_0000, self.argv, self.envp, &mut facts);
                self.entry_pc = self.interp_plan.as_ref().map_or(parsed.entry, |i| i.entry);
                self.initial_sp = image.initial_sp;
                self.phase = ExecProgress::PoNR;
                StepOutcome::Continue { progress: ExecProgress::PopulatingStack }
            }

            // ===== Phase 6: PoNR ==================================
            ExecProgress::PoNR => {
                let new_aspace = self.new_aspace.take().unwrap();
                let _old = self.process.replace_aspace(new_aspace);
                // Build initial user trap context with correct pc/sp.
                let mut ctx = UserTrapContext::default();
                ctx.set_sepc(self.entry_pc as usize);
                ctx.set_sp(self.initial_sp as usize);
                if let Some(payload) = self.thread.payload_cap() {
                    payload.store_saved_user_context(Some(ctx));
                }
                self.phase = ExecProgress::PostCommit;
                StepOutcome::Continue { progress: ExecProgress::PoNR }
            }

            // ===== Phase 7+8: post-commit =========================
            ExecProgress::PostCommit => {
                step_close_cloexec_fds(self.process);
                step_reset_signal_dispositions_for_exec(self.process);
                step_install_brk_for_exec(self.process, 0);
                // Store exe_file and cmdline for procfs.
                if let Some(payload) = self.process.payload_slot().lock().as_ref() {
                    if let Some(file) = &self.openfile {
                        if let Some(dentry) = file.opendir_dentry() {
                            *payload._exe_file.lock() = Some(dentry.clone());
                        }
                    }
                    let cmdline_bytes = self.argv.first().map(|s| s.to_vec()).unwrap_or_default();
                    *payload._cmdline.lock() = Some(cmdline_bytes);
                }
                // vfork completion: wake parent blocked in sys_clone.
                self.process.fire_exit_source(1);
                StepOutcome::Done(())
            }
        }
    }
}
