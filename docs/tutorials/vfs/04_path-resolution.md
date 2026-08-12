# Part 4 — Path resolution: the walker

Turning `/tmp/foo` into a file object is the VFS's signature operation. In Linux
it is `link_path_walk` / `walk_component` (historically `namei`): a loop over
path components, each one a directory lookup, with symlinks, `..`, mount points,
and permission checks woven in. txKernel's walker is the same loop expressed as
a **step state machine** — and it is the purest demonstration of Chapter 2's
refcount-free-traversal motivation.

## The shape of the problem

A path walk is a fold over components. Start at a directory, repeatedly: take
the next component, look it up in the current directory, make the result the new
current directory, until the path is exhausted. The complications are all the
ways a component is not just a simple name:

- `.` stays put; `..` ascends (but not past the namespace root).
- a component may resolve to a *symlink*, whose target must be spliced into the
  remaining path (with a hop limit to stop loops).
- a component may be a *mount point*, where the walk crosses into another
  filesystem.
- every intermediate directory needs *search* (`x`) permission.
- the backend lookup may need to *wait* on storage (disk read).

txKernel threads all of this through one mutable frame and advances it one
component per call.

## The walker frame: `WalkingState`

```rust
pub struct WalkingState {
    pub current: Cap<DEntry>,       // the directory we're looking up in
    pub remaining: Vec<u8>,         // unconsumed path bytes
    pub hop_count: u32,             // symlinks followed so far (ELOOP guard)
    pub mount_root: Cap<DEntry>,    // namespace root: bounds `..` and absolute symlinks
    pub must_be_directory: bool,    // true if the path ended with `/`
}
```

This is the entire walker state. `current` is the cursor; `remaining` shrinks as
components are consumed; `mount_root` is the ceiling for `..` (you cannot escape
your namespace root) and the restart point for absolute symlinks.

The walk is driven through a small enum, `WalkState`, which is either `Walking`
(more to do), `Terminal` (a `PathResolution`), `Error`, or `Defer` (suspended on
I/O):

```rust
pub enum WalkState {
    Walking(WalkingState),
    Defer { request: IORequest, resume: ResumeToken, cause: WalkCause },
    Error(WalkCause),
    Terminal(PathResolution),
}

pub struct PathResolution {       // the successful result
    pub dentry: Cap<DEntry>,
    pub rnode: Cap<RNode>,
    pub fs_object_id: FsObjectId,
    pub meta: InodeMeta,
}
```

As a state machine, the driver loops `Walking` through `kernel_step` until it
reaches one of the three sinks. The `NeedIO` edge is the interesting one — it
exits to the reactor and re-enters later, reconstructed from a `ResumeToken`:

```
                 ┌──────────────────────────────────────────┐
                 │                                           │
                 ▼               kernel_step                 │ Continue
            ┌─────────┐   ┌──────────────────────┐           │ (next component,
   start ──▶│ Walking │──▶│ component? ./.. ? dir?│───────────┘  symlink splice,
            └─────────┘   │ perm? lookup? mount?  │              "." )
                 ▲        └───────┬───────┬───────┘
                 │                │       │
       resume_walker        Terminal    NeedIO ──▶ suspend syscall future
       (rebuild from        (Path-       (serialise WalkingState into
        ResumeToken)        Resolution)   ResumeToken; await fs I/O)
                 │                │
                 └────────────────┘  Error(WalkCause) ──▶ ENOENT/EACCES/ELOOP/…
```

Three properties to hold onto: (1) the *entire* live state is the `WalkingState`
value — no parked stack; (2) the `NeedIO` token carries that value out and back,
so a disk-blocked walk is a heap value, not a frozen call frame; (3) only the
`Terminal` carries `Cap`s out — every intermediate hop is a guard-scoped borrow.

## One component: `kernel_step`

`kernel_step` consumes a `WalkingState` and returns a `KernelStep`
(`Continue(WalkState)`, `NeedIO(IORequest, ResumeToken)`, or `Error(WalkCause)`).
Following the real control flow (`resolution/step.rs:51`):

```rust
fn kernel_step(walking, fs_ops, mount_payload, mount_ns, cred, rules, guard) -> KernelStep {
    // 1. End of path? Produce the terminal.
    let (component, remaining) = match take_next_component(walking.remaining) {
        None => {
            if walking.must_be_directory && current.kind() != Directory {
                return Error(NotADirectory);          // trailing "/" on a file
            }
            let resolved = PathResolution { dentry: current, rnode, fs_object_id, meta };
            return Continue(Terminal(resolved));      // (subject to WalkMode acceptance)
        }
        Some(next) => next,
    };

    // 2. "." stays; ".." ascends, bounded at mount_root.
    if component == b"." { return Continue(Walking(/* unchanged */)); }
    if component == b".." {
        if let Some(parent) = current.parent_hint() {
            if !is_same_dentry(&current, &mount_root) { current = parent; }
        }
        return Continue(Walking(/* current updated */));
    }

    // 3. Current must be a directory.
    if current.rnode().meta().kind() != Directory { return Error(NotADirectory); }

    // 4. POSIX search permission on the directory (the `x` bit).
    if require_path_search(cred, &current.meta(), guard).is_err() {
        return Error(Permission(SearchDenied));
    }

    // 5. Look up the component — dentry cache first, then the backend.
    let (child_dentry, child_rnode, …) = if let Some(cached) = current.cached_child(name) {
        (cached, cached.rnode().clone(), …)            // cache hit: no backend call
    } else {
        match fs_ops.lookup(parent_id, &component, guard) {
            Done(id)  => /* then load_inode_meta, materialise RNode, build DEntry */,
            Yield{..} => return NeedIO(DirLookup{..}, resume_token),  // wait on storage
            Err(e)    => return Error(FsOpsRejected(e)),
        }
    };

    // 6. Symlink? Splice the target into `remaining`, bump hop_count, check ELOOP.
    // 7. Mount point? If child.mounted_hint() upgrades, cross into the mount's root.
    // 8. Otherwise: advance — current = child_dentry.
    Continue(Walking(/* current = child */))
}
```

Each numbered stage maps onto a piece of traditional `namei`:

| Stage | Traditional counterpart |
|---|---|
| 1. terminal | end of `link_path_walk` |
| 2. `.` / `..` | `handle_dots` |
| 3. directory check | `ENOTDIR` on non-dir component |
| 4. search permission | `inode_permission(MAY_EXEC)` on each dir |
| 5. lookup (cache then backend) | `__d_lookup` then `i_op->lookup` |
| 6. symlink | `step_into` / `trailing_symlink` |
| 7. mount crossing | `__follow_mount` / `lookup_mnt` |

### The dentry cache

Stage 5 checks `current.cached_child(name)` *before* calling the backend. This
is the dcache hit path — and it is where Chapter 1's parent-strong/child-weak
design pays off. `cached_child` (`structure.rs:923`) upgrades the stored
`Weak<DEntry>`; on success it returns the cached `Cap<DEntry>` and *no backend
call happens*. On failure (the child was reclaimed) it removes the dead weak
entry and falls through to `fs_ops.lookup`. The cache never keeps a subtree
alive on its own; it only accelerates lookups of things still in use elsewhere.

On a miss, the walker runs a three-call sequence — *lookup the name, load its
metadata, materialise an `RNode`* — and each call can independently yield, so
each has its own `NeedIO` arm carrying a `ResumeToken`. Here is the real
control flow, lookup through cache-install (`resolution/step.rs:152`, abridged):

```rust
let child_fs_object_id = match fs_ops.lookup(parent_fs_object_id, &component, guard) {
    StepOutcome::Done(id) => id,
    StepOutcome::Yield { .. } => {                       // backend must hit storage
        let request = IORequest::DirLookup { fs_object_id: parent_fs_object_id,
                                             name: component.clone().into_boxed_slice() };
        let token = ResumeToken { walking: WalkingState { current,
                                      remaining: remaining_with_component(&component, &remaining),
                                      hop_count, mount_root, must_be_directory },
                                  request: request.clone(), mount_namespace: None, hop_count };
        return KernelStep::NeedIO(request, token);       // ← suspend, remember everything
    }
    StepOutcome::Err(e) => return KernelStep::Error(WalkCause::FsOpsRejected(e.into())),
    StepOutcome::Continue { .. } => return /* re-enter Walking, retry component */,
};

let child_meta = match fs_ops.load_inode_meta(child_fs_object_id, guard) {
    StepOutcome::Done(m) => m,
    StepOutcome::Yield { .. } => return KernelStep::NeedIO(
        IORequest::LoadInodeMeta { fs_object_id: child_fs_object_id }, /* token */),
    StepOutcome::Err(e) => return KernelStep::Error(WalkCause::FsOpsRejected(e.into())),
    StepOutcome::Continue { .. } => return /* retry */,
};

let child_rnode_cap = materialise_child(&fs_ops, child_fs_object_id, &child_meta,
                                        mount_payload.as_ref(), /* walking */, guard)?;

let mut child_dentry_raw = DEntry::new(child_inline, child_rnode_cap.clone());
child_dentry_raw.set_parent_hint(&current);              // parent-strong link (Chapter 1)
let child_dentry = step_engine::sign(child_dentry_raw)?; // zone-allocate → Cap<DEntry>
current.cache_child(child_dentry.clone());               // install weak in parent's children
```

Read the shape, not just the lines:

- **Each backend call is a separate yield point.** `lookup`, `load_inode_meta`,
  and `materialise_child` each get their own `IORequest` variant (`DirLookup`,
  `LoadInodeMeta`, `MaterialiseRnode`) so that a walk blocked while loading
  metadata resumes *at that call*, not back at the lookup — the `ResumeToken`
  re-uses `remaining_with_component(&component, &remaining)` to put the
  half-consumed component back at the front of the path.
- **`set_parent_hint` is the strong parent edge.** The freshly-built `DEntry`
  points strongly at `current`, keeping the path-to-root alive; `cache_child`
  installs only a *weak* back-reference in the parent's `children` map. Cache
  asymmetry (Chapter 1) is established right here, at the moment of insertion.
- **`sign` is the identity allocation.** A `DEntry` becomes a `Cap<DEntry>` by
  being zone-signed; failure is `ENOMEM`. Until signed it is a raw struct with
  no identity slot.

`materialise_child` is the inode-cache analogue: given an `fs_object_id` and its
`meta`, it produces a live `Cap<RNode>` (allocating one, tagged with the right
`RNodeBacking` and a weak `containing_mount` back to the `MountPayload`), or
yields `MaterialiseRnode` if that needs I/O. This is `iget` in txKernel terms.

### Symlinks and `ELOOP`

When a resolved component is a symlink, the walker splices its target into
`remaining`. The body is explicit about the relative-vs-absolute fork and the
hop accounting (`resolution/step.rs:280`):

```rust
if let RNodeBacking::Symlink { target } = child_rnode_cap.backing() {
    if rules.policy == FinalSymlinkPolicy::NoFollow && remaining.is_empty() {
        return KernelStep::Continue(WalkState::Terminal(/* the symlink itself */));
    }
    // /tmp symlink-race mitigation: refuse to follow in a sticky world-writable dir
    if protected_symlink_follow_denied(cred, &protected_parent_meta, &child_meta) {
        return KernelStep::Error(WalkCause::FsOpsRejected(Errno::EACCES));
    }
    hop_count += 1;
    if hop_count > SYMLOOP_MAX { return KernelStep::Error(WalkCause::SymlinkLimit); } // ELOOP
    let target_bytes = target.clone();
    if target_bytes.first() == Some(&b'/') {
        // absolute: drop leading '/', restart from mount_root, then the rest of the path
        let mut new_remaining = target_bytes[1..].to_vec();
        if !remaining.is_empty() { new_remaining.push(b'/'); new_remaining.extend(&remaining); }
        return KernelStep::Continue(WalkState::Walking(WalkingState {
            current: mount_root.clone(), remaining: new_remaining, hop_count, mount_root, must_be_directory }));
    } else {
        // relative: prepend target before the rest, keep current directory
        let mut new_remaining = target_bytes;
        if !remaining.is_empty() { new_remaining.push(b'/'); new_remaining.extend(&remaining); }
        return KernelStep::Continue(WalkState::Walking(WalkingState {
            current, remaining: new_remaining, hop_count, mount_root, must_be_directory }));
    }
}
```

The two branches differ only in *where the walk continues*: an absolute target
resets `current` to `mount_root` (chroot-bounded — symlinks cannot escape the
namespace root, which is why this is `mount_root` and not a hardcoded `/`); a
relative target leaves `current` where it is. Both rebuild `remaining` by
concatenating target + `/` + leftover, increment `hop_count`, and trip
`SYMLOOP_MAX` (40; the 41st hop) into `WalkCause::SymlinkLimit` — POSIX `ELOOP`.
`protected_symlink_follow_denied` is the `/tmp` mitigation: in a world-writable
sticky directory, refuse to follow a symlink the caller doesn't own.

### Mount crossing

Stage 7 is how `/` and `/proc` can be different filesystems. When a resolved
child `DEntry` carries a `mounted_hint()` that upgrades to a live
`MountIdentity`, the walker switches `current` to that mount's *root* dentry and
continues from there, now consulting the mounted filesystem's `FsOps`. The
mount namespace passed into `kernel_step` constrains which mounts are visible
(per-process mount namespaces). Chapter 5 covers the mount side; here it is just
"the dentry says something is mounted on me, so step into it."

## The driver loop and the I/O yield

`kernel_step` advances *one* component. The **driver** loops it. Three entry
points exist (`resolution/driver.rs`): `walk_to_completion` (synchronous, treats
`Yield` as `EAGAIN` — used by boot scaffolds), and `run_walker`/`resume_walker`
(the suspendable pair). The synchronous loop is the readable one:

```rust
fn walk_to_completion(rooted_at, path, mode, policy, cred, guard) -> Result<PathResolution, Errno> {
    let mut state = Walking(initial_frame(rooted_at, path));
    loop {
        let walking = match state {
            Walking(w)        => w,
            Terminal(r)       => return Ok(r),
            Error(c)|Defer{c} => return Err(classify(c)),
        };
        let fs_ops = fs_ops_for(&walking.current, guard)
            .or_else(|| fs_ops_for(&walking.mount_root, guard))
            .ok_or(ENODEV)?;                 // which filesystem owns this dentry?
        state = match kernel_step(walking, fs_ops, …, guard) {
            Continue(next) => next,
            NeedIO(req, _) => /* suspendable variant turns this into a .await */,
            Error(cause)   => Error(cause),
        };
    }
}
```

Two things to draw out.

**`fs_ops_for` and the containing-mount weak ref.** Each iteration asks "which
filesystem owns the current dentry?" — `fs_ops_for(&dentry)` upgrades the
`RNode`'s `containing_mount: Weak<MountPayload>` to get the live `FsOps`
(`walker.rs:277`). This is the weak ref from Chapter 1 doing its job: the node
knows its filesystem without a strong reference that would pin a force-unmounted
mount's backing.

**The I/O yield is the step model again.** When the backend `lookup` returns
`Yield` (a disk read must wait), `kernel_step` returns `NeedIO(request,
ResumeToken)`. The `ResumeToken` *serialises the entire `WalkingState`* — current
dentry, remaining bytes, hop count — so the walk can be torn down and rebuilt
later. The suspendable driver turns that into a `.await` in the syscall future
(Chapter 3's `drive` bridge), and when the page arrives, `resume_walker`
reconstructs the frame from the token and calls `kernel_step` again. tmpfs never
yields (in-memory), so against tmpfs the loop runs straight through; against
ext4 it suspends exactly at the components that miss the page cache.

> **Traditional VFS vs txKernel.** When Linux's `i_op->lookup` must read from
> disk, the walking task *blocks* on the page — the kernel stack and its
> on-stack `nameidata` park on a wait queue. txKernel's walker has *no* parked
> stack: its entire state is the `WalkingState`/`ResumeToken` value, the
> syscall future suspends, and the hart runs other work. The walk is a
> reconstructable value, not a frozen call stack. That is the same trade as the
> rest of the kernel (a suspended operation is a parked future, not a parked
> stack), applied to path resolution.

## Refcount-free, end to end

Now Chapter 2's Motivation 3 is concrete. The whole loop runs under *one* epoch
`Guard` (`guard`), passed into every `kernel_step` and every `fs_ops` call. Each
component is reached by dereferencing through that guard — `current.rnode()`,
`current.cached_child(name)` — memory-safe because the guard defers reclamation,
but touching *no* atomic refcount per hop. Only at the terminal, when
`PathResolution` must carry a `Cap<DEntry>` and `Cap<RNode>` out of the walk to
outlive the guard, is identity actually pinned. One walk, one guard, a handful
of `Cap` upgrades at the end — not one per component. That is why path lookup,
the hottest filesystem path, costs pointer-chasing rather than a storm of atomic
increments.

## From walk to open: `step_open`

`step_open` (`walker.rs:202`) composes the walk with open-permission checking
and `OpenFile` construction:

```rust
fn step_open(rooted_at, path, flags, mode, cred, guard) -> StepOutcome<Cap<OpenFile>, NoProgress> {
    let dentry = step_walk(rooted_at, path, cred, guard)?;      // resolve the path
    let meta = dentry.rnode().meta();
    require_open(cred, &meta, flags, guard)?;                   // R/W vs permission bits
    let rnode = dentry.rnode().clone();
    OpenFile::new_cap_with_dentry(rnode, flags, dentry)         // build the open file
}
```

The comment block in the source narrates the five-phase step discipline
(observe → upgrade → reserve → commit → publish): observe the credentials and
flags, upgrade the walk's terminal `IdentRef` to a `Cap`, reserve the
`OpenFile`'s zone slot, commit it, publish (nothing here). The result is a
`Cap<OpenFile>` holding a `Cap<RNode>` — the open edge from Chapter 1's object
graph. Installing it in the fd table is Chapter 6.

## Source anchors

- Walker state vocabulary (`WalkMode`, `WalkingState`, `PathResolution`, `IORequest`): `crates/tx-subsystems/src/vfs/resolution/state.rs:19,74,112,134`
- `kernel_step`: `crates/tx-subsystems/src/vfs/resolution/step.rs:51`
- Protected-symlink rule: `crates/tx-subsystems/src/vfs/resolution/step.rs:386`
- Driver loop (`walk_to_completion`, `run_walker`, `resume_walker`): `crates/tx-subsystems/src/vfs/resolution/driver.rs:34`
- `step_walk` / `step_open` / `fs_ops_for`: `crates/tx-subsystems/src/vfs/walker.rs:141,202,277`
- dentry cache (`cached_child`, `cache_child`): `crates/tx-subsystems/src/vfs/structure.rs:923,936`
- Permission predicates: `crates/tx-subsystems/src/vfs/predicates.rs`
- Walker spec: `docs/design/05_filesystem/VFS_CHECKS_V2.1.md`
