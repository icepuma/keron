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

/// A non-fatal issue encountered while applying a plan. Collected and
/// surfaced to the user rather than aborting the run, because none of
/// them block the outcome the user asked for — only degrade some
/// secondary behaviour. Kept as a typed enum (not a free-form string)
/// so callers can pattern-match and tests can assert on the variant.
#[derive(Debug, Clone)]
pub enum Warning {
    /// A managed tap was registered (`brew tap` succeeded) but
    /// `brew trust` failed. Brew 6.0 fully-qualified installs
    /// auto-trust per-item, so dependent installs still succeed; only
    /// `brew doctor` and bare-name installs from the tap are degraded.
    TapUntrusted { user_tap: String, error: String },
}

impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TapUntrusted { user_tap, error } => write!(
                f,
                "tap `{user_tap}` registered but not trusted; brew 6.0 may warn or \
                 refuse bare-name installs from it: {error}",
            ),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ExecuteSummary {
    pub added: usize,
    pub changed: usize,
    pub ran: usize,
    pub warnings: Vec<Warning>,
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
    apply_change_one_in_with_os(
        change,
        ApplyContext::Unprivileged,
        os,
        &mut summary.warnings,
    )?;
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
    // validation path off the `OsOverride` thread-local. Warnings are
    // dropped here: this entry serves the elevated child (which reports
    // outcomes via its own channel) and single-change tests, and no
    // warning-producing resource (Tap) is ever elevated.
    let mut warnings = Vec::new();
    apply_change_one_in_with_os(change, ctx, detect_os_family(), &mut warnings)
}

fn apply_change_one_in_with_os(
    change: &ResourceChange,
    ctx: ApplyContext,
    os: OsFamily,
    warnings: &mut Vec<Warning>,
) -> Result<()> {
    match change.action {
        Action::NoOp => Ok(()),
        Action::Create => {
            let state = change
                .after
                .as_ref()
                .with_context(|| format!("create `{}` has no desired state", change.address))?;
            apply_create(state, ctx, os, warnings)
                .with_context(|| format!("creating `{}`", change.address))
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
            // Elevated updates route through the same `openat`/`renameat`
            // safe-write walk as Create (anchored to an `O_NOFOLLOW`
            // parent fd), and every update replaces atomically via a
            // temp-then-rename so the target is never left missing.
            apply_update(before, after, ctx, warnings)
                .with_context(|| format!("updating `{}`", change.address))
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

fn apply_create(
    state: &ResourceState,
    ctx: ApplyContext,
    os: OsFamily,
    warnings: &mut Vec<Warning>,
) -> Result<()> {
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
        ResourceState::Tap(spec) => packages::tap(spec, Action::Create, warnings),
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

fn apply_update(
    before: &ResourceState,
    after: &ResourceState,
    ctx: ApplyContext,
    warnings: &mut Vec<Warning>,
) -> Result<()> {
    match (before, after) {
        (
            ResourceState::Symlink {
                from: bt,
                to: before_to,
            },
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
            // Re-verify the live link still matches the state the user
            // approved at the force prompt, then replace atomically so
            // the old link is never destroyed without a replacement.
            reverify_symlink_before(bt, before_to)?;
            replace_symlink(at, source, ctx)
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
            replace_template(ap, content, *sensitive, ctx)
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
            packages::tap(after_spec, Action::Update, warnings)
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

/// Confirm the live symlink at `target` still points where the plan's
/// `before` snapshot recorded. The force prompt approved replacing
/// *that* state; if the link changed (or is no longer a symlink) since
/// the plan was rendered, bail rather than clobber something the user
/// never saw in the diff. Both targets come from `read_link`, so an
/// exact comparison is correct.
fn reverify_symlink_before(target: &Path, before_to: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(target)
        .with_context(|| format!("re-inspecting `{}` before update", target.display()))?;
    if !meta.file_type().is_symlink() {
        bail!(
            "`{}` is no longer a symlink (changed since the plan was rendered); re-run `keron apply`",
            target.display()
        );
    }
    let live = fs::read_link(target)
        .with_context(|| format!("re-reading symlink `{}` before update", target.display()))?;
    if live != before_to {
        bail!(
            "`{}` changed since the plan was rendered (now points at `{}`, the plan expected `{}`); re-run `keron apply`",
            target.display(),
            live.display(),
            before_to.display(),
        );
    }
    Ok(())
}

/// Atomically re-point `target` at `source`. A temp sibling symlink is
/// created and `rename`d over the target, which on Unix replaces an
/// existing symlink atomically — so a failure can never leave the
/// target missing (the previous remove-then-create could). The elevated
/// path performs the create + rename relative to an `O_NOFOLLOW` parent
/// fd, closing the same ancestor-swap TOCTOU window that Create's
/// safe-write walk closes.
fn replace_symlink(target: &Path, source: &Path, ctx: ApplyContext) -> Result<()> {
    let tmp = temp_sibling(target);
    #[cfg(unix)]
    if matches!(ctx, ApplyContext::Elevated)
        && let Some(parent_path) = target.parent()
        && let Some(leaf) = target.file_name()
        && let Some(tmp_leaf) = tmp.file_name()
    {
        let parent = crate::elevated::safe_write::ParentDir::open(parent_path)
            .with_context(|| format!("opening elevated parent of `{}`", target.display()))?;
        crate::elevated::safe_write::symlink_at(&parent, tmp_leaf, source).with_context(|| {
            format!(
                "creating temporary symlink for `{}` (elevated)",
                target.display()
            )
        })?;
        return crate::elevated::safe_write::rename_at(&parent, tmp_leaf, leaf)
            .with_context(|| format!("atomically re-pointing `{}` (elevated)", target.display()));
    }
    let _ = ctx;
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent directory `{}`", parent.display()))?;
    }
    let guard = TmpFileGuard::new(tmp.clone());
    symlink_impl(source, &tmp)
        .with_context(|| format!("creating temporary symlink `{}`", tmp.display()))?;
    fs::rename(&tmp, target).with_context(|| {
        format!(
            "atomically re-pointing `{}` via `{}`",
            target.display(),
            tmp.display()
        )
    })?;
    guard.disarm();
    Ok(())
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

fn replace_template(path: &Path, content: &str, sensitive: bool, ctx: ApplyContext) -> Result<()> {
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

    // Elevated replace: build the temp file and rename it over the
    // target relative to an `O_NOFOLLOW` parent fd, so a root-owned
    // template update gets the same ancestor-swap protection as Create
    // instead of re-resolving the full path with `fs::*`.
    #[cfg(unix)]
    if matches!(ctx, ApplyContext::Elevated)
        && let Some(parent_path) = path.parent()
        && let Some(leaf) = path.file_name()
        && let Some(tmp_leaf) = tmp.file_name()
    {
        use std::os::unix::fs::PermissionsExt;
        let parent = crate::elevated::safe_write::ParentDir::open(parent_path)
            .with_context(|| format!("opening elevated parent of `{}`", path.display()))?;
        let mut file = crate::elevated::safe_write::create_file_at(&parent, tmp_leaf, mode)
            .with_context(|| {
                format!("creating temporary template `{}` (elevated)", tmp.display())
            })?;
        file.set_permissions(fs::Permissions::from_mode(mode))
            .with_context(|| format!("setting mode on temporary template `{}`", tmp.display()))?;
        file.write_all(content.as_bytes())
            .with_context(|| format!("writing temporary template `{}`", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing temporary template `{}`", tmp.display()))?;
        drop(file);
        return crate::elevated::safe_write::rename_at(&parent, tmp_leaf, leaf)
            .with_context(|| format!("atomically replacing `{}` (elevated)", path.display()));
    }
    let _ = ctx;

    let guard = TmpFileGuard::new(tmp.clone());
    let mut file = open_new_leaf_no_follow(&tmp, mode)
        .with_context(|| format!("creating temporary template `{}`", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // `open(2)`'s mode argument is masked by the process umask, so
        // an open alone would silently drop preserved bits (a 0644 file
        // under `umask 077` would become 0600). `fchmod` to the exact
        // mode so "preserve the existing file's permissions" actually
        // holds — and so a sensitive clamp lands precisely at 0600.
        file.set_permissions(fs::Permissions::from_mode(mode))
            .with_context(|| format!("setting mode on temporary template `{}`", tmp.display()))?;
    }
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
    validate_gpg_fingerprint(fingerprint)?;
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
    check_gpg_import_status(status.success(), &status.to_string(), fingerprint)?;
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
    check_gpg_probe_status(probe.success(), fingerprint)
}

/// Reject a fingerprint that isn't a plain hex key id before it reaches
/// `gpg --list-secret-keys <fingerprint>` as a positional argv. A
/// leading `-` would otherwise be parsed as a flag (argument injection),
/// the same class of hole the package-name / tap-URL validators close.
/// Accepts the canonical 40-hex-char form, optional `0x` prefix, and
/// space-grouped digits; nothing else.
fn validate_gpg_fingerprint(fingerprint: &str) -> Result<()> {
    let body = fingerprint
        .strip_prefix("0x")
        .or_else(|| fingerprint.strip_prefix("0X"))
        .unwrap_or(fingerprint);
    let hex: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!(
            "gpg fingerprint `{fingerprint}` is not a valid hex fingerprint; expected hex digits (optionally `0x`-prefixed)"
        );
    }
    Ok(())
}

/// Pure helper: turn the `gpg --import` exit status into Ok / Err with
/// the canonical diagnostic. Factored out so the success-gate can be
/// unit-tested without spawning a real gpg.
fn check_gpg_import_status(ok: bool, status_label: &str, fingerprint: &str) -> Result<()> {
    if !ok {
        bail!(
            "`gpg --batch --import` exited with status {status_label} for fingerprint `{fingerprint}`",
        );
    }
    Ok(())
}

/// Pure helper: turn the post-import `gpg --list-secret-keys` exit
/// status into Ok / Err. Factored out so the wrong-fingerprint
/// detection branch can be unit-tested without spawning a real gpg.
fn check_gpg_probe_status(ok: bool, fingerprint: &str) -> Result<()> {
    if !ok {
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

fn symlink_impl(target: &Path, link: &Path) -> io::Result<()> {
    // The `symlink` crate's `symlink_auto` forwards to
    // `std::os::unix::fs::symlink` on Unix and probes the target's
    // file-or-directory kind on Windows so dotfile flows that link
    // whole config dirs (`~/.config/nvim` -> `<repo>/nvim`) work
    // without ceremony.
    symlink::symlink_auto(target, link)
}

#[cfg(test)]
#[path = "execute_tests.rs"]
mod tests;
