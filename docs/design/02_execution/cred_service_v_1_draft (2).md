# Cred Service

<!-- txdoc:02-EXECUTION-CRED-SERVICE-V-1-DRAFT-2 -->

## Status
<!-- txdoc:CRED-STATUS -->

Draft v1.1.

Aligned with the current txKernel framework:

- cred is a **service subsystem** in reduced form;
- scripts remain **state-blind** and may observe only syscall-entry metadata;
- full subsystems own their own semantic bindings and commit points;
- authorization materialization is explained as **ARCH-5 applied to authorization**.

---

## Purpose
<!-- txdoc:CRED-PURPOSE -->

The cred service is txKernel's subsystem for **durable identity-derived policy**.

It is **not**:

- a new namespace,
- a new userspace handle table,
- a universal capability plane,
- a replacement for POSIX identity.

Its job is to:

1. hold long-lived credential state;
2. expose pure authorization checks over that state;
3. support credential-changing transitions such as `setuid` and `setgid`;
4. justify the minting of subsystem-local grants at the commit points where other subsystems publish them.

The design separates:

- **who are you?** — durable credential state;
- **what may you do through this already-published subsystem object?** — subsystem-local grants minted against that credential state.

---

## Core claim
<!-- txdoc:CRED-CORE-CLAIM -->

txKernel authorization is a **two-book model**.

### Book A — the credential ledger
<!-- txdoc:CRED-BOOK-A-THE-CREDENTIAL-LEDGER -->

The kernel maintains a durable credential ledger answering:

> who is this subject, and what baseline policy state does it carry?

This ledger is represented by a `Credential` object. It is durable, read-mostly, long-lived, and the authoritative basis for authorization within the authorization scope.

### Book B — subsystem-local grants
<!-- txdoc:CRED-BOOK-B-SUBSYSTEM-LOCAL-GRANTS -->

Subsystems publish local grants answering:

> what may this subject do through this already-published subsystem relation?

Examples:

- access bits in `FdEntry` / `OpenFile` minted at `open`;
- mount-local administrative authority minted by mount operations;
- ptrace attachment authority minted at attach time;
- other subsystem-owned grant bits living in already-existing subsystem structures.

These grants are derived, possession-style, owned by the subsystem that publishes them, and honored on the hot path without re-deriving from live credential state.

### Relationship between the two
<!-- txdoc:CRED-RELATIONSHIP-BETWEEN-THE-TWO -->

Within the authorization scope:

- `Credential` is the **authoritative basis**;
- subsystem-local grants are **derived materializations**;
- minting is **publication**.

Minting must therefore obey the publication rule:

> a derived authorization materialization may be published only if its justifying credential basis is valid at publication time, and publication re-validates that basis atomically with the commit that installs the materialization.

This is not a special-case model outside the rest of the kernel. It is the same architectural shape used, for example, by VM, where recipes are authoritative and PTEs are derived materializations: authoritative bindings justify derived materializations, and publication validates that justification at the moment of commit.

---

## Position in the system
<!-- txdoc:CRED-POSITION-IN-THE-SYSTEM -->

Cred is a **service subsystem**.

It does not own a userspace signifier space and does not resolve path / pid / fd names itself. Instead:

- scripts may carry a syscall-entry `Credential` value as metadata;
- full semantic subsystems such as VFS, process, mount, and ptrace call into `cred::checks::*` to authorize operations;
- those full subsystems remain the publication sites for their own bindings and grants.

In particular:

- **cred authorizes**;
- **the owning subsystem publishes**.

There is no cred-side publication of `FdEntry`, mount handles, ptrace state, or any other foreign subsystem object.

---

## Scope
<!-- txdoc:CRED-SCOPE -->

### In scope for v1
<!-- txdoc:CRED-IN-SCOPE-FOR-V1 -->

- a durable `Credential` type;
- storage of credentials in process policy;
- syscall-entry copying of `Credential` into thread-context metadata;
- pure authorization checks over `Credential` plus subsystem-exported value types;
- credential-changing transitions:
  - `setuid`
  - `setgid`
  - `setgroups`
  - `capset` (drop-only / no-op transitions in v1)
  - `umask`
- read-only projections of credential state.

### Deferred to phase 2
<!-- txdoc:CRED-DEFERRED-TO-PHASE-2 -->

The following are **explicitly deferred**:

- privileged `execve` recomputation with suid / sgid elevation;
- file capabilities from executable metadata;
- full Linux securebits semantics;
- full Linux ambient / inheritable / permitted / bounding transition rules;
- complete `no_new_privs` interaction with `execve` privilege gain.

For v1, `execve` is treated as **credential-preserving**. The presence of fields required for later phases does not imply that full Linux semantics are already implemented.

### Out of scope
<!-- txdoc:CRED-OUT-OF-SCOPE -->

- a universal capability namespace;
- a separate per-task token table;
- replacing POSIX identity with pure object capabilities;
- revoking ordinary already-open fd rights when caller credentials later change.

---

## One type, two roles
<!-- txdoc:CRED-ONE-TYPE-TWO-ROLES -->

The design uses **one credential type**, not two semantically distinct types.

```rust
pub struct Credential {
    pub ruid: Uid,
    pub euid: Uid,
    pub suid: Uid,

    pub rgid: Gid,
    pub egid: Gid,
    pub sgid: Gid,

    pub fsuid: Uid,
    pub fsgid: Gid,

    pub groups: ArrayVec<Gid, 32>,

    pub permitted: CapSet,
    pub effective: CapSet,
    pub inheritable: CapSet,
    pub ambient: CapSet,
    pub bounding: CapSet,

    pub securebits: SecureBits,
    pub no_new_privs: bool,

    pub umask: Umask,
}
```

This one type plays two roles.

### At rest
<!-- txdoc:CRED-AT-REST -->

The canonical credential state lives in process policy:

```rust
pub struct ProcessPolicy {
    pub cred: Cap<Credential>,
    pub rlimits: Cap<RLimitBag>,
    pub signal_mask: SigMask,
}
```

### In flight
<!-- txdoc:CRED-IN-FLIGHT -->

At syscall entry, prelude copies the current credential value into thread-context metadata:

```rust
pub struct ThreadContext {
    pub cred: Credential,
    // ... other prelude metadata
}
```

The copied value is what scripts and checks carry.

This gives the system the intended distinction:

- canonical credential state remains inside process policy;
- the script holds only a by-value metadata copy;
- no script needs to dereference `Cap<Credential>` on the hot path.

The distinction is therefore one of **storage role** and **lifetime**, not of semantic type.

---

## ARCH-5 mapping
<!-- txdoc:CRED-ARCH-5-MAPPING -->

The narrative relationship between the credential ledger and subsystem-local grants was stated in the core claim above. Formally, within the authorization scope:

- **authoritative basis**: the current `Credential` referenced from process policy;
- **derived materialization**: subsystem-local grants published into `FdEntry`, `OpenFile`, mount state, ptrace state, and similar semantic structures;
- **publication event**: the owning subsystem's commit that both validates policy and publishes the grant-bearing structure.

The important consequence is that **minting is publication**, not a side calculation.

### V1 re-validation is mostly trivial
<!-- txdoc:CRED-V1-RE-VALIDATION-IS-MOSTLY-TRIVIAL -->

The full architecture is stronger than what v1 usually needs.

Because v1 defers privileged `execve` recomputation and does not attempt elaborate intra-syscall credential mutation, the syscall-entry `Credential` value in `ThreadContext` is, in the overwhelming majority of cases, still the current credential basis at the moment a grant is minted. The type and step structure already leave room for stricter re-validation in later phases, but in v1 that re-validation is usually observationally trivial: the basis the script carried is the basis the mint sees.

This is a feature, not a gap. The architecture is already prepared for phase-2 cases where credential state can matter more dynamically, without burdening v1 with machinery it does not yet need.

---

## Ownership split
<!-- txdoc:CRED-OWNERSHIP-SPLIT -->

### Process owns subject lifetime
<!-- txdoc:CRED-PROCESS-OWNS-SUBJECT-LIFETIME -->

Process owns:

- process identity;
- thread identity;
- exit and zombie behavior;
- pid namespace state;
- the policy container that references the current credential.

### Cred owns credential semantics
<!-- txdoc:CRED-CRED-OWNS-CREDENTIAL-SEMANTICS -->

Cred owns:

- the structure of `Credential`;
- pure authorization predicates over credentials;
- transitions that produce a new credential value;
- rules for `setuid`, `setgid`, `setgroups`, `capset`, and related operations.

### Full subsystems own grant semantics
<!-- txdoc:CRED-FULL-SUBSYSTEMS-OWN-GRANT-SEMANTICS -->

VFS, process-control, mount, ptrace, and other full subsystems own:

- the bindings and grant-bearing structures they publish;
- the meaning of those grants;
- the commit points at which those grants become externally visible.

Cred does not define the semantics of `FdEntry`, mount handles, or ptrace state. It only authorizes their minting.

### Cred is not resource accounting
<!-- txdoc:CRED-CRED-IS-NOT-RESOURCE-ACCOUNTING -->

Questions of the form:

- may this identity perform this kind of action?

belong to cred.

Questions of the form:

- do we still have budget to allocate this many descriptors or pages?

belong to resource/accounting services such as rlimit or credit.

Composite syscall logic should compose these services rather than collapsing them into one oversized policy subsystem.

---

## Data model
<!-- txdoc:CRED-DATA-MODEL -->

### Canonical object
<!-- txdoc:CRED-CANONICAL-OBJECT -->

```rust
pub struct Credential { ... }
```

### Foreign inputs consumed by cred
<!-- txdoc:CRED-FOREIGN-INPUTS-CONSUMED-BY-CRED -->

Cred does **not** import foreign subsystem `structure/` and inspect live semantic objects directly.

Instead, cred consumes **subsystem-exported value types** that already live at public boundaries.

Examples:

- VFS / filesystem operations should pass `&InodeMeta`;
- process-control operations may pass a small exported value type such as `TargetProcCred`;
- mount operations should pass a mount-side value type only if a second argument is actually needed.

The general rule is:

> cred consumes value types already exported at subsystem boundaries. New wrapper types are introduced only where no such exported value type already exists.

### Example process-exported value type
<!-- txdoc:CRED-EXAMPLE-PROCESS-EXPORTED-VALUE-TYPE -->

`TargetProcCred` is illustrative rather than normative. Its exact shape belongs with the signal-send check, not with the cred core model. The point is only that process may export a small value type containing the target-side facts the authorization rule needs, rather than exposing process internals.

```rust
pub struct TargetProcCred {
    pub ruid: Uid,
    pub euid: Uid,
    pub suid: Uid,

    pub rgid: Gid,
    pub egid: Gid,
    pub sgid: Gid,

    pub same_session: bool,
    pub dumpable: bool,
}
```

---

## Module layout
<!-- txdoc:CRED-MODULE-LAYOUT -->

```text
subsystems/cred/
    structure/
        credential.rs
        caps.rs
        securebits.rs

    checks/
        predicates.rs
        require.rs
        witness.rs

    execution/
        step_setuid.rs
        step_setgid.rs
        step_setgroups.rs
        step_capset.rs
        step_umask.rs
        step_exec_recompute.rs

    project.rs
```

This follows the ordinary reduced-form service-subsystem shape:

- `structure/` holds the durable definitions and helpers for the authoritative credential object;
- `checks/` defines pure authorization queries;
- `execution/` defines credential-changing transitions;
- `project.rs` exposes read-only projections of credential state.

---

## Checks surface
<!-- txdoc:CRED-CHECKS-SURFACE -->

Cred checks are pure authorization checks.

They answer questions such as:

- may this credential search this directory?
- may this credential open this inode with these access bits?
- may this credential unlink this child from this parent?
- may this credential send a signal to this target process?
- may this credential perform this mount-administrative action?

A proposed surface is:

```rust
cred::checks::require_path_search(
    cred: &Credential,
    dir: &InodeMeta,
    guard: &Guard,
) -> Result<SearchAuthorized<'g>, Errno>;

cred::checks::require_open(
    cred: &Credential,
    file: &InodeMeta,
    acc: AccessMode,
    guard: &Guard,
) -> Result<OpenAuthorized<'g>, Errno>;

cred::checks::require_unlink(
    cred: &Credential,
    parent: &InodeMeta,
    child: &InodeMeta,
    guard: &Guard,
) -> Result<UnlinkAuthorized<'g>, Errno>;

cred::checks::require_signal_send(
    cred: &Credential,
    target: &TargetProcCred,
    sig: Signal,
    guard: &Guard,
) -> Result<SignalAuthorized<'g>, Errno>;

cred::checks::require_setuid(
    current: &Credential,
    req: &SetUidReq,
    guard: &Guard,
) -> Result<SetUidAuthorized<'g>, Errno>;
```

Here `AccessMode` is the requested data-access class derived from `O_RDONLY`, `O_WRONLY`, or `O_RDWR` style flags, not a separate authority object.

The important boundary rule is:

- cred checks consume value types,
- not live-node entities,
- not foreign subsystem `structure/` internals,
- not script-side ad hoc field reads.

---

## Cred witnesses
<!-- txdoc:CRED-CRED-WITNESSES -->

Cred witnesses are **zero-sized provenance tokens**.

This is a deliberate difference from witnesses in subsystems such as VFS.

### Why they are zero-sized
<!-- txdoc:CRED-WHY-THEY-ARE-ZERO-SIZED -->

A VFS witness carries meaningful observation evidence such as `IdentRef<DEntry>` and `IdentRef<RNode>`. Cred does not need that shape because:

- `Credential` is already passed by value;
- the foreign authorization input is also passed by value (`InodeMeta`, `TargetProcCred`, etc.);
- the caller already owns those inputs;
- the witness only needs to prove that the check ran successfully under the current guard.

A representative shape is:

```rust
pub struct OpenAuthorized<'g> {
    _guard: core::marker::PhantomData<&'g ()>,
    _priv: (),
}
```

### Meaning
<!-- txdoc:CRED-MEANING -->

A cred witness means:

> this authorization check succeeded under this guard for the provided inputs.

It does **not** carry:

- strong retention,
- a live reference to the credential,
- a live reference to the foreign subsystem object,
- a second copy of the checked metadata.

The checked data remain owned by the caller. The witness is only the provenance token required by the framework.

### Uniform shape
<!-- txdoc:CRED-UNIFORM-SHAPE -->

All cred witnesses follow this pattern. `SearchAuthorized`, `OpenAuthorized`, `UnlinkAuthorized`, `SignalAuthorized`, and similar types are all zero-sized, carry a guard phantom, and rely on private construction by `cred::checks::require_*`.

---

## Execution surface
<!-- txdoc:CRED-EXECUTION-SURFACE -->

Cred execution contains only credential-changing transitions.

Representative steps:

- `step_setuid`
- `step_setgid`
- `step_setgroups`
- `step_capset`
- `step_umask`
- `step_exec_recompute`

Here `SetUidReq` is shorthand for the setuid-family request variants handled by the credential-transition path. The exact enum shape is an implementation detail; the point of the example is that the step consumes a credential-transition request value, not that v1 commits to one particular naming scheme.

These steps follow the standard step discipline:

1. observe;
2. upgrade if needed;
3. reserve;
4. commit;
5. publish.

The process-side accessor names used below (`cred_ref(&guard)`, `cred_cap()`, `replace_cred(...)`) are illustrative names for the obvious observation / retention / replacement operations process policy must expose. This document does not fix the exact process-subsystem API spelling.

A representative shape is:

```rust
fn step_setuid(
    proc: Cap<ProcessIdentity>,
    req: SetUidReq,
) -> StepOutcome<()> {
    let guard = epoch::guard();

    // Phase 1: observe — read current cred and run policy check.
    let old_cred = proc.policy().cred_ref(&guard);
    let _cred_w = cred::checks::require_setuid(old_cred, &req, &guard)?;

    // Phase 2: upgrade — no witness upgrade is required because cred
    // witnesses are zero-sized provenance tokens. We only retain a Cap
    // on the old credential value for construction and commit.
    let old_cred_cap = proc.policy().cred_cap();

    // Phase 3: reserve.
    let new_slot = substrate::zone::reserve::<Credential>()?;
    let new_cred_value = Credential::recompute_setuid(&old_cred_cap, &req);

    // Phase 4: commit — publish replacement credential and install it
    // into process policy.
    let new_cred = substrate::zone::sign(new_slot, new_cred_value);
    proc.policy_mut().replace_cred(new_cred.clone());

    // Phase 5: publish.
    cred::trace_cred_changed(proc.pid(), new_cred.euid);

    StepOutcome::Done(())
}
```

The exact reservation mechanics are implementation detail. The architectural point is that credential transitions are semantic mutations and therefore live in step code, not in scripts.

---

## Minting rule
<!-- txdoc:CRED-MINTING-RULE -->

The minting rule is:

> cred authorizes; the owning subsystem publishes.

That means:

- cred never publishes foreign subsystem grant-bearing objects;
- full semantic subsystems remain the commit sites for their own grant materializations;
- a grant is minted exactly when the owning subsystem publishes the binding or relation that carries it.

This is what keeps subsystem ownership clear and keeps the design inside the ordinary txKernel module layout.

---

## Minting walkthrough: `open`
<!-- txdoc:CRED-MINTING-WALKTHROUGH-OPEN -->

The `open` path is the canonical minting example.

### Shape
<!-- txdoc:CRED-SHAPE -->

```rust
async fn open(ctx: &ThreadContext, path: &CStr, flags: OpenFlags, mode: u16) {
    let cred = &ctx.cred;

    loop {
        let guard = epoch::guard();

        // VFS resolves the path and produces a VFS witness.
        let path_w = vfs::checks::require_entity_or_parent_and_name(path, ctx, &guard).await?;

        // VFS exposes the already-extracted inode metadata.
        let inode_meta = path_w.inode_meta();

        // Cred authorizes using value inputs.
        let auth_w = cred::checks::require_open(cred, inode_meta, flags.access(), &guard)?;

        // VFS publishes the OpenFile / FdEntry carrying the grant.
        // For O_CREAT paths, the VFS step also consumes the caller's
        // Credential-derived ownership inputs when constructing the new inode.
        match vfs::execution::step_open(path_w, auth_w, cred, flags, mode) {
            StepOutcome::Done(fd) => return Ok(fd),
            StepOutcome::Blocked(c, m) => wait_on(c, m).await?,
            StepOutcome::Err(e) => return Err(e),
            _ => unreachable!(),
        }
    }
}
```

### What this example shows
<!-- txdoc:CRED-WHAT-THIS-EXAMPLE-SHOWS -->

1. The script carries only the syscall-entry `Credential` value.
2. VFS owns pathname resolution and produces its own witness.
3. Cred consumes `&Credential + &InodeMeta`, not VFS internals.
4. Cred's contribution is the zero-sized authorization witness.
5. The VFS step consumes both witnesses and publishes `OpenFile` / `FdEntry`.
6. The rights bits embedded in those structures are the minted grant.

There is therefore **one publication**, not two:

- cred does **not** publish a token and hand it to VFS;
- VFS publishes the grant-bearing structure, justified by cred authorization.

This is the authorization instance of ARCH-5: the justifying basis is checked at mint time, and publication installs the derived materialization.

---

## Hot path and cold path
<!-- txdoc:CRED-HOT-PATH-AND-COLD-PATH -->

### Cold path: mint
<!-- txdoc:CRED-COLD-PATH-MINT -->

Cold-path operations are the points where the system reads credential state, evaluates policy, and may publish a new grant-bearing relation.

Examples:

- `open`
- mount establishment
- ptrace attach
- `setuid`
- `capset`

These are comparatively rare and may be expensive.

### Hot path: use
<!-- txdoc:CRED-HOT-PATH-USE -->

Hot-path operations are the ones that use an already-published grant without re-deriving it from live credentials.

Examples:

- `read`
- `write`
- `pread`
- `pwrite`
- other data-plane operations against already-opened objects.

These operations should consult the already-minted grant bits carried by subsystem-local structures rather than re-running full authorization.

### Not every operation is tokenized
<!-- txdoc:CRED-NOT-EVERY-OPERATION-IS-TOKENIZED -->

Some operations remain live-checked in v1.

The clearest examples are:

- `kill(pid)`, where there is no especially natural persistent relation object to carry a reusable grant;
- `access(2)` and relatives, which are policy oracles by design and therefore intentionally perform a live cred check without minting a reusable grant.

Such operations use cred as an authorization oracle at action time rather than as a minting authority for a reusable grant.

---

## Grant classes
<!-- txdoc:CRED-GRANT-CLASSES -->

The architecture distinguishes three grant classes.

### Immutable-use grants
<!-- txdoc:CRED-IMMUTABLE-USE-GRANTS -->

These are minted once and remain valid until the owning subsystem object dies. They are not revoked merely because the caller later changes credentials.

Canonical example:

- fd access rights minted at `open`.

### Scoped grants
<!-- txdoc:CRED-SCOPED-GRANTS -->

These are minted into subsystem-local structures but have narrower invalidation rules defined by the owning subsystem.

Examples may include:

- mount-administrative attachments;
- ptrace attachment state;
- subsystem-specific admin relations.

### Non-materialized checks
<!-- txdoc:CRED-NON-MATERIALIZED-CHECKS -->

These operations do not mint a reusable grant in v1. They simply perform a live authorization check when the action occurs.

This classification prevents over-generalization. The model is not “everything becomes a token.” The model is “where a stable subsystem relation exists and hot-path reuse matters, authority is materialized once and then honored.”

---

## Lifecycle rules
<!-- txdoc:CRED-LIFECYCLE-RULES -->

### Fork
<!-- txdoc:CRED-FORK -->

`fork` produces a **new credential object with copied value** for the child.

This is an intentional contrast with other process-frame slots. Objects such as `vm`, `fd_table`, `sig_actions`, and `fs_context` can be modeled as shared state with COW-on-mutate because they have structural identities whose mutation must preserve object continuity. `Credential` does not need that treatment. It is a small value object with no independent structural identity beyond “the current credential value for this process.” When `setuid` publishes a new `Credential`, the old one is simply dropped and the process policy pointer is replaced.

A `Shared<Credential>` design would therefore buy little: on first credential-changing mutation, parent and child would still have to detach, which is exactly fork's copy behavior performed lazily rather than eagerly. Since `Credential` is small and copyable, eager copy is both simpler and cheaper than carrying a permanent “current cred may be shared” invariant.

### Threads
<!-- txdoc:CRED-THREADS -->

Threads in one process share the same current process credential through process policy.

### Exec
<!-- txdoc:CRED-EXEC -->

For v1, `execve` is **credential-preserving**.

The full privileged-exec recomputation problem is explicitly deferred. In particular, v1 does not claim complete semantics for:

- suid / sgid bits on executables;
- file capability xattrs;
- securebits interactions;
- `no_new_privs` privilege suppression;
- ambient / bounding / inheritable set recomputation.

The `step_exec_recompute` entry point may still exist structurally, but in v1 its behavior is identity-preserving.

---

## Projections
<!-- txdoc:CRED-PROJECTIONS -->

Cred exposes read-only projections of a process's current credential state.

These projections are consumed by procfs-style adapters when rendering process status and related files. Typical output includes fields such as:

- `Uid:`
- `Gid:`
- `Groups:`
- `CapInh:`
- `CapPrm:`
- `CapEff:`
- `CapBnd:`
- `CapAmb:`

The important rules are:

- projections are read-only;
- projections are metadata-only;
- projections do not retain or mutate credential state;
- procfs calls into `cred::project` rather than re-deriving credential formatting ad hoc.

A representative surface is:

```rust
impl Credential {
    pub fn status_projection(&self) -> StatusCredView;
    pub fn groups_projection(&self) -> GroupsView;
    pub fn caps_projection(&self) -> CapsView;
}
```

The exact procfs file layout is an adapter concern. The cred subsystem's job is only to provide stable read-only projections of the current credential value.

---

## Capability-state scope
<!-- txdoc:CRED-CAPABILITY-STATE-SCOPE -->

The `Credential` type includes the full Linux-shaped capability-state fields for forward compatibility:

- permitted;
- effective;
- inheritable;
- ambient;
- bounding.

However, the presence of these fields in v1 does **not** imply that the full Linux capability transition state machine is implemented.

The v1 rule is:

- the fields exist now so that the credential shape does not have to change later;
- `capset` is in scope only for no-op and capability-dropping transitions;
- capability elevation and the full Linux transition semantics are deferred with privileged `execve` and related phase-2 work.

---

## `umask` placement
<!-- txdoc:CRED-UMASK-PLACEMENT -->

`umask` is stored in `Credential` as an intentional consolidation of process-local authorization-relevant policy. It uses a distinct `Umask` nominal type rather than plain `FileMode`, even though the underlying bit representation is compatible. This differs from Linux's placement in `fs_struct`; both placements work.

---

## Filesystem/backend boundary
<!-- txdoc:CRED-FILESYSTEM-BACKEND-BOUNDARY -->

Filesystems and backends should consume the same by-value `Credential` type rather than live process or VFS entities.

That means:

- tx-ext4 and similar backends should never depend on live-node VFS types for authorization;
- VFS remains responsible for resolution and grant minting;
- the backend consumes credential values only where backend-side mutation policy actually requires them.

This keeps the backend boundary purely value-oriented and consistent with the one-type credential model.

---

## Non-goals
<!-- txdoc:CRED-NON-GOALS -->

This design does not introduce:

- a universal capability ABI;
- a second userspace-visible token namespace;
- Fuchsia-style all-handle authority;
- automatic revocation of ordinary already-open grants when credentials later change;
- script-side semantic authorization logic.

---

## Short version
<!-- txdoc:CRED-SHORT-VERSION -->

> txKernel authorization is a two-book model. A durable `Credential` ledger records identity and baseline policy state. Subsystem-local grants are derived materializations minted against that ledger at subsystem commit points. Minting re-validates the justifying credential state atomically with publication. After mint, each grant is honored according to its owning subsystem's semantics without re-deriving from live credentials on the hot path.

