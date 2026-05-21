//! Apply phase. Walks the [`Plan`] in order and performs the side
//! effects each [`ResourceChange`] demands.
//!
//! v1 supports symlinks, templates, and packages end-to-end (create,
//! update, no-op). Other resource kinds bail with a clear
//! "not yet implemented" diagnostic — they land alongside the
//! planner work that diffs them against live state.
//!
//! Package execution is *phased*: any contiguous run of `Package`
//! changes is collapsed into one "package phase". Inside a phase, all
//! changes are grouped by `(manager, action)` and each group becomes
//! one subprocess. When a phase has multiple groups they run
//! concurrently across `std::thread::scope` workers with captured
//! stdio; the captures flush in stable manager order after the phase
//! joins so the user's terminal stays readable. A single-group phase
//! takes a fast path that inherits stdio for live progress bars.
//!
//! A non-package change (Symlink / Template / Tap / Shell) ends the
//! current package phase and runs in declaration order, preserving the
//! tap-before-install ordering the planner already synthesises and
//! avoiding any surprise about side-effect interleaving.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{self, Context, Result, bail};

use crate::packages::{self, BatchOutput};
use crate::plan::{
    Action, PackageManager, Plan, ResourceChange, ResourceKind, ResourceState, ShellKind,
};
use crate::platform::{OsFamily, detect_os_family};

#[derive(Debug, Clone, Copy, Default)]
pub struct ExecuteSummary {
    pub added: usize,
    pub changed: usize,
    pub ran: usize,
}

pub fn execute(plan: &Plan) -> Result<ExecuteSummary> {
    // Snapshot the host OS once on the calling (main) thread so the
    // parallel package phase can pass it as a value to its workers
    // instead of relying on the `OsOverride` thread-local — which is
    // per-thread and would silently fall back to the real host OS
    // inside spawned probe / install workers.
    let os = detect_os_family();
    let mut summary = ExecuteSummary::default();
    let mut applied_addresses: Vec<&str> = Vec::new();
    let total = plan.changes.len();
    let mut i = 0;
    while i < plan.changes.len() {
        // Greedily collect a contiguous Package run. A non-package
        // change forces a phase boundary so the user's apparent
        // declaration order (e.g. a Template that bootstraps a config
        // before a brew install hook reads it) lands deterministically.
        let mut j = i;
        while j < plan.changes.len() && plan.changes[j].kind == ResourceKind::Package {
            j += 1;
        }
        if j > i {
            let phase = &plan.changes[i..j];
            run_package_phase(phase, &mut summary, os)
                .map_err(|e| annotate_phase_failure(e, phase, &applied_addresses, total))?;
            for change in phase {
                applied_addresses.push(change.address.as_str());
            }
            i = j;
        } else {
            let change = &plan.changes[i];
            apply_change(change, &mut summary, os).map_err(|e| {
                annotate_partial_apply(e, change.address.as_str(), &applied_addresses, total)
            })?;
            applied_addresses.push(change.address.as_str());
            i += 1;
        }
    }
    Ok(summary)
}

fn annotate_partial_apply(
    err: anyhow::Error,
    failed_address: &str,
    applied: &[&str],
    total: usize,
) -> anyhow::Error {
    if applied.is_empty() {
        return err.context(format!(
            "apply failed at resource 1 of {total} (`{failed_address}`); nothing was applied"
        ));
    }
    let mut summary = format!(
        "apply failed at resource {} of {total} (`{failed_address}`); {} resource(s) already applied:",
        applied.len() + 1,
        applied.len(),
    );
    for addr in applied {
        summary.push_str("\n  - ");
        summary.push_str(addr);
    }
    err.context(summary)
}

/// Annotate a failure that happened inside a package phase. The phase
/// may have fanned out across several `(manager, action)` batches, so
/// the report names the failing batch(es) by manager and lists the
/// addresses applied before the phase started (singletons + earlier
/// phases) so the user can see exactly where the apply stopped.
fn annotate_phase_failure(
    err: anyhow::Error,
    phase: &[ResourceChange],
    applied: &[&str],
    total: usize,
) -> anyhow::Error {
    let first = phase
        .iter()
        .map(|c| c.address.as_str())
        .next()
        .unwrap_or("<empty phase>");
    let last = phase
        .iter()
        .map(|c| c.address.as_str())
        .next_back()
        .unwrap_or(first);
    let mut summary = if applied.is_empty() {
        format!(
            "apply failed inside package phase (`{first}` … `{last}`, {} resources); nothing was applied before the phase",
            phase.len(),
        )
    } else {
        let mut s = format!(
            "apply failed inside package phase (`{first}` … `{last}`, {} resources) of {total} total; {} resource(s) already applied before the phase:",
            phase.len(),
            applied.len(),
        );
        for addr in applied {
            s.push_str("\n  - ");
            s.push_str(addr);
        }
        s
    };
    let _ = &mut summary;
    err.context(summary)
}

fn apply_change(change: &ResourceChange, summary: &mut ExecuteSummary, os: OsFamily) -> Result<()> {
    apply_change_one_in_with_os(change, ApplyContext::Unprivileged, os)?;
    match change.action {
        Action::Create => summary.added += 1,
        Action::Update => summary.changed += 1,
        Action::Run => summary.ran += 1,
        Action::NoOp => {}
    }
    Ok(())
}

/// A `(manager, action)` group inside one package phase: every package
/// change in the phase that maps to the same manager + action becomes
/// one batched subprocess. Names are owned (cloned out of the slice)
/// so the worker thread can take them across `std::thread::scope`.
#[derive(Debug, Clone)]
struct BatchGroup {
    manager: PackageManager,
    action: Action,
    names: Vec<String>,
}

/// Result of running one [`BatchGroup`] inside a parallel package phase.
struct GroupOutcome {
    group: BatchGroup,
    result: Result<BatchOutput>,
}

fn run_package_phase(
    phase: &[ResourceChange],
    summary: &mut ExecuteSummary,
    os: OsFamily,
) -> Result<()> {
    let groups = group_package_changes(phase)?;
    if groups.is_empty() {
        return Ok(());
    }
    if groups.len() == 1 {
        let group = groups.into_iter().next().expect("len == 1");
        run_group_inherit(&group, os)?;
        bump_summary(summary, group.action, group.names.len());
        return Ok(());
    }
    run_phase_parallel(groups, summary, os)
}

/// Walk a phase slice, classify each change, group `Create` changes
/// by `(manager, action)`. `NoOp` package changes are tracked by the
/// caller (their addresses still go into `applied`) but don't produce
/// a group. Returns groups in stable manager-and-action order so the
/// parallel scheduler and the post-phase flush share a single
/// canonical ordering.
fn group_package_changes(phase: &[ResourceChange]) -> Result<Vec<BatchGroup>> {
    let mut buckets: BTreeMap<(usize, usize), BatchGroup> = BTreeMap::new();
    for change in phase {
        if change.action == Action::NoOp {
            continue;
        }
        let state = change
            .after
            .as_ref()
            .or(change.before.as_ref())
            .with_context(|| format!("package change `{}` has no state", change.address))?;
        let ResourceState::Package { manager, name, .. } = state else {
            bail!(
                "non-package state inside package phase for `{}`",
                change.address,
            );
        };
        if !matches!(change.action, Action::Create) {
            bail!(
                "unsupported action {:?} on package `{}`; `keron apply` only ensures presence, expected Create/NoOp",
                change.action,
                change.address,
            );
        }
        let key = (manager_order(*manager), action_order(change.action));
        let entry = buckets.entry(key).or_insert_with(|| BatchGroup {
            manager: *manager,
            action: change.action,
            names: Vec::new(),
        });
        entry.names.push(name.clone());
    }
    Ok(buckets.into_values().collect())
}

fn run_group_inherit(group: &BatchGroup, os: OsFamily) -> Result<()> {
    let refs: Vec<&str> = group.names.iter().map(String::as_str).collect();
    debug_assert!(
        matches!(group.action, Action::Create),
        "package batches only carry Create — apply does not upgrade",
    );
    packages::install_many(group.manager, &refs, os)
}

fn run_group_capture(group: &BatchGroup, os: OsFamily) -> Result<BatchOutput> {
    let refs: Vec<&str> = group.names.iter().map(String::as_str).collect();
    debug_assert!(
        matches!(group.action, Action::Create),
        "package batches only carry Create — apply does not upgrade",
    );
    packages::install_many_captured(group.manager, &refs, os)
}

fn run_phase_parallel(
    groups: Vec<BatchGroup>,
    summary: &mut ExecuteSummary,
    os: OsFamily,
) -> Result<()> {
    let outcomes: Vec<GroupOutcome> = std::thread::scope(|s| {
        // `collect::<Vec<_>>()` here is load-bearing: it materialises
        // every spawn before any join, so the workers actually run in
        // parallel. A chained `.map(spawn).map(join)` would interleave
        // spawn-then-join lazily and serialise the phase. The
        // `needless_collect` lint is therefore wrong in this context.
        #[allow(clippy::needless_collect)]
        let handles: Vec<(BatchGroup, _)> = groups
            .into_iter()
            .map(|group| {
                let captured = group.clone();
                let handle = s.spawn(move || run_group_capture(&captured, os));
                (group, handle)
            })
            .collect();
        handles
            .into_iter()
            .map(|(group, handle)| {
                let result = handle.join().unwrap_or_else(|_| {
                    Err(anyhow::anyhow!(
                        "batch worker for `{} {}` panicked",
                        group.manager.label(),
                        action_label(group.action),
                    ))
                });
                GroupOutcome { group, result }
            })
            .collect()
    });

    flush_phase_outputs(&outcomes);

    let mut first_err: Option<anyhow::Error> = None;
    let mut succeeded: Vec<&BatchGroup> = Vec::new();
    let mut failed_descriptions: Vec<String> = Vec::new();
    for outcome in &outcomes {
        match &outcome.result {
            Ok(_) => succeeded.push(&outcome.group),
            Err(e) => {
                failed_descriptions.push(describe_group(&outcome.group));
                if first_err.is_none() {
                    let cause = format!("{e:#}");
                    first_err = Some(anyhow::anyhow!(cause));
                }
            }
        }
    }
    for group in &succeeded {
        bump_summary(summary, group.action, group.names.len());
    }
    if let Some(err) = first_err {
        let mut ctx = format!(
            "package phase failed: {} batch(es) failed:",
            failed_descriptions.len(),
        );
        for desc in &failed_descriptions {
            ctx.push_str("\n  ✗ ");
            ctx.push_str(desc);
        }
        if !succeeded.is_empty() {
            use std::fmt::Write as _;
            let _ = write!(
                ctx,
                "\n{} batch(es) succeeded in the same phase:",
                succeeded.len(),
            );
            for group in &succeeded {
                ctx.push_str("\n  ✓ ");
                ctx.push_str(&describe_group(group));
            }
        }
        return Err(err.context(ctx));
    }
    Ok(())
}

/// Flush each captured batch's stdout / stderr to the parent terminal
/// in stable manager order. Runs after the parallel scope joins so the
/// user sees one full manager block at a time. Errors from the stdout
/// / stderr writers are intentionally ignored — losing the flush is
/// strictly less bad than masking the underlying batch failure.
fn flush_phase_outputs(outcomes: &[GroupOutcome]) {
    let stdout = io::stdout();
    let stderr = io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    flush_phase_outputs_to(outcomes, &mut out, &mut err);
}

/// Writer-parameterised variant used by [`flush_phase_outputs`] and by
/// unit tests that need to capture the rendered flush into a `Vec<u8>`
/// instead of going to the real terminal.
fn flush_phase_outputs_to(outcomes: &[GroupOutcome], out: &mut dyn Write, err: &mut dyn Write) {
    for outcome in outcomes {
        let label = describe_group(&outcome.group);
        let status = outcome.result.as_ref().map_or("FAILED", |_| "ok");
        let _ = writeln!(out, "--- {label} [{status}] ---");
        let (stdout_bytes, stderr_bytes) = outcome
            .result
            .as_ref()
            .map_or((&[][..], &[][..]), |captured| {
                (captured.stdout.as_slice(), captured.stderr.as_slice())
            });
        let _ = out.write_all(stdout_bytes);
        let _ = err.write_all(stderr_bytes);
    }
}

fn describe_group(group: &BatchGroup) -> String {
    format!(
        "{} {} ({} package{}: {})",
        group.manager.label(),
        action_label(group.action),
        group.names.len(),
        if group.names.len() == 1 { "" } else { "s" },
        group.names.join(", "),
    )
}

/// Stable ordering for the manager + action display. Drives both the
/// `BTreeMap` keys in [`group_package_changes`] and the flush order in
/// [`flush_phase_outputs`].
const fn manager_order(m: PackageManager) -> usize {
    match m {
        PackageManager::Brew => 0,
        PackageManager::BrewCask => 1,
        PackageManager::Cargo => 2,
        PackageManager::Winget => 3,
    }
}

const fn action_order(a: Action) -> usize {
    match a {
        Action::Create => 0,
        Action::Update => 1,
        Action::Run => 2,
        Action::NoOp => 3,
    }
}

const fn action_label(a: Action) -> &'static str {
    match a {
        Action::Create => "install",
        Action::Update => "upgrade",
        Action::Run => "run",
        Action::NoOp => "noop",
    }
}

const fn bump_summary(summary: &mut ExecuteSummary, action: Action, count: usize) {
    match action {
        Action::Create => summary.added += count,
        Action::Update => summary.changed += count,
        Action::Run => summary.ran += count,
        Action::NoOp => {}
    }
}

/// Privilege context in which a change is being applied.
///
/// The elevated child routes Create actions through `openat`-based
/// component walks so a concurrent symlink swap in an intermediate
/// directory can't redirect a root-owned write. Unprivileged
/// execution uses the simpler `fs::create_dir_all` + leaf-write
/// dance because the user can't escalate via their own writes.
#[derive(Debug, Clone, Copy)]
pub enum ApplyContext {
    /// Run by the user themselves.
    Unprivileged,
    /// Run by the elevated child (sudo / `ShellExecuteExW`).
    Elevated,
}

/// Apply a single change. Shared between the in-process executor and
/// the elevated child entry point so the two stay in lockstep.
///
/// # Errors
/// Errors when the underlying filesystem call fails or when the
/// resource kind has no executor support yet for the action.
/// Apply a single change with an explicit privilege context. The
/// elevated child should always pass [`ApplyContext::Elevated`] so
/// Create actions route through the TOCTOU-safe `openat` walk
/// (`elevated::safe_write`).
///
/// # Errors
/// Errors when the underlying filesystem call fails or when the
/// resource kind has no executor support yet for the action.
pub fn apply_change_one_in(change: &ResourceChange, ctx: ApplyContext) -> Result<()> {
    // Single-change entry: snapshot OS on the caller's thread and pass
    // it down. Same rationale as `execute` — keeps the package
    // validation path off the `OsOverride` thread-local.
    apply_change_one_in_with_os(change, ctx, detect_os_family())
}

fn apply_change_one_in_with_os(
    change: &ResourceChange,
    ctx: ApplyContext,
    os: OsFamily,
) -> Result<()> {
    match change.action {
        Action::NoOp => Ok(()),
        Action::Create => {
            let state = change
                .after
                .as_ref()
                .with_context(|| format!("create `{}` has no desired state", change.address))?;
            apply_create(state, ctx, os).with_context(|| format!("creating `{}`", change.address))
        }
        Action::Update => {
            let before = change
                .before
                .as_ref()
                .with_context(|| format!("update `{}` has no prior state", change.address))?;
            let after = change
                .after
                .as_ref()
                .with_context(|| format!("update `{}` has no desired state", change.address))?;
            // Update reuses the existing fs::* path on both Unix and
            // Windows. The `before` state proves the leaf already
            // existed at plan time; the residual TOCTOU window (stat
            // ↔ rename, or stat ↔ remove+create) is narrower than
            // Create's mkdir-then-symlink race and is documented in
            // the elevated/child.rs ancestor pre-check.
            apply_update(before, after).with_context(|| format!("updating `{}`", change.address))
        }
        Action::Run => {
            let state = change
                .after
                .as_ref()
                .with_context(|| format!("run `{}` has no desired state", change.address))?;
            apply_run(state).with_context(|| format!("running `{}`", change.address))
        }
    }
}

fn apply_create(state: &ResourceState, ctx: ApplyContext, os: OsFamily) -> Result<()> {
    match state {
        ResourceState::Symlink { from, to } => create_symlink(from, to, ctx),
        ResourceState::Template {
            path,
            content,
            sensitive,
        } => create_template(path, content, *sensitive, ctx),
        ResourceState::Package { manager, name, .. } => {
            // `apply_change_one_in` is the single-change entry used by
            // the elevated child (and a couple of tests). The main
            // executor routes Package changes through the phased /
            // batched path in [`execute`]; reaching this arm means a
            // singleton dispatch, so `install_many` with a one-element
            // slice is the natural reuse — same validation, same argv,
            // no duplicated spawn code.
            packages::install_many(*manager, &[name.as_str()], os)
        }
        ResourceState::Tap(spec) => packages::tap(spec, Action::Create),
        ResourceState::SshKey {
            private_path,
            public_path,
            private_key,
            public_key,
        } => create_ssh_key(private_path, public_path, private_key, public_key),
        ResourceState::GpgKey { fingerprint, key } => create_gpg_key(fingerprint, key),
        ResourceState::Shell { .. } => bail!(unsupported_kind(state)),
    }
}

fn apply_run(state: &ResourceState) -> Result<()> {
    let ResourceState::Shell {
        kind,
        name,
        cwd,
        script,
        ..
    } = state
    else {
        bail!(unsupported_kind(state));
    };
    run_shell(*kind, name, cwd, script)
}

fn apply_update(before: &ResourceState, after: &ResourceState) -> Result<()> {
    match (before, after) {
        (
            ResourceState::Symlink { from: bt, .. },
            ResourceState::Symlink {
                from: at,
                to: source,
            },
        ) => {
            // Planner guarantees matched target on both sides; bail
            // loudly if that invariant ever drifts.
            if bt != at {
                bail!(
                    "symlink update target mismatch: `{}` vs `{}`",
                    bt.display(),
                    at.display(),
                );
            }
            remove_symlink(bt)?;
            create_symlink(at, source, ApplyContext::Unprivileged)
        }
        (
            ResourceState::Template { path: bp, .. },
            ResourceState::Template {
                path: ap,
                content,
                sensitive,
            },
        ) => {
            if bp != ap {
                bail!(
                    "template update target mismatch: `{}` vs `{}`",
                    bp.display(),
                    ap.display(),
                );
            }
            replace_template(ap, content, *sensitive)
        }
        // `keron apply` does not upgrade packages — the classifier
        // never produces `Update` for `ResourceState::Package`, so
        // reaching this arm means a planner bug. Fail loudly with the
        // address so the bug is easy to locate.
        (ResourceState::Package { .. }, ResourceState::Package { name, .. }) => {
            bail!(
                "package `{name}` reached the Update path; `keron apply` only ensures presence — upgrade is the user's responsibility (e.g. `brew upgrade`). This indicates a planner bug.",
            )
        }
        // Tap Update is a remote-URL drift — re-tap with
        // `--custom-remote` so brew rewrites the local git remote in
        // place. No untap+retap (which would force a re-clone).
        (ResourceState::Tap(_), ResourceState::Tap(after_spec)) => {
            packages::tap(after_spec, Action::Update)
        }
        _ => bail!(unsupported_kind(after)),
    }
}

fn create_symlink(target: &Path, source: &Path, ctx: ApplyContext) -> Result<()> {
    #[cfg(unix)]
    if matches!(ctx, ApplyContext::Elevated)
        && let Some(parent_path) = target.parent()
        && let Some(leaf) = target.file_name()
    {
        // Walk each ancestor with O_NOFOLLOW; symlinkat onto the
        // resulting parent fd. Closes the TOCTOU window between
        // `mkdir_all(parent)` and `symlink(2)` that the elevated
        // child would otherwise be racing.
        let parent = crate::elevated::safe_write::ParentDir::open(parent_path)
            .with_context(|| format!("opening elevated parent of `{}`", target.display()))?;
        return crate::elevated::safe_write::symlink_at(&parent, leaf, source).with_context(|| {
            format!(
                "creating symlink target `{}` from source `{}` (elevated)",
                target.display(),
                source.display()
            )
        });
    }
    let _ = ctx;
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent directory `{}`", parent.display()))?;
    }
    symlink_impl(source, target).with_context(|| {
        format!(
            "creating symlink target `{}` from source `{}`",
            target.display(),
            source.display()
        )
    })
}

fn remove_symlink(path: &Path) -> Result<()> {
    // `symlink_metadata` does not traverse the link, so a broken
    // symlink (target missing) still reports `is_symlink()`.
    let meta =
        fs::symlink_metadata(path).with_context(|| format!("inspecting `{}`", path.display()))?;
    if !meta.file_type().is_symlink() {
        bail!("`{}` is not a symlink; refusing to remove", path.display());
    }
    fs::remove_file(path).with_context(|| format!("removing symlink `{}`", path.display()))
}

fn create_template(path: &Path, content: &str, sensitive: bool, ctx: ApplyContext) -> Result<()> {
    let mode = create_mode(sensitive);
    #[cfg(unix)]
    if matches!(ctx, ApplyContext::Elevated)
        && let Some(parent_path) = path.parent()
        && let Some(leaf) = path.file_name()
    {
        let parent = crate::elevated::safe_write::ParentDir::open(parent_path)
            .with_context(|| format!("opening elevated parent of `{}`", path.display()))?;
        let mut file = crate::elevated::safe_write::create_file_at(&parent, leaf, mode)
            .with_context(|| format!("creating template `{}` (elevated)", path.display()))?;
        file.write_all(content.as_bytes())
            .with_context(|| format!("writing template `{}`", path.display()))?;
        return file
            .sync_all()
            .with_context(|| format!("syncing template `{}`", path.display()));
    }
    let _ = ctx;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent directory `{}`", parent.display()))?;
    }
    let mut file = open_new_leaf_no_follow(path, mode)
        .with_context(|| format!("creating template `{}`", path.display()))?;
    file.write_all(content.as_bytes())
        .with_context(|| format!("writing template `{}`", path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing template `{}`", path.display()))
}

fn replace_template(path: &Path, content: &str, sensitive: bool) -> Result<()> {
    let meta =
        fs::symlink_metadata(path).with_context(|| format!("inspecting `{}`", path.display()))?;
    if !meta.file_type().is_file() {
        bail!(
            "`{}` is not a regular file; refusing to replace",
            path.display()
        );
    }
    let tmp = temp_sibling(path);
    let mode = replace_mode(sensitive, &meta);
    let guard = TmpFileGuard::new(tmp.clone());
    let mut file = open_new_leaf_no_follow(&tmp, mode)
        .with_context(|| format!("creating temporary template `{}`", tmp.display()))?;
    file.write_all(content.as_bytes())
        .with_context(|| format!("writing temporary template `{}`", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing temporary template `{}`", tmp.display()))?;
    drop(file);
    fs::rename(&tmp, path).with_context(|| {
        format!(
            "atomically replacing `{}` with `{}`",
            path.display(),
            tmp.display()
        )
    })?;
    guard.disarm();
    Ok(())
}

/// Write an SSH keypair to disk as a single atomic create.
///
/// Modes are hard-coded — `0o600` for the private half, `0o644` for
/// the public half — and the parent directory is materialised at
/// `0o700` on Unix (the conventional `~/.ssh` permission). Writes go
/// through [`open_new_leaf_no_follow`] so a hostile symlink at either
/// path is rejected rather than followed.
///
/// The classifier guarantees both files are missing before this runs
/// (it errors out on any prior occupant), so no atomic-replace dance
/// is needed; if the second write fails after the first has landed,
/// the next plan classifies as "out of sync" and tells the user to
/// clean up. That asymmetric state is unusual enough — only reached
/// by an interrupted apply — that surfacing it on the next run is the
/// right ergonomics.
fn create_ssh_key(
    private_path: &Path,
    public_path: &Path,
    private_key: &str,
    public_key: &str,
) -> Result<()> {
    ensure_ssh_parent(private_path)?;
    ensure_ssh_parent(public_path)?;
    write_ssh_file(private_path, private_key.as_bytes(), 0o600)?;
    write_ssh_file(public_path, public_key.as_bytes(), 0o644)?;
    Ok(())
}

/// Create the parent directory of an SSH key file at `0o700` on Unix.
/// The mode is only enforced when we create the directory ourselves —
/// an existing `~/.ssh` is left as the user configured it (chmod'ing
/// down without warning would be a surprise; chmod'ing up would
/// degrade their setup).
fn ensure_ssh_parent(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    match fs::symlink_metadata(parent) {
        Ok(meta) if meta.file_type().is_dir() => Ok(()),
        Ok(_) => bail!(
            "ssh key parent `{}` exists and is not a directory; refusing to overwrite",
            parent.display()
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating parent directory `{}`", parent.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("setting `{}` to mode 0700", parent.display()))?;
            }
            Ok(())
        }
        Err(e) => Err(anyhow::anyhow!(
            "inspecting ssh key parent `{}`: {e}",
            parent.display()
        )),
    }
}

fn write_ssh_file(path: &Path, content: &[u8], mode: u32) -> Result<()> {
    let mut file = open_new_leaf_no_follow(path, mode)
        .with_context(|| format!("creating ssh key file `{}`", path.display()))?;
    file.write_all(content)
        .with_context(|| format!("writing ssh key file `{}`", path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing ssh key file `{}`", path.display()))
}

/// Import a GPG secret-key blob into the user's keyring.
///
/// `gpg --batch --import` reads the blob from child stdin. We never
/// pass it via argv (visible in `/proc/<pid>/cmdline`) and never stage
/// it through a tempfile. Stdout is piped to `/dev/null` so gpg's
/// keyring listing never enters keron's memory — the only signal we
/// consume is the exit status. Stderr is inherited so any gpg error
/// (bad pinentry, malformed blob) reaches the user's terminal
/// directly.
///
/// After a successful import, we re-probe `gpg --list-secret-keys
/// <fingerprint>` to confirm the blob actually carried the declared
/// fingerprint. Without this check, a user pointing at the wrong
/// op:// secret would import some other key, the classifier would
/// continue to see the declared fingerprint as absent, and `apply`
/// would loop importing on every run.
fn create_gpg_key(fingerprint: &str, key: &str) -> Result<()> {
    which::which("gpg").context("`gpg` is not available on PATH")?;
    let mut child = Command::new("gpg")
        .args(["--batch", "--import"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawning `gpg --batch --import`")?;
    {
        let mut stdin = child
            .stdin
            .take()
            .context("opening stdin for `gpg --batch --import`")?;
        stdin
            .write_all(key.as_bytes())
            .context("writing key material to `gpg --batch --import`")?;
    }
    let status = child
        .wait()
        .context("waiting for `gpg --batch --import` to finish")?;
    if !status.success() {
        bail!("`gpg --batch --import` exited with status {status} for fingerprint `{fingerprint}`");
    }
    let probe = Command::new("gpg")
        .args([
            "--batch",
            "--list-secret-keys",
            "--with-colons",
            fingerprint,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("re-probing gpg keyring after import")?;
    if !probe.success() {
        bail!(
            "`gpg --import` succeeded but fingerprint `{fingerprint}` was not added to the keyring; \
             the supplied key likely has a different fingerprint than declared",
        );
    }
    Ok(())
}

/// Removes `path` on drop unless [`Self::disarm`] is called first.
/// Used by [`replace_template`] so every failure path between the
/// `open(.tmp)` and the final `rename` cleans up the sibling temp
/// — including panic unwinding, where the previous
/// `let _ = fs::remove_file(&tmp)` would never have run.
struct TmpFileGuard {
    path: std::path::PathBuf,
    disarmed: bool,
}

impl TmpFileGuard {
    const fn new(path: std::path::PathBuf) -> Self {
        Self {
            path,
            disarmed: false,
        }
    }

    fn disarm(mut self) {
        self.disarmed = true;
    }
}

impl Drop for TmpFileGuard {
    fn drop(&mut self) {
        if !self.disarmed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Permission bits to use when creating a fresh template file.
/// Sensitive content (rendered with `unwrap_secret(...)` anywhere
/// in `vars`) lands at `0o600` so a secret-bearing render of e.g.
/// `~/.netrc` doesn't briefly exist world-readable. Non-sensitive
/// templates keep the standard `0o644`-after-umask behavior.
#[cfg_attr(not(unix), allow(clippy::needless_pass_by_value))]
const fn create_mode(sensitive: bool) -> u32 {
    if sensitive { 0o600 } else { 0o644 }
}

/// Mode for the replacement tempfile. Preserves the existing file's
/// permissions so `chmod 600 ~/.ssh/config` survives an idempotent
/// update; if sensitive, additionally clamps group/other bits off.
#[cfg(unix)]
fn replace_mode(sensitive: bool, existing: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    let existing_mode = existing.mode() & 0o777;
    if sensitive {
        existing_mode & 0o700
    } else {
        existing_mode
    }
}

// `#[mutants::skip]` because this branch only compiles on non-unix
// hosts (Windows ignores the mode argument when opening files), and
// the CI mutant runner is unix-only — no test on a unix host can
// execute it.
#[cfg(not(unix))]
#[cfg_attr(test, mutants::skip)]
const fn replace_mode(_sensitive: bool, _existing: &fs::Metadata) -> u32 {
    0
}

fn temp_sibling(path: &Path) -> std::path::PathBuf {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map_or_else(|| "template".into(), std::ffi::OsStr::to_os_string);
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(format!(
        ".keron-tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    parent.join(tmp_name)
}

fn open_new_leaf_no_follow(path: &Path, mode: u32) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode).custom_flags(libc::O_NOFOLLOW);
    }

    #[cfg(not(unix))]
    {
        let _ = mode;
    }

    options.open(path)
}

fn unsupported_kind(state: &ResourceState) -> String {
    let kind = match state {
        ResourceState::Symlink { .. } => ResourceKind::Symlink,
        ResourceState::Template { .. } => ResourceKind::Template,
        ResourceState::Package { .. } => ResourceKind::Package,
        ResourceState::Tap(_) => ResourceKind::Tap,
        ResourceState::Shell { .. } => ResourceKind::Shell,
        ResourceState::SshKey { .. } => ResourceKind::SshKey,
        ResourceState::GpgKey { .. } => ResourceKind::GpgKey,
    };
    format!(
        "executor not yet implemented for {} resources",
        kind.label()
    )
}

fn run_shell(kind: ShellKind, name: &str, cwd: &Path, script: &str) -> Result<()> {
    let shell_path = which::which(kind.label())
        .with_context(|| format!("shell `{}` is not available on PATH", kind.label()))?;
    let mut command = Command::new(shell_path);
    command.current_dir(cwd);
    match kind {
        ShellKind::Sh | ShellKind::Bash | ShellKind::Zsh => {
            command.arg("-s");
        }
        ShellKind::Pwsh | ShellKind::Powershell => {
            command.args(["-NoProfile", "-NonInteractive", "-Command", "-"]);
        }
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("spawning shell resource `{name}`"))?;
    let mut stdin = child
        .stdin
        .take()
        .with_context(|| format!("opening stdin for shell resource `{name}`"))?;
    stdin
        .write_all(script.as_bytes())
        .with_context(|| format!("writing script for shell resource `{name}`"))?;
    drop(stdin);
    let status = child
        .wait()
        .with_context(|| format!("waiting for shell resource `{name}`"))?;
    if status.success() {
        Ok(())
    } else {
        bail!("shell resource `{name}` exited with {status}")
    }
}

#[cfg(unix)]
fn symlink_impl(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink_impl(target: &Path, link: &Path) -> io::Result<()> {
    // Windows splits file vs directory symlinks at the API level; pick
    // the right call so dotfile flows that link whole config dirs
    // (`~/.config/nvim` -> `<repo>/nvim`) work without ceremony.
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{PackageManager, ResourceKind, ShellKind};
    use std::env;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let p = env::temp_dir().join(format!(
                "keron-execute-test-{tag}-{}-{n}",
                std::process::id()
            ));
            if p.exists() {
                fs::remove_dir_all(&p).ok();
            }
            fs::create_dir_all(&p).unwrap();
            Self { path: p }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    struct CwdFile {
        path: PathBuf,
    }

    impl CwdFile {
        fn new(tag: &str) -> Self {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = PathBuf::from(format!(".keron-execute-{tag}-{}-{n}", std::process::id()));
            if path.exists() {
                fs::remove_file(&path).ok();
            }
            Self { path }
        }
    }

    impl Drop for CwdFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    fn change(
        action: Action,
        before: Option<ResourceState>,
        after: Option<ResourceState>,
    ) -> ResourceChange {
        let probe = before
            .as_ref()
            .or(after.as_ref())
            .expect("a change must have at least one state");
        ResourceChange {
            address: match probe {
                ResourceState::Symlink { from, .. } => from.display().to_string(),
                ResourceState::Template { path, .. } => path.display().to_string(),
                ResourceState::Package { manager, name, .. } => {
                    format!("{}:{}", manager.kind_label(), name)
                }
                ResourceState::Tap(spec) => format!("tap:{}", spec.user_tap),
                ResourceState::Shell { name, .. } => name.clone(),
                ResourceState::SshKey { private_path, .. } => private_path.display().to_string(),
                ResourceState::GpgKey { fingerprint, .. } => format!("gpg:{fingerprint}"),
            },
            kind: match probe {
                ResourceState::Symlink { .. } => ResourceKind::Symlink,
                ResourceState::Template { .. } => ResourceKind::Template,
                ResourceState::Package { .. } => ResourceKind::Package,
                ResourceState::Tap(_) => ResourceKind::Tap,
                ResourceState::Shell { .. } => ResourceKind::Shell,
                ResourceState::SshKey { .. } => ResourceKind::SshKey,
                ResourceState::GpgKey { .. } => ResourceKind::GpgKey,
            },
            action,
            before,
            after,
            requires_elevation: false,
            requires_force: false,
        }
    }

    #[cfg(unix)]
    fn write_noop_binary(dir: &std::path::Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("noop.sh");
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
        path
    }

    #[cfg(windows)]
    fn write_noop_binary(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("noop.bat");
        fs::write(&path, "@echo off\r\nexit /b 0\r\n").unwrap();
        path
    }

    #[cfg(unix)]
    fn write_fake_shell(dir: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("sh");
        fs::write(
            &path,
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > \"$KERON_TEST_SHELL_ARGS\"\n\
             pwd > \"$KERON_TEST_SHELL_CWD\"\n\
             /bin/cat > \"$KERON_TEST_SHELL_STDIN\"\n\
             echo shell-stdout\n\
             echo shell-stderr >&2\n\
             exit \"$KERON_TEST_SHELL_EXIT\"\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).unwrap();
    }

    #[cfg(unix)]
    struct ShellEnvGuard {
        original_path: Option<std::ffi::OsString>,
        original_args: Option<std::ffi::OsString>,
        original_cwd: Option<std::ffi::OsString>,
        original_stdin: Option<std::ffi::OsString>,
        original_exit: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    #[cfg(unix)]
    impl ShellEnvGuard {
        fn set(
            path: &std::path::Path,
            args: &std::path::Path,
            cwd: &std::path::Path,
            stdin: &std::path::Path,
            exit: Option<&str>,
        ) -> Self {
            static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
            let lock = LOCK
                .get_or_init(|| std::sync::Mutex::new(()))
                .lock()
                .unwrap();
            let guard = Self {
                original_path: env::var_os("PATH"),
                original_args: env::var_os("KERON_TEST_SHELL_ARGS"),
                original_cwd: env::var_os("KERON_TEST_SHELL_CWD"),
                original_stdin: env::var_os("KERON_TEST_SHELL_STDIN"),
                original_exit: env::var_os("KERON_TEST_SHELL_EXIT"),
                _lock: lock,
            };
            // SAFETY: this test guard serializes process-env mutation and restores on drop.
            #[allow(unsafe_code)]
            unsafe {
                env::set_var("PATH", path);
                env::set_var("KERON_TEST_SHELL_ARGS", args);
                env::set_var("KERON_TEST_SHELL_CWD", cwd);
                env::set_var("KERON_TEST_SHELL_STDIN", stdin);
                env::set_var("KERON_TEST_SHELL_EXIT", exit.unwrap_or("0"));
            }
            guard
        }
    }

    #[cfg(unix)]
    impl Drop for ShellEnvGuard {
        fn drop(&mut self) {
            restore_env("PATH", self.original_path.as_ref());
            restore_env("KERON_TEST_SHELL_ARGS", self.original_args.as_ref());
            restore_env("KERON_TEST_SHELL_CWD", self.original_cwd.as_ref());
            restore_env("KERON_TEST_SHELL_STDIN", self.original_stdin.as_ref());
            restore_env("KERON_TEST_SHELL_EXIT", self.original_exit.as_ref());
        }
    }

    #[cfg(unix)]
    fn restore_env(key: &str, value: Option<&std::ffi::OsString>) {
        // SAFETY: this test guard serializes process-env mutation and restores on drop.
        #[allow(unsafe_code)]
        unsafe {
            if let Some(value) = value {
                env::set_var(key, value);
            } else {
                env::remove_var(key);
            }
        }
    }

    #[test]
    fn create_symlink_writes_link_on_disk() {
        let d = TempDir::new("create");
        let target = d.path.join("real");
        fs::write(&target, "hi").unwrap();
        let link = d.path.join("alias");

        let plan = Plan {
            changes: vec![change(
                Action::Create,
                None,
                Some(ResourceState::Symlink {
                    from: link.clone(),
                    to: target.clone(),
                }),
            )],
        };
        let summary = execute(&plan).unwrap();
        assert_eq!(summary.added, 1);
        assert_eq!(summary.changed, 0);
        let resolved = fs::read_link(&link).unwrap();
        assert_eq!(resolved, target);
    }

    #[test]
    fn create_symlink_creates_missing_parent_directories() {
        let d = TempDir::new("create-parent");
        let target = d.path.join("real");
        fs::write(&target, "hi").unwrap();
        let link = d.path.join("a/b/c/alias");

        let plan = Plan {
            changes: vec![change(
                Action::Create,
                None,
                Some(ResourceState::Symlink {
                    from: link.clone(),
                    to: target.clone(),
                }),
            )],
        };
        execute(&plan).unwrap();
        assert!(link.is_symlink(), "missing symlink at {}", link.display());
        let resolved = fs::read_link(&link).unwrap();
        assert_eq!(resolved, target);
    }

    #[test]
    fn update_symlink_replaces_existing_target() {
        let d = TempDir::new("update");
        let old_target = d.path.join("old");
        let new_target = d.path.join("new");
        fs::write(&old_target, "old").unwrap();
        fs::write(&new_target, "new").unwrap();
        let link = d.path.join("alias");
        symlink_impl(&old_target, &link).unwrap();

        let plan = Plan {
            changes: vec![change(
                Action::Update,
                Some(ResourceState::Symlink {
                    from: link.clone(),
                    to: old_target,
                }),
                Some(ResourceState::Symlink {
                    from: link.clone(),
                    to: new_target.clone(),
                }),
            )],
        };
        let summary = execute(&plan).unwrap();
        assert_eq!(summary.changed, 1);
        let resolved = fs::read_link(&link).unwrap();
        assert_eq!(resolved, new_target);
    }

    #[test]
    fn noop_change_does_nothing() {
        let d = TempDir::new("noop");
        let target = d.path.join("real");
        fs::write(&target, "hi").unwrap();
        let link = d.path.join("alias");
        symlink_impl(&target, &link).unwrap();

        let plan = Plan {
            changes: vec![change(
                Action::NoOp,
                Some(ResourceState::Symlink {
                    from: link.clone(),
                    to: target.clone(),
                }),
                Some(ResourceState::Symlink {
                    from: link,
                    to: target,
                }),
            )],
        };
        let summary = execute(&plan).unwrap();
        assert_eq!(summary.added, 0);
        assert_eq!(summary.changed, 0);
    }

    #[test]
    fn summary_tallies_each_action_independently() {
        let d = TempDir::new("mixed");
        let target = d.path.join("real");
        fs::write(&target, "hi").unwrap();
        let to_create = d.path.join("a");
        let to_update_link = d.path.join("b");
        let old_target = d.path.join("old");
        fs::write(&old_target, "old").unwrap();
        symlink_impl(&old_target, &to_update_link).unwrap();

        let plan = Plan {
            changes: vec![
                change(
                    Action::Create,
                    None,
                    Some(ResourceState::Symlink {
                        from: to_create,
                        to: target.clone(),
                    }),
                ),
                change(
                    Action::Update,
                    Some(ResourceState::Symlink {
                        from: to_update_link.clone(),
                        to: old_target,
                    }),
                    Some(ResourceState::Symlink {
                        from: to_update_link,
                        to: target,
                    }),
                ),
            ],
        };
        let summary = execute(&plan).unwrap();
        assert_eq!(summary.added, 1);
        assert_eq!(summary.changed, 1);
    }

    #[test]
    fn create_template_writes_file_with_content() {
        let d = TempDir::new("template-create");
        let path = d.path.join("nested").join("config.toml");
        let plan = Plan {
            changes: vec![change(
                Action::Create,
                None,
                Some(ResourceState::Template {
                    path: path.clone(),
                    content: "key = \"value\"\n".into(),
                    sensitive: false,
                }),
            )],
        };
        let summary = execute(&plan).unwrap();
        assert_eq!(summary.added, 1);
        let written = fs::read_to_string(&path).expect("file written");
        assert_eq!(written, "key = \"value\"\n");
    }

    #[test]
    fn update_template_overwrites_content() {
        let d = TempDir::new("template-update");
        let path = d.path.join("config.toml");
        fs::write(&path, "old contents\n").unwrap();
        let plan = Plan {
            changes: vec![change(
                Action::Update,
                Some(ResourceState::Template {
                    path: path.clone(),
                    content: "old contents\n".into(),
                    sensitive: false,
                }),
                Some(ResourceState::Template {
                    path: path.clone(),
                    content: "new contents\n".into(),
                    sensitive: false,
                }),
            )],
        };
        let summary = execute(&plan).unwrap();
        assert_eq!(summary.changed, 1);
        let written = fs::read_to_string(&path).expect("file written");
        assert_eq!(written, "new contents\n");
    }

    #[test]
    fn update_template_handles_relative_leaf_paths() {
        let file = CwdFile::new("relative-template-update");
        fs::write(&file.path, "old").unwrap();
        replace_template(&file.path, "new", false).unwrap();
        assert_eq!(fs::read_to_string(&file.path).unwrap(), "new");
    }

    #[cfg(unix)]
    #[test]
    fn create_template_sensitive_writes_mode_0600() {
        use std::os::unix::fs::MetadataExt;
        let d = TempDir::new("sensitive-create");
        let path = d.path.join("creds");
        create_template(&path, "TOKEN=hunter2\n", true, ApplyContext::Unprivileged).unwrap();
        let mode = fs::metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "sensitive template must be owner-only: {mode:o}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn replace_template_preserves_existing_mode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let d = TempDir::new("preserve-mode");
        let path = d.path.join("config");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        replace_template(&path, "new", false).unwrap();
        let mode = fs::metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600, "existing mode should be preserved: {mode:o}");
    }

    #[cfg(unix)]
    #[test]
    fn replace_template_sensitive_clamps_group_other_bits() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let d = TempDir::new("clamp-mode");
        let path = d.path.join("creds");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        replace_template(&path, "new", true).unwrap();
        let mode = fs::metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "sensitive replace must drop group/other bits: {mode:o}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn create_ssh_key_writes_private_at_0600_and_public_at_0644() {
        use std::os::unix::fs::MetadataExt;
        let d = TempDir::new("ssh-create");
        let priv_path = d.path.join("id_ed25519");
        let pub_path = d.path.join("id_ed25519.pub");
        create_ssh_key(
            &priv_path,
            &pub_path,
            "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\n",
            "ssh-ed25519 AAAA host\n",
        )
        .unwrap();
        let priv_mode = fs::metadata(&priv_path).unwrap().mode() & 0o777;
        let pub_mode = fs::metadata(&pub_path).unwrap().mode() & 0o777;
        assert_eq!(
            priv_mode, 0o600,
            "private key must be owner-only: {priv_mode:o}"
        );
        assert_eq!(pub_mode, 0o644, "public key mode: {pub_mode:o}");
        // Content survives byte-for-byte.
        assert!(fs::read_to_string(&priv_path).unwrap().contains("OPENSSH"));
        assert_eq!(
            fs::read_to_string(&pub_path).unwrap(),
            "ssh-ed25519 AAAA host\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn create_ssh_key_creates_parent_dir_at_0700() {
        use std::os::unix::fs::MetadataExt;
        let d = TempDir::new("ssh-parent");
        let ssh_dir = d.path.join(".ssh");
        let priv_path = ssh_dir.join("id_ed25519");
        let pub_path = ssh_dir.join("id_ed25519.pub");
        assert!(!ssh_dir.exists(), "fixture invariant: parent absent");
        create_ssh_key(&priv_path, &pub_path, "PRIV", "ssh-ed25519 AAAA").unwrap();
        let mode = fs::metadata(&ssh_dir).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o700, "parent dir must be 0700: {mode:o}");
    }

    #[test]
    fn temp_sibling_for_relative_leaf_uses_current_dir_parent() {
        let tmp = temp_sibling(Path::new("config.toml"));
        assert_eq!(tmp.parent(), Some(Path::new(".")));
        let name = tmp.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with(".config.toml.keron-tmp-"), "got: {tmp:?}");
    }

    #[test]
    fn open_new_leaf_creates_the_requested_path() {
        let d = TempDir::new("open-new-leaf");
        let path = d.path.join("leaf");
        let mut file = open_new_leaf_no_follow(&path, 0o644).unwrap();
        file.write_all(b"x").unwrap();
        drop(file);
        assert_eq!(fs::read_to_string(path).unwrap(), "x");
    }

    #[cfg(unix)]
    #[test]
    fn open_new_leaf_refuses_symlink_leaf() {
        let d = TempDir::new("open-new-leaf-symlink");
        let real = d.path.join("real");
        fs::write(&real, "original").unwrap();
        let link = d.path.join("link");
        symlink_impl(&real, &link).unwrap();
        let err = open_new_leaf_no_follow(&link, 0o644).expect_err("symlink leaf must not open");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(real).unwrap(), "original");
    }

    fn pkg_change(manager: PackageManager, name: &str, action: Action) -> ResourceChange {
        let state = ResourceState::Package {
            manager,
            name: name.into(),
            tap: None,
        };
        change(action, Some(state.clone()), Some(state))
    }

    #[test]
    fn group_package_changes_buckets_by_manager_in_stable_order() {
        // Plan declaration order is intentionally mixed to pin that
        // group_package_changes sorts by manager_order — the same key
        // the parallel flush relies on.
        let phase = vec![
            pkg_change(PackageManager::Cargo, "sccache", Action::Create),
            pkg_change(PackageManager::Brew, "fd", Action::Create),
            pkg_change(PackageManager::BrewCask, "alacritty", Action::Create),
            pkg_change(PackageManager::Brew, "ripgrep", Action::Create),
        ];
        let groups = group_package_changes(&phase).unwrap();
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].manager, PackageManager::Brew);
        assert_eq!(groups[0].action, Action::Create);
        assert_eq!(groups[0].names, vec!["fd", "ripgrep"]);
        assert_eq!(groups[1].manager, PackageManager::BrewCask);
        assert_eq!(groups[1].names, vec!["alacritty"]);
        assert_eq!(groups[2].manager, PackageManager::Cargo);
        assert_eq!(groups[2].names, vec!["sccache"]);
    }

    #[test]
    fn group_package_changes_rejects_update_on_package() {
        // The classifier never returns Update for packages, so if one
        // appears here it indicates a planner bug — surface it loudly
        // instead of silently routing into a no-longer-existing
        // upgrade path.
        let phase = vec![pkg_change(PackageManager::Brew, "ripgrep", Action::Update)];
        let err = group_package_changes(&phase).expect_err("Update on Package must error");
        assert!(
            format!("{err:#}").contains("only ensures presence"),
            "got: {err:#}",
        );
    }

    #[test]
    fn group_package_changes_drops_noops() {
        let phase = vec![
            pkg_change(PackageManager::Brew, "ripgrep", Action::Create),
            pkg_change(PackageManager::Brew, "fd", Action::NoOp),
            pkg_change(PackageManager::Cargo, "sccache", Action::NoOp),
        ];
        let groups = group_package_changes(&phase).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].manager, PackageManager::Brew);
        assert_eq!(groups[0].names, vec!["ripgrep"]);
    }

    #[test]
    fn flush_phase_outputs_writes_in_stable_manager_order() {
        // Outcomes intentionally provided out of stable order to pin
        // that flush prints brew before cargo regardless of the input
        // sequence. (The real flow gets stable order from
        // group_package_changes' BTreeMap, but the flush function
        // itself should still write in the order it received — pinning
        // that contract here keeps the two pieces honest.)
        let cargo_group = BatchGroup {
            manager: PackageManager::Cargo,
            action: Action::Create,
            names: vec!["sccache".into()],
        };
        let brew_group = BatchGroup {
            manager: PackageManager::Brew,
            action: Action::Create,
            names: vec!["ripgrep".into(), "fd".into()],
        };
        let outcomes = vec![
            GroupOutcome {
                group: brew_group,
                result: Ok(BatchOutput {
                    stdout: b"brew-out\n".to_vec(),
                    stderr: Vec::new(),
                }),
            },
            GroupOutcome {
                group: cargo_group,
                result: Ok(BatchOutput {
                    stdout: b"cargo-out\n".to_vec(),
                    stderr: Vec::new(),
                }),
            },
        ];
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        flush_phase_outputs_to(&outcomes, &mut out, &mut err);
        let rendered = String::from_utf8(out).unwrap();
        let brew_idx = rendered.find("brew install").expect("brew header present");
        let cargo_idx = rendered
            .find("cargo install")
            .expect("cargo header present");
        assert!(
            brew_idx < cargo_idx,
            "brew block must precede cargo block; got: {rendered:?}",
        );
        let brew_payload_idx = rendered.find("brew-out").expect("brew payload present");
        let cargo_payload_idx = rendered.find("cargo-out").expect("cargo payload present");
        assert!(brew_payload_idx < cargo_payload_idx);
        assert!(brew_idx < brew_payload_idx);
        assert!(cargo_idx < cargo_payload_idx);
    }

    #[test]
    fn flush_phase_outputs_marks_failed_batches_in_the_banner() {
        let group = BatchGroup {
            manager: PackageManager::Brew,
            action: Action::Create,
            names: vec!["does-not-exist".into()],
        };
        let outcomes = vec![GroupOutcome {
            group,
            result: Err(anyhow::anyhow!("spy refused")),
        }];
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        flush_phase_outputs_to(&outcomes, &mut out, &mut err);
        let rendered = String::from_utf8(out).unwrap();
        assert!(
            rendered.contains("[FAILED]"),
            "failed batch banner must be marked: {rendered:?}",
        );
    }

    #[cfg(unix)]
    fn write_argv_recording_spy(dir: &std::path::Path, log: &std::path::Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("argv-spy.sh");
        // Each invocation appends one line to the log: "<binary>\t<arg1>,<arg2>,…"
        // — arg ',' is impossible in our argv (we validate package names), so the
        // simple comma split in the assertion is unambiguous. Newline-separated
        // entries are atomic appends ≤ PIPE_BUF on every relevant platform, so
        // parallel writes from multiple workers don't interleave.
        let script = "#!/bin/sh\n\
                      log=\"$KERON_TEST_ARGV_LOG\"\n\
                      printf '%s\\t' \"$0\" >> \"$log\"\n\
                      first=1\n\
                      for a in \"$@\"; do\n\
                      \tif [ \"$first\" -eq 1 ]; then\n\
                      \t\tprintf '%s' \"$a\" >> \"$log\"\n\
                      \t\tfirst=0\n\
                      \telse\n\
                      \t\tprintf ',%s' \"$a\" >> \"$log\"\n\
                      \tfi\n\
                      done\n\
                      printf '\\n' >> \"$log\"\n\
                      exit 0\n";
        fs::write(&path, script).unwrap();
        let mut perm = fs::metadata(&path).unwrap().permissions();
        perm.set_mode(0o755);
        fs::set_permissions(&path, perm).unwrap();
        let _ = log;
        path
    }

    #[cfg(unix)]
    #[test]
    fn execute_collapses_multiple_brew_packages_into_one_subprocess() {
        let _g = crate::packages::lock_env();
        let _os = crate::platform::OsOverride::set(crate::platform::OsFamily::Macos);
        let d = TempDir::new("batch-brew");
        let log = d.path.join("argv.log");
        let spy = write_argv_recording_spy(&d.path, &log);
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("KERON_ALLOW_TEST_OVERRIDES", "1");
            std::env::set_var("KERON_TEST_PACKAGE_BIN_BREW", &spy);
            std::env::set_var("KERON_TEST_ARGV_LOG", &log);
        }
        let plan = Plan {
            changes: vec![
                pkg_change(PackageManager::Brew, "ripgrep", Action::Create),
                pkg_change(PackageManager::Brew, "bat", Action::Create),
                pkg_change(PackageManager::Brew, "fd", Action::Create),
            ],
        };
        let result = execute(&plan);
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("KERON_TEST_PACKAGE_BIN_BREW");
            std::env::remove_var("KERON_TEST_ARGV_LOG");
            std::env::remove_var("KERON_ALLOW_TEST_OVERRIDES");
        }
        let summary = result.expect("spy should accept all batched packages");
        assert_eq!(summary.added, 3, "all three packages count toward added");
        let recorded = fs::read_to_string(&log).expect("log written");
        let lines: Vec<&str> = recorded.lines().collect();
        assert_eq!(
            lines.len(),
            1,
            "three brew packages must collapse into one subprocess; got: {recorded:?}",
        );
        let line = lines[0];
        let (_binary, args_str) = line.split_once('\t').expect("tab-separated row");
        let args: Vec<&str> = args_str.split(',').collect();
        assert_eq!(args, vec!["install", "ripgrep", "bat", "fd"]);
    }

    #[cfg(unix)]
    #[test]
    fn execute_runs_distinct_manager_batches_concurrently() {
        // Plan: 2 brew Create + 1 cargo Create + 1 cask Create, all in
        // one Package phase → 3 batches (brew/cask share the brew bin
        // but spawn separate subprocesses). Each spy sleeps 0.8 s.
        // Sequential = 2.4 s; ideal parallel ≈ 0.8 s + spawn overhead.
        // Threshold is 1.7 s — strictly under sequential, with a wide
        // margin above ideal-parallel so a busy CI machine isn't
        // flaky.
        use std::os::unix::fs::PermissionsExt;
        let _g = crate::packages::lock_env();
        let _os = crate::platform::OsOverride::set(crate::platform::OsFamily::Macos);
        let d = TempDir::new("batch-parallel");
        let log = d.path.join("argv.log");
        let spy_path = d.path.join("sleepy-spy.sh");
        let script = "#!/bin/sh\n\
                      log=\"$KERON_TEST_ARGV_LOG\"\n\
                      sleep 0.8\n\
                      printf '%s\\t' \"$0\" >> \"$log\"\n\
                      first=1\n\
                      for a in \"$@\"; do\n\
                      \tif [ \"$first\" -eq 1 ]; then\n\
                      \t\tprintf '%s' \"$a\" >> \"$log\"\n\
                      \t\tfirst=0\n\
                      \telse\n\
                      \t\tprintf ',%s' \"$a\" >> \"$log\"\n\
                      \tfi\n\
                      done\n\
                      printf '\\n' >> \"$log\"\n\
                      exit 0\n";
        fs::write(&spy_path, script).unwrap();
        let mut perm = fs::metadata(&spy_path).unwrap().permissions();
        perm.set_mode(0o755);
        fs::set_permissions(&spy_path, perm).unwrap();
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("KERON_ALLOW_TEST_OVERRIDES", "1");
            std::env::set_var("KERON_TEST_PACKAGE_BIN_BREW", &spy_path);
            std::env::set_var("KERON_TEST_PACKAGE_BIN_CARGO", &spy_path);
            std::env::set_var("KERON_TEST_ARGV_LOG", &log);
        }
        let plan = Plan {
            changes: vec![
                pkg_change(PackageManager::Brew, "ripgrep", Action::Create),
                pkg_change(PackageManager::Brew, "fd", Action::Create),
                pkg_change(PackageManager::Cargo, "sccache", Action::Create),
                pkg_change(PackageManager::BrewCask, "alacritty", Action::Create),
            ],
        };
        let start = std::time::Instant::now();
        let result = execute(&plan);
        let elapsed = start.elapsed();
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("KERON_TEST_PACKAGE_BIN_BREW");
            std::env::remove_var("KERON_TEST_PACKAGE_BIN_CARGO");
            std::env::remove_var("KERON_TEST_ARGV_LOG");
            std::env::remove_var("KERON_ALLOW_TEST_OVERRIDES");
        }
        let summary = result.expect("all spies should succeed");
        assert_eq!(summary.added, 4);
        assert!(
            elapsed < std::time::Duration::from_millis(1700),
            "batches must run in parallel; took {elapsed:?} (sequential lower bound ≈ 2.4 s)",
        );
        // Sanity: three subprocess invocations recorded — brew (2 names),
        // cask (1 name), cargo (1 name).
        let recorded = fs::read_to_string(&log).expect("log written");
        assert_eq!(
            recorded.lines().count(),
            3,
            "expected three subprocess invocations, got: {recorded:?}",
        );
    }

    #[cfg(unix)]
    #[test]
    fn execute_failed_batch_in_parallel_phase_reports_succeeded_and_failed() {
        use std::os::unix::fs::PermissionsExt;
        let _g = crate::packages::lock_env();
        let _os = crate::platform::OsOverride::set(crate::platform::OsFamily::Macos);
        let d = TempDir::new("batch-mixed");
        let ok_spy = d.path.join("ok.sh");
        fs::write(&ok_spy, "#!/bin/sh\necho cargo-ok\nexit 0\n").unwrap();
        let mut perm = fs::metadata(&ok_spy).unwrap().permissions();
        perm.set_mode(0o755);
        fs::set_permissions(&ok_spy, perm).unwrap();
        let fail_spy = d.path.join("fail.sh");
        fs::write(&fail_spy, "#!/bin/sh\necho >&2 spy-refused\nexit 7\n").unwrap();
        let mut perm = fs::metadata(&fail_spy).unwrap().permissions();
        perm.set_mode(0o755);
        fs::set_permissions(&fail_spy, perm).unwrap();
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("KERON_ALLOW_TEST_OVERRIDES", "1");
            std::env::set_var("KERON_TEST_PACKAGE_BIN_BREW", &fail_spy);
            std::env::set_var("KERON_TEST_PACKAGE_BIN_CARGO", &ok_spy);
        }
        let plan = Plan {
            changes: vec![
                pkg_change(PackageManager::Brew, "broken", Action::Create),
                pkg_change(PackageManager::Cargo, "sccache", Action::Create),
            ],
        };
        let result = execute(&plan);
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("KERON_TEST_PACKAGE_BIN_BREW");
            std::env::remove_var("KERON_TEST_PACKAGE_BIN_CARGO");
            std::env::remove_var("KERON_ALLOW_TEST_OVERRIDES");
        }
        let err = result.expect_err("failed batch must surface as an error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("brew install (1 package: broken)"),
            "error must name the failing batch; got: {msg}",
        );
        assert!(
            msg.contains("cargo install (1 package: sccache)"),
            "error must list the sibling success; got: {msg}",
        );
        assert!(
            msg.contains("batch(es) succeeded"),
            "error must call out the succeeded batch; got: {msg}",
        );
    }

    #[test]
    fn create_package_dispatches_to_packages_install() {
        // Share the process-wide env lock with packages::tests so a
        // concurrent test there doesn't clobber `KERON_ALLOW_TEST_OVERRIDES`
        // mid-classify.
        let _g = crate::packages::lock_env();
        let _os = crate::platform::OsOverride::set(crate::platform::OsFamily::Macos);
        let d = TempDir::new("package-noop");
        let noop = write_noop_binary(&d.path);
        // SAFETY: edition 2024 env mutation; the ENV_LOCK guard above
        // serialises against the packages::tests env mutators.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("KERON_ALLOW_TEST_OVERRIDES", "1");
            std::env::set_var("KERON_TEST_PACKAGE_BIN_BREW", noop);
        }
        let plan = Plan {
            changes: vec![change(
                Action::Create,
                None,
                Some(ResourceState::Package {
                    manager: PackageManager::Brew,
                    name: "ripgrep".into(),
                    tap: None,
                }),
            )],
        };
        let result = execute(&plan);
        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("KERON_TEST_PACKAGE_BIN_BREW");
            std::env::remove_var("KERON_ALLOW_TEST_OVERRIDES");
        }
        let summary = result.expect("install spy should succeed");
        assert_eq!(summary.added, 1);
    }

    #[cfg(unix)]
    #[test]
    fn run_shell_writes_script_to_stdin_and_uses_cwd() {
        let d = TempDir::new("shell-run");
        write_fake_shell(&d.path);
        let args = d.path.join("args");
        let cwd_file = d.path.join("cwd");
        let stdin_file = d.path.join("stdin");
        let _env = ShellEnvGuard::set(&d.path, &args, &cwd_file, &stdin_file, None);
        let plan = Plan {
            changes: vec![change(
                Action::Run,
                None,
                Some(ResourceState::Shell {
                    kind: ShellKind::Sh,
                    name: "refresh".into(),
                    cwd: d.path.clone(),
                    script: "echo one\necho two\n".into(),
                    sensitive: false,
                }),
            )],
        };
        let summary = execute(&plan).unwrap();
        assert_eq!(summary.ran, 1);
        assert_eq!(fs::read_to_string(args).unwrap(), "-s\n");
        assert_eq!(
            fs::read_to_string(cwd_file).unwrap().trim(),
            fs::canonicalize(&d.path).unwrap().display().to_string()
        );
        assert_eq!(
            fs::read_to_string(stdin_file).unwrap(),
            "echo one\necho two\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_shell_nonzero_exit_fails_with_context() {
        let d = TempDir::new("shell-nonzero");
        write_fake_shell(&d.path);
        let _env = ShellEnvGuard::set(
            &d.path,
            &d.path.join("args"),
            &d.path.join("cwd"),
            &d.path.join("stdin"),
            Some("7"),
        );
        let plan = Plan {
            changes: vec![change(
                Action::Run,
                None,
                Some(ResourceState::Shell {
                    kind: ShellKind::Sh,
                    name: "fail".into(),
                    cwd: d.path.clone(),
                    script: "exit 7\n".into(),
                    sensitive: false,
                }),
            )],
        };
        let err = execute(&plan).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("running `fail`"), "got: {msg}");
        assert!(msg.contains("exited"), "got: {msg}");
    }

    #[cfg(unix)]
    #[test]
    fn run_shell_rechecks_missing_shell_before_spawn() {
        let d = TempDir::new("shell-missing");
        let _env = ShellEnvGuard::set(
            &d.path,
            &d.path.join("args"),
            &d.path.join("cwd"),
            &d.path.join("stdin"),
            None,
        );
        let plan = Plan {
            changes: vec![change(
                Action::Run,
                None,
                Some(ResourceState::Shell {
                    kind: ShellKind::Bash,
                    name: "missing".into(),
                    cwd: d.path.clone(),
                    script: "echo ok\n".into(),
                    sensitive: false,
                }),
            )],
        };
        let err = execute(&plan).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("shell `bash` is not available on PATH"));
    }

    #[test]
    fn create_shell_action_reports_shell_executor_mismatch() {
        let d = TempDir::new("shell-create-mismatch");
        let plan = Plan {
            changes: vec![change(
                Action::Create,
                None,
                Some(ResourceState::Shell {
                    kind: ShellKind::Sh,
                    name: "refresh".into(),
                    cwd: d.path.clone(),
                    script: "echo ok\n".into(),
                    sensitive: false,
                }),
            )],
        };
        let err = execute(&plan).expect_err("shell create action must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("executor not yet implemented for shell resources"),
            "got: {msg}"
        );
    }

    #[test]
    fn tmp_file_guard_removes_file_on_drop_when_armed() {
        // Pins `Drop::drop with ()` mutation: if drop is a no-op,
        // an interrupted `replace_template` would leak its sibling
        // tempfile. Also pins `delete ! in drop`: with the inversion,
        // an armed guard would skip removal.
        let d = TempDir::new("guard-armed");
        let path = d.path.join("leaked-tmp");
        fs::write(&path, "scratch").unwrap();
        assert!(path.exists(), "fixture invariant");
        {
            let _g = TmpFileGuard::new(path.clone());
        } // drop here
        assert!(
            !path.exists(),
            "armed guard's drop must delete the tempfile: {path:?}"
        );
    }

    #[test]
    fn tmp_file_guard_disarm_prevents_removal_on_drop() {
        // Pins `TmpFileGuard::disarm with ()`: if disarm fails to set
        // the flag, the drop path still fires and silently removes
        // the file the caller just renamed into place — losing the
        // template content. Also pins `delete ! in drop`: with the
        // inversion, a disarmed guard would still remove the file.
        let d = TempDir::new("guard-disarmed");
        let path = d.path.join("survives");
        fs::write(&path, "kept").unwrap();
        {
            let g = TmpFileGuard::new(path.clone());
            g.disarm();
        }
        assert!(
            path.exists(),
            "disarmed guard must NOT delete the file: {path:?}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "kept");
    }

    #[test]
    fn empty_plan_executes_with_zero_summary() {
        let summary = execute(&Plan::default()).unwrap();
        assert_eq!(summary.added, 0);
        assert_eq!(summary.changed, 0);
    }

    #[test]
    fn update_aborts_when_path_changes_between_before_and_after() {
        let d = TempDir::new("update-drift");
        let plan = Plan {
            changes: vec![change(
                Action::Update,
                Some(ResourceState::Symlink {
                    from: d.path.join("a"),
                    to: d.path.join("t1"),
                }),
                Some(ResourceState::Symlink {
                    from: d.path.join("b"),
                    to: d.path.join("t2"),
                }),
            )],
        };
        let err = execute(&plan).unwrap_err();
        assert!(
            format!("{err:#}").contains("target mismatch"),
            "got: {err:#}"
        );
    }
}
